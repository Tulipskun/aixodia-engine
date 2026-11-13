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
use axum::routing::{get, post};
use axum::{Json, Router};
use jsonwebtoken::{decode, decode_header, Algorithm, DecodingKey, Validation};
use serde::Deserialize;
use serde_json::json;

/// Always-usable default for `ALLOWED_ORIGINS` when neither env nor KV set it.
///
/// The console lives at the bare hostname and the engine at `engine.`: they are
/// separate Cloudflare Apps now, so the engine's own origin is no longer the one
/// a browser shows the console on.
const DEFAULT_ALLOWED_ORIGINS: &str = "https://aixodia.dpdns.org,http://localhost:5173";

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
///
/// `aud`, `iss` and `exp` are checked by `jsonwebtoken` against the validation
/// built in `verify_access`, which reads them through `serde` rather than through
/// these fields — so they are deserialised but never read here. Keeping them in
/// the struct is what makes a malformed claim set fail to decode at all.
#[allow(dead_code)]
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
    cache: tokio::sync::Mutex<KeyCache>,
    exchange: crate::auth_exchange::Exchange,
    /// Emails allowed to ask for a token by name (`AUTH_ALLOWED_EMAILS`).
    allowlist: crate::auth_gate::Allowlist,
    /// Tokens issued by the email gate, and their confirm/active state.
    ///
    /// Only used when `auth_worker` is empty. With a Worker configured, the Worker
    /// is the authority and this map is never consulted.
    gate: std::sync::Arc<crate::auth_gate::Gate>,
    /// Base URL of the auth Worker, e.g. `https://aixodia-auth.example.workers.dev`.
    auth_worker: String,
    /// Recently confirmed tokens, so one page load costs one Worker lookup rather
    /// than one per request. Holds only tokens the Worker has just accepted.
    gate_cache: std::sync::Mutex<std::collections::HashMap<u64, std::time::Instant>>,
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
        let allowlist = crate::auth_gate::Allowlist::parse(
            &resolve_scalar("AUTH_ALLOWED_EMAILS", "auth/allowed_emails", db).await,
        );
        if allowlist.is_empty() {
            tracing::warn!("AUTH_ALLOWED_EMAILS unset: the email gate will refuse every request");
        }
        Arc::new(Self {
            team_domain,
            app_aud,
            cf_token,
            allowed_origins,
            cache: tokio::sync::Mutex::new(KeyCache::new()),
            exchange: crate::auth_exchange::Exchange::new(),
            allowlist,
            gate: std::sync::Arc::new(crate::auth_gate::Gate::new()),
            auth_worker: resolve_scalar("AUTH_WORKER_URL", "auth/worker_url", db).await,
            gate_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        })
    }

    /// Emails permitted to ask for a token by name.
    pub fn allowlist(&self) -> &crate::auth_gate::Allowlist {
        &self.allowlist
    }

    /// Tokens issued by the email gate.
    pub fn gate(&self) -> &std::sync::Arc<crate::auth_gate::Gate> {
        &self.gate
    }

    /// The browser address of the console, for redirecting a human who has just
    /// signed in.
    ///
    /// Taken from the first configured allowed origin rather than a constant, so
    /// that a deployment pointing at a different dashboard does not send people
    /// to someone else's. Falls back to the engine's own host, which has no page
    /// but is at least not another site.
    fn dashboard_url(&self) -> String {
        self.allowed_origins
            .iter()
            .find(|origin| origin.starts_with("https://"))
            .cloned()
            .unwrap_or_else(|| "/".to_string())
    }

    /// Asks the auth Worker whether a token is usable, reusing a recent answer.
    ///
    /// The Worker owns the sessions, so it is the only thing that can answer. But
    /// asking on every request would put an internet round trip in front of every
    /// call — including the polling and long-lived streams a console makes while a
    /// user is just watching it — and would make the Worker's availability the
    /// engine's availability, which is the coupling this split exists to remove.
    ///
    /// The answer is cached for [`GATE_CACHE_TTL`], and only while it is `Active`:
    /// a rejection is never cached, so finishing a confirmation or fixing a
    /// credential takes effect on the next request rather than after a wait.
    ///
    /// The cost is that a revoked session keeps working for up to that TTL. Five
    /// seconds is chosen for that reason: it is long enough that a burst of
    /// requests from one page load costs one lookup, and short enough that a
    /// revoked token cannot be used for anything meaningful. Anything that has to
    /// act on a revocation immediately has to wait for the session to expire, which
    /// is the same guarantee every cached credential offers.
    pub async fn gate_check(&self, token: &str) -> crate::auth_gate::GateVerdict {
        use crate::auth_gate::GateVerdict;

        if self.auth_worker.is_empty() {
            // No authority configured: fall back to the in-process gate so a
            // deployment that has not moved yet still works.
            return match crate::auth_gate::check_token(self, token).await {
                Ok(_) => GateVerdict::Active,
                Err(crate::auth_gate::Rejection::Pending) => GateVerdict::Pending,
                Err(crate::auth_gate::Rejection::Expired) => GateVerdict::Expired,
                Err(_) => GateVerdict::Unknown,
            };
        }

        // Only a successful lookup is worth caching, and only the fact that the
        // token is usable — the cache must not become a way to outlive a revocation
        // beyond the TTL, so the entry is keyed by token and expires on its own.
        let key = gate_cache_key(token);
        if let Ok(cache) = self.gate_cache.lock() {
            if let Some(seen) = cache.get(&key) {
                if seen.elapsed() < GATE_CACHE_TTL {
                    return crate::auth_gate::GateVerdict::Active;
                }
            }
        }

        let url = format!("{}/v1/auth/verify", self.auth_worker.trim_end_matches('/'));
        let response = match reqwest::Client::new()
            .post(&url)
            .header("Authorization", format!("Bearer {token}"))
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                tracing::warn!("auth worker unreachable: {err}");
                return GateVerdict::Unreachable;
            }
        };

        let status = response.status();
        let body: serde_json::Value = response.json().await.unwrap_or_default();
        let code = body
            .get("code")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        match (status.is_success(), code) {
            (true, _) => {
                // Bounded so a stream of one-off tokens cannot grow this map without
                // limit; clearing is correct here because the entries are only a
                // performance hint, never a permission.
                if let Ok(mut cache) = self.gate_cache.lock() {
                    if cache
                        .get(&key)
                        .is_some_and(|seen| seen.elapsed() >= GATE_CACHE_TTL)
                    {
                        cache.remove(&key);
                    }
                    if cache.len() >= GATE_CACHE_CAPACITY {
                        cache.clear();
                    }
                    cache.insert(key, std::time::Instant::now());
                }
                GateVerdict::Active
            }
            (false, "pending_confirmation") => GateVerdict::Pending,
            (false, "expired") => GateVerdict::Expired,
            (false, "unknown_token") => GateVerdict::Unknown,
            _ => {
                tracing::warn!("auth worker answered {status} without a usable code");
                GateVerdict::Unreachable
            }
        }
    }

    /// The sign-in exchange state, used by the native-app auth routes.
    pub fn exchange(&self) -> &crate::auth_exchange::Exchange {
        &self.exchange
    }

    /// The Access subject this request proves, for the callback handler.
    pub async fn access_subject(&self, headers: &HeaderMap) -> Option<String> {
        self.verify_access_jwt(access_jwt(headers)).await
    }

    /// Every route is guarded by `client_guard`; the sign-in routes opt out
    /// because the caller has no credential yet (see `is_public_auth_path`).
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/v1/auth/whoami", get(whoami))
            .route("/v1/auth/authorize", post(crate::auth_exchange::authorize))
            .route("/v1/auth/callback", get(crate::auth_exchange::callback))
            .route(
                "/v1/auth/exchange",
                post(crate::auth_exchange::exchange_code),
            )
            .route("/v1/auth/request", post(crate::auth_gate::request))
            .route("/v1/auth/confirm", get(crate::auth_gate::confirm_page))
            .route("/v1/auth/confirm", post(crate::auth_gate::confirm))
            .route("/v1/auth/status", get(crate::auth_gate::status))
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

