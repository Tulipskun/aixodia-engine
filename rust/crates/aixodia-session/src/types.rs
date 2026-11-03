//! The engine's chat vocabulary: sessions, turns, and the display blocks each
//! turn carries.
//!
//! Port of the Go `session` package types.

use serde::{Deserialize, Serialize};

/// One chat.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub reasoning: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub max_tokens: i64,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    /// The chat's cost, summed by the store over every turn. It rides with the
    /// session so a phone that only holds the newest page of rows can still
    /// draw the whole chat's figures.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<SessionUsage>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

/// One chat's cost. The two prompt conventions stay apart: a provider that puts
/// the cache inside its input total must not have that cache added on top of it,
/// so `prompt_tokens` is the whole prompt either way while `fresh_input` is only
/// the part that was not served from cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsage {
    pub prompt_tokens: i64,
    pub cache_read: i64,
    pub fresh_input: i64,
    pub output_tokens: i64,
    pub total_ms: i64,
    /// The newest answer that reported a duration, not the longest: the strip
    /// calls it "last turn", not an average.
    pub last_turn_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Turn {
    pub session_id: String,
    /// The D1 row id: a number that only moves forward, so it is unique for
    /// every message and easy for the phone to use as a key.
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub seq: i64,
    #[serde(default)]
    pub role: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub job_id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub input_tokens: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub output_tokens: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_read_tokens: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_write_tokens: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning_tokens: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reasoning_content: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub input_includes_cache: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub duration_ms: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub generation_ms: i64,
    /// How this row draws. Empty on rows that predate blocks, which the phone
    /// reads through the role instead.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub block: String,
    #[serde(default)]
    pub created_at: i64,
}

/// A chat to create.
#[derive(Debug, Clone, Default)]
pub struct NewSession {
    /// Optional. The phone makes a draft chat locally and keeps that id, so the
    /// draft does not have to be migrated to a server-generated one.
    pub id: String,
    pub title: String,
    pub provider: String,
    pub model: String,
    pub reasoning: bool,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub max_tokens: i64,
}

/// One turn to append.
#[derive(Debug, Clone, Default)]
pub struct TurnInput {
    pub role: String,
    pub text: String,
    pub model: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    pub input_includes_cache: bool,
    /// Travels with an assistant turn so the next request can replay it.
    pub reasoning_content: String,
    pub duration_ms: i64,
    pub generation_ms: i64,
    /// The row's display block as JSON. Replay ignores it.
    pub block: String,
}

impl TurnInput {
    /// A plain text turn, the common case.
    pub fn text(role: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            text: text.into(),
            ..Self::default()
        }
    }
}

/// The exact row inserted, without another history query.
pub fn stored_turn(id: &str, seq: i64, created_at: i64, input: &TurnInput) -> Turn {
    Turn {
        session_id: id.to_string(),
        seq,
        created_at,
        role: input.role.clone(),
        text: input.text.clone(),
        model: input.model.clone(),
        input_tokens: input.input_tokens,
        output_tokens: input.output_tokens,
        cache_read_tokens: input.cache_read_tokens,
        cache_write_tokens: input.cache_write_tokens,
        reasoning_tokens: input.reasoning_tokens,
        reasoning_content: input.reasoning_content.clone(),
        input_includes_cache: input.input_includes_cache,
        duration_ms: input.duration_ms,
        generation_ms: input.generation_ms,
        block: input.block.clone(),
        ..Turn::default()
    }
}

/// A question that was stored and never answered — what a turn interrupted by
/// the session ending looks like.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UnfinishedTurn {
    pub session_id: String,
    pub seq: i64,
    pub text: String,
    pub created_at: i64,
}

/// The part of a chat's agent choice a PATCH changes. `None` keeps what is
/// stored: a field the phone left out is not a request to clear it.
#[derive(Debug, Clone, Default)]
pub struct AgentPatch {
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl AgentPatch {
    pub fn empty(&self) -> bool {
        self.provider.is_none() && self.model.is_none()
    }
}

/// Random 16-hex-character chat id.
pub fn new_id() -> String {
    let mut buffer = [0u8; 8];
    if rand::RngCore::try_fill_bytes(&mut rand::thread_rng(), &mut buffer).is_err() {
        return format!("{}", now_unix_nanos());
    }
    hex::encode(buffer)
}

fn now_unix_nanos() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

pub fn bool_int(value: bool) -> i64 {
    if value {
        1
    } else {
        0
    }
}
