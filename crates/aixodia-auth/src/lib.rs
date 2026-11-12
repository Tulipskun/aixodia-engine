//! GitHub bearer authentication for the engine's HTTP and WebSocket surface.
//!
//! Callers present a GitHub access token as `Authorization: Bearer <token>`.
//! The token is verified against the GitHub user API, and the login must be on
//! the configured allowlist (`GITHUB_ALLOWED_USERS`). Verified results are
//! cached per token so a chatty client does not call GitHub on every request;
//! a token that stops resolving to an allowed login is rejected once its cache
//! entry expires. When GitHub itself is unreachable, a cached verification is
//! honoured for a grace period instead of locking the user out of their own
//! engine.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;

const GITHUB_USER_API: &str = "https://api.github.com/user";
const USER_AGENT: &str = "aixodia-engine";

/// How long a successful verification answers from cache.
const VERIFY_TTL: Duration = Duration::from_secs(15 * 60);
/// How long a cached verification still counts when GitHub cannot be reached.
const OUTAGE_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug)]
pub enum AuthError {
    /// The token is missing, malformed, or GitHub rejected it.
    Unauthorized,
    /// The token is valid but the login is not on the allowlist.
    Forbidden(String),
    /// GitHub could not be reached and no usable cached verdict exists.
    Unavailable(String),
}

impl AuthError {
    pub fn status(&self) -> u16 {
        match self {
            AuthError::Unauthorized => 401,
            AuthError::Forbidden(_) => 403,
            AuthError::Unavailable(_) => 503,
        }
    }

    pub fn message(&self) -> String {
        match self {
            AuthError::Unauthorized => "token rejected".to_string(),
            AuthError::Forbidden(login) => format!("user {login} is not allowed"),
            AuthError::Unavailable(err) => format!("github verification failed: {err}"),
        }
    }
}

#[derive(Debug, Clone)]
struct CachedLogin {
    login: String,
    verified_at: Instant,
}

/// Verifies GitHub bearer tokens against the allowlist, with a per-token cache.
pub struct GitHubAuth {
    allowed: Vec<String>,
    api_url: String,
    client: reqwest::Client,
    cache: Mutex<HashMap<String, CachedLogin>>,
}

#[derive(Deserialize)]
struct GitHubUser {
    login: String,
}

