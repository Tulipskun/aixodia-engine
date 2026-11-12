//! Provider adapters for the AIxodia engine.
//!
//! Port of the Go `provider` package: the shared types and `Adapter` trait in
//! [`types`], the OpenAI-compatible adapter in [`openai`], and the OpenCode Zen
//! adapter in [`opencode`].

pub mod anthropic;
pub mod openai;
pub mod opencode;
pub mod registry;
pub mod types;

pub use registry::Registry;

pub use types::{
    endpoint, error_message, read_sse, retryable, APIError, Adapter, Image, Message, Provider,
    ReasoningFn, Request, Response, ToolCall, ToolDef, Usage, RIDEABLE_WAIT, USER_AGENT,
};
