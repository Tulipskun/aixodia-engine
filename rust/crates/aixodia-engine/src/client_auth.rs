//! Cloudflare Access guard for the engine's HTTP and WebSocket surface.
//!
//! Sign-in moved out of the engine. Cloudflare Access sits in front of the
//! engine's hostname, runs the GitHub / one-time-PIN login, and stamps every
//! request it forwards with a `Cf-Access-Jwt-Assertion` header. The engine's
//! only remaining job is to verify that JWT locally: it fetches the team's
//! public keys from `https://<team>/cdn-cgi/access/certs`, checks the signature,
//! `aud`, `exp` and issuer, and lets the request through.
//!
//! Why this replaced the old scheme: the engine used to run the GitHub code
//! exchange itself, which made it a single point of failure for sign-in — if
//! the engine was down, nobody could log in even though GitHub had already
//! authorised them. Nothing here calls GitHub, and nothing here needs the
//! engine to be reachable to *obtain* a session.
//!
//! `CF_TOKEN` (the master token) is still honoured: the local CLI and the
//! in-process tools have no Access cookie to present, and the engine also uses
//! it as a bearer over loopback.

use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aixodia_db::Client as DbClient;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::json;

/// Always-usable default for `ALLOWED_ORIGINS` when neither env nor KV set it.
const DEFAULT_ALLOWED_ORIGINS: &str = "https://dashboard.aixodia.dpdns.org,http://localhost:5173";

/// The header Cloudflare Access adds to every request it authorises.
const ACCESS_JWT_HEADER: &str = "cf-access-jwt-assertion";

/// How long a fetched key set is trusted before it is fetched again. Access
/// rotates keys rarely; ten minutes keeps rotation latency small without
/// hammering the certs endpoint.
const JWKS_TTL: Duration = Duration::from_secs(600);

/// Grace period served from cache when the certs endpoint cannot be reached,
/// so a Cloudflare blip does not lock the operator out of their own engine.
const JWKS_OUTAGE_GRACE: Duration = Duration::from_secs(6 * 60 * 60);

/// Access signs with RSA and publishes `kid`s, so keys are cached per `kid`.
#[derive(Debug, Clone, Deserialize)]
struct AccessClaims {
    aud: Vec<String>,
    iss: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    sub: Option<String>,
    exp: u64,
}

/// One published signing key, as returned by `/cdn-cgi/access/certs`.
#[derive(Debug, Clone, Deserialize)]
struct Jwk {
    kid: String,
    #[serde(default)]
    n: String,
    #[serde(default)]
    e: String,
    #[serde(default)]
    alg: Option<String>,
}

#[derive(Debug, Deserialize)]
struct JwksDocument {
    keys: Vec<Jwk>,
}

/// Everything the guard needs, resolved once at startup.
pub struct ClientAuth {
    /// The Access team domain, e.g. `purple-queen-b699.cloudflareaccess.com`.
    team_domain: String,
    /// The Access application audience tag (`aud`) for this hostname.
    app_aud: String,
    /// Master token; still accepted as `Authorization: Bearer`.
    cf_token: String,
    allowed_origins: Vec<String>,
    db: DbClient,
    cache: tokio::sync::Mutex<KeyCache>,
}

struct KeyCache {
    keys: HashMap<String, DecodingKey>,
    fetched_at: Option<Instant>,
    last_success: Option<Instant>,
}

impl KeyCache {
    fn new() -> Self {
        Self {
            keys: HashMap::new(),
            fetched_at: None,
            last_success: None,
        }
    }

    fn fresh(&self) -> bool {
        self.fetched_at
            .is_some_and(|at| at.elapsed() < JWKS_TTL && !self.keys.is_empty())
    }

    fn usable_after_outage(&self) -> bool {
        self.last_success
            .is_some_and(|at| at.elapsed() < JWKS_OUTAGE_GRACE)
            && !self.keys.is_empty()
    }
}

fn parse_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

