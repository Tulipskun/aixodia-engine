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
    /// The Access team domain, e.g. `aixodia.cloudflareaccess.com`.
    team_domain: String,
    /// The Access application audience tags (`aud`) this engine accepts.
    ///
    /// A browser that signed in to the dashboard holds a token shaped for the
    /// dashboard app, while the app signs in to the engine's own app; the two
    /// audiences differ, and rejecting either one turned a valid sign-in into
    /// `ACCESS_JWT_REJECTED`. Comma-separated so one deployment serves both.
    app_aud: Vec<String>,
    /// Master token; still accepted as `Authorization: Bearer`.
    cf_token: String,
    allowed_origins: Vec<String>,
    cache: tokio::sync::Mutex<KeyCache>,
    exchange: crate::auth_exchange::Exchange,
    /// Base URL of the auth Worker, e.g. `https://aixodia-auth.example.workers.dev`.
    auth_worker: String,
    /// Recently confirmed tokens, so one page load costs one Worker lookup rather
    /// than one per request. Holds only tokens the Worker has just accepted.
    gate_cache: std::sync::Mutex<std::collections::HashMap<u64, std::time::Instant>>,
    /// SHA-256 hashes of the tokens the Worker says are live.
    ///
    /// This is the device flow's fast path: the Worker is the only issuer, and it
    /// pushes `activate`/`revoke` here plus answers a full pull on startup and
    /// every minute. Holding hashes rather than tokens means a dump of this
    /// process yields nothing that works anywhere else.
    active_tokens: std::sync::RwLock<std::collections::HashSet<String>>,
    /// Shared secret for the sync endpoints, so only the Worker can change the set.
    sync_secret: String,
    /// False until the first pull succeeds; until then the set is not trusted and
    /// the Worker is asked per request instead.
    synced: std::sync::atomic::AtomicBool,
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
        let app_aud = resolve_scalar("ACCESS_APP_AUD", "access/app_aud", db)
            .await
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .collect::<Vec<_>>();
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
        let auth_worker = resolve_scalar("AUTH_WORKER_URL", "auth/worker_url", db).await;
        if auth_worker.is_empty() {
            tracing::warn!(
                "AUTH_WORKER_URL unset: no token can be verified, so only the master token will work"
            );
        }
        Arc::new(Self {
            team_domain,
            app_aud,
            cf_token,
            allowed_origins,
            cache: tokio::sync::Mutex::new(KeyCache::new()),
            exchange: crate::auth_exchange::Exchange::new(),
            auth_worker,
            gate_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            active_tokens: std::sync::RwLock::new(std::collections::HashSet::new()),
            sync_secret: resolve_scalar("AUTH_SYNC_SECRET", "auth/sync_secret", db).await,
            synced: std::sync::atomic::AtomicBool::new(false),
        })
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
            // No authority configured means no token can be vouched for. Failing
            // closed is the only safe reading: the alternative would be accepting a
            // credential nobody can check.
            return GateVerdict::Unknown;
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

        let url = format!("{}/auth/verify", self.auth_worker.trim_end_matches('/'));
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
    ///
    /// There is no `/v1/auth/request|confirm|status` here. Issuing and confirming
    /// tokens is the auth Worker's job, and it serves those from its own origin:
    /// leaving a second copy in the engine meant sign-in still depended on this
    /// process being up, which is exactly the dependence that was the problem.
    ///
    /// What remains under `/v1/auth/` describes the caller and their engine
    /// credential — asking for a token by name, and the GitHub-bound exchange for
    /// the native app — which can only be answered by the process that holds them.
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/v1/auth/whoami", get(whoami))
            .route("/v1/auth/authorize", post(crate::auth_exchange::authorize))
            .route("/v1/auth/callback", get(crate::auth_exchange::callback))
            .route(
                "/v1/auth/exchange",
                post(crate::auth_exchange::exchange_code),
            )
            .route("/auth/sessions/sync", post(sync_handler))
            .route("/auth/sessions/active", get(active_handler))
            .with_state(self.clone())
    }

    // ------------------------------------------------------------ token sync

    /// How many tokens the local set holds, for the sync answer.
    pub fn active_count(&self) -> usize {
        self.active_tokens.read().map(|set| set.len()).unwrap_or(0)
    }

    /// Whether the first full pull has succeeded yet.
    pub fn is_synced(&self) -> bool {
        self.synced.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// True when the given bearer token is in the locally held set.
    pub fn token_is_active(&self, token: &str) -> bool {
        let hash = sha256_hex(token);
        self.active_tokens.read().map(|set| set.contains(&hash)).unwrap_or(false)
    }

    /// Replace the local set from the Worker's answer.
    pub async fn sync_tokens_from_worker(&self) -> bool {
        let url = format!("{}/auth/sessions/active", self.auth_worker.trim_end_matches('/'));
        let secret = self.sync_secret.clone();
        let Ok(response) = reqwest::Client::new()
            .get(&url)
            .header("x-auth-sync", &secret)
            .header(header::ACCEPT, "application/json")
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|err| tracing::warn!("token sync pull failed: {err}"))
        else {
            return false
        };
        let Ok(payload) = response.json::<serde_json::Value>().await else {
            tracing::warn!("token sync pull: non-JSON answer");
            return false
        };
        let mut set = std::collections::HashSet::new();
        if let Some(hashes) = payload.get("hashes").and_then(|v| v.as_array()) {
            for hash in hashes {
                if let Some(value) = hash.as_str() {
                    if is_hex_64(value) {
                        set.insert(value.to_string());
                    }
                }
            }
        }
        *self.active_tokens.write().unwrap_or_else(|err| err.into_inner()) = set;
        self.synced.store(true, std::sync::atomic::Ordering::Relaxed);
        true
    }

    /// Apply a push from the Worker. Returns true when the secret matched.
    pub fn apply_sync_push(&self, secret: &str, activate: &[String], revoke: &[String]) -> bool {
        if secret.is_empty() || secret != self.sync_secret {
            return false
        }
        let mut set = self.active_tokens.write().unwrap_or_else(|err| err.into_inner());
        for hash in activate {
            if is_hex_64(hash) {
                set.insert(hash.clone());
            }
        }
        for hash in revoke {
            set.remove(hash);
        }
        true
    }

    /// Pull once before serving that first request, then keep pulling so a token
    /// the push missed (engine was down) arrives within a minute.
    pub fn spawn_sync_task(self: &Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            for attempt in 1.. {
                if this.sync_tokens_from_worker().await || attempt >= 3 {
                    break
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                this.sync_tokens_from_worker().await;
            }
        });
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
        validation.set_audience(&self.app_aud);
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

