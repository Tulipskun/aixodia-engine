//! Direct client access: CORS, short-lived access tokens, and persistent refresh credentials.
//!
//! The dashboard used to sit behind a Cloudflare Worker that injected
//! `CF_TOKEN` server-side, kept the GitHub OAuth session and proxied
//! `/api/engine/*`. With the Worker removed the browser talks to the engine
//! itself, so the engine issues its own short-lived opaque token instead of
//! ever handing `CF_TOKEN` to a browser. A GitHub OAuth access token OR the
//! `CF_TOKEN` itself (`master_token`, for clients with no GitHub PAT) is
//! exchanged for a short-lived access token plus a long-lived refresh token.
//! Both are HMAC-signed with the server-only `CF_TOKEN`, so Engine restarts do
//! not invalidate a user's refresh credential. `ALLOWED_GITHUB_USERS` / `ALLOWED_ORIGINS` come from
//! env, else Workers KV `AIXODIA_CONFIG` (`client/*`), else a usable default.

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
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;

/// Always-usable default for `ALLOWED_ORIGINS` when neither env nor KV set it.
const DEFAULT_ALLOWED_ORIGINS: &str = "https://dashboard.aixodia.dpdns.org,http://localhost:5173";

/// Refresh credentials are deliberately stateless: they survive Engine restarts
/// without creating realtime KV/D1 session rows. The refresh token is stored by
/// clients only under their platform's secure credential store.
const REFRESH_TOKEN_TTL_SECS: u64 = 365 * 24 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenClaims {
    sub: String,
    exp: u64,
    kind: String,
    scope: String,
}

