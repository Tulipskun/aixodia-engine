//! OpenAI-compatible adapter: streaming, tools, usage, and the Responses API.
//!
//! Port of the engine's Go `provider/openai` package.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, Result};
use base64::Engine as _;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::types::{
    endpoint, error_message, error_wait, read_sse, APIError, Emitter, Image, Message, Provider,
    Request, Response, ToolCall, Usage, USER_AGENT,
};

/// Called when an upstream returns 429, so a provider that rotates its egress
/// can request a new exit IP before the retry.
static ON_RATE_LIMITED: OnceLock<Arc<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Installs the 429 hook. Only the first call has an effect, like a package
/// variable set once at boot.
pub fn set_on_rate_limited(hook: Arc<dyn Fn() + Send + Sync>) {
    let _ = ON_RATE_LIMITED.set(hook);
}

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

type HeaderFn = Arc<dyn Fn() -> HashMap<String, String> + Send + Sync>;

/// The chat-completions adapter.
#[derive(Clone, Default)]
pub struct Adapter {
    /// Called once per HTTP request; its entries are merged into the outbound
    /// headers and overwrite the standard ones (the opencode contract).
    pub header_func: Option<HeaderFn>,
    /// Overrides the shared HTTP client.
    pub client: Option<reqwest::Client>,
    /// When set, every request goes through this BoringSSL client instead.
    /// The Opencode adapter uses it to present the official client's
    /// ClientHello; the rest of the engine keeps reqwest.
    tls_client: Option<wreq::Client>,
}

