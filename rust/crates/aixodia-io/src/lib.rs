//! The HTTP/WS gateway: the routes the phone talks to.
//!
//! Port of the Go `io/gateway` package. The engine serves one HTTP surface on
//! `127.0.0.1:8787`: the OpenAI-compatible chat API, the session and provider
//! API the settings screen calls, attachment uploads and the engine log tail.
//!
//! [`Gateway::router`] is the whole surface. The WebSocket endpoints
//! (`/room`, `/terminal`) are mounted by the binary, because they are the only
//! routes that need the connection itself rather than a request and a response.

pub mod api;
pub mod attach;
pub mod auto_backup;
pub mod backup;
pub mod breaker;
pub mod chat;
pub mod gateway;
pub mod models;
pub mod remind;
pub mod restart;
pub mod session_store;
pub mod toolimage;
pub mod upload;

/// Port of the Go `io/gateway` tests. They are a module rather than an
/// integration test because the Go tests reach the package's private helpers,
/// and a port that could only see the public API would not test the same thing.
#[cfg(test)]
mod gateway_tests;

pub use chat::{ChatRequest, ChatSink, CompletionOptions, NullSink, TrailWriter};
pub use gateway::{
    new_completion_id, require_token, turn_page, Gateway, LogReader, SessionStore, StateStore,
    SESSION_ID_PATTERN, VERSION,
};
pub use upload::max_upload;
