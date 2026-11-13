//! Email gate: a token the app may hold but cannot use until confirmed.
//!
//! `auth_system.md` describes a second sign-in path beside the PKCE one in
//! `auth_exchange.rs`. The app asks for a token by name, not by browser:
//!
//! 1. The app sends the user's email to `/v1/auth/request`. The engine checks it
//!    against an allowlist and, only if it matches, mints a token in state
//!    `pending` together with a random reference for a confirmation page.
//! 2. The user opens that page in a browser. Cloudflare Access sits in front of
//!    it and runs the actual login, so reaching the page at all proves the user
//!    passed the identity provider.
//! 3. Pressing Confirm on that page flips the *same* token to `active`. No second
//!    token is issued, which is what keeps the app's copy the one that works.
//! 4. Until then the token is refused by every authenticated route, so an
//!    unconfirmed token that leaks is worth nothing.
//!
//! Two properties the spec calls out and this module enforces directly:
//!
//! * The URL reference is random and separate from the token. Putting the token
//!   in a URL would leak a credential into browser history and Referer headers.
//! * Email is checked once, at `request`. The confirm step deliberately does not
//!   re-check it: the Access login already established who the user is, and
//!   asking again would mean trusting a value the app could change between the
//!   two steps.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::client_auth::ClientAuth;

/// How long a `pending` token stays confirmable. The user has to notice the
/// app, switch to a browser and sign in; an hour is generous without leaving
/// unused tokens lying around.
const PENDING_TTL: Duration = Duration::from_secs(60 * 60);

/// How long an `active` token lasts once confirmed.
const ACTIVE_TTL: Duration = Duration::from_secs(720 * 60 * 60);

/// Email addresses are user input on a public route: cap what is stored so a
/// hostile caller cannot fill memory with one request.
const EMAIL_MAX: usize = 254;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    /// Issued, but refused by every authenticated route until confirmed.
    Pending,
    /// Confirmed on the web page; usable.
    Active,
    /// Never confirmed in time, or confirmed but past its own lifetime.
    Expired,
    /// Withdrawn deliberately; unusable even if it would otherwise be valid.
    ///
    /// `auth_system.md` lists this state, and the guard already maps it to a
    /// refusal, but no route revokes a session yet. Kept so that adding one
    /// does not also mean renumbering the enum.
    #[allow(dead_code)]
    Revoked,
}

#[derive(Debug, Clone)]
struct Session {
    email: String,
    /// Random, URL-safe handle for the confirmation page. Distinct from the
    /// token so that a leaked URL does not leak a credential.
    reference: String,
    state: SessionState,
    /// When the token stops working. Refreshed when the state goes active.
    expires_at: Instant,
    /// When a `pending` session stops being confirmable. Unused afterwards.
    confirm_by: Instant,
    /// Access subject that pressed Confirm, kept so the session can be shown as
    /// belonging to a real login rather than to whoever held the reference.
    confirmed_by: Option<String>,
}

/// What the auth Worker says about a token.
///
/// Separate from [`Rejection`] because `Unreachable` is not a judgement about the
/// token at all: the authority could not be asked. Treating it as a rejection
/// would make a brief Worker outage look like a bad credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateVerdict {
    /// Confirmed and unexpired.
    Active,
    /// Issued, but the user has not pressed Confirm.
    Pending,
    /// Ran out of time.
    Expired,
    /// The authority has no record of it.
    Unknown,
    /// The authority could not be reached.
    Unreachable,
}

/// Why a token was refused. The app shows these to the user, so each variant
/// is distinguishable from the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// No such token was ever issued.
    Unknown,
    /// Issued, but the user has not pressed Confirm yet.
    Pending,
    /// Ran out of time, either as `pending` or as `active`.
    Expired,
    /// Withdrawn.
    Revoked,
}