impl Adapter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_header_func(mut self, header_func: HeaderFn) -> Self {
        self.header_func = Some(header_func);
        self
    }

    pub(crate) fn with_tls_client(mut self, client: wreq::Client) -> Self {
        self.tls_client = Some(client);
        self
    }

    fn client(&self) -> reqwest::Client {
        self.client.clone().unwrap_or_else(shared_client)
    }

    /// Posts one build to the chat-completions endpoint.
    pub(crate) async fn complete(
        &self,
        req: &Request,
        prov: &Provider,
        key: &str,
    ) -> Result<Response> {
        let sent = self.send(req, prov, key, false).await?;
        let (status, retry_after, raw) = match sent {
            Sent::Live(response) => {
                let response = *response;
                let status = response.status();
                let retry_after = response.retry_after;
                (status, retry_after, response.read(8 << 20).await?)
            }
            Sent::Buffered {
                status,
                retry_after,
                raw,
            } => (status, retry_after, raw),
        };
        if !(200..300).contains(&status) {
            return Err(APIError::new(status, error_message(&raw))
                .with_retry_after(error_wait(&raw, retry_after))
                .into_anyhow());
        }
        parse_chat_response(&raw)
    }

    /// Posts one build and folds the SSE chunks back into a [`Response`].
    pub(crate) async fn stream(
        &self,
        req: &Request,
        prov: &Provider,
        key: &str,
        emit: &mut dyn Emitter,
    ) -> Result<Response> {
        let sent = self.send(req, prov, key, true).await?;
        let response = match sent {
            Sent::Live(response) => *response,
            Sent::Buffered {
                status,
                retry_after,
                raw,
            } => {
                return Err(APIError::new(status, error_message(&raw))
                    .with_retry_after(error_wait(&raw, retry_after))
                    .into_anyhow());
            }
        };

        let status = response.status();
        let retry_after = response.retry_after;
        if !(200..300).contains(&status) {
            let raw = response.read(1 << 20).await?;
            return Err(APIError::new(status, error_message(&raw))
                .with_retry_after(error_wait(&raw, retry_after))
                .into_anyhow());
        }

        let mut out = Response::default();
        let mut calls: BTreeMap<i64, ToolCall> = BTreeMap::new();
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut finish = String::new();

        read_sse(response.into_stream(), |data| {
            if data == "[DONE]" {
                return Ok(());
            }
            let Ok(part) = serde_json::from_str::<Chunk>(data) else {
                return Ok(());
            };
            if out.id.is_empty() {
                out.id = part.id.clone();
            }
            if out.model.is_empty() {
                out.model = part.model.clone();
            }
            if let Some(usage) = &part.usage {
                out.usage = Usage {
                    prompt_tokens: usage.prompt_tokens,
                    completion_tokens: usage.completion_tokens,
                    total_tokens: usage.total_tokens,
                    cache_read_tokens: usage.prompt_tokens_details.cached_tokens,
                    reasoning_tokens: usage.completion_tokens_details.reasoning_tokens,
                    input_includes_cache: true,
                    ..Usage::default()
                };
            }
            for choice in &part.choices {
                if let Some(reason) = &choice.finish_reason {
                    finish = reason.clone();
                }
                if let Some(content) = &choice.delta.content {
                    if !content.is_empty() {
                        text.push_str(content);
                    }
                }
                if let Some(reasoning_part) = &choice.delta.reasoning_content {
                    if !reasoning_part.is_empty() {
                        reasoning.push_str(reasoning_part);
                        if let Some(on_reasoning) = &req.on_reasoning {
                            on_reasoning(reasoning_part)?;
                        }
                    }
                }
                if let Some(content) = &choice.delta.content {
                    if !content.is_empty() {
                        emit.emit(content)?;
                    }
                }
                if let Some(tool_calls) = &choice.delta.tool_calls {
                    for call in tool_calls {
                        let target = calls.entry(call.index).or_default();
                        if !call.id.is_empty() {
                            target.id = call.id.clone();
                        }
                        if !call.function.name.is_empty() {
                            target.name = call.function.name.clone();
                        }
                        if !call.function.arguments.is_empty() {
                            target.arguments.push_str(&call.function.arguments);
                        }
                    }
                }
            }
            Ok(())
        })
        .await?;

        out.content = text;
        out.reasoning_content = reasoning;
        out.finish_reason = finish;
        out.tool_calls = unique_tool_calls(calls.into_values().collect());
        Ok(out)
    }

    /// Posts the request and repairs one refusal.
    ///
    /// Some OpenAI-compatible gateways validate a history that contains an
    /// assistant turn against the contract their reasoning models are served
    /// under, and reject the whole request when `reasoning_content` is absent.
    /// One retry with the key present and empty covers the gateways that check
    /// only for its presence; anything else is handed back exactly as it came,
    /// body and all, so the caller reports it unchanged.
    async fn send(&self, req: &Request, prov: &Provider, key: &str, stream: bool) -> Result<Sent> {
        let body = serde_json::to_vec(&build_request(req, stream))?;
        let response = self.do_request(prov, key, &body).await?;
        if response.status() != 400 {
            return Ok(Sent::Live(Box::new(response)));
        }

        let status = 400;
        let retry_after = response.retry_after;
        let raw = response.read(1 << 20).await?;
        if !error_message(&raw).contains("reasoning_content") {
            return Ok(Sent::Buffered {
                status,
                retry_after,
                raw,
            });
        }

        let body = serde_json::to_vec(&build_request_with_reasoning(req, stream))?;
        let response = self.do_request(prov, key, &body).await?;
        Ok(Sent::Live(Box::new(response)))
    }

    pub(crate) async fn do_request(
        &self,
        prov: &Provider,
        key: &str,
        body: &[u8],
    ) -> Result<WireResponse> {
        let endpoint = endpoint(&prov.endpoint, "/v1/chat/completions", "/chat/completions")?;
        let response = self.post(&endpoint, key, body, "application/json").await?;
        if response.status() == 429 {
            if let Some(hook) = ON_RATE_LIMITED.get() {
                drop(response);
                hook();
                return self.post(&endpoint, key, body, "application/json").await;
            }
        }
        Ok(response)
    }

    async fn post(
        &self,
        endpoint: &str,
        key: &str,
        body: &[u8],
        accept: &str,
    ) -> Result<WireResponse> {
        let headers = self.headers(accept, key);
        if let Some(client) = &self.tls_client {
            let response = client
                .post(endpoint)
                .headers(headers)
                .body(body.to_vec())
                .send()
                .await?;
            let retry_after = retry_after_from(response.headers());
            return Ok(WireResponse::new(
                response.status().as_u16(),
                retry_after,
                WireBody::Wreq(response),
            ));
        }
        let response = self
            .client()
            .post(endpoint)
            .headers(headers)
            .body(body.to_vec())
            .send()
            .await?;
        let retry_after = retry_after_from(response.headers());
        Ok(WireResponse::new(
            response.status().as_u16(),
            retry_after,
            WireBody::Reqwest(response),
        ))
    }

    /// Builds the outbound headers once. `header_func` entries **replace** the
    /// standard ones, which is the Go `Header.Set` behaviour the opencode
    /// contract depends on: reqwest's `RequestBuilder::header` appends, which
    /// would leave two User-Agent values on the wire.
    fn headers(&self, accept: &str, key: &str) -> reqwest::header::HeaderMap {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_str(accept).unwrap_or(HeaderValue::from_static("application/json")),
        );
        headers.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static(USER_AGENT),
        );
        if !key.is_empty() {
            if let Ok(value) = HeaderValue::from_str(&format!("Bearer {key}")) {
                headers.insert(reqwest::header::AUTHORIZATION, value);
            }
        }
        if let Some(header_func) = &self.header_func {
            for (name, value) in header_func() {
                let (Ok(name), Ok(value)) = (
                    HeaderName::from_bytes(name.as_bytes()),
                    HeaderValue::from_str(&value),
                ) else {
                    continue;
                };
                headers.insert(name, value);
            }
        }
        headers
    }

    /// Executes `req` against `/v1/responses` (non-streaming). The fallback for
    /// models that reject chat completions.
    pub async fn complete_via_responses(
        &self,
        req: &Request,
        prov: &Provider,
        key: &str,
    ) -> Result<Response> {
        let endpoint = endpoint(&prov.endpoint, "/v1/responses", "/responses")?;
        let body = serde_json::to_vec(&build_responses_request(req, false))?;
        let response = self.post(&endpoint, key, &body, "application/json").await?;
        let status = response.status();
        let retry_after = response.retry_after;
        let raw = response.read(8 << 20).await?;
        if !(200..300).contains(&status) {
            return Err(APIError::new(status, error_message(&raw))
                .with_retry_after(error_wait(&raw, retry_after))
                .into_anyhow());
        }

        let parsed: ResponsesResponse = serde_json::from_slice(&raw)
            .map_err(|e| anyhow!("openai: decode responses response: {e}"))?;
        let mut out = Response {
            id: parsed.id.clone(),
            model: parsed.model.clone(),
            ..Response::default()
        };
        let (content, calls, usage) = parse_responses_output(&parsed);
        out.content = content;
        out.tool_calls = calls;
        out.usage = usage;
        out.reasoning_content = responses_reasoning(&parsed);
        if out.content.is_empty() && out.tool_calls.is_empty() {
            return Err(anyhow!("openai: empty responses output"));
        }
        Ok(out)
    }

    /// Executes `req` against `/v1/responses` with streaming, mirroring what
    /// reference clients send.
    pub async fn stream_via_responses(
        &self,
        req: &Request,
        prov: &Provider,
        key: &str,
        emit: &mut dyn Emitter,
    ) -> Result<Response> {
        let endpoint = endpoint(&prov.endpoint, "/v1/responses", "/responses")?;
        let body = serde_json::to_vec(&build_responses_request(req, true))?;
        let response = self
            .post(&endpoint, key, &body, "text/event-stream")
            .await?;
        let status = response.status();
        let retry_after = response.retry_after;
        if !(200..300).contains(&status) {
            let raw = response.read(1 << 20).await?;
            return Err(APIError::new(status, error_message(&raw))
                .with_retry_after(error_wait(&raw, retry_after))
                .into_anyhow());
        }

        let mut out = Response::default();
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut arg_bufs: HashMap<String, String> = HashMap::new();
        let mut call_order: Vec<String> = Vec::new();
        let mut completed: Option<ResponsesResponse> = None;
        let mut finished = false;
        let mut cut_short = false;

        read_sse(response.into_stream(), |data| {
            if data == "[DONE]" {
                finished = true;
                return Ok(());
            }
            let Ok(event) = serde_json::from_str::<ResponsesStreamEvent>(data) else {
                return Ok(());
            };
            match event.kind.as_str() {
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    reasoning.push_str(&event.delta);
                    if let Some(on_reasoning) = &req.on_reasoning {
                        on_reasoning(&event.delta)?;
                    }
                }
                "response.output_text.delta" => {
                    text.push_str(&event.delta);
                    emit.emit(&event.delta)?;
                }
                "response.function_call_arguments.delta" => {
                    let id = if event.item_id.is_empty() {
                        "call_0".to_string()
                    } else {
                        event.item_id.clone()
                    };
                    let buffer = arg_bufs.entry(id.clone()).or_insert_with(|| {
                        call_order.push(id.clone());
                        String::new()
                    });
                    buffer.push_str(&event.delta);
                }
                "response.output_item.added" | "response.output_item.done" => {}
                "response.completed" => {
                    if let Some(response) = event.response {
                        completed = Some(response);
                    }
                    finished = true;
                }
                "response.incomplete" => {
                    if let Some(response) = event.response {
                        completed = Some(response);
                    }
                    cut_short = true;
                    finished = true;
                }
                "response.failed" | "error" => {
                    let snippet: String = data.chars().take(300).collect();
                    return Err(anyhow!("openai: responses stream failed: {snippet}"));
                }
                _ => {}
            }
            Ok(())
        })
        .await?;

        if let Some(completed) = &completed {
            let (content, calls, usage) = parse_responses_output(completed);
            out.content = content;
            out.tool_calls = calls;
            out.usage = usage;
            out.id = completed.id.clone();
            out.model = completed.model.clone();
            out.finish_reason = if cut_short { "length" } else { "stop" }.to_string();
        } else {
            out.content = text;
            out.tool_calls = unique_tool_calls(
                call_order
                    .iter()
                    .map(|id| ToolCall {
                        id: id.clone(),
                        name: String::new(),
                        arguments: arg_bufs.get(id).cloned().unwrap_or_default(),
                    })
                    .collect(),
            );
        }

        if out.content.is_empty() && out.tool_calls.is_empty() && !finished {
            return Err(anyhow!("openai: empty responses stream"));
        }
        out.reasoning_content = reasoning;
        if out.reasoning_content.is_empty() {
            if let Some(completed) = &completed {
                out.reasoning_content = responses_reasoning(completed);
                if !out.reasoning_content.is_empty() {
                    if let Some(on_reasoning) = &req.on_reasoning {
                        on_reasoning(&out.reasoning_content)?;
                    }
                }
            }
        }
        Ok(out)
    }
}

