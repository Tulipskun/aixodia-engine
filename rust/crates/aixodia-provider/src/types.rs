//! Shared provider types and the adapter interface.
//!
//! Port of the engine's Go `provider` package. `session`, `tools` and the
//! gateway all speak these types.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::{Map, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio_util::io::StreamReader;

/// The user agent every outbound request must carry. Cloudflare answers a 403
/// HTML challenge for the default client user agent.
pub const USER_AGENT: &str = "aixodia-engine/1.0";

/// One picture carried by a message. `data` is the encoded file and `media_type`
/// its MIME type (`image/jpeg`); both wire formats need the type to decode the
/// bytes, and neither accepts a bare blob.
#[derive(Debug, Clone, Default)]
pub struct Image {
    pub media_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default)]
pub struct Message {
    pub role: String,
    /// The text of the message. When `images` is empty it is what the wire
    /// carries; otherwise the text becomes one part of a multi-part body.
    pub content: String,
    pub images: Vec<Image>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: String,
    pub tool_name: String,
    /// What a reasoning model wrote before the answer. Providers that serve a
    /// reasoning model validate the next request against it: an assistant turn
    /// replayed without the field is refused with a missing-parameter 400.
    pub reasoning_content: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Default)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Map<String, Value>,
}

/// Callback that receives only reasoning text actually exposed by upstream.
pub type ReasoningFn = Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;

#[derive(Clone, Default)]
pub struct Request {
    pub model: String,
    pub messages: Vec<Message>,
    /// This engine's session. An adapter that keeps its own conversation — the
    /// OpenCode client does — names it so the two stay in step without a
    /// mapping table.
    pub session_id: String,
    pub max_tokens: i64,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub stream: bool,
    pub tools: Vec<ToolDef>,
    pub user: String,
    pub on_reasoning: Option<ReasoningFn>,
}

impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Request")
            .field("model", &self.model)
            .field("messages", &self.messages.len())
            .field("session_id", &self.session_id)
            .field("max_tokens", &self.max_tokens)
            .field("stream", &self.stream)
            .field("tools", &self.tools.len())
            .finish()
    }
}

#[derive(Debug, Clone, Default)]
pub struct Response {
    pub id: String,
    pub model: String,
    pub provider: String,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    pub finish_reason: String,
    /// The reasoning the model wrote before `content`. It belongs in the
    /// assistant message the turn leaves behind.
    pub reasoning_content: String,
    /// The tool steps taken while producing `content`, in order: an assistant
    /// message carrying tool calls, then one tool message per call.
    pub trail: Vec<Message>,
    /// Time spent inside the provider calls, summed over every round.
    pub generation_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    /// Whether `prompt_tokens` already contains the cache read. Providers
    /// disagree, and guessing wrong doubles or halves the prompt in the footer,
    /// so it is recorded rather than assumed.
    pub input_includes_cache: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Provider {
    pub name: String,
    pub endpoint: String,
    pub keys: Vec<String>,
    pub adapter: String,
    pub model: String,
}

/// An upstream HTTP failure. Constructing it through [`APIError`] lets
/// [`retryable`] find it again with a downcast.
#[derive(Debug, Clone)]
pub struct APIError {
    pub status_code: u16,
    pub message: String,
    /// How long the upstream asked us to wait, when it said. Read from its
    /// `retry-after` header or from a `retryAfterMs`-style field in the error
    /// body; `None` means the upstream gave no number at all.
    pub retry_after: Option<Duration>,
}

impl std::fmt::Display for APIError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.message.is_empty() {
            write!(f, "upstream HTTP {}", self.status_code)
        } else {
            write!(f, "upstream HTTP {}: {}", self.status_code, self.message)
        }
    }
}

impl std::error::Error for APIError {}

impl APIError {
    pub fn new(status_code: u16, message: impl Into<String>) -> Self {
        Self {
            status_code,
            message: message.into(),
            retry_after: None,
        }
    }

    /// Attaches the wait the upstream asked for. Kept separate from `new` so an
    /// adapter that has no number to report stays a two-argument call.
    pub fn with_retry_after(mut self, retry_after: Option<Duration>) -> Self {
        self.retry_after = retry_after;
        self
    }

