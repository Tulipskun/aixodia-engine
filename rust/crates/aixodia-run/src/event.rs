//! A run's lifecycle and the numbered events it emits.
//!
//! Port of the Go `run/event.go`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The lifecycle of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum Status {
    #[default]
    Running,
    Done,
    Failed,
    Cancelled,
    Interrupted,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
            Status::Interrupted => "interrupted",
        }
    }

    /// Whether no further events will arrive.
    pub fn terminal(self) -> bool {
        matches!(
            self,
            Status::Done | Status::Failed | Status::Cancelled | Status::Interrupted
        )
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Event kinds. The phone only needs the kind to choose a renderer; `data` is
/// the payload the gateway already speaks (a turn row, a tool_event, a delta).
pub const KIND_STATUS: &str = "status";
pub const KIND_USER: &str = "user";
pub const KIND_ASSISTANT: &str = "assistant";
pub const KIND_REASONING: &str = "reasoning";
pub const KIND_TOOL: &str = "tool";
pub const KIND_TURN: &str = "turn";
pub const KIND_FINAL: &str = "final";
pub const KIND_ERROR: &str = "error";

/// One numbered entry in a run's log. `seq` is per run and starts at 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Not part of the wire shape; the store needs it to key a batch.
    #[serde(skip)]
    pub run_id: String,
    pub seq: i64,
    pub kind: String,
    pub at: i64,
    pub data: Value,
}

/// The metadata a client polls or resumes from.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_run_id: String,
    pub status: Status,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub last_seq: i64,
}

/// A file the phone uploaded before starting the run.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Attachment {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mime: String,
}

/// One user turn. `session_id` must already exist.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StartRequest {
    #[serde(default)]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_run_id: String,
    #[serde(default)]
    pub message: String,
    #[serde(default, rename = "retry")]
    pub retry: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// The request handed to the chat executor.
#[derive(Debug, Clone, Default)]
pub struct ChatJob {
    pub run_id: String,
    pub session_id: String,
    pub message: String,
    pub retry: bool,
    pub attachments: Vec<Attachment>,
}

pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
