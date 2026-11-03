//! Display blocks: the engine's vocabulary for everything a turn shows.
//!
//! Changing how a turn looks means changing the blocks the engine emits, not
//! shipping a new app: a type the phone has never seen still draws as a plain
//! card. Port of the Go `session/block.go`.

use serde::{Deserialize, Serialize};

/// Narration or an answer: the model's own words.
pub const BLOCK_TEXT: &str = "text";
/// What the model thought before it acted.
pub const BLOCK_REASONING: &str = "reasoning";
/// One tool call and, when it lands, its result.
pub const BLOCK_TOOL: &str = "tool";
/// One provider request round of a turn.
pub const BLOCK_REQUEST: &str = "request";
/// A transient line that names what the turn is doing right now. Never stored.
pub const BLOCK_STATUS: &str = "status";

pub const STATE_WAITING: &str = "waiting";
pub const STATE_THINKING: &str = "thinking";
pub const STATE_RUNNING: &str = "running";
pub const STATE_DONE: &str = "done";
pub const STATE_ERROR: &str = "error";

/// One visible unit of a turn. Flat on purpose: every field fits every type, and
/// a renderer reads only the fields its type needs.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Block {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub state: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub arguments: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub result: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub is_error: bool,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub duration_ms: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub thinking_ms: i64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub round: i64,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

impl Block {
    /// Renders the block the way it rides on a turn and on a live frame.
    pub fn json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }

    /// Decodes a stored block, reporting `None` when it is missing or not a
    /// block at all.
    pub fn parse(raw: &str) -> Option<Block> {
        if raw.is_empty() {
            return None;
        }
        let block: Block = serde_json::from_str(raw).ok()?;
        (!block.kind.is_empty()).then_some(block)
    }
}

/// Names the block of one provider request round: the request's own lifecycle
/// and the reasoning that belongs to it share it.
pub fn request_id(round: i64) -> String {
    format!("req{round}")
}

/// Opens (or settles) one provider request round.
pub fn request_block(round: i64, state: &str) -> Block {
    Block {
        id: request_id(round),
        kind: BLOCK_REQUEST.to_string(),
        state: state.to_string(),
        round,
        ..Block::default()
    }
}

/// One chunk of what the round is thinking. Chunks share the request's id.
pub fn reasoning_block(round: i64, text: &str) -> Block {
    Block {
        id: request_id(round),
        kind: BLOCK_REASONING.to_string(),
        text: text.to_string(),
        round,
        ..Block::default()
    }
}

/// The model's own words: narration before a tool call, or the closing answer.
pub fn text_block(id: &str, text: &str) -> Block {
    Block {
        id: id.to_string(),
        kind: BLOCK_TEXT.to_string(),
        state: STATE_DONE.to_string(),
        text: text.to_string(),
        ..Block::default()
    }
}

/// One tool step. `has_result` says whether this block carries the tool's
/// result at all: a call that has not returned yet is running, a result settles
/// it. The result text cannot stand in for that — a command that succeeds
/// quietly (`rm`, `mkdir`, `cd`) returns an empty string, and inferring
/// "still running" from that left the phone spinning a finished card for the
/// rest of the turn.
pub fn tool_block(
    call_id: &str,
    name: &str,
    args: &str,
    result: &str,
    is_error: bool,
    duration_ms: i64,
    has_result: bool,
) -> Block {
    let state = if !has_result {
        STATE_RUNNING
    } else if is_error {
        STATE_ERROR
    } else {
        STATE_DONE
    };
    Block {
        id: call_id.to_string(),
        kind: BLOCK_TOOL.to_string(),
        state: state.to_string(),
        name: name.to_string(),
        arguments: args.to_string(),
        result: result.to_string(),
        is_error,
        duration_ms,
        ..Block::default()
    }
}

/// A transient line. It is never stored.
pub fn status_block(text: &str) -> Block {
    Block {
        id: "status".to_string(),
        kind: BLOCK_STATUS.to_string(),
        text: text.to_string(),
        ..Block::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_block_settles_on_a_result() {
        let running = tool_block("call_1", "shell", "{}", "", false, 0, false);
        assert_eq!(running.state, STATE_RUNNING);

        let done = tool_block("call_1", "shell", "{}", "ok", false, 12, true);
        assert_eq!(done.state, STATE_DONE);
        assert_eq!(done.duration_ms, 12);

        let failed = tool_block("call_1", "shell", "{}", "{\"error\":\"x\"}", true, 0, true);
        assert_eq!(failed.state, STATE_ERROR);
        assert!(failed.is_error);
    }

    #[test]
    fn a_quiet_success_is_done_not_running() {
        // `rm`/`mkdir`/`cd` return no output. That is a finished step, not one
        // still in flight — the empty text must not be read as "no result yet".
        let quiet = tool_block("call_1", "shell", "{}", "", false, 4, true);
        assert_eq!(quiet.state, STATE_DONE);
        assert_eq!(quiet.duration_ms, 4);
    }

    #[test]
    fn parse_rejects_a_non_block() {
        assert!(Block::parse("").is_none());
        assert!(Block::parse("not json").is_none());
        assert!(Block::parse("{\"id\":\"x\"}").is_none());
        let parsed = Block::parse(&text_block("n1", "hi").json()).expect("block");
        assert_eq!(parsed.kind, BLOCK_TEXT);
        assert_eq!(parsed.text, "hi");
        assert_eq!(parsed.state, STATE_DONE);
    }

    #[test]
    fn request_and_reasoning_share_an_id() {
        assert_eq!(request_id(3), "req3");
        assert_eq!(request_block(3, STATE_WAITING).id, "req3");
        assert_eq!(reasoning_block(3, "hmm").id, "req3");
    }
}
