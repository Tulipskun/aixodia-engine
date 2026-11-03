//! Anthropic Messages adapter: tool use and SSE streaming.
//!
//! Port of the engine's Go `provider/anthropic` package. The API is
//! `POST {endpoint}/v1/messages`, and it differs from the OpenAI shape in two
//! ways that matter here: system messages travel in a top-level field, and tool
//! results are user messages containing `tool_result` blocks.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use anyhow::{anyhow, Result};
use base64::Engine as _;
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::types::{
    endpoint, error_message, error_wait, read_sse, APIError, Emitter, Provider, Request, Response,
    ToolCall, Usage, USER_AGENT,
};

const API_VERSION: &str = "2023-06-01";
const DEFAULT_PATH: &str = "/v1/messages";
const DEFAULT_MAX_TOKENS: i64 = 4096;

fn shared_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .build()
                .expect("provider HTTP client")
        })
        .clone()
}

/// The Anthropic Messages adapter.
#[derive(Clone, Default)]
pub struct Adapter {
    /// Overrides the shared HTTP client.
    pub client: Option<reqwest::Client>,
}

impl Adapter {
    pub fn new() -> Self {
        Self::default()
    }

    fn client(&self) -> reqwest::Client {
        self.client.clone().unwrap_or_else(shared_client)
    }

    async fn do_request(
        &self,
        prov: &Provider,
        key: &str,
        body: &[u8],
    ) -> Result<reqwest::Response> {
        let endpoint = endpoint(&prov.endpoint, DEFAULT_PATH, "/messages")?;
        let response = self
            .client()
            .post(endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header("x-api-key", key)
            .header("anthropic-version", API_VERSION)
            .body(body.to_vec())
            .send()
            .await?;
        Ok(response)
    }
}

#[async_trait::async_trait]
impl crate::types::Adapter for Adapter {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn complete(&self, req: &Request, prov: &Provider, key: &str) -> Result<Response> {
        let body = serde_json::to_vec(&build_request(req, false))?;
        let response = self.do_request(prov, key, &body).await?;
        let status = response.status().as_u16();
        let retry_after = retry_after_from(&response);
        let raw = read_up_to(response, 8 << 20).await?;
        if !(200..300).contains(&status) {
            return Err(APIError::new(status, error_message(&raw))
                .with_retry_after(error_wait(&raw, retry_after))
                .into_anyhow());
        }

        let parsed: WireResponse =
            serde_json::from_slice(&raw).map_err(|e| anyhow!("anthropic: decode response: {e}"))?;
        let mut out = Response {
            id: parsed.id.clone(),
            model: parsed.model.clone(),
            finish_reason: finish_reason(&parsed.stop_reason),
            // InputTokens is only the uncached part; the cache read/write parts
            // stay beside it, so the engine's total is the whole prompt.
            usage: Usage {
                prompt_tokens: parsed.usage.input_tokens,
                completion_tokens: parsed.usage.output_tokens,
                cache_read_tokens: parsed.usage.cache_read_input_tokens,
                cache_write_tokens: parsed.usage.cache_creation_input_tokens,
                total_tokens: parsed.usage.input_tokens
                    + parsed.usage.cache_read_input_tokens
                    + parsed.usage.cache_creation_input_tokens
                    + parsed.usage.output_tokens,
                ..Usage::default()
            },
            ..Response::default()
        };
        let mut text = String::new();
        for block in &parsed.content {
            match block.kind.as_str() {
                "text" => text.push_str(&block.text),
                "tool_use" => out.tool_calls.push(ToolCall {
                    id: block.id.clone(),
                    name: block.name.clone(),
                    arguments: arguments_string(block.input.as_ref()),
                }),
                _ => {}
            }
        }
        out.content = text;
        Ok(out)
    }