pub struct ClientAuth {
    cf_token: String,
    allowed_users: Vec<String>,
    allowed_origins: Vec<String>,
    ttl_secs: u64,
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
        })
    }

    /// The one route that needs no engine bearer: it is the exchange itself.
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/v1/auth/token", post(issue_token))
            .route("/v1/auth/refresh", post(refresh_token))
            .with_state(self.clone())
    }

    fn master_matches(&self, presented: &str) -> bool {
        !self.cf_token.is_empty() && !presented.is_empty() && presented == self.cf_token
    }

    fn subject_allowed(&self, subject: &str) -> bool {
        subject == "master" || self.allowed_users.iter().any(|allowed| allowed == subject)
    }

    fn sign_token(&self, subject: &str, kind: &str, expires_at: u64) -> String {
        let claims = TokenClaims {
            sub: subject.to_string(),
            exp: expires_at,
            kind: kind.to_string(),
            scope: "client".to_string(),
        };
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&claims).expect("serializing fixed token claims cannot fail"),
        );
        let mut mac = Hmac::<Sha256>::new_from_slice(self.cf_token.as_bytes())
            .expect("HMAC accepts keys of any length");
        mac.update(payload.as_bytes());
        format!(
            "{payload}.{}",
            URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
        )
    }

    fn verify_token(&self, token: &str, expected_kind: &str) -> Option<TokenClaims> {
        let (payload, signature) = token.split_once('.')?;
        if payload.is_empty()
            || signature.is_empty()
            || signature.contains('.')
            || self.cf_token.is_empty()
        {
            return None;
        }
        let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
        let mut mac = Hmac::<Sha256>::new_from_slice(self.cf_token.as_bytes()).ok()?;
        mac.update(payload.as_bytes());
        mac.verify_slice(&signature).ok()?;
        let claims: TokenClaims =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
        if claims.exp <= now_unix()
            || claims.kind != expected_kind
            || claims.scope != "client"
            || !self.subject_allowed(&claims.sub)
        {
            return None;
        }
        Some(claims)
    }

    fn valid(&self, token: &str) -> Option<String> {
        self.verify_token(token, "access").map(|claims| claims.sub)
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

    fn issue_session(&self, sub: String) -> Response {
        let now = now_unix();
        let expires_at = now + self.ttl_secs;
        let refresh_expires_at = now + REFRESH_TOKEN_TTL_SECS;
        let token = self.sign_token(&sub, "access", expires_at);
        let refresh_token = self.sign_token(&sub, "refresh", refresh_expires_at);
        (
            StatusCode::OK,
            Json(json!({
                "token": token,
                "expires_at": expires_at,
                "refresh_token": refresh_token,
                "refresh_expires_at": refresh_expires_at,
                "user": sub,
                "scope": "client",
            })),
        )
            .into_response()
    }

    fn refresh_session(&self, refresh_token: &str) -> Response {
        let Some(claims) = self.verify_token(refresh_token, "refresh") else {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "refresh token rejected", "code": "REFRESH_TOKEN_REJECTED"})),
            )
                .into_response();
        };
        let expires_at = now_unix() + self.ttl_secs;
        let token = self.sign_token(&claims.sub, "access", expires_at);
        (
            StatusCode::OK,
            Json(json!({
                "token": token,
                "expires_at": expires_at,
                "refresh_token": refresh_token,
                "refresh_expires_at": claims.exp,
                "user": claims.sub,
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
        "/healthz" | "/terminal" | "/room" | "/v1/auth/token" | "/v1/auth/refresh"
    );
    if !open {
        let presented = bearer(request.headers());
        let by_master = auth.master_matches(presented);
        let by_session = auth.valid(presented).is_some();
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
            return auth.issue_session("master".to_string());
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

    auth.issue_session(login.to_string())
}

#[derive(Deserialize)]
struct RefreshRequest {
    refresh_token: String,
}

async fn refresh_token(
    State(auth): State<Arc<ClientAuth>>,
    Json(body): Json<RefreshRequest>,
) -> Response {
    if body.refresh_token.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "refresh_token is required"})),
        )
            .into_response();
    }
    auth.refresh_session(body.refresh_token.trim())
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

    #[test]
    fn signed_access_and_refresh_tokens_are_distinct_and_restart_independent() {
        let mut auth = test_auth("server-secret");
        auth.allowed_users = vec!["Tulipskun".to_string()];
        let access = auth.sign_token("Tulipskun", "access", now_unix() + 900);
        let refresh = auth.sign_token("Tulipskun", "refresh", now_unix() + REFRESH_TOKEN_TTL_SECS);

        assert_eq!(auth.valid(&access).as_deref(), Some("Tulipskun"));
        assert!(auth.verify_token(&access, "refresh").is_none());
        assert!(auth.valid(&refresh).is_none());
        assert_eq!(
            auth.verify_token(&refresh, "refresh").unwrap().sub,
            "Tulipskun"
        );

        // No in-memory session registry is involved: another ClientAuth instance
        // with the same server secret and allowlist verifies the same token.
        let mut restarted = test_auth("server-secret");
        restarted.allowed_users = vec!["Tulipskun".to_string()];
        assert_eq!(restarted.valid(&access).as_deref(), Some("Tulipskun"));
        assert_eq!(
            restarted.verify_token(&refresh, "refresh").unwrap().sub,
            "Tulipskun"
        );
    }

    #[test]
    fn signed_tokens_reject_tampering_and_expiry() {
        let auth = test_auth("server-secret");
        let token = auth.sign_token("master", "access", now_unix() + 900);
        let mut tampered = token.clone();
        tampered.push('x');
        assert!(auth.valid(&tampered).is_none());
        let expired = auth.sign_token("master", "access", now_unix() - 1);
        assert!(auth.valid(&expired).is_none());
    }

    #[test]
    fn refresh_tokens_stop_working_when_github_user_is_removed_from_allowlist() {
        let mut auth = test_auth("server-secret");
        auth.allowed_users = vec!["Tulipskun".to_string()];
        let refresh = auth.sign_token("Tulipskun", "refresh", now_unix() + REFRESH_TOKEN_TTL_SECS);
        assert!(auth.verify_token(&refresh, "refresh").is_some());
        auth.allowed_users.clear();
        assert!(auth.verify_token(&refresh, "refresh").is_none());
    }

    #[test]
    fn master_signed_refresh_remains_valid_without_github_allowlist() {
        let auth = test_auth("server-secret");
        let refresh = auth.sign_token("master", "refresh", now_unix() + REFRESH_TOKEN_TTL_SECS);
        assert!(auth.verify_token(&refresh, "refresh").is_some());
    }

    #[tokio::test]
    async fn refresh_issues_a_new_access_token_without_reauthenticating_with_github() {
        let mut auth = test_auth("server-secret");
        auth.allowed_users = vec!["Tulipskun".to_string()];
        let refresh = auth.sign_token("Tulipskun", "refresh", now_unix() + REFRESH_TOKEN_TTL_SECS);
        let response = auth.refresh_session(&refresh);
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let access = json.get("token").and_then(Value::as_str).unwrap();
        assert_eq!(json.get("user").and_then(Value::as_str), Some("Tulipskun"));
        assert_eq!(
            json.get("refresh_token").and_then(Value::as_str),
            Some(refresh.as_str())
        );
        assert_eq!(auth.valid(access).as_deref(), Some("Tulipskun"));
    }

    #[test]
    fn refresh_rejects_access_tokens_and_tampered_credentials() {
        let auth = test_auth("server-secret");
        let access = auth.sign_token("master", "access", now_unix() + 900);
        let access_response = auth.refresh_session(&access);
        assert_eq!(access_response.status(), StatusCode::UNAUTHORIZED);
        let tampered = format!("{access}x");
        let tampered_response = auth.refresh_session(&tampered);
        assert_eq!(tampered_response.status(), StatusCode::UNAUTHORIZED);
    }
}