/// Reads the `Retry-After` header off either HTTP stack's response headers.
/// `reqwest` and `wreq` re-export the same `http` header map, which is what lets
/// one set of built headers feed both clients here.
fn retry_after_from(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    crate::types::header_retry_after(
        headers
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
    )
}

enum Sent {
    Live(Box<WireResponse>),
    Buffered {
        status: u16,
        retry_after: Option<std::time::Duration>,
        raw: Vec<u8>,
    },
}

/// One response from either HTTP stack.
pub(crate) struct WireResponse {
    status: u16,
    body: WireBody,
    /// The `Retry-After` header, read before the response is consumed. Kept
    /// beside the status because a 429's body is the last place the value is
    /// guaranteed to survive: an HTML error page from an edge carries none.
    retry_after: Option<std::time::Duration>,
}

enum WireBody {
    Reqwest(reqwest::Response),
    Wreq(wreq::Response),
}

impl WireResponse {
    fn new(status: u16, retry_after: Option<std::time::Duration>, body: WireBody) -> Self {
        Self {
            status,
            body,
            retry_after,
        }
    }

    fn status(&self) -> u16 {
        self.status
    }

    /// Reads the body, keeping at most `limit` bytes.
    async fn read(self, limit: usize) -> Result<Vec<u8>> {
        let bytes = match self.body {
            WireBody::Reqwest(response) => response.bytes().await?,
            WireBody::Wreq(response) => response.bytes().await?,
        };
        Ok(bytes[..bytes.len().min(limit)].to_vec())
    }