    async fn stream(
        &self,
        req: &Request,
        prov: &Provider,
        key: &str,
        emit: &mut dyn Emitter,
    ) -> Result<Response> {
        let body = serde_json::to_vec(&build_request(req, true))?;
        let response = self.do_request(prov, key, &body).await?;
        let status = response.status().as_u16();
        let retry_after = retry_after_from(&response);
        if !(200..300).contains(&status) {
            let raw = read_up_to(response, 8 << 20).await?;
            return Err(APIError::new(status, error_message(&raw))
                .with_retry_after(error_wait(&raw, retry_after))
                .into_anyhow());
        }

        let mut out = Response::default();
        let mut blocks: BTreeMap<i64, Block> = BTreeMap::new();
        let mut order: Vec<i64> = Vec::new();
        let mut stream_error: Option<anyhow::Error> = None;

        read_sse(response.bytes_stream(), |data| {
            let Ok(event) = serde_json::from_str::<StreamEvent>(data) else {
                return Ok(());
            };
            match event.kind.as_str() {
                "message_start" => {
                    out.id = event.message.id.clone();
                    out.model = event.message.model.clone();
                    out.usage.prompt_tokens = event.message.usage.input_tokens;
                    out.usage.cache_read_tokens = event.message.usage.cache_read_input_tokens;
                    out.usage.cache_write_tokens = event.message.usage.cache_creation_input_tokens;
                }
                "content_block_start" => {
                    blocks.insert(
                        event.index,
                        Block {
                            kind: event.content_block.kind.clone(),
                            id: event.content_block.id.clone(),
                            name: event.content_block.name.clone(),
                            args: String::new(),
                            text: String::new(),
                        },
                    );
                    order.push(event.index);
                }
                "content_block_delta" => {
                    let Some(block) = blocks.get_mut(&event.index) else {
                        return Ok(());
                    };
                    match event.delta.kind.as_str() {
                        "text_delta" => {
                            block.text.push_str(&event.delta.text);
                            out.content.push_str(&event.delta.text);
                            emit.emit(&event.delta.text)?;
                        }
                        "input_json_delta" => {
                            block.args.push_str(&event.delta.partial_json);
                        }
                        _ => {}
                    }
                }
                "message_delta" => {
                    out.finish_reason = finish_reason(&event.delta.stop_reason);
                    out.usage.completion_tokens = event.usage.output_tokens;
                }
                "error" => {
                    let error = anyhow!("anthropic stream: {}", event.error.message);
                    stream_error = Some(anyhow!("anthropic stream: {}", event.error.message));
                    return Err(error);
                }
                _ => {}
            }
            Ok(())
        })
        .await?;

        if let Some(error) = stream_error {
            return Err(error);
        }

        for index in order {
            let Some(block) = blocks.get(&index) else {
                continue;
            };
            if block.kind == "tool_use" {
                out.tool_calls.push(ToolCall {
                    id: block.id.clone(),
                    name: block.name.clone(),
                    arguments: arguments_string_text(&block.args),
                });
            }
        }
        out.usage.total_tokens = out.usage.prompt_tokens
            + out.usage.cache_read_tokens
            + out.usage.cache_write_tokens
            + out.usage.completion_tokens;
        Ok(out)
    }
}

struct Block {
    kind: String,
    id: String,
    name: String,
    args: String,
    #[allow(dead_code)]
    text: String,
}

/// Reads the `Retry-After` header before the response body consumes it.
fn retry_after_from(response: &reqwest::Response) -> Option<std::time::Duration> {
    crate::types::header_retry_after(
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
    )
}

async fn read_up_to(response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    let bytes = response.bytes().await?;
    Ok(bytes[..bytes.len().min(limit)].to_vec())
}

fn finish_reason(stop: &str) -> String {
    match stop {
        "end_turn" | "stop_sequence" => "stop".to_string(),
        "tool_use" => "tool_calls".to_string(),
        "max_tokens" => "length".to_string(),
        other => other.to_string(),
    }
}

fn arguments_string(input: Option<&Value>) -> String {
    match input {
        None | Some(Value::Null) => "{}".to_string(),
        Some(value) => serde_json::to_string(value).unwrap_or_else(|_| "{}".to_string()),
    }
}

fn arguments_string_text(input: &str) -> String {
    if input.is_empty() || input == "null" {
        return "{}".to_string();
    }
    input.to_string()
}

// --- wire shapes ---------------------------------------------------------

#[derive(Serialize)]
struct WireRequest {
    model: String,
    max_tokens: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    system: String,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "is_false")]
    stream: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Serialize, Clone, Default)]
