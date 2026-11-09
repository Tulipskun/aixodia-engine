//! Direct client access: CORS and short-lived session tokens.
//!
//! The dashboard used to sit behind a Cloudflare Worker that injected `CF_TOKEN`
//! server-side, kept the GitHub OAuth session and proxied `/api/engine/*`.
//! With the Worker removed the browser talks to the engine itself, so the engine
//! issues its own short-lived opaque token instead of ever handing `CF_TOKEN`
//! to a browser. A GitHub OAuth access token is exchanged for it once, checked
//! against `ALLOWED_GITHUB_USERS`, and the resulting token expires in
//! `AIXODIA_CLIENT_TOKEN_TTL` seconds (default 900).

use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::RwLock;

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

fn csv(name: &str) -> Vec<String> {
    env::var(name)
        .unwrap_or_default()
        .split(',')
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

impl ClientAuth {
    /// `ALLOWED_GITHUB_USERS` and `ALLOWED_ORIGINS` are required: an empty list
    /// means "nobody may log in" / "no browser origin is trusted", never "allow
    /// all". Failing closed is the point of removing the Worker's allowlist.
    pub fn from_env(cf_token: String) -> Arc<Self> {
        let ttl_secs = env::var("AIXODIA_CLIENT_TOKEN_TTL")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value >= 60 && *value <= 86_400)
            .unwrap_or(900);
        Arc::new(Self {
            cf_token,
            allowed_users: csv("ALLOWED_GITHUB_USERS"),
            allowed_origins: csv("ALLOWED_ORIGINS"),
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
        if let Some(origin) = origin.filter(|value| self.allowed_origins.iter().any(|o| o == value)) {
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
        return with_cors((StatusCode::NO_CONTENT, cors.clone()).into_response(), &cors);
    }

    let path = request.uri().path().to_string();
    let open = matches!(
        path.as_str(),
        "/healthz" | "/terminal" | "/room" | "/v1/auth/token"
    );
    if !open {
        let presented = bearer(request.headers());
        let by_master = !auth.cf_token.is_empty() && presented == auth.cf_token;
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
    github_token: String,
}

/// Exchange a GitHub OAuth access token for a short-lived engine token.
/// The engine never sees a client secret and never stores the GitHub token.
async fn issue_token(
    State(auth): State<Arc<ClientAuth>>,
    Json(body): Json<TokenRequest>,
) -> Response {
    if auth.allowed_users.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "ALLOWED_GITHUB_USERS is not configured"})),
        )
            .into_response();
    }
    if body.github_token.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "github_token is required"})),
        )
            .into_response();
    }

    let client = reqwest::Client::new();
    let response = client
        .get("https://api.github.com/user")
        .header(header::AUTHORIZATION, format!("Bearer {}", body.github_token.trim()))
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

    let token = uuid::Uuid::new_v4().simple().to_string();
    let expires_at = now_unix() + auth.ttl_secs;
    auth.sessions.write().await.insert(
        token.clone(),
        Session {
            sub: login.to_string(),
            exp: expires_at,
        },
    );
    (
        StatusCode::OK,
        Json(json!({
            "token": token,
            "expires_at": expires_at,
            "user": login,
            "scope": "client",
        })),
    )
        .into_response()
}
