//! Direct client access: CORS and short-lived session tokens.
//!
//! The dashboard used to sit behind a Cloudflare Worker that injected
//! `CF_TOKEN` server-side, kept the GitHub OAuth session and proxied
//! `/api/engine/*`. With the Worker removed the browser talks to the engine
//! itself, so the engine issues its own short-lived opaque token instead of
//! ever handing `CF_TOKEN` to a browser. A GitHub OAuth access token OR the
//! `CF_TOKEN` itself (`master_token`, for clients with no GitHub PAT) is
//! exchanged for it once; the result expires in `AIXODIA_CLIENT_TOKEN_TTL`
//! seconds (default 900). `ALLOWED_GITHUB_USERS` / `ALLOWED_ORIGINS` come from
//! env, else Workers KV `AIXODIA_CONFIG` (`client/*`), else a usable default.

use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aixodia_db::Client as DbClient;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::RwLock;

/// Always-usable default for `ALLOWED_ORIGINS` when neither env nor KV set it.
const DEFAULT_ALLOWED_ORIGINS: &str = "https://dashboard.aixodia.dpdns.org,http://localhost:5173";

/// One issued client token. Opaque, random, in memory: an engine restart simply
/// invalidates every outstanding session, which is fine for a short-lived one.
struct Session {
    sub: String,
    exp: u64,
}

pub struct ClientAuth {
    cf_token: String,
    allowed_users: Vec<String>,
    allowed_origins: Vec<String>,
    ttl_secs: u64,
    sessions: RwLock<HashMap<String, Session>>,
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn parse_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

/// Env first; when unset/empty, Workers KV `AIXODIA_CONFIG`; then an optional
/// hard default. `ALLOWED_GITHUB_USERS` has no default (empty => the GitHub
/// path fails closed), while `ALLOWED_ORIGINS` always ends up usable.
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

impl ClientAuth {
    /// `ALLOWED_GITHUB_USERS` / `ALLOWED_ORIGINS` resolve env -> Workers KV
    /// `AIXODIA_CONFIG` -> default. The GitHub allowlist may be empty (fails
    /// closed on that path only); `master_token` never needs it.
    pub async fn from_env(cf_token: String, db: &DbClient) -> Arc<Self> {
        let ttl_secs = env::var("AIXODIA_CLIENT_TOKEN_TTL")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value >= 60 && *value <= 86_400)
            .unwrap_or(900);
        let allowed_users =
            resolve_allowed("ALLOWED_GITHUB_USERS", "client/allowed_users", db, None).await;
        let allowed_origins = resolve_allowed(
            "ALLOWED_ORIGINS",
            "client/allowed_origins",
            db,
            Some(DEFAULT_ALLOWED_ORIGINS),
        )
        .await;
        if allowed_users.is_empty() {
            tracing::warn!(
                "ALLOWED_GITHUB_USERS/KV unset: GitHub-token exchange is disabled; master_token still works"
            );
        }
        Arc::new(Self {
            cf_token,
            allowed_users,
            allowed_origins,
            ttl_secs,
            sessions: RwLock::new(HashMap::new()),
        })
    }