struct WireBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    text: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Value>,
    #[serde(skip_serializing_if = "String::is_empty")]
    tool_use_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<WireImage>,
}

/// Anthropic's inline picture: base64 with its media type spelled out, because
/// the block carries no other hint about how to decode it.
#[derive(Serialize, Clone)]
struct WireImage {
    #[serde(rename = "type")]
    kind: String,
    media_type: String,
    data: String,
}

#[derive(Serialize, Clone)]
struct WireMessage {
    role: String,
    content: Vec<WireBlock>,
}

#[derive(Serialize)]
struct WireTool {
    name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    description: String,
    input_schema: Map<String, Value>,
}

/// Converts the provider-neutral conversation into Anthropic's shape. System
/// messages move to the top-level field, assistant tool calls become tool_use
/// blocks, and tool results become tool_result blocks inside a user message.
/// Consecutive tool results share one user message, which is what the API
/// expects after a multi-call assistant turn.
fn build_request(req: &Request, stream: bool) -> WireRequest {
    let mut out = WireRequest {
        model: req.model.clone(),
        max_tokens: if req.max_tokens <= 0 {
            DEFAULT_MAX_TOKENS
        } else {
            req.max_tokens
        },
        system: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
        temperature: req.temperature,
        top_p: req.top_p,
        stream,
    };

    let mut system: Vec<String> = Vec::new();
    for message in &req.messages {
        match message.role.as_str() {
            "system" => {
                if !message.content.trim().is_empty() {
                    system.push(message.content.clone());
                }
            }
            "assistant" => {
                let mut blocks: Vec<WireBlock> = Vec::new();
                if !message.content.is_empty() {
                    blocks.push(WireBlock {
                        kind: "text".to_string(),
                        text: message.content.clone(),
                        ..WireBlock::default()
                    });
                }
                for call in &message.tool_calls {
                    let input = if call.arguments.is_empty() {
                        Value::Object(Map::new())
                    } else {
                        serde_json::from_str(&call.arguments)
                            .unwrap_or_else(|_| Value::Object(Map::new()))
                    };
                    blocks.push(WireBlock {
                        kind: "tool_use".to_string(),
                        id: call.id.clone(),
                        name: call.name.clone(),
                        input: Some(input),
                        ..WireBlock::default()
                    });
                }
                if blocks.is_empty() {
                    blocks.push(WireBlock {
                        kind: "text".to_string(),
                        text: " ".to_string(),
                        ..WireBlock::default()
                    });
                }
                out.messages.push(WireMessage {
                    role: "assistant".to_string(),
                    content: blocks,
                });
            }
            "tool" => {
                let block = WireBlock {
                    kind: "tool_result".to_string(),
                    tool_use_id: message.tool_call_id.clone(),
                    content: message.content.clone(),
                    ..WireBlock::default()
                };
                if let Some(last) = out.messages.last_mut() {
                    if last.role == "user" && is_tool_result_message(last) {
                        last.content.push(block);
                        continue;
                    }
                }
                out.messages.push(WireMessage {
                    role: "user".to_string(),
                    content: vec![block],
                });
            }
            _ => {
                let mut blocks: Vec<WireBlock> = Vec::with_capacity(message.images.len() + 1);
                if !message.content.trim().is_empty() {
                    blocks.push(WireBlock {
                        kind: "text".to_string(),
                        text: message.content.clone(),
                        ..WireBlock::default()
                    });
                }
                for image in &message.images {
                    let media_type = if image.media_type.is_empty() {
                        "image/jpeg".to_string()
                    } else {
                        image.media_type.clone()
                    };
                    blocks.push(WireBlock {
                        kind: "image".to_string(),
                        source: Some(WireImage {
                            kind: "base64".to_string(),
                            media_type,
                            data: base64::engine::general_purpose::STANDARD.encode(&image.data),
                        }),
                        ..WireBlock::default()
                    });
                }
                if blocks.is_empty() {
                    blocks.push(WireBlock {
                        kind: "text".to_string(),
                        text: " ".to_string(),
                        ..WireBlock::default()
                    });
                }
                out.messages.push(WireMessage {
                    role: "user".to_string(),
                    content: blocks,
                });
            }
        }
    }
    out.system = system.join("\n\n");

    for tool in &req.tools {
        let schema = if tool.parameters.is_empty() {
            let mut map = Map::new();
            map.insert("type".to_string(), Value::from("object"));
            map.insert("properties".to_string(), json!({}));
            map
        } else {
            tool.parameters.clone()
        };
        out.tools.push(WireTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: schema,
        });
    }
    out
}