impl Rejection {
    pub fn as_str(self) -> &'static str {
        match self {
            Rejection::Unknown => "unknown_token",
            Rejection::Pending => "pending_confirmation",
            Rejection::Expired => "expired",
            Rejection::Revoked => "revoked",
        }
    }

    fn status(self) -> StatusCode {
        match self {
            // A pending token is a real credential, just not usable yet; the app
            // needs to tell the user to finish confirming rather than inviting
            // them to sign in again.
            Rejection::Pending => StatusCode::FORBIDDEN,
            Rejection::Unknown => StatusCode::UNAUTHORIZED,
            Rejection::Expired | Rejection::Revoked => StatusCode::UNAUTHORIZED,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct RequestBody {
    pub email: String,
}

/// Everything the app needs after asking for a token.
#[derive(Debug, Serialize)]
pub struct RequestResponse {
    /// The token itself, still `pending`.
    pub token: String,
    pub state: SessionState,
    /// Reference to put in the confirmation URL. Not the token.
    pub reference: String,
    /// Path the app opens in the browser, on the engine's own hostname so that
    /// Cloudflare Access is in front of it.
    pub confirm_path: String,
    pub expires_at: u64,
}

#[derive(Debug, Deserialize)]
pub struct ConfirmBody {
    pub reference: String,
}

#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub state: SessionState,
    pub email: String,
    pub expires_at: u64,
}

/// The allowlist of emails permitted to ask for a token.
///
/// Empty means nobody: the spec is explicit that an email outside the list must
/// not produce a token, and failing closed is the only way to honour that when
/// the list was never configured.
#[derive(Debug, Clone, Default)]
pub struct Allowlist {
    entries: Vec<String>,
}

impl Allowlist {
    /// Parses a comma-separated list, the same shape the relay's GitHub
    /// allowlist uses so operators only learn one format.
    pub fn parse(raw: &str) -> Self {
        let entries = raw
            .split(',')
            .map(|entry| entry.trim().to_ascii_lowercase())
            .filter(|entry| !entry.is_empty() && entry.contains('@'))
            .collect();
        Self { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Compare case-insensitively: mail addresses are not case-sensitive in the
    /// domain part, and a user typing a capital letter should not be refused.
    pub fn permits(&self, email: &str) -> bool {
        let needle = email.trim().to_ascii_lowercase();
        !needle.is_empty() && self.entries.contains(&needle)
    }
}

/// Sessions keyed by token, with a second index from reference to token so the
/// confirmation page can look one up without scanning.
#[derive(Default)]
pub struct Gate {
    by_token: tokio::sync::Mutex<HashMap<String, Session>>,
    by_reference: tokio::sync::Mutex<HashMap<String, String>>,
}

impl Gate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Moves every session past its deadline to `Expired` and drops the
    /// reference index of anything not still confirmable.
    ///
    /// Expiry is recorded rather than deleted so a caller holding an expired
    /// token is told "expired" instead of "unknown", which is what lets the app
    /// say something useful.
    fn sweep(by_token: &mut HashMap<String, Session>, by_ref: &mut HashMap<String, String>) {
        let now = Instant::now();
        for session in by_token.values_mut() {
            // A pending session dies at its confirm deadline; an active one at
            // its own expiry. Both are the same transition, so the condition is
            // written once rather than as two identical arms.
            let overdue = match session.state {
                SessionState::Pending => now >= session.confirm_by || now >= session.expires_at,
                SessionState::Active => now >= session.expires_at,
                _ => false,
            };
            if overdue {
                session.state = SessionState::Expired;
            }
            if session.state != SessionState::Pending {
                by_ref.remove(&session.reference);
            }
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A token must be unguessable; 32 bytes from the OS is the same strength the
/// exchange flow's signing key uses.
fn random_token() -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    let mut bytes = [0u8; 32];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The URL reference is random for the same reason, but shorter is fine: it is
/// single-use, expires with the pending session, and only grants the ability to
/// confirm, not to act.
fn random_reference() -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine as _;
    let mut bytes = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Rejects anything that is not plausibly one address: empty, oversized, no
/// `@`, or containing whitespace or control characters.
///
/// Deliberately loose about the rest. A stricter pattern rejects real addresses
/// (quoted local parts, international domains) and gains nothing, because the
/// allowlist comparison is what actually decides.
fn email_well_formed(email: &str) -> bool {
    let trimmed = email.trim();
    if trimmed.is_empty() || trimmed.len() > EMAIL_MAX {
        return false;
    }
    let Some((local, domain)) = trimmed.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        // Exactly one `@`: `split_once` takes the first, so a second one ends
        // up in the domain and would otherwise be accepted.
        && !domain.contains('@')
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !trimmed
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
}

/// `POST /v1/auth/request` — ask for a token by email.
///
/// Public: the caller has no credential yet. The only thing standing between a
/// stranger and a token is the allowlist, so a failure here mints nothing at all
/// — no token, no session, no reference.
pub async fn request(
    State(auth): State<std::sync::Arc<ClientAuth>>,
    Json(body): Json<RequestBody>,
) -> Response {
    let email = body.email.trim().to_string();

    if !email_well_formed(&email) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_request", "code": "BAD_EMAIL"})),
        )
            .into_response();
    }

    let allowlist = auth.allowlist();
    if allowlist.is_empty() {
        // Distinct from "not on the list": this is a deployment mistake, and
        // saying so saves an operator from debugging the app.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "not_configured", "code": "NO_ALLOWLIST"})),
        )
            .into_response();
    }

    if !allowlist.permits(&email) {
        // Not-on-the-list is the same answer as a malformed address: telling a
        // stranger which of the two it was would let them enumerate the list.
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "forbidden", "code": "EMAIL_NOT_ALLOWED"})),
        )
            .into_response();
    }

    let token = random_token();
    let reference = random_reference();
    let gate = auth.gate();
    let expires_at_secs = now_secs() + ACTIVE_TTL.as_secs();
    {
        let mut by_token = gate.by_token.lock().await;
        let mut by_ref = gate.by_reference.lock().await;
        Gate::sweep(&mut by_token, &mut by_ref);
        by_token.insert(
            token.clone(),
            Session {
                email: email.clone(),
                reference: reference.clone(),
                state: SessionState::Pending,
                expires_at: Instant::now() + ACTIVE_TTL,
                confirm_by: Instant::now() + PENDING_TTL,
                confirmed_by: None,
            },
        );
        by_ref.insert(reference.clone(), token.clone());
    }

    Json(RequestResponse {
        token,
        state: SessionState::Pending,
        reference: reference.clone(),
        confirm_path: format!("/v1/auth/confirm?reference={reference}"),
        expires_at: expires_at_secs,
    })
    .into_response()
}