    /// Wraps into an `anyhow::Error` whose downcast recovers the `APIError`.
    pub fn into_anyhow(self) -> anyhow::Error {
        anyhow::Error::new(self)
    }
}

/// Whether a 429 is a hard quota rather than a momentary throttle, judged from
/// its wording alone. Only used when the upstream stated no wait: a free tier
/// that has run out says so ("Free usage limit reached", "subscribe", "quota");
/// waiting seconds will not refill it, so the paced keyless retry only makes
/// the phone stare at "Sending request to provider…" for half a minute before
/// the same answer. A plain "rate limit" / "too many requests" is the opposite:
/// a per-IP window that a short retry rides out.
fn quota_is_terminal(message: &str) -> bool {
    let m = message.to_lowercase();
    m.contains("free usage limit")
        || m.contains("limit reached")
        || m.contains("subscribe")
        || m.contains("upgrade")
        || m.contains("quota")
        || m.contains("insufficient")
        || m.contains("out of funds")
        || m.contains("billing")
}

/// The longest wait the paced keyless retry can cover. `gateway::KEYLESS_BACKOFF`
/// sums to exactly this, so a stated wait within it can be ridden out by
/// sleeping it off, and anything longer is a free tier closing a window the
/// turn cannot wait inside. Pinned to the backoff table by a test in
/// `aixodia-io`, so the two cannot drift apart.
pub const RIDEABLE_WAIT: Duration = Duration::from_secs(31);

/// Whether the gateway should try again: a transport failure, or one of the
/// statuses providers use for transient conditions.
///
/// A 429 is decided by the upstream's own number when it gave one, and by the
/// wording only when it did not. The number is the only signal that separates a
/// per-IP window a short retry rides out from a daily allowance no retry inside
/// a turn will refill — and both free tiers this engine talks to phrase the two
/// the same way, which is what made the wording guess wrong in both directions.
pub fn retryable(err: &anyhow::Error) -> bool {
    match err.downcast_ref::<APIError>() {
        Some(api) => match api.status_code {
            429 => match api.retry_after {
                Some(wait) => wait <= RIDEABLE_WAIT,
                None => !quota_is_terminal(&api.message),
            },
            401 | 402 | 408 => true,
            code => code >= 500,
        },
        None => true,
    }
}

/// The keys an upstream puts a wait under, in seconds and in milliseconds.
const RETRY_KEYS_SECONDS: [&str; 4] = ["retry_after", "retryAfter", "retry-after", "Retry-After"];
const RETRY_KEYS_MILLIS: [&str; 3] = ["retryAfterMs", "retry_after_ms", "retry-after-ms"];

fn as_whole_number(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        // Gateways that quote the number rather than type it.
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn stated_wait(map: &Map<String, Value>) -> Option<Duration> {
    for key in RETRY_KEYS_MILLIS {
        if let Some(millis) = map.get(key).and_then(as_whole_number) {
            return Some(Duration::from_millis(millis));
        }
    }
    for key in RETRY_KEYS_SECONDS {
        if let Some(seconds) = map.get(key).and_then(as_whole_number) {
            return Some(Duration::from_secs(seconds));
        }
    }
    None
}

/// Reads the wait an upstream asked for out of an error body, at the top level
/// or nested in `error` / the first `errors` entry.
pub fn body_retry_after(body: &[u8]) -> Option<Duration> {
    let root: Value = serde_json::from_slice(body).ok()?;
    let root = root.as_object()?;
    if let Some(wait) = stated_wait(root) {
        return Some(wait);
    }
    for container in ["error", "errors"] {
        let nested = match root.get(container) {
            Some(Value::Object(map)) => Some(map),
            Some(Value::Array(items)) => items.first().and_then(Value::as_object),
            _ => None,
        };
        if let Some(map) = nested {
            if let Some(wait) = stated_wait(map) {
                return Some(wait);
            }
        }
    }
    None
}

/// Reads the `Retry-After` header. Only the delta-seconds form is understood; an
/// HTTP-date is reported as no wait, which leaves the wording to decide — no
/// date crate is pulled in for a form these gateways do not send.
pub fn header_retry_after(value: Option<&str>) -> Option<Duration> {
    let seconds = value?.trim().parse::<u64>().ok()?;
    Some(Duration::from_secs(seconds))
}

/// The wait a failed response asked for: its header when it carried one,
/// otherwise a number in its body. Shared by both adapters, because the header
/// has to be captured before the body is read and the body cannot answer for a
/// 429 an edge wrote as HTML.
pub fn error_wait(raw: &[u8], retry_after: Option<Duration>) -> Option<Duration> {
    retry_after.or_else(|| body_retry_after(raw))
}

/// Resolves the endpoint for one operation, appending `suffix` when the caller
/// gave a base URL.
///
/// Port of Go `provider.Endpoint`: a missing scheme becomes `https://`, a path
/// that already ends in `suffix` is left alone, an empty path becomes
/// `default_path`, and anything else gets `suffix` appended.
pub fn endpoint(endpoint: &str, default_path: &str, suffix: &str) -> Result<String> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return Err(anyhow!("provider endpoint is empty"));
    }
    let with_scheme = if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("https://{endpoint}")
    };

    let mut parsed =
        url::Url::parse(&with_scheme).map_err(|e| anyhow!("provider endpoint: {e}"))?;

    let path = parsed.path().trim_end_matches('/').to_string();
    if path.ends_with(suffix) {
        // Already the operation path.
    } else if path.is_empty() {
        parsed.set_path(default_path);
    } else {
        parsed.set_path(&format!("{path}{suffix}"));
    }
    Ok(parsed.to_string())
}

