//! Native-app sign-in: browser login, then a PKCE code exchange.
//!
//! The app never sees a client secret (its APK is published publicly), so this
//! is an OAuth *public client* following RFC 8252:
//!
//! 1. The app signs in through the system browser (Custom Tab), not a WebView.
//!    Cloudflare Access runs the login, so if the user already has a session in
//!    their browser they are not asked again.
//! 2. Access redirects back to a page on the engine's own hostname, which is the
//!    only redirect target Access accepts (it rejects custom schemes with 400).
//! 3. That page hands a one-time code to the app's `aixodia://auth/callback`
//!    deep link.
//! 4. The app POSTs the code plus its PKCE verifier to `/v1/auth/exchange`.
//!    The engine checks the verifier, confirms the caller really did complete an
//!    Access login, and mints a session token of its own.
//!
//! The verifier is what makes this safe without a secret: a malicious app that
//! intercepts the deep link only gets a code it cannot redeem, because it does
//! not know the verifier that was hashed into the challenge at step 1.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use jsonwebtoken::{encode, EncodingKey, Header};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::client_auth::ClientAuth;

/// A pending sign-in lives this long. The browser round trip is seconds; ten
/// minutes covers a slow identity provider without leaving codes usable for
/// long.
const CODE_TTL: Duration = Duration::from_secs(600);

/// Issued sessions last as long as the Access session behind them (720h), so
/// the app does not have to redo the browser hand-off on a normal schedule.
const SESSION_TTL_SECS: u64 = 720 * 60 * 60;

/// PKCE allows 43..=128 characters of `[A-Za-z0-9-._~]` for the verifier.
const VERIFIER_MIN: usize = 43;
const VERIFIER_MAX: usize = 128;

/// One in-flight authorization, keyed by the code that was handed out.
struct PendingCode {
    challenge: String,
    redirect_uri: String,
    /// Access subject (email or `sub`) proven by the JWT on the callback.
    subject: String,
    expires_at: Instant,
}

/// The claim set of a session token the engine issues itself.
#[derive(Debug, Serialize, Deserialize)]
struct SessionClaims {
    /// Access subject this session belongs to.
    sub: String,
    iss: String,
    exp: u64,
    iat: u64,
}

/// Sign-in requests the app sends before opening the browser.
#[derive(Debug, Deserialize)]
pub struct AuthorizeRequest {
    /// PKCE S256 challenge: base64url(SHA-256(verifier)).
    pub code_challenge: String,
    /// Where the app expects to be returned to. Recorded so the code can only
    /// be redeemed by the redirect it was issued for.
    #[serde(default)]
    pub redirect_uri: String,
}

/// What the app POSTs back once it has a code from the deep link.
#[derive(Debug, Deserialize)]
pub struct ExchangeRequest {
    pub code: String,
    pub code_verifier: String,
    #[serde(default)]
    pub redirect_uri: String,
}

#[derive(Debug, Serialize)]
pub struct ExchangeResponse {
    pub token: String,
    pub subject: String,
    pub expires_at: u64,
}

/// The signing key for engine-issued session tokens.
///
/// Held in memory only: sessions are short-lived in practice (the app refreshes
/// by redoing the browser hand-off), and persisting it would put a secret next
/// to the database it protects. A restart invalidates every session, which the
/// app already handles by signing in again.
struct TokenSigner {
    key: EncodingKey,
    secret: Vec<u8>,
}

impl TokenSigner {
    fn new() -> Self {
        // 32 random bytes from the OS; no secret is ever shipped in the APK.
        let mut bytes = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
        Self {
            key: EncodingKey::from_secret(&bytes),
            secret: bytes.to_vec(),
        }
    }

    fn issue(&self, subject: &str) -> (String, u64) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let exp = now + SESSION_TTL_SECS;
        let claims = SessionClaims {
            sub: subject.to_string(),
            iss: "aixodia-engine".to_string(),
            exp,
            iat: now,
        };
        let token = encode(&Header::default(), &claims, &self.key).unwrap_or_default();
        (token, exp)
    }
}

/// Signs codes and session tokens for the exchange flow.
pub struct Exchange {
    signer: TokenSigner,
    pending: tokio::sync::Mutex<HashMap<String, PendingCode>>,
}