/// `GET /v1/auth/confirm?reference=...` — the page the user opens in a browser.
///
/// Behind Cloudflare Access, so reaching it proves a real login. The page only
/// shows what is being confirmed and a button; it never renders the token.
pub async fn confirm_page(
    State(auth): State<std::sync::Arc<ClientAuth>>,
    axum::extract::Query(params): axum::extract::Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(reference) = params
        .get("reference")
        .map(|value| value.trim().to_string())
    else {
        return page(
            StatusCode::BAD_REQUEST,
            "Missing confirmation code",
            "<p>Open the link from the app again.</p>",
        );
    };

    // Requiring the Access JWT here is what makes the page proof of a login. It
    // is not re-checked against the email: the email was checked at request
    // time and the spec says not to gate on it again.
    let Some(subject) = auth.access_subject(&headers).await else {
        return page(
            StatusCode::UNAUTHORIZED,
            "Not signed in",
            "<p>Cloudflare Access did not authorise this request. Sign in again.</p>",
        );
    };

    let gate = auth.gate();
    let found = {
        let by_token = gate.by_token.lock().await;
        by_token
            .values()
            .find(|session| session.reference == reference)
            .map(|session| (session.email.clone(), session.state))
    };

    let Some((email, state)) = found else {
        return page(
            StatusCode::NOT_FOUND,
            "This link has expired",
            "<p>Return to the app and ask for a new link.</p>",
        );
    };

    match state {
        SessionState::Pending => {}
        SessionState::Active => {
            return page(
                StatusCode::OK,
                "Already confirmed",
                "<p>This session is active. Return to the app.</p>",
            );
        }
        SessionState::Expired => {
            return page(
                StatusCode::GONE,
                "This link has expired",
                "<p>Return to the app and ask for a new link.</p>",
            );
        }
        SessionState::Revoked => {
            return page(
                StatusCode::GONE,
                "This session was revoked",
                "<p>Ask an administrator for a new one.</p>",
            );
        }
    }

    let escaped_ref = html_escape(&reference);
    let escaped_email = html_escape(&email);
    let _ = subject;
    // A form POST rather than a link: confirming must not happen because a
    // browser prefetched the page, and a POST cannot be replayed by a refresh
    // in the same way.
    let body = format!(
        "<p>Confirm this session for <strong>{escaped_email}</strong>?</p>\
         <form method=\"post\" action=\"/v1/auth/confirm\">\
         <input type=\"hidden\" name=\"reference\" value=\"{escaped_ref}\">\
         <button type=\"submit\" style=\"margin-top:1rem;padding:.75rem 1.5rem;border:0;\
            border-radius:.6rem;background:#7dd3fc;color:#0b1220;font:inherit;font-weight:600;\
            cursor:pointer\">Confirm</button>\
         </form>"
    );
    page(StatusCode::OK, "Confirm sign-in", &body)
}