/// Turns an upstream error body into the message a client should see.
pub fn error_message(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }

    #[derive(serde::Deserialize)]
    struct Payload {
        #[serde(default)]
        error: Nested,
        #[serde(default)]
        errors: Vec<Nested>,
    }
    #[derive(serde::Deserialize, Default)]
    struct Nested {
        #[serde(default)]
        message: String,
    }

    if let Ok(payload) = serde_json::from_str::<Payload>(text) {
        if !payload.error.message.is_empty() {
            return payload.error.message;
        }
        if let Some(first) = payload.errors.first() {
            if !first.message.is_empty() {
                return first.message.clone();
            }
        }
    }

    #[derive(serde::Deserialize)]
    struct Flat {
        #[serde(default)]
        message: String,
    }
    if let Ok(flat) = serde_json::from_str::<Flat>(text) {
        if !flat.message.is_empty() {
            return flat.message;
        }
    }

    let text = if text.chars().count() > 512 {
        text.chars().take(512).collect::<String>()
    } else {
        text.to_string()
    };
    text
}

/// Reads a Server-Sent Events body, calling `on_data` once per event with the
/// joined `data:` payload.
///
/// Port of Go `provider.ReadSSE`: `data:` lines accumulate (joined with `\n`),
/// a blank line dispatches, and whatever is left is dispatched at end of input.
pub async fn read_sse<S, E>(stream: S, mut on_data: impl FnMut(&str) -> Result<()>) -> Result<()>
where
    S: Stream<Item = std::result::Result<Bytes, E>>,
    E: std::error::Error + Send + Sync + 'static,
{
    let stream = Box::pin(stream.map(|item| item.map_err(|e| std::io::Error::other(e))));
    let reader = StreamReader::new(stream);
    let mut lines = BufReader::new(reader).lines();

    let mut data = String::new();
    while let Some(line) = lines.next_line().await? {
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else if line.is_empty() && !data.is_empty() {
            on_data(&data)?;
            data.clear();
        }
    }
    if !data.is_empty() {
        on_data(&data)?;
    }
    Ok(())
}

/// Helper for adapters that need the same stream type name.
pub type ByteStream =
    Pin<Box<dyn Stream<Item = std::result::Result<Bytes, reqwest::Error>> + Send>>;

/// Receives streamed answer text.
///
/// A concrete trait rather than a boxed `FnMut`, because `dyn FnMut(&str)`
/// passed through several async layers needs a higher-ranked bound that the
/// compilers cannot infer from a `&mut dyn` parameter.
pub trait Emitter: Send {
    fn emit(&mut self, delta: &str) -> Result<()>;
}