async fn resolve_allowed(
    env_name: &str,
    kv_key: &str,
    db: &DbClient,
    default: Option<&str>,
) -> Vec<String> {
    if let Ok(value) = env::var(env_name) {
        let parsed = parse_csv(&value);
        if !parsed.is_empty() {
            return parsed;
        }
    }
    match db.config_kv_get(kv_key).await {
        Ok(Some(value)) => {
            let parsed = parse_csv(&value);
            if !parsed.is_empty() {
                return parsed;
            }
        }
        Ok(None) => {}
        Err(err) => tracing::warn!("config KV {kv_key}: {err}"),
    }
    default.map(parse_csv).unwrap_or_default()
}

/// Env first, then Workers KV `AIXODIA_CONFIG`.
async fn resolve_scalar(env_name: &str, kv_key: &str, db: &DbClient) -> String {
    if let Ok(value) = env::var(env_name) {
        let trimmed = value.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    db.config_kv_get(kv_key)
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
        .trim()
        .to_string()
}

impl ClientAuth {
    /// `ACCESS_TEAM_DOMAIN` / `ACCESS_APP_AUD` come from env, else Workers KV
    /// `AIXODIA_CONFIG` (`access/*`). When either is missing the Access path
    /// fails closed and only the master token works.
    pub async fn from_env(cf_token: String, db: &DbClient) -> Arc<Self> {
        let team_domain = resolve_scalar("ACCESS_TEAM_DOMAIN", "access/team_domain", db)
            .await
            .trim_start_matches("https://")
            .trim_end_matches('/')
            .to_string();
        let app_aud = resolve_scalar("ACCESS_APP_AUD", "access/app_aud", db).await;
        let allowed_origins = resolve_allowed(
            "ALLOWED_ORIGINS",
            "client/allowed_origins",
            db,
            Some(DEFAULT_ALLOWED_ORIGINS),
        )
        .await;
        if team_domain.is_empty() || app_aud.is_empty() {
            tracing::warn!(
                "ACCESS_TEAM_DOMAIN/ACCESS_APP_AUD unset: Cloudflare Access sign-in is disabled; only the master token works"
            );
        }
        Arc::new(Self {
            team_domain,
            app_aud,
            cf_token,
            allowed_origins,
            db: db.clone(),
            cache: tokio::sync::Mutex::new(KeyCache::new()),
        })
    }

    /// Every route is guarded by `client_guard`; nothing needs a public router.
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/v1/auth/whoami", get(whoami))
            .with_state(self.clone())
    }

    fn access_enabled(&self) -> bool {
        !self.team_domain.is_empty() && !self.app_aud.is_empty()
    }

    fn master_matches(&self, presented: &str) -> bool {
        !self.cf_token.is_empty() && !presented.is_empty() && presented == self.cf_token
    }

    /// CORS headers for a preflight or a normal response. An unknown Origin
    /// gets no `Access-Control-Allow-Origin` at all, so the browser blocks it.
    fn cors(&self, origin: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::VARY, HeaderValue::from_static("Origin"));
        if let Some(origin) = origin.filter(|value| self.allowed_origins.iter().any(|o| o == value))
        {
            if let Ok(value) = HeaderValue::from_str(origin) {
                headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
            }
            headers.insert(
                header::ACCESS_CONTROL_ALLOW_METHODS,
                HeaderValue::from_static("GET, POST, DELETE, OPTIONS"),
            );
            headers.insert(
                header::ACCESS_CONTROL_ALLOW_HEADERS,
                HeaderValue::from_static("Authorization, Content-Type"),
            );
            headers.insert(
                header::ACCESS_CONTROL_MAX_AGE,
                HeaderValue::from_static("600"),
            );
        }
        headers
    }

    /// Fetch and cache the team's signing keys. Returns the keys to use, or an
    /// empty map when the endpoint is unreachable and nothing is cached.
    async fn signing_keys(&self) -> HashMap<String, DecodingKey> {
        {
            let cache = self.cache.lock().await;
            if cache.fresh() {
                return cache.keys.clone();
            }
        }

        let url = format!("https://{}/cdn-cgi/access/certs", self.team_domain);
        let fetched = async {
            let response = reqwest::Client::new()
                .get(&url)
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .map_err(|err| err.to_string())?
                .error_for_status()
                .map_err(|err| err.to_string())?
                .json::<JwksDocument>()
                .await
                .map_err(|err| err.to_string())?;
            let mut keys = HashMap::new();
            for jwk in response.keys {
                if jwk.n.is_empty() || jwk.e.is_empty() {
                    continue;
                }
                if let Ok(alg) = jwk.alg.as_deref().unwrap_or("RS256").parse::<Algorithm>() {
                    if let Ok(key) = DecodingKey::from_rsa_components(&jwk.n, &jwk.e) {
                        let _ = alg;
                        keys.insert(jwk.kid, key);
                    }
                }
            }
            Ok::<_, String>(keys)
        }
        .await;

        let mut cache = self.cache.lock().await;
        match fetched {
            Ok(keys) if !keys.is_empty() => {
                cache.keys = keys.clone();
                cache.fetched_at = Some(Instant::now());
                cache.last_success = Some(Instant::now());
                keys
            }
            Ok(_) => {
                tracing::warn!("Access certs endpoint returned no usable keys");
                cache.keys.clone()
            }
            Err(err) => {
                tracing::warn!("Access certs fetch failed: {err}");
                if cache.usable_after_outage() {
                    cache.keys.clone()
                } else {
                    HashMap::new()
                }
            }
        }
    }

    /// Verify a `Cf-Access-Jwt-Assertion` header value. Returns the subject
    /// (email when present, else `sub`) on success.
    async fn verify_access_jwt(&self, token: &str) -> Option<String> {
        if !self.access_enabled() || token.is_empty() {
            return None;
        }
        let header = decode_header(token).ok()?;
        let kid = header.kid.unwrap_or_default();
        let keys = self.signing_keys().await;
        let key = keys.get(&kid)?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[self.app_aud.as_str()]);
        validation.set_issuer(&[format!("https://{}", self.team_domain)]);
        validation.validate_exp = true;

        let data = decode::<AccessClaims>(token, key, &validation).ok()?;
        Some(
            data.claims
                .email
                .filter(|value| !value.is_empty())
                .or(data.claims.sub)
                .unwrap_or_default(),
        )
    }
}