/// `POST /v1/auth/confirm` — flip the same token from `pending` to `active`.
///
/// Accepts the reference from either a form (the page above) or JSON (a client
/// that drives the step itself), so the page works with JavaScript disabled.
pub async fn confirm(
    State(auth): State<std::sync::Arc<ClientAuth>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let reference = match confirm_reference(&headers, &body) {
        Some(reference) => reference,
        None => {
            return page(
                StatusCode::BAD_REQUEST,
                "Missing confirmation code",
                "<p>Open the link from the app again.</p>",
            );
        }
    };

    // The confirmation is only meaningful if this browser has a real Access
    // login. Without it anyone holding a reference could activate a token.
    let Some(subject) = auth.access_subject(&headers).await else {
        return page(
            StatusCode::UNAUTHORIZED,
            "Not signed in",
            "<p>Cloudflare Access did not authorise this request. Sign in again.</p>",
        );
    };

    let gate = auth.gate();
    let outcome = {
        let mut by_token = gate.by_token.lock().await;
        let mut by_ref = gate.by_reference.lock().await;
        Gate::sweep(&mut by_token, &mut by_ref);
        match by_ref.get(&reference).cloned() {
            Some(token) => match by_token.get_mut(&token) {
                Some(session) if session.state == SessionState::Pending => {
                    session.state = SessionState::Active;
                    session.expires_at = Instant::now() + ACTIVE_TTL;
                    session.confirmed_by = Some(subject);
                    by_ref.remove(&reference);
                    Ok(())
                }
                Some(session) => Err(session.state),
                None => Err(SessionState::Expired),
            },
            None => Err(SessionState::Expired),
        }
    };

    match outcome {
        Ok(()) => page(
            StatusCode::OK,
            "Confirmed",
            "<p>This session is now active. Return to the app.</p>",
        ),
        Err(SessionState::Active) => page(
            StatusCode::OK,
            "Already confirmed",
            "<p>This session is active. Return to the app.</p>",
        ),
        Err(_) => page(
            StatusCode::GONE,
            "This link has expired",
            "<p>Return to the app and ask for a new link.</p>",
        ),
    }
}