    /// The body as a byte stream, for SSE.
    fn into_stream(self) -> BodyStream {
        match self.body {
            WireBody::Reqwest(response) => response
                .bytes_stream()
                .map(|item| item.map_err(std::io::Error::other))
                .boxed(),
            WireBody::Wreq(response) => response
                .bytes_stream()
                .map(|item| item.map_err(std::io::Error::other))
                .boxed(),
        }
    }
}

type BodyStream = Pin<Box<dyn Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send>>;

#[async_trait::async_trait]
impl crate::types::Adapter for Adapter {
    fn name(&self) -> &str {
        "openai"
    }

    async fn complete(&self, req: &Request, prov: &Provider, key: &str) -> Result<Response> {
        self.complete(req, prov, key).await
    }

    async fn stream(
        &self,
        req: &Request,
        prov: &Provider,
        key: &str,
        emit: &mut dyn Emitter,
    ) -> Result<Response> {
        self.stream(req, prov, key, emit).await
    }
}

// --- wire shapes ---------------------------------------------------------

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<WireMessage>,
    #[serde(skip_serializing_if = "is_false")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<WireTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "is_zero")]
    max_tokens: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    user: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

#[derive(Serialize, Deserialize, Default)]
struct StreamOptions {
    include_usage: bool,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct WireMessage {
    #[serde(default)]
    role: String,
    // A turn that only calls a tool carries no content at all, so both fields
    // have to survive being absent rather than being named in the payload.
    #[serde(default)]
    content: Value,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    tool_calls: Vec<WireToolCall>,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    tool_call_id: String,
    /// A pointer so it can be absent (the normal case) and present-but-empty at
    /// the same time, which is exactly the difference between a gateway that
    /// ignores the key and one that rejects the request for not finding it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    reasoning_content: Option<String>,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: WireFunction,
}

#[derive(Serialize, Deserialize, Default, Clone)]
struct WireFunction {
    // A streamed tool call names its function once, in the fragment that opens
    // it; the fragments after that carry arguments alone. Requiring the name here
    // made every continuation chunk fail to decode, and the whole call arrived
    // with empty arguments — a tool the provider had clearly named and filled.
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Serialize)]
struct WireTool {
    #[serde(rename = "type")]
    kind: String,
    function: WireToolDef,
}