    /// The one route that needs no engine bearer: it is the exchange itself.
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/v1/auth/token", post(issue_token))
            .with_state(self.clone())
    }

    fn master_matches(&self, presented: &str) -> bool {
        !self.cf_token.is_empty() && !presented.is_empty() && presented == self.cf_token
    }

    async fn valid(&self, token: &str) -> Option<String> {
        let now = now_unix();
        let mut sessions = self.sessions.write().await;
        sessions.retain(|_, session| session.exp > now);
        sessions.get(token).map(|session| session.sub.clone())
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

    async fn issue_session(&self, sub: String) -> Response {
        let token = uuid::Uuid::new_v4().simple().to_string();
        let expires_at = now_unix() + self.ttl_secs;
        self.sessions.write().await.insert(
            token.clone(),
            Session {
                sub: sub.clone(),
                exp: expires_at,
            },
        );
        (
            StatusCode::OK,
            Json(json!({
                "token": token,
                "expires_at": expires_at,
                "user": sub,
                "scope": "client",
            })),
        )
            .into_response()
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

fn with_cors(mut response: Response, cors: &HeaderMap) -> Response {
    for (name, value) in cors.iter() {
        response.headers_mut().insert(name, value.clone());
    }
    response
}

/// Replaces `aixodia_io::require_token` for the outer mux. The original bearer
/// still works (Android, CLI, in-process tools); a short-lived client token is
/// accepted as well. The open paths match the old guard exactly, plus the new
/// exchange route.
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
    let open = matches!(
        path.as_str(),
        "/healthz" | "/terminal" | "/room" | "/v1/auth/token"
    );
    if !open {
        let presented = bearer(request.headers());
        let by_master = auth.master_matches(presented);
        let by_session = auth.valid(presented).await.is_some();
        if !by_master && !by_session {
            return with_cors(
                (
                    StatusCode::UNAUTHORIZED,
                    cors.clone(),
                    Json(json!({"error": "unauthorized", "code": "TOKEN_REJECTED"})),
                )
                    .into_response(),
                &cors,
            );
        }
    }

    with_cors(next.run(request).await, &cors)
}

#[derive(Deserialize)]
struct TokenRequest {
    #[serde(default)]
    github_token: Option<String>,
    #[serde(default)]
    master_token: Option<String>,
}

/// Exchange a GitHub OAuth access token OR the master `CF_TOKEN` for a
/// short-lived engine token. `CF_TOKEN` is never returned to the client.
async fn issue_token(
    State(auth): State<Arc<ClientAuth>>,
    Json(body): Json<TokenRequest>,
) -> Response {
    let master = body
        .master_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let github = body
        .github_token
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    if master.is_none() && github.is_none() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "github_token or master_token is required"})),
        )
            .into_response();
    }

    // master_token: for an Android/CLI client that holds CF_TOKEN but has no
    // GitHub PAT. Works even when the GitHub allowlist is empty (fail-closed
    // applies to the GitHub path only).
    if let Some(master) = master {
        if auth.master_matches(master) {
            return auth.issue_session("master".to_string()).await;
        }
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "master_token rejected", "code": "MASTER_TOKEN_REJECTED"})),
        )
            .into_response();
    }

    if auth.allowed_users.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "ALLOWED_GITHUB_USERS is not configured (env or AIXODIA_CONFIG/client/allowed_users)"})),
        )
            .into_response();
    }
    let github_token = github.unwrap_or_default();

    let client = reqwest::Client::new();
    let response = client
        .get("https://api.github.com/user")
        .header(header::AUTHORIZATION, format!("Bearer {}", github_token))
        .header(header::ACCEPT, "application/vnd.github+json")
        .header(header::USER_AGENT, "aixodia-engine")
        .send()
        .await;
    let Ok(response) = response else {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": "GitHub is unreachable"})),
        )
            .into_response();
    };
    if !response.status().is_success() {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "GitHub token rejected"})),
        )
            .into_response();
    }
    let Ok(user) = response.json::<Value>().await else {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error": "GitHub returned an unreadable user"})),
        )
            .into_response();
    };
    let login = user.get("login").and_then(Value::as_str).unwrap_or("");
    if !auth.allowed_users.iter().any(|allowed| allowed == login) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "GitHub user is not allowed"})),
        )
            .into_response();
    }

    auth.issue_session(login.to_string()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_auth(cf_token: &str) -> ClientAuth {
        ClientAuth {
            cf_token: cf_token.to_string(),
            allowed_users: Vec::new(),
            allowed_origins: parse_csv(DEFAULT_ALLOWED_ORIGINS),
            ttl_secs: 900,
            sessions: RwLock::new(HashMap::new()),
        }
    }

    #[test]
    fn master_token_matches_only_the_real_cf_token() {
        let auth = test_auth("secret");
        assert!(auth.master_matches("secret"));
        assert!(!auth.master_matches("wrong"));
        assert!(!auth.master_matches(""));
    }

    #[test]
    fn master_token_path_needs_no_github_allowlist() {
        let auth = test_auth("secret");
        assert!(auth.allowed_users.is_empty());
        assert!(auth.master_matches("secret"));
    }

    #[test]
    fn allowed_origins_default_is_usable() {
        let auth = test_auth("secret");
        assert_eq!(auth.allowed_origins.len(), 2);
        assert!(auth
            .allowed_origins
            .iter()
            .any(|o| o == "http://localhost:5173"));
    }

    #[test]
    fn parse_csv_trims_and_drops_empty() {
        assert_eq!(parse_csv(" a, ,b ,c"), vec!["a", "b", "c"]);
    }
}