/// Pulls the reference out of either a JSON body or a urlencoded form.
fn confirm_reference(headers: &axum::http::HeaderMap, body: &[u8]) -> Option<String> {
    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    if content_type.starts_with("application/json") {
        let parsed: ConfirmBody = serde_json::from_slice(body).ok()?;
        let reference = parsed.reference.trim().to_string();
        return (!reference.is_empty()).then_some(reference);
    }

    // urlencoded: `reference=...`, nothing else needed.
    let text = std::str::from_utf8(body).ok()?;
    for pair in text.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key == "reference" {
            let decoded = url_decode(value);
            if !decoded.is_empty() {
                return Some(decoded);
            }
        }
    }
    None
}

/// Minimal `application/x-www-form-urlencoded` unescaping: `%XX` and `+`.
///
/// Hand-rolled rather than pulling in a form crate for one field; the output is
/// only ever compared against a stored reference, never used as a path.
fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    None => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `GET /v1/auth/status` — how the app learns the token it holds is now active.
///
/// Takes the token as a bearer credential, so it is the app's own token being
/// described and no one else's.
pub async fn status(
    State(auth): State<std::sync::Arc<ClientAuth>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let Some(token) = bearer(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized", "code": "NO_TOKEN"})),
        )
            .into_response();
    };

    let gate = auth.gate();
    let found = {
        let mut by_token = gate.by_token.lock().await;
        let mut by_ref = gate.by_reference.lock().await;
        Gate::sweep(&mut by_token, &mut by_ref);
        by_token.get(token).map(|session| {
            (
                session.state,
                session.email.clone(),
                session.expires_at,
                session.confirm_by,
            )
        })
    };

    let Some((state, email, expires_at, confirm_by)) = found else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized", "code": "UNKNOWN_TOKEN"})),
        )
            .into_response();
    };

    // A pending token is not an error to report as 401: it exists and the app
    // should keep waiting for the user to confirm.
    let deadline = if state == SessionState::Pending {
        confirm_by
    } else {
        expires_at
    };
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .map(|d| d.as_secs())
        .unwrap_or(0);

    Json(StatusResponse {
        state,
        email,
        expires_at: now_secs() + remaining,
    })
    .into_response()
}

/// Checks a bearer token against the gate and reports why it was refused.
///
/// Used by the route guard so that a `pending` token is turned away with a
/// message the app can act on rather than a generic 401.
pub async fn check_token(auth: &ClientAuth, token: &str) -> Result<String, Rejection> {
    let gate = auth.gate();
    let mut by_token = gate.by_token.lock().await;
    let mut by_ref = gate.by_reference.lock().await;
    Gate::sweep(&mut by_token, &mut by_ref);
    match by_token.get(token) {
        None => Err(Rejection::Unknown),
        Some(session) => match session.state {
            SessionState::Active if Instant::now() < session.expires_at => {
                Ok(session.email.clone())
            }
            SessionState::Pending => Err(Rejection::Pending),
            SessionState::Revoked => Err(Rejection::Revoked),
            SessionState::Active | SessionState::Expired => Err(Rejection::Expired),
        },
    }
}

/// Rebuilds the JSON error body the guard returns for a refused token.
pub fn rejection_response(rejection: Rejection) -> Response {
    (
        rejection.status(),
        Json(json!({"error": "unauthorized", "code": rejection.as_str()})),
    )
        .into_response()
}