#[derive(Serialize)]
struct WireToolDef {
    name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    description: String,
    #[serde(skip_serializing_if = "Map::is_empty")]
    parameters: Map<String, Value>,
}

#[derive(Deserialize, Default)]
struct ChatResponse {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: ChatUsage,
}

#[derive(Deserialize, Default)]
struct ChatChoice {
    #[serde(default)]
    message: WireMessage,
    #[serde(default)]
    finish_reason: String,
}

#[derive(Deserialize, Default)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: i64,
    #[serde(default)]
    completion_tokens: i64,
    #[serde(default)]
    total_tokens: i64,
    #[serde(default)]
    prompt_tokens_details: PromptDetails,
    #[serde(default)]
    completion_tokens_details: CompletionDetails,
}

#[derive(Deserialize, Default)]
struct PromptDetails {
    #[serde(default)]
    cached_tokens: i64,
}

#[derive(Deserialize, Default)]
struct CompletionDetails {
    #[serde(default)]
    reasoning_tokens: i64,
}

#[derive(Deserialize, Default)]
struct Chunk {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Deserialize, Default)]
struct ChunkChoice {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    // The provider spells an absent field as `null` in finish and reasoning
    // chunks; a bare `String`/`Vec` rejects that and would drop the whole
    // chunk, including its `finish_reason`.
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<DeltaToolCall>>,
}

#[derive(Deserialize, Default)]
struct DeltaToolCall {
    #[serde(default)]
    index: i64,
    #[serde(default)]
    id: String,
    #[serde(default)]
    function: WireFunction,
}

fn build_request(req: &Request, stream: bool) -> ChatRequest {
    let mut tools = Vec::with_capacity(req.tools.len());
    for def in &req.tools {
        tools.push(WireTool {
            kind: "function".to_string(),
            function: WireToolDef {
                name: def.name.clone(),
                description: def.description.clone(),
                parameters: def.parameters.clone(),
            },
        });
    }
    ChatRequest {
        model: req.model.clone(),
        messages: to_messages(&req.messages),
        stream,
        stream_options: None,
        tool_choice: (!tools.is_empty()).then(|| "auto".to_string()),
        tools,
        max_tokens: req.max_tokens,
        user: req.user.clone(),
        temperature: req.temperature,
        top_p: req.top_p,
    }
}

/// `build_request` with an empty `reasoning_content` on every assistant turn.
fn build_request_with_reasoning(req: &Request, stream: bool) -> ChatRequest {
    let mut out = build_request(req, stream);
    for message in &mut out.messages {
        if message.role == "assistant" && message.reasoning_content.is_none() {
            message.reasoning_content = Some(String::new());
        }
    }
    out
}