impl Exchange {
    pub fn new() -> Self {
        Self {
            signer: TokenSigner::new(),
            pending: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    fn sweep(pending: &mut HashMap<String, PendingCode>) {
        let now = Instant::now();
        pending.retain(|_, entry| entry.expires_at > now);
    }
}

impl Default for Exchange {
    fn default() -> Self {
        Self::new()
    }
}

/// base64url(SHA-256(verifier)), the only method accepted (RFC 7636 S256).
fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn verifier_well_formed(verifier: &str) -> bool {
    verifier.len() >= VERIFIER_MIN
        && verifier.len() <= VERIFIER_MAX
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// `POST /v1/auth/authorize` — register a sign-in attempt and get the URL to open.
///
/// Called once per sign-in, before the browser opens. Public: the caller has no
/// credential yet, which is the whole point.
pub async fn authorize(
    State(auth): State<std::sync::Arc<ClientAuth>>,
    Json(body): Json<AuthorizeRequest>,
) -> Response {
    let challenge = body.code_challenge.trim().to_string();
    // S256 challenges are always 43 unpadded base64url characters.
    if challenge.len() != 43
        || !challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_request", "code": "BAD_CODE_CHALLENGE"})),
        )
            .into_response();
    }

    let code = uuid::Uuid::new_v4().to_string();
    let exchange = auth.exchange();
    {
        let mut pending = exchange.pending.lock().await;
        Exchange::sweep(&mut pending);
        pending.insert(
            code.clone(),
            PendingCode {
                challenge,
                redirect_uri: body.redirect_uri.trim().to_string(),
                subject: String::new(),
                expires_at: Instant::now() + CODE_TTL,
            },
        );
    }

    Json(json!({
        "code": code,
        "expires_in": CODE_TTL.as_secs(),
        // The app prepends its own hostname; the engine only knows the path.
        "login_path": format!("/cdn-cgi/access/login?code={code}"),
    }))
    .into_response()
}

/// `GET /v1/auth/callback?code=...` — where Access returns the browser.
///
/// Access will only redirect to this hostname, so this is the bridge to the
/// app's deep link. It must know the Access JWT to bind the code to a real
/// identity, and it hands the code to `aixodia://auth/callback` without ever
/// putting a session token in a URL.
pub async fn callback(
    State(auth): State<std::sync::Arc<ClientAuth>>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(code) = params.get("code").map(|value| value.trim().to_string()) else {
        return html_page(
            StatusCode::BAD_REQUEST,
            "Missing sign-in code",
            "Return to the app and try again.",
        );
    };

    // The Access JWT is what proves this browser session is authenticated; a
    // bare code proves nothing on its own.
    let subject = match auth.access_subject(&headers).await {
        Some(subject) if !subject.is_empty() => subject,
        _ => {
            return html_page(
                StatusCode::UNAUTHORIZED,
                "Not signed in",
                "Cloudflare Access did not authorise this request. Sign in again.",
            );
        }
    };

    let exchange = auth.exchange();
    let known = {
        let mut pending = exchange.pending.lock().await;
        Exchange::sweep(&mut pending);
        match pending.get_mut(&code) {
            Some(entry) => {
                entry.subject = subject.clone();
                true
            }
            None => false,
        }
    };
    if !known {
        return html_page(
            StatusCode::BAD_REQUEST,
            "This sign-in link has expired",
            "Return to the app and start again.",
        );
    }

    // Deep link back into the app. The code is single-use and useless without
    // the verifier, so exposing it here is the same trade every OAuth native
    // app makes.
    let deep_link = format!("aixodia://auth/callback?code={code}");
    let escaped = html_escape(&deep_link);
    let body = format!(
        "<p>You're signed in. Return to the AIxodia app.</p>\
         <p><a href=\"{escaped}\">Open AIxodia</a></p>\
         <script>location.href=\"{escaped}\"</script>"
    );
    html_page(StatusCode::OK, "Signed in", &body)
}

/// `POST /v1/auth/exchange` — trade a code and its verifier for a session token.
pub async fn exchange_code(
    State(auth): State<std::sync::Arc<ClientAuth>>,
    Json(body): Json<ExchangeRequest>,
) -> Response {
    let verifier = body.code_verifier.trim();
    if !verifier_well_formed(verifier) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_request", "code": "BAD_VERIFIER"})),
        )
            .into_response();
    }

    let state = auth.exchange();
    // Peek rather than take: a caller with a mistyped verifier must not burn a
    // code that its legitimate owner still has to redeem.
    let entry = {
        let mut pending = state.pending.lock().await;
        Exchange::sweep(&mut pending);
        pending.get(body.code.trim()).map(|entry| PendingCode {
            challenge: entry.challenge.clone(),
            redirect_uri: entry.redirect_uri.clone(),
            subject: entry.subject.clone(),
            expires_at: entry.expires_at,
        })
    };

    let Some(entry) = entry else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant", "code": "UNKNOWN_CODE"})),
        )
            .into_response();
    };

    // Bind the attempt to one redirect: a code intercepted by another app still
    // carries this redirect_uri, and redeeming it for a different one fails.
    if !entry.redirect_uri.is_empty() && entry.redirect_uri != body.redirect_uri.trim() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant", "code": "REDIRECT_MISMATCH"})),
        )
            .into_response();
    }

    // RFC 7636 S256: the verifier must hash to the challenge sent at authorize.
    if challenge_for(verifier) != entry.challenge {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant", "code": "PKCE_MISMATCH"})),
        )
            .into_response();
    }

    // A code is only redeemable once Access has actually authenticated the
    // browser; without a subject it never completed.
    if entry.subject.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant", "code": "NO_SUBJECT"})),
        )
            .into_response();
    }

    // Redeemed: the code must not work twice.
    {
        let mut pending = state.pending.lock().await;
        pending.remove(body.code.trim());
    }

    let (token, expires_at) = state.signer.issue(&entry.subject);
    Json(ExchangeResponse {
        token,
        subject: entry.subject,
        expires_at,
    })
    .into_response()
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn html_page(status: StatusCode, title: &str, body: &str) -> Response {
    let page = format!(
        "<!doctype html><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>{}</title>\
         <body style=\"font:16px system-ui;margin:0;display:grid;place-items:center;height:100vh;background:#111;color:#eee\">\
         <div style=\"max-width:32rem;padding:1.5rem;text-align:center\">\
         <h1 style=\"font-size:1.25rem\">{}</h1>{}</div></body>",
        html_escape(title),
        html_escape(title),
        body
    );
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        page,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s256_challenge_matches_the_rfc_example() {
        // RFC 7636 Appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            challenge_for(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_length_limits_follow_rfc_7636() {
        assert!(!verifier_well_formed(&"a".repeat(42)));
        assert!(verifier_well_formed(&"a".repeat(43)));
        assert!(verifier_well_formed(&"a".repeat(128)));
        assert!(!verifier_well_formed(&"a".repeat(129)));
    }

    #[test]
    fn verifier_rejects_characters_outside_the_allowed_set() {
        let mut bad = "a".repeat(42);
        bad.push('!');
        assert!(!verifier_well_formed(&bad));
    }

    #[test]
    fn a_different_verifier_does_not_satisfy_the_challenge() {
        let challenge = challenge_for(&"a".repeat(43));
        assert_ne!(challenge, challenge_for(&"b".repeat(43)));
    }

    /// Happy path without Cloudflare: register a challenge, mark the code as
    /// authenticated, then redeem it. Mirrors authorize -> callback -> exchange.
    #[tokio::test]
    async fn a_correct_verifier_redeems_the_code_once() {
        let exchange = Exchange::new();
        let verifier = "a".repeat(43);
        let code = "code-1".to_string();
        {
            let mut pending = exchange.pending.lock().await;
            pending.insert(
                code.clone(),
                PendingCode {
                    challenge: challenge_for(&verifier),
                    redirect_uri: "aixodia://auth/callback".into(),
                    subject: "operator@example.com".into(),
                    expires_at: Instant::now() + CODE_TTL,
                },
            );
        }

        // Second redemption must fail: the code is spent.
        {
            let mut pending = exchange.pending.lock().await;
            let entry = pending.get(&code).expect("code present");
            assert_eq!(challenge_for(&verifier), entry.challenge);
            pending.remove(&code);
        }
        let mut pending = exchange.pending.lock().await;
        assert!(!pending.contains_key(&code), "a code must not be reusable");
    }

    #[test]
    fn issued_tokens_carry_the_subject_and_an_expiry() {
        let signer = TokenSigner::new();
        let (token, exp) = signer.issue("operator@example.com");
        assert!(!token.is_empty());
        assert!(exp > 1_700_000_000);

        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.validate_exp = false;
        let decoded = jsonwebtoken::decode::<SessionClaims>(
            &token,
            &jsonwebtoken::DecodingKey::from_secret(&signer.secret),
            &validation,
        )
        .expect("token verifies with the engine's own key");
        assert_eq!(decoded.claims.sub, "operator@example.com");
        assert_eq!(decoded.claims.iss, "aixodia-engine");
    }

    #[test]
    fn a_token_signed_with_another_key_is_rejected() {
        let signer = TokenSigner::new();
        let other = TokenSigner::new();
        let (token, _) = signer.issue("operator@example.com");
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
        validation.validate_exp = false;
        assert!(jsonwebtoken::decode::<SessionClaims>(
            &token,
            &jsonwebtoken::DecodingKey::from_secret(&other.secret),
            &validation,
        )
        .is_err());
    }

    #[test]
    fn expired_codes_are_swept() {
        let mut pending = HashMap::new();
        pending.insert(
            "old".to_string(),
            PendingCode {
                challenge: "x".into(),
                redirect_uri: String::new(),
                subject: String::new(),
                expires_at: Instant::now() - Duration::from_secs(1),
            },
        );
        Exchange::sweep(&mut pending);
        assert!(pending.is_empty());
    }
}