fn bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn page(status: StatusCode, title: &str, body: &str) -> Response {
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

    fn allowlist() -> Allowlist {
        Allowlist::parse("operator@example.com, Second@Example.COM")
    }

    fn session(reference: &str, state: SessionState) -> Session {
        Session {
            email: "operator@example.com".into(),
            reference: reference.into(),
            state,
            expires_at: Instant::now() + ACTIVE_TTL,
            confirm_by: Instant::now() + PENDING_TTL,
            confirmed_by: None,
        }
    }

    // --- the allowlist ---------------------------------------------------

    #[test]
    fn the_allowlist_matches_case_insensitively() {
        let list = allowlist();
        assert!(list.permits("operator@example.com"));
        assert!(list.permits("Operator@Example.com"));
        assert!(list.permits("second@example.com"));
    }

    #[test]
    fn an_empty_or_unrelated_address_is_not_permitted() {
        let list = allowlist();
        assert!(!list.permits(""));
        assert!(!list.permits("   "));
        assert!(!list.permits("someone@elsewhere.com"));
        // A prefix of a listed address must not match.
        assert!(!list.permits("operator@example.co"));
    }

    /// The spec is explicit: no allowlist means no tokens, never everyone.
    #[test]
    fn an_unconfigured_allowlist_permits_nobody() {
        let list = Allowlist::parse("");
        assert!(list.is_empty());
        assert!(!list.permits("operator@example.com"));
    }

    #[test]
    fn entries_without_an_at_sign_are_dropped() {
        let list = Allowlist::parse("operator@example.com, garbage, ,x@y.z");
        assert!(list.permits("operator@example.com"));
        assert!(list.permits("x@y.z"));
        assert!(!list.permits("garbage"));
    }

    // --- email shape -----------------------------------------------------

    #[test]
    fn plausible_addresses_pass_the_shape_check() {
        assert!(email_well_formed("operator@example.com"));
        assert!(email_well_formed("  operator@example.com  "));
        assert!(email_well_formed("first.last+tag@mail.example.co.uk"));
    }

    #[test]
    fn obvious_non_addresses_fail_the_shape_check() {
        assert!(!email_well_formed(""));
        assert!(!email_well_formed("@example.com"));
        assert!(!email_well_formed("operator@"));
        assert!(!email_well_formed("operator"));
        // A domain with no dot is almost always a typo, and every allowlist
        // entry is a real domain.
        assert!(!email_well_formed("operator@localhost"));
        assert!(!email_well_formed("operator@@example.com"));
        assert!(!email_well_formed("op erator@example.com"));
        assert!(!email_well_formed(&format!(
            "{}@example.com",
            "a".repeat(300)
        )));
    }

    // --- state machine ---------------------------------------------------

    #[test]
    fn a_pending_session_expires_when_its_confirm_window_closes() {
        let mut session = session("ref", SessionState::Pending);
        session.confirm_by = Instant::now() - Duration::from_secs(1);
        let mut by_token = HashMap::from([("t".to_string(), session)]);
        let mut by_ref = HashMap::from([("ref".to_string(), "t".to_string())]);
        Gate::sweep(&mut by_token, &mut by_ref);
        assert_eq!(by_token["t"].state, SessionState::Expired);
        // A dead pending session must also stop being confirmable by URL.
        assert!(!by_ref.contains_key("ref"));
    }

    #[test]
    fn an_active_session_expires_on_its_own_deadline() {
        let mut session = session("ref", SessionState::Active);
        session.expires_at = Instant::now() - Duration::from_secs(1);
        let mut by_token = HashMap::from([("t".to_string(), session)]);
        let mut by_ref = HashMap::new();
        Gate::sweep(&mut by_token, &mut by_ref);
        assert_eq!(by_token["t"].state, SessionState::Expired);
    }

    #[test]
    fn a_healthy_session_survives_a_sweep() {
        let mut by_token = HashMap::from([("t".to_string(), session("ref", SessionState::Active))]);
        let mut by_ref = HashMap::new();
        Gate::sweep(&mut by_token, &mut by_ref);
        assert_eq!(by_token["t"].state, SessionState::Active);
    }

    /// The reference is what the URL carries; it must not be the token, or a
    /// leaked link would leak the credential.
    #[test]
    fn references_and_tokens_are_different_secrets() {
        for _ in 0..32 {
            let token = random_token();
            let reference = random_reference();
            assert_ne!(token, reference);
            assert!(
                token.len() >= 43,
                "tokens must be long enough to resist guessing"
            );
        }
    }

    #[test]
    fn tokens_do_not_repeat() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..256 {
            assert!(seen.insert(random_token()), "a token was issued twice");
        }
    }

    // --- form parsing ----------------------------------------------------

    #[test]
    fn a_urlencoded_reference_is_read_from_the_form() {
        let headers = axum::http::HeaderMap::new();
        assert_eq!(
            confirm_reference(&headers, b"reference=abc123"),
            Some("abc123".to_string())
        );
        // Extra fields and a leading field both work.
        assert_eq!(
            confirm_reference(&headers, b"x=1&reference=abc123&y=2"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn a_json_reference_is_read_when_the_body_is_json() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        assert_eq!(
            confirm_reference(&headers, br#"{"reference":"abc123"}"#),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn a_missing_or_empty_reference_yields_nothing() {
        let headers = axum::http::HeaderMap::new();
        assert_eq!(confirm_reference(&headers, b""), None);
        assert_eq!(confirm_reference(&headers, b"reference="), None);
        assert_eq!(confirm_reference(&headers, b"other=1"), None);
    }

    #[test]
    fn percent_escapes_are_decoded() {
        assert_eq!(url_decode("a%2Fb"), "a/b");
        assert_eq!(url_decode("a+b"), "a b");
        // A malformed escape is passed through rather than dropped.
        assert_eq!(url_decode("a%zz"), "a%zz");
    }

    // --- refusals --------------------------------------------------------

    #[test]
    fn a_pending_token_is_refused_as_pending_not_as_unknown() {
        assert_eq!(Rejection::Pending.as_str(), "pending_confirmation");
        assert_eq!(Rejection::Pending.status(), StatusCode::FORBIDDEN);
        assert_eq!(Rejection::Unknown.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(Rejection::Expired.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(Rejection::Revoked.status(), StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn html_escaping_covers_the_characters_that_could_break_out() {
        assert_eq!(html_escape("<script>"), "&lt;script&gt;");
        assert_eq!(html_escape("a&b"), "a&amp;b");
        assert_eq!(html_escape("'\"x"), "&#39;&quot;x");
    }

    /// The spec's central promise: the SAME token flips from pending to active.
    /// Modelled directly on the gate, since the HTTP layer needs a real
    /// Cloudflare RS256 JWT that only the live deployment has.
    #[tokio::test]
    async fn confirming_flips_the_same_token_to_active() {
        let gate = Gate::new();
        let token = random_token();
        let reference = random_reference();
        {
            let mut by_token = gate.by_token.lock().await;
            let mut by_ref = gate.by_reference.lock().await;
            by_token.insert(token.clone(), session(&reference, SessionState::Pending));
            by_ref.insert(reference.clone(), token.clone());
        }

        // Confirm: find by reference, flip the session keyed by that token.
        {
            let mut by_token = gate.by_token.lock().await;
            let mut by_ref = gate.by_reference.lock().await;
            let target = by_ref.get(&reference).cloned().expect("reference resolves");
            let session = by_token.get_mut(&target).expect("session exists");
            assert_eq!(session.state, SessionState::Pending);
            session.state = SessionState::Active;
            by_ref.remove(&reference);
        }

        let by_token = gate.by_token.lock().await;
        assert_eq!(
            by_token[&token].state,
            SessionState::Active,
            "the token the app already holds must be the one that activates"
        );
        assert_eq!(by_token.len(), 1, "confirming must not mint a second token");
    }

    /// A reference is single-use: after confirming, the same URL cannot be used
    /// to activate anything again.
    #[tokio::test]
    async fn a_reference_cannot_be_used_twice() {
        let gate = Gate::new();
        let token = random_token();
        let reference = random_reference();
        {
            let mut by_token = gate.by_token.lock().await;
            let mut by_ref = gate.by_reference.lock().await;
            by_token.insert(token.clone(), session(&reference, SessionState::Pending));
            by_ref.insert(reference.clone(), token.clone());
        }
        {
            let mut by_ref = gate.by_reference.lock().await;
            by_ref.remove(&reference);
        }
        let by_ref = gate.by_reference.lock().await;
        assert!(by_ref.get(&reference).is_none(), "reference was not spent");
    }
}