fn to_messages(messages: &[Message]) -> Vec<WireMessage> {
    let mut out = Vec::with_capacity(messages.len());
    for incoming in messages {
        match incoming.role.as_str() {
            "tool" => out.push(WireMessage {
                role: "tool".to_string(),
                content: Value::String(tool_output(&incoming.content)),
                tool_call_id: incoming.tool_call_id.clone(),
                ..WireMessage::default()
            }),
            "assistant" => {
                // A tool-call turn often has no text. An assistant turn with
                // neither text nor calls is an empty answer kept for the
                // record; it must not be replayed at all.
                let content = if !incoming.images.is_empty() || !incoming.content.trim().is_empty()
                {
                    content_of(incoming)
                } else if incoming.tool_calls.is_empty() {
                    continue;
                } else {
                    Value::Null
                };

                let mut converted = WireMessage {
                    role: "assistant".to_string(),
                    content,
                    ..WireMessage::default()
                };
                if !incoming.reasoning_content.is_empty() {
                    converted.reasoning_content = Some(incoming.reasoning_content.clone());
                }
                for call in &incoming.tool_calls {
                    converted.tool_calls.push(WireToolCall {
                        id: call.id.clone(),
                        kind: "function".to_string(),
                        function: WireFunction {
                            name: call.name.clone(),
                            arguments: replay_arguments(&call.arguments),
                        },
                    });
                }
                out.push(converted);
            }
            _ => {
                if incoming.images.is_empty() && incoming.content.trim().is_empty() {
                    continue;
                }
                out.push(WireMessage {
                    role: incoming.role.clone(),
                    content: content_of(incoming),
                    ..WireMessage::default()
                });
            }
        }
    }
    out
}

/// Keeps an empty tool result from reaching providers that require message
/// content to be a non-empty string.
fn tool_output(content: &str) -> String {
    if content.trim().is_empty() {
        "(no output)".to_string()
    } else {
        content.to_string()
    }
}

/// Replays a tool call's arguments in a shape the upstream parser accepts.
/// A call whose arguments never parsed — a stream cut mid-JSON, or a model that
/// named a function and filled nothing — is replayed on every later round of the
/// turn, and the upstream rejects the whole request (`function.arguments must be
/// valid JSON`) rather than the one call. `{}` is what the Anthropic wire path
/// already substitutes for the same broken call.
fn replay_arguments(raw: &str) -> String {
    if raw.trim().is_empty() {
        return "{}".to_string();
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(_) => raw.to_string(),
        Err(_) => "{}".to_string(),
    }
}

/// A plain string unless the message carries pictures, in which case it becomes
/// the array of parts an OpenAI-compatible endpoint expects.
fn content_of(message: &Message) -> Value {
    if message.images.is_empty() {
        return Value::String(message.content.clone());
    }
    let mut parts: Vec<Value> = Vec::with_capacity(message.images.len() + 1);
    if !message.content.is_empty() {
        parts.push(json!({ "type": "text", "text": message.content }));
    }
    for image in &message.images {
        parts.push(json!({
            "type": "image_url",
            "image_url": { "url": data_uri(image) },
        }));
    }
    Value::Array(parts)
}

/// The inline data URI both OpenAI shapes accept.
fn data_uri(image: &Image) -> String {
    let media = if image.media_type.is_empty() {
        "image/jpeg"
    } else {
        &image.media_type
    };
    format!(
        "data:{media};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&image.data)
    )
}

fn text_content(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

fn parse_chat_response(raw: &[u8]) -> Result<Response> {
    let parsed: ChatResponse =
        serde_json::from_slice(raw).map_err(|e| anyhow!("openai: decode response: {e}"))?;

    let mut out = Response {
        id: parsed.id.clone(),
        model: parsed.model.clone(),
        usage: Usage {
            prompt_tokens: parsed.usage.prompt_tokens,
            completion_tokens: parsed.usage.completion_tokens,
            total_tokens: parsed.usage.total_tokens,
            cache_read_tokens: parsed.usage.prompt_tokens_details.cached_tokens,
            reasoning_tokens: parsed.usage.completion_tokens_details.reasoning_tokens,
            input_includes_cache: true,
            ..Usage::default()
        },
        ..Response::default()
    };

    if let Some(choice) = parsed.choices.first() {
        out.content = text_content(&choice.message.content);
        out.finish_reason = choice.finish_reason.clone();
        if let Some(reasoning) = &choice.message.reasoning_content {
            out.reasoning_content = reasoning.clone();
        }
        out.tool_calls = unique_tool_calls(
            choice
                .message
                .tool_calls
                .iter()
                .map(|call| ToolCall {
                    id: call.id.clone(),
                    name: call.function.name.clone(),
                    arguments: call.function.arguments.clone(),
                })
                .collect(),
        );
    }
    Ok(out)
}

// --- Responses API -------------------------------------------------------

#[derive(Serialize)]
struct ResponsesRequest {
    model: String,
    input: Value,
    #[serde(skip_serializing_if = "is_false")]
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<RespTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "String::is_empty")]
    user: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    store: Option<bool>,
    #[serde(skip_serializing_if = "String::is_empty")]
    prompt_cache_key: String,
}