impl GitHubAuth {
    /// Builds the verifier from a comma-separated allowlist of GitHub logins.
    /// Comparison is case-insensitive, matching GitHub's own login handling.
    pub fn new(allowed: &str) -> Self {
        let allowed = allowed
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_lowercase)
            .collect::<Vec<_>>();
        Self {
            allowed,
            api_url: GITHUB_USER_API.to_string(),
            client: reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .timeout(Duration::from_secs(10))
                .build()
                .expect("github auth client"),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Points the verifier at a different user API. Tests use this to run the
    /// full verification flow against a local server; production never calls it.
    pub fn with_api_url(mut self, api_url: impl Into<String>) -> Self {
        self.api_url = api_url.into();
        self
    }

    pub fn is_configured(&self) -> bool {
        !self.allowed.is_empty()
    }

    /// Verifies the bearer token and returns the GitHub login it belongs to.
    pub async fn verify(&self, token: &str) -> Result<String, AuthError> {
        let token = token.trim();
        if token.is_empty() || !self.is_configured() {
            return Err(AuthError::Unauthorized);
        }

        if let Some(cached) = self.cache.lock().expect("github cache").get(token) {
            if cached.verified_at.elapsed() < VERIFY_TTL {
                return Ok(cached.login.clone());
            }
        }

        let response = self
            .client
            .get(&self.api_url)
            .header("accept", "application/vnd.github+json")
            .bearer_auth(token)
            .send()
            .await;

        let response = match response {
            Ok(response) => response,
            Err(err) => return self.unavailable(token, err.to_string()),
        };

        match response.status() {
            status if status.is_success() => {
                let user = response
                    .json::<GitHubUser>()
                    .await
                    .map_err(|err| AuthError::Unavailable(err.to_string()))?;
                let login = user.login.trim().to_string();
                if login.is_empty() {
                    return Err(AuthError::Unavailable("github returned no login".into()));
                }
                if !self.allowed.contains(&login.to_lowercase()) {
                    return Err(AuthError::Forbidden(login));
                }
                self.cache.lock().expect("github cache").insert(
                    token.to_string(),
                    CachedLogin {
                        login: login.clone(),
                        verified_at: Instant::now(),
                    },
                );
                Ok(login)
            }
            status if status.as_u16() == 401 || status.as_u16() == 403 => {
                self.cache.lock().expect("github cache").remove(token);
                Err(AuthError::Unauthorized)
            }
            status => self.unavailable(token, format!("github returned HTTP {status}")),
        }
    }

    /// GitHub could not be reached. Fall back to a previously verified verdict
    /// while it is still within the outage grace window.
    fn unavailable(&self, token: &str, err: String) -> Result<String, AuthError> {
        let cache = self.cache.lock().expect("github cache");
        if let Some(cached) = cache.get(token) {
            if cached.verified_at.elapsed() < OUTAGE_GRACE {
                return Ok(cached.login.clone());
            }
        }
        drop(cache);
        Err(AuthError::Unavailable(err))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A one-response HTTP server on an ephemeral port. Returns its base URL.
    async fn mock_server(status: u16, body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = socket.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        format!("http://{addr}/user")
    }

    #[test]
    fn allowlist_parses_and_normalises_case() {
        let auth = GitHubAuth::new(" Tulipskun, other-user ,");
        assert!(auth.is_configured());
        assert!(auth.allowed.contains(&"tulipskun".to_string()));
        assert!(auth.allowed.contains(&"other-user".to_string()));
        assert!(!GitHubAuth::new(" , ").is_configured());
    }

    #[tokio::test]
    async fn empty_token_is_unauthorized() {
        let auth = GitHubAuth::new("Tulipskun");
        assert!(matches!(
            auth.verify("").await,
            Err(AuthError::Unauthorized)
        ));
        assert!(matches!(
            auth.verify("   ").await,
            Err(AuthError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn allowed_login_is_verified_and_cached() {
        let url = mock_server(200, r#"{"login":"Tulipskun"}"#).await;
        let auth = GitHubAuth::new("tulipskun").with_api_url(url);
        assert_eq!(auth.verify("gh-token").await.unwrap(), "Tulipskun");
        // The second call hits the cache: the mock server answers once only.
        assert_eq!(auth.verify("gh-token").await.unwrap(), "Tulipskun");
    }

    #[tokio::test]
    async fn login_outside_the_allowlist_is_forbidden() {
        let url = mock_server(200, r#"{"login":"stranger"}"#).await;
        let auth = GitHubAuth::new("Tulipskun").with_api_url(url);
        assert!(matches!(
            auth.verify("gh-token").await,
            Err(AuthError::Forbidden(_))
        ));
    }

    #[tokio::test]
    async fn github_rejecting_the_token_is_unauthorized() {
        let url = mock_server(401, r#"{}"#).await;
        let auth = GitHubAuth::new("Tulipskun").with_api_url(url);
        assert!(matches!(
            auth.verify("gh-token").await,
            Err(AuthError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn unreachable_github_without_cache_is_unavailable() {
        let auth = GitHubAuth::new("Tulipskun").with_api_url("http://127.0.0.1:9/user");
        assert!(matches!(
            auth.verify("gh-token").await,
            Err(AuthError::Unavailable(_))
        ));
    }

    #[test]
    fn error_statuses() {
        assert_eq!(AuthError::Unauthorized.status(), 401);
        assert_eq!(AuthError::Forbidden("x".into()).status(), 403);
        assert_eq!(AuthError::Unavailable("e".into()).status(), 503);
    }
}