fn is_tool_result_message(message: &WireMessage) -> bool {
    message
        .content
        .first()
        .map(|block| block.kind == "tool_result")
        .unwrap_or(false)
}

#[derive(serde::Deserialize, Default)]
struct WireResponse {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    content: Vec<ResponseBlock>,
    #[serde(default)]
    stop_reason: String,
    #[serde(default)]
    usage: ResponseUsage,
}

#[derive(serde::Deserialize, Default)]
struct ResponseBlock {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    input: Option<Value>,
}

#[derive(serde::Deserialize, Default)]
struct ResponseUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    cache_read_input_tokens: i64,
    #[serde(default)]
    cache_creation_input_tokens: i64,
}

#[derive(serde::Deserialize, Default)]
struct StreamEvent {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    index: i64,
    #[serde(default)]
    message: StreamMessage,
    #[serde(default)]
    content_block: StreamContentBlock,
    #[serde(default)]
    delta: StreamDelta,
    #[serde(default)]
    usage: StreamUsage,
    #[serde(default)]
    error: StreamError,
}

#[derive(serde::Deserialize, Default)]
struct StreamMessage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    usage: StreamMessageUsage,
}

#[derive(serde::Deserialize, Default)]
struct StreamMessageUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    cache_read_input_tokens: i64,
    #[serde(default)]
    cache_creation_input_tokens: i64,
}

#[derive(serde::Deserialize, Default)]
struct StreamContentBlock {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
}

#[derive(serde::Deserialize, Default)]
struct StreamDelta {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    partial_json: String,
    #[serde(default)]
    stop_reason: String,
}

#[derive(serde::Deserialize, Default)]
struct StreamUsage {
    #[serde(default)]
    output_tokens: i64,
}

#[derive(serde::Deserialize, Default)]
struct StreamError {
    #[serde(default)]
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;

    fn message(role: &str, content: &str) -> Message {
        Message {
            role: role.to_string(),
            content: content.to_string(),
            ..Message::default()
        }
    }

    #[test]
    fn system_messages_move_to_the_top_level_field() {
        let request = Request {
            model: "claude".to_string(),
            messages: vec![message("system", "be brief"), message("user", "hi")],
            ..Request::default()
        };
        let built = build_request(&request, false);
        assert_eq!(built.system, "be brief");
        assert_eq!(built.messages.len(), 1);
        assert_eq!(built.messages[0].role, "user");
        assert_eq!(built.max_tokens, DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn consecutive_tool_results_share_one_user_message() {
        let mut first = message("tool", "one");
        first.tool_call_id = "a".to_string();
        let mut second = message("tool", "two");
        second.tool_call_id = "b".to_string();
        let request = Request {
            messages: vec![message("user", "go"), first, second],
            ..Request::default()
        };
        let built = build_request(&request, false);
        assert_eq!(built.messages.len(), 2);
        assert_eq!(built.messages[1].role, "user");
        assert_eq!(built.messages[1].content.len(), 2);
        assert_eq!(built.messages[1].content[1].tool_use_id, "b");
    }

    #[test]
    fn an_empty_assistant_turn_gets_a_space() {
        let request = Request {
            messages: vec![Message {
                role: "assistant".to_string(),
                ..Message::default()
            }],
            ..Request::default()
        };
        let built = build_request(&request, false);
        assert_eq!(built.messages[0].content[0].text, " ");
    }

    #[test]
    fn stop_reasons_are_translated() {
        assert_eq!(finish_reason("end_turn"), "stop");
        assert_eq!(finish_reason("tool_use"), "tool_calls");
        assert_eq!(finish_reason("max_tokens"), "length");
        assert_eq!(finish_reason("weird"), "weird");
    }
}