/// The FLAT function-tool shape the Responses API expects (NOT nested under
/// `function` like chat completions).
#[derive(Serialize)]
struct RespTool {
    #[serde(rename = "type")]
    kind: String,
    name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    description: String,
    #[serde(skip_serializing_if = "Map::is_empty")]
    parameters: Map<String, Value>,
}

#[derive(Deserialize, Default)]
struct ResponsesResponse {
    #[serde(default)]
    id: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    output: Vec<OutputItem>,
    #[serde(default)]
    usage: Option<RespUsage>,
}

#[derive(Deserialize, Default)]
struct OutputItem {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    content: Vec<OutputPart>,
    #[serde(default)]
    summary: Vec<OutputPart>,
    #[serde(default)]
    call_id: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Deserialize, Default)]
struct OutputPart {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
}

#[derive(Deserialize, Default)]
struct RespUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    total_tokens: i64,
    #[serde(default)]
    input_tokens_details: RespInputDetails,
    #[serde(default)]
    output_tokens_details: RespOutputDetails,
}

#[derive(Deserialize, Default)]
struct RespInputDetails {
    #[serde(default)]
    cached_tokens: i64,
    #[serde(default)]
    cache_write_tokens: i64,
}

#[derive(Deserialize, Default)]
struct RespOutputDetails {
    #[serde(default)]
    reasoning_tokens: i64,
}

#[derive(Deserialize, Default)]
struct ResponsesStreamEvent {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    delta: String,
    #[serde(default)]
    item_id: String,
    #[serde(default)]
    response: Option<ResponsesResponse>,
}

/// Mirrors what reference clients send to `/v1/responses`: full tool
/// definitions, streaming flags, disabled persistence and a prompt cache key for
/// session affinity.
fn build_responses_request(req: &Request, stream: bool) -> ResponsesRequest {
    let mut items: Vec<Value> = Vec::with_capacity(req.messages.len());
    // Call ids must be unique across the whole input.
    let mut seen_calls: HashSet<String> = HashSet::new();
    let mut seen_results: HashSet<String> = HashSet::new();

    for message in &req.messages {
        // The Responses API has no role=tool message: tool results travel as
        // function_call_output items and model tool calls as function_call
        // items.
        if message.role == "tool" {
            if !message.tool_call_id.is_empty() && seen_results.contains(&message.tool_call_id) {
                continue;
            }
            if !message.tool_call_id.is_empty() {
                seen_results.insert(message.tool_call_id.clone());
            }
            items.push(json!({
                "type": "function_call_output",
                "call_id": message.tool_call_id,
                "output": tool_output(&text_content(&Value::String(message.content.clone()))),
            }));
            continue;
        }
        if message.role == "assistant" && !message.tool_calls.is_empty() {
            let mut calls = Vec::new();
            for call in &message.tool_calls {
                if !call.id.is_empty() && seen_calls.contains(&call.id) {
                    continue;
                }
                if !call.id.is_empty() {
                    seen_calls.insert(call.id.clone());
                }
                calls.push(call);
            }
            let text = message.content.trim();
            if !text.is_empty() {
                items.push(json!({ "role": "assistant", "content": text }));
            } else if !calls.is_empty() {
                // This gateway translates a bare function_call item into an
                // assistant chat message with content "". Its validator rejects
                // that. A space is the smallest text that survives the check
                // without inventing an answer.
                items.push(json!({ "role": "assistant", "content": " " }));
            }
            for call in calls {
                items.push(json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": replay_arguments(&call.arguments),
                }));
            }
            continue;
        }
        // An empty text item is invalid on every role, and null is only
        // accepted on an assistant message.
        if message.images.is_empty()
            && text_content(&Value::String(message.content.clone()))
                .trim()
                .is_empty()
        {
            continue;
        }
        items.push(json!({ "role": message.role, "content": responses_content(message) }));
    }

    let mut body = ResponsesRequest {
        model: req.model.clone(),
        input: Value::Array(items),
        stream,
        max_output_tokens: Some(if req.max_tokens > 0 {
            req.max_tokens
        } else {
            32000
        }),
        tools: Vec::new(),
        tool_choice: None,
        user: req.user.clone(),
        include: vec!["reasoning.encrypted_content".to_string()],
        store: Some(false),
        prompt_cache_key: String::new(),
    };

    // The prompt cache key is what lets the provider reuse the conversation's
    // prefix. It has to name the conversation and nothing else.
    if !req.session_id.is_empty() {
        body.prompt_cache_key = cache_key_for(&req.session_id);
    }
    for def in &req.tools {
        body.tools.push(RespTool {
            kind: "function".to_string(),
            name: def.name.clone(),
            description: def.description.clone(),
            parameters: def.parameters.clone(),
        });
    }
    if !body.tools.is_empty() {
        body.tool_choice = Some("auto".to_string());
    }
    body
}