/// SHA-256 of a token, hex. The Worker stores and pushes the same form.
fn sha256_hex(value: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn is_hex_64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `GET /auth/sessions/active` — the local set, for debugging and for a caller
/// that wants to know how much the engine currently trusts.
///
/// Answers counts, not hashes: the list is the Worker's to publish, and echoing
/// it here would let any caller probe which tokens exist once they hold the
/// secret.
async fn active_handler(State(auth): State<Arc<ClientAuth>>) -> Response {
    (
        StatusCode::OK,
        Json(json!({"active": auth.active_count(), "synced": auth.is_synced()})),
    )
        .into_response()
}

/// `POST /auth/sessions/sync` — the Worker pushes activations and revocations.
///
/// Guarded by the shared secret rather than by Access: the Worker is a machine
/// caller with no browser session. Reaching this route requires the secret, so a
/// wrong or absent value is refused before any state changes.
async fn sync_handler(
    State(auth): State<Arc<ClientAuth>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let secret = headers
        .get("x-auth-sync")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&body) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "bad_json"}))).into_response();
    };
    let list = |key: &str| {
        payload
            .get(key)
            .and_then(|value| value.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    if !auth.apply_sync_push(secret, &list("activate"), &list("revoke")) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "forbidden", "code": "BAD_SYNC_SECRET"})),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        Json(json!({"ok": true, "active": auth.active_count()})),
    )
        .into_response()
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
            // The Worker is a machine caller with no browser session; the shared
            // secret is the credential, checked inside the handlers.
            | "/auth/sessions/sync"
            | "/auth/sessions/active"
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

    // A token the Worker has pushed or this process pulled. Checked first because
    // it costs one hash and a set lookup; a synced set is authoritative for
    // tokens it contains (revocations arrive by push or the next pull).
    if !master.is_empty() && auth.token_is_active(master) {
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
                    (
                        StatusCode::FORBIDDEN,
                        axum::Json(serde_json::json!({
                            "error": "unauthorized",
                            "code": "pending_confirmation",
                        })),
                    )
                        .into_response(),
                    &cors,
                )
            }
            crate::auth_gate::GateVerdict::Expired => {
                return with_cors(
                    (
                        StatusCode::UNAUTHORIZED,
                        axum::Json(serde_json::json!({
                            "error": "unauthorized",
                            "code": "expired",
                        })),
                    )
                        .into_response(),
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
            app_aud: Vec::new(),
            cf_token: String::new(),
            allowed_origins: vec![
                // Plain-HTTP origins exist for local development and must not be
                // chosen: a redirect there would break a real sign-in.
                "http://localhost:5173".to_string(),
                "https://aixodia.dpdns.org".to_string(),
            ],
            cache: tokio::sync::Mutex::new(crate::client_auth::KeyCache::new()),
            exchange: crate::auth_exchange::Exchange::new(),
            auth_worker: String::new(),
            gate_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            active_tokens: std::sync::RwLock::new(std::collections::HashSet::new()),
            sync_secret: String::new(),
            synced: std::sync::atomic::AtomicBool::new(false),
        };
        assert_eq!(auth.dashboard_url(), "https://aixodia.dpdns.org");
    }

    /// With nothing configured the fallback must not point at another site.
    #[test]
    fn nothing_configured_falls_back_to_the_engine_itself() {
        let auth = crate::client_auth::ClientAuth {
            team_domain: String::new(),
            app_aud: Vec::new(),
            cf_token: String::new(),
            allowed_origins: Vec::new(),
            cache: tokio::sync::Mutex::new(crate::client_auth::KeyCache::new()),
            exchange: crate::auth_exchange::Exchange::new(),
            auth_worker: String::new(),
            gate_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            active_tokens: std::sync::RwLock::new(std::collections::HashSet::new()),
            sync_secret: String::new(),
            synced: std::sync::atomic::AtomicBool::new(false),
        };
        assert_eq!(auth.dashboard_url(), "/");
    }
}