impl<F> Emitter for F
where
    F: FnMut(&str) -> Result<()> + Send,
{
    fn emit(&mut self, delta: &str) -> Result<()> {
        self(delta)
    }
}

/// The contract every adapter implements.
#[async_trait::async_trait]
pub trait Adapter: Send + Sync {
    fn name(&self) -> &str;

    async fn complete(&self, req: &Request, prov: &Provider, key: &str) -> Result<Response>;

    async fn stream(
        &self,
        req: &Request,
        prov: &Provider,
        key: &str,
        emit: &mut dyn Emitter,
    ) -> Result<Response>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn api(status: u16, message: &str) -> anyhow::Error {
        APIError::new(status, message).into_anyhow()
    }

    #[test]
    fn a_momentary_throttle_is_worth_retrying() {
        assert!(retryable(&api(429, "Too many requests, slow down")));
        assert!(retryable(&api(
            429,
            "rate limit exceeded, retry in 30 seconds"
        )));
        assert!(retryable(&api(503, "temporarily unavailable")));
    }

    #[test]
    fn a_spent_free_quota_fails_fast_instead_of_stalling() {
        // The Opencode free-tier answer: waiting seconds will not refill it, so
        // the paced retry only keeps the phone staring at a spinner.
        let spent = api(
            429,
            "Free usage limit reached. Free models work in OpenCode; subscribe to OpenCode Go to keep going.",
        );
        assert!(!retryable(&spent));
        assert!(!retryable(&api(429, "Monthly quota exceeded.")));
        assert!(!retryable(&api(
            429,
            "Insufficient credits, upgrade your plan."
        )));
    }

    #[test]
    fn the_stated_wait_decides_a_429_and_not_the_wording() {
        // Both free tiers phrase the momentary window and the spent allowance
        // alike, so the number is what tells them apart. A wait the paced retry
        // can cover is worth riding out even when the text says "free usage
        // limit"; a wait longer than the budget is terminal even when the text
        // reads like a transient throttle.
        let momentary = APIError::new(
            429,
            "Free usage limit reached. Free models work in OpenCode.",
        )
        .with_retry_after(Some(Duration::from_secs(4)))
        .into_anyhow();
        assert!(retryable(&momentary));

        let window_closed = APIError::new(429, "Too many requests, slow down")
            .with_retry_after(Some(Duration::from_secs(500)))
            .into_anyhow();
        assert!(!retryable(&window_closed));

        // The budget exactly: a wait that is coverable is coverable.
        let at_budget = APIError::new(429, "throttled")
            .with_retry_after(Some(RIDEABLE_WAIT))
            .into_anyhow();
        assert!(retryable(&at_budget));
    }

    #[test]
    fn a_wait_is_read_from_the_body_in_either_shape() {
        // Opencode nests it under `error` and names it in milliseconds.
        let nested =
            br#"{"error":{"type":"FreeUsageLimitError","message":"no","retryAfterMs":3000}}"#;
        assert_eq!(body_retry_after(nested), Some(Duration::from_secs(3)));

        // A quoted number is as good as a typed one.
        let quoted = br#"{"error":{"retry_after":"12"}}"#;
        assert_eq!(body_retry_after(quoted), Some(Duration::from_secs(12)));

        // Pollinations-style top level.
        let top = br#"{"retryAfter":45}"#;
        assert_eq!(body_retry_after(top), Some(Duration::from_secs(45)));

        // An errors array, and a body with no number at all.
        let array = br#"{"errors":[{"message":"rate limited","retryAfterMs":2500}]}"#;
        assert_eq!(body_retry_after(array), Some(Duration::from_millis(2500)));
        assert_eq!(
            body_retry_after(br#"{"error":{"message":"no number here"}}"#),
            None
        );
        assert_eq!(body_retry_after(b"not json"), None);
    }

    #[test]
    fn a_wait_is_read_from_the_header_when_there_is_one() {
        assert_eq!(header_retry_after(Some("7")), Some(Duration::from_secs(7)));
        // A date is not parsed, so it reports no wait and the wording decides.
        assert_eq!(
            header_retry_after(Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            None
        );
        assert_eq!(header_retry_after(None), None);
        assert_eq!(header_retry_after(Some("")), None);
    }
}