/// `content_of` in the `/v1/responses` spelling: the same data URI under a
/// different part name.
fn responses_content(message: &Message) -> Value {
    if message.images.is_empty() {
        return Value::String(text_content(&Value::String(message.content.clone())));
    }
    let mut parts: Vec<Value> = Vec::with_capacity(message.images.len() + 1);
    let text = text_content(&Value::String(message.content.clone()));
    if !text.is_empty() {
        parts.push(json!({ "type": "input_text", "text": text }));
    }
    for image in &message.images {
        parts.push(json!({ "type": "input_image", "image_url": data_uri(image) }));
    }
    Value::Array(parts)
}

/// Names the engine so a key cannot collide with one another client of the same
/// provider sent, and the session follows unmodified.
fn cache_key_for(session: &str) -> String {
    format!("aixodia-engine-{session}")
}

/// Keeps the first call for each id.
fn unique_tool_calls(calls: Vec<ToolCall>) -> Vec<ToolCall> {
    if calls.len() < 2 {
        return calls;
    }
    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::with_capacity(calls.len());
    for call in calls {
        if !call.id.is_empty() {
            if seen.contains(&call.id) {
                continue;
            }
            seen.insert(call.id.clone());
        }
        out.push(call);
    }
    out
}

fn parse_responses_output(parsed: &ResponsesResponse) -> (String, Vec<ToolCall>, Usage) {
    let mut text = String::new();
    let mut calls = Vec::new();
    let usage = match &parsed.usage {
        Some(usage) => Usage {
            prompt_tokens: usage.input_tokens,
            completion_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cache_read_tokens: usage.input_tokens_details.cached_tokens,
            cache_write_tokens: usage.input_tokens_details.cache_write_tokens,
            reasoning_tokens: usage.output_tokens_details.reasoning_tokens,
            input_includes_cache: true,
        },
        None => Usage::default(),
    };

    for item in &parsed.output {
        match item.kind.as_str() {
            "message" => {
                for part in &item.content {
                    if part.kind == "output_text" {
                        text.push_str(&part.text);
                    }
                }
            }
            "function_call" => {
                let id = if item.call_id.is_empty() {
                    item.id.clone()
                } else {
                    item.call_id.clone()
                };
                calls.push(ToolCall {
                    id,
                    name: item.name.clone(),
                    arguments: item.arguments.clone(),
                });
            }
            _ => {}
        }
    }
    (text, unique_tool_calls(calls), usage)
}

/// Only readable upstream text is displayable; encrypted reasoning is not text.
fn responses_reasoning(response: &ResponsesResponse) -> String {
    let mut text = String::new();
    for item in &response.output {
        if item.kind != "reasoning" {
            continue;
        }
        for part in &item.summary {
            if part.kind == "summary_text" {
                text.push_str(&part.text);
            }
        }
        for part in &item.content {
            if part.kind == "reasoning_text" {
                text.push_str(&part.text);
            }
        }
    }
    text
}