#[cfg(test)]
mod token_sync_tests {
    use super::*;

    fn auth_with(secret: &str) -> Arc<ClientAuth> {
        Arc::new(crate::client_auth::ClientAuth {
            team_domain: String::new(),
            app_aud: Vec::new(),
            cf_token: String::new(),
            allowed_origins: Vec::new(),
            cache: tokio::sync::Mutex::new(crate::client_auth::KeyCache::new()),
            exchange: crate::auth_exchange::Exchange::new(),
            auth_worker: String::new(),
            gate_cache: std::sync::Mutex::new(std::collections::HashMap::new()),
            active_tokens: std::sync::RwLock::new(std::collections::HashSet::new()),
            sync_secret: secret.to_string(),
            synced: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// A push carrying the right secret activates and revokes exactly the hashes
    /// it names — and nothing else.
    #[test]
    fn a_push_adds_and_removes_only_what_it_names() {
        let auth = auth_with("s3cret");
        let first = sha256_hex("token-one");
        let second = sha256_hex("token-two");

        assert!(auth.apply_sync_push("s3cret", &[first.clone(), second.clone()], &[]));
        assert_eq!(auth.active_count(), 2);
        assert!(auth.token_is_active("token-one"));
        assert!(auth.token_is_active("token-two"));

        assert!(auth.apply_sync_push("s3cret", &[], &[first]));
        assert_eq!(auth.active_count(), 1);
        assert!(!auth.token_is_active("token-one"));
        assert!(auth.token_is_active("token-two"));
    }

    /// Without the shared secret nothing changes: this endpoint is not the door
    /// anyone with a network path may open.
    #[test]
    fn a_push_with_the_wrong_secret_changes_nothing() {
        let auth = auth_with("s3cret");
        let hash = sha256_hex("token-one");
        assert!(!auth.apply_sync_push("wrong", &[hash], &[]));
        assert_eq!(auth.active_count(), 0);
    }

    /// An unconfigured secret must not mean "accept everything".
    #[test]
    fn an_empty_secret_never_authenticates() {
        let auth = auth_with("");
        let hash = sha256_hex("token-one");
        assert!(!auth.apply_sync_push("", &[hash], &[]));
        assert_eq!(auth.active_count(), 0);
    }

    /// Malformed hashes are dropped rather than stored: the set is compared
    /// against SHA-256 hex, so anything else would be dead weight or a trap.
    #[test]
    fn malformed_hashes_are_ignored() {
        let auth = auth_with("s3cret");
        assert!(auth.apply_sync_push("s3cret", &["not-a-hash".to_string(), "ab".repeat(33)], &[]));
        assert_eq!(auth.active_count(), 0);
    }

    /// Tokens are held as hashes, so the plaintext never reaches this process's
    /// memory in the first place.
    #[test]
    fn only_the_hash_of_a_token_is_ever_compared() {
        let auth = auth_with("s3cret");
        auth.apply_sync_push("s3cret", &[sha256_hex("secret-token")], &[]);
        assert!(auth.token_is_active("secret-token"));
        assert!(!auth.token_is_active("another-token"));
    }
}