/// Sign-in routes the caller must reach *before* holding a credential.
///
/// `/v1/auth/authorize` and `/v1/auth/exchange` are guarded by PKCE plus a
/// single-use code, and `/v1/auth/callback` is guarded by the Access JWT that
/// Cloudflare puts on it — requiring a token to reach them would make sign-in
/// impossible.
///
/// `/v1/auth/request` is the email gate's entry point: it has to be reachable
/// before a token exists. `/v1/auth/confirm` is the page and post that activate
/// the token, and `/v1/auth/status` takes the token itself as the credential, so
/// none of them can require the guard.
fn is_public_auth_path(path: &str) -> bool {
    matches!(
        path,
        "/v1/auth/authorize"
            | "/v1/auth/callback"
            | "/v1/auth/exchange"
            | "/v1/auth/request"
            | "/v1/auth/confirm"
            | "/v1/auth/status"
    )
}

/// Rejects any request that carries neither an Access JWT nor the master token.
///
/// How long a Worker verdict is reused.
///
/// Short on purpose: the trade is Worker load against how long a revoked session
/// keeps working, and five seconds is long enough to collapse the requests a single
/// page load makes and short enough that a revocation is not meaningfully delayed.
const GATE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(5);

/// Cap on cached tokens, so a client offering many distinct tokens cannot grow the
/// map without limit.
const GATE_CACHE_CAPACITY: usize = 512;