fn bearer(headers: &HeaderMap) -> &str {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim()
        .trim_start_matches("Bearer ")
        .trim()
}

fn access_jwt(headers: &HeaderMap) -> &str {
    headers
        .get(ACCESS_JWT_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim()
}

fn with_cors(mut response: Response, cors: &HeaderMap) -> Response {
    for (name, value) in cors.iter() {
        response.headers_mut().insert(name, value.clone());
    }
    response
}

/// Rejects any request that carries neither an Access JWT nor the master token.
///
/// Paths are no longer individually opened for sign-in: there is no sign-in
/// route left in the engine, so only `/healthz` stays public (it has to, or
/// Cloudflare and local probes could not reach it).
pub async fn client_guard(
    State(auth): State<Arc<ClientAuth>>,
    request: Request,
    next: Next,
) -> Response {
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let cors = auth.cors(origin.as_deref());

    if request.method() == Method::OPTIONS {
        return with_cors(
            (StatusCode::NO_CONTENT, cors.clone()).into_response(),
            &cors,
        );
    }

    let path = request.uri().path().to_string();
    if path == "/healthz" {
        return with_cors(next.run(request).await, &cors);
    }

    let master = bearer(request.headers());
    if auth.master_matches(master) {
        return with_cors(next.run(request).await, &cors);
    }

    let jwt = access_jwt(request.headers());
    match auth.verify_access_jwt(jwt).await {
        Some(subject) if !subject.is_empty() => with_cors(next.run(request).await, &cors),
        _ => with_cors(
            (
                StatusCode::UNAUTHORIZED,
                cors.clone(),
                Json(json!({"error": "unauthorized", "code": "ACCESS_JWT_REJECTED"})),
            )
                .into_response(),
            &cors,
        ),
    }
}

/// Reports which credentials the caller presented. Useful when the app has to
/// tell "the Access session expired" apart from "the engine rejected me".
async fn whoami(State(auth): State<Arc<ClientAuth>>, headers: HeaderMap) -> Response {
    let master = auth.master_matches(bearer(&headers));
    let subject = auth.verify_access_jwt(access_jwt(&headers)).await;
    Json(json!({
        "master": master,
        "access_subject": subject,
        "access_enabled": auth.access_enabled(),
        "host": auth.team_domain,
    }))
    .into_response()
}
