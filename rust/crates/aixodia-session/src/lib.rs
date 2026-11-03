//! The D1-backed chat store: sessions, turns, display blocks and the context
//! window.
//!
//! Port of the Go `session` package.

pub mod block;
pub mod d1;
pub mod trail;
pub mod types;
pub mod window;

pub use block::Block;
pub use d1::{Appended, Error, Result, D1, SESSION_SELECT, TURN_COLUMNS};
pub use trail::{messages_from_turns, trail_turns, ToolCallRow, ToolResultRow};
pub use types::{
    new_id, stored_turn, AgentPatch, NewSession, Session, SessionUsage, Turn, TurnInput,
    UnfinishedTurn,
};
pub use window::{
    anchor, compact, estimate_tokens, message_tokens, tokens, Compacted, Window, DEFAULT_MAX_TOKENS,
};

/// Every builtin block builder and constant, re-exported for the gateway.
pub use block::{
    reasoning_block, request_block, request_id, status_block, text_block, tool_block, BLOCK_TEXT,
    STATE_DONE, STATE_ERROR, STATE_RUNNING, STATE_THINKING, STATE_WAITING,
};