/// A token is a secret, so it is hashed before being used as a key: the map must
/// not become a place where live tokens can be read.
fn gate_cache_key(token: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    token.hash(&mut hasher);
    hasher.finish()
}

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
    if path == "/healthz" || is_public_auth_path(&path) {
        return with_cors(next.run(request).await, &cors);
    }

    let master = bearer(request.headers());
    if auth.master_matches(master) {
        return with_cors(next.run(request).await, &cors);
    }

    // A token from the email gate. The authority is the auth Worker, not this
    // process: it holds the sessions and the allowlist, so a session revoked there
    // is revoked here on the next request, and an engine restart does not
    // invalidate anyone's sign-in.
    //
    // "unknown" is not conclusive — the token might be the master token, which was
    // already checked above — so it falls through rather than being reported.
    if !master.is_empty() {
        match auth.gate_check(master).await {
            crate::auth_gate::GateVerdict::Active => {
                return with_cors(next.run(request).await, &cors)
            }
            crate::auth_gate::GateVerdict::Pending => {
                return with_cors(
                    crate::auth_gate::rejection_response(crate::auth_gate::Rejection::Pending),
                    &cors,
                )
            }
            crate::auth_gate::GateVerdict::Expired => {
                return with_cors(
                    crate::auth_gate::rejection_response(crate::auth_gate::Rejection::Expired),
                    &cors,
                )
            }
            // Unknown, or the Worker could not be reached. Either way this token is
            // not proven and the Access JWT below is the remaining path.
            crate::auth_gate::GateVerdict::Unknown | crate::auth_gate::GateVerdict::Unreachable => {
            }
        }
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
///
/// A browser that lands here was sent by `signIn()` in the dashboard: that
/// navigates to this guarded URL so Cloudflare Access will run the login, and
/// Access returns the browser to the path it was asked for. Without the redirect
/// below the user finishes signing in and is shown this JSON, which looks like
/// the sign-in failed rather than succeeded.
///
/// The two cases are told apart by what the caller asked for: a programmatic
/// client (the app, fetch) sends no `Accept: text/html` and keeps getting JSON.
async fn whoami(State(auth): State<Arc<ClientAuth>>, headers: HeaderMap) -> Response {
    // `Accept` is the only signal available. A browser navigation always prefers
    // HTML; `fetch` omits it or asks for JSON.
    let wants_html = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("text/html"));

    if wants_html {
        // Back to the dashboard, which the operator already has open. Chosen over
        // showing a "you're signed in" page because the next useful thing the
        // user wants is the console, not an acknowledgement.
        return (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, auth.dashboard_url())],
        )
            .into_response();
    }

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

#[cfg(test)]
mod dashboard_url_tests {
    /// The redirect target is the console, taken from the configured origins.
    #[test]
    fn the_first_https_origin_is_the_dashboard() {
        let auth = crate::client_auth::ClientAuth {
            team_domain: String::new(),
            app_aud: String::new(),
            cf_token: String::new(),
            allowed_origins: vec![
                // Plain-HTTP origins exist for local development and must not be
                // chosen: a redirect there would break a real sign-in.
                "http://localhost:5173".to_string(),
                "https://aixodia.dpdns.org".to_string(),
            ],
            cache: tokio::sync::Mutex::new(crate::client_auth::KeyCache::new()),
            exchange: crate::auth_exchange::Exchange::new(),
            allowlist: crate::auth_gate::Allowlist::default(),
            gate: std::sync::Arc::new(crate::auth_gate::Gate::new()),
            auth_worker: String::new(),
            gate_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        };
        assert_eq!(auth.dashboard_url(), "https://aixodia.dpdns.org");
    }

    /// With nothing configured the fallback must not point at another site.
    #[test]
    fn nothing_configured_falls_back_to_the_engine_itself() {
        let auth = crate::client_auth::ClientAuth {
            team_domain: String::new(),
            app_aud: String::new(),
            cf_token: String::new(),
            allowed_origins: Vec::new(),
            cache: tokio::sync::Mutex::new(crate::client_auth::KeyCache::new()),
            exchange: crate::auth_exchange::Exchange::new(),
            allowlist: crate::auth_gate::Allowlist::default(),
            gate: std::sync::Arc::new(crate::auth_gate::Gate::new()),
            auth_worker: String::new(),
            gate_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
        };
        assert_eq!(auth.dashboard_url(), "/");
    }
}
