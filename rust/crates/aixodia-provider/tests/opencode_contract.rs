//! The OpenCode free-tier contract, driven end to end against a fake upstream.
//!
//! Port of the meaningful half of the Go `provider/opencode/opencode_test.go`:
//! headers, forced streaming, injected tools, the environment block, and the
//! fallback ladder.

use std::sync::{Arc, Mutex};

use aixodia_provider::opencode::{Adapter, CLIENT_AGENT_MEASURED};
use aixodia_provider::{Adapter as AdapterTrait, Message, Provider, Request, ToolDef};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::Router;
use serde_json::Value;

#[derive(Clone, Default)]
struct Captured {
    requests: Arc<Mutex<Vec<(String, HeaderMap, Value)>>>,
}

fn responses_body() -> &'static str {
    concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello \"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"there\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\",\"model\":\"zen-model\",\"output\":[{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello there\"}]}],\"usage\":{\"input_tokens\":11,\"output_tokens\":22,\"total_tokens\":33}}}\n\n"
    )
}

/// Spelled the way the real gateway streams: an opening chunk whose `content`,
/// `reasoning` and `tool_calls` are null, then text, then a finish chunk that is
/// null again.
fn chat_body() -> &'static str {
    concat!(
        "data: {\"id\":\"gen-1\",\"model\":\"zen-model\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"delta\":{\"role\":\"assistant\",\"content\":null,\"reasoning\":null,\"tool_calls\":null}}]}\n\n",
        "data: {\"id\":\"gen-1\",\"model\":\"zen-model\",\"choices\":[{\"index\":0,\"finish_reason\":null,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello \"}}]}\n\n",
        "data: {\"id\":\"gen-1\",\"model\":\"zen-model\",\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{\"role\":\"assistant\",\"content\":\"there\"}}],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":22,\"total_tokens\":33}}\n\n",
        "data: [DONE]\n\n"
    )
}

async fn upstream(
    State(captured): State<Captured>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> impl axum::response::IntoResponse {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    captured
        .requests
        .lock()
        .unwrap()
        .push((uri.path().to_string(), headers, parsed));

    let payload = if uri.path().ends_with("/responses") {
        responses_body()
    } else {
        chat_body()
    };
    (
        [
            ("content-type", "text/event-stream"),
            ("connection", "keep-alive"),
        ],
        payload,
    )
}

/// A fake Zen that answers both API shapes the way the ladder asks for them.
async fn fake_zen() -> (String, Captured) {
    let captured = Captured::default();
    let app = Router::new()
        .route("/v1/responses", post(upstream))
        .route("/v1/chat/completions", post(upstream))
        .with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{address}"), captured)
}

fn provider_for(base: &str, keys: Vec<String>) -> Provider {
    Provider {
        name: "opencode".to_string(),
        endpoint: base.to_string(),
        keys,
        ..Provider::default()
    }
}

fn tool_names(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    tool.get("name")
                        .and_then(Value::as_str)
                        .or_else(|| tool.pointer("/function/name").and_then(Value::as_str))
                        .unwrap_or("")
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

fn has_stream(body: &Value) -> bool {
    body.get("stream").and_then(Value::as_bool).unwrap_or(false)
}

fn message(content: &str) -> Message {
    Message {
        role: "user".to_string(),
        content: content.to_string(),
        ..Message::default()
    }
}

#[tokio::test]
async fn free_tier_contract_on_complete() {
    let (base, captured) = fake_zen().await;
    let adapter = Adapter::new();

    let req = Request {
        model: "zen-model".to_string(),
        messages: vec![message("hi")],
        tools: vec![ToolDef {
            name: "current_time".to_string(),
            description: "now".to_string(),
            ..ToolDef::default()
        }],
        ..Request::default()
    };

    let resp = adapter
        .complete(&req, &provider_for(&base, vec!["key".to_string()]), "key")
        .await
        .expect("complete");

    let requests = captured.requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "one request for a successful complete");
    let (path, headers, body) = &requests[0];

    // The client's own request shape: chat completions first.
    assert!(
        path.ends_with("/chat/completions"),
        "first rung is chat, path = {path}"
    );
    assert_eq!(
        headers.get("user-agent").and_then(|v| v.to_str().ok()),
        Some(CLIENT_AGENT_MEASURED)
    );
    assert_eq!(
        headers.get("authorization").and_then(|v| v.to_str().ok()),
        Some("Bearer key")
    );

    // ses_<12 hex><14 base62>, and the low twelve bits of the inverted head
    // (the counter) must be below 64 — the free-tier check.
    let session = headers
        .get("x-opencode-session")
        .and_then(|v| v.to_str().ok())
        .expect("session header");
    let rest = session.strip_prefix("ses_").expect("ses_ prefix");
    assert_eq!(rest.len(), 26, "session = {session}");
    let (head, tail) = rest.split_at(12);
    assert!(head.chars().all(|c| c.is_ascii_hexdigit()), "head = {head}");
    assert!(
        tail.chars().all(|c| c.is_ascii_alphanumeric()),
        "tail = {tail}"
    );
    let encoded = u64::from_str_radix(head, 16).expect("hex head");
    let counter = (!encoded) & 0xfff;
    assert!(counter < 64, "counter = {counter}");

    // The engine must not name its own request.
    assert!(headers.get("x-opencode-request").is_none());
    assert_eq!(
        headers
            .get("x-opencode-client")
            .and_then(|v| v.to_str().ok()),
        Some("cli")
    );
    let project = headers
        .get("x-opencode-project")
        .and_then(|v| v.to_str().ok())
        .expect("project header");
    assert_eq!(project.len(), 40, "project = {project}");
    assert!(
        project.chars().all(|c| c.is_ascii_hexdigit()),
        "project = {project}"
    );

    // Contract: forced streaming, and shell + read declared alongside ours.
    assert!(has_stream(body), "stream must be true: {body}");
    let names = tool_names(body);
    for want in ["current_time", "shell", "read"] {
        assert!(names.iter().any(|name| name == want), "tools = {names:?}");
    }

    // The environment block leads the conversation; the caller's turn follows.
    let messages = body.get("messages").and_then(Value::as_array).unwrap();
    assert_eq!(
        messages[0].get("role").and_then(Value::as_str),
        Some("system")
    );
    let env = messages[0]
        .get("content")
        .and_then(Value::as_str)
        .expect("env content");
    assert!(env.contains("<env>"), "env = {env}");
    assert!(env.contains("Working directory:"), "env = {env}");
    assert_eq!(
        messages[1].get("content").and_then(Value::as_str),
        Some("hi")
    );

    // The deltas win, including the turns where the gateway spells the fields
    // as null; the repeated completed object must not double the text.
    assert_eq!(resp.content, "Hello there");
    assert_eq!(resp.finish_reason, "stop");
    assert_eq!(resp.usage.total_tokens, 33);
}

#[tokio::test]
async fn anonymous_request_uses_the_public_token() {
    let (base, captured) = fake_zen().await;
    Adapter::new()
        .complete(
            &Request {
                model: "zen-model".to_string(),
                messages: vec![message("hi")],
                ..Request::default()
            },
            &provider_for(&base, Vec::new()),
            "",
        )
        .await
        .expect("keyless complete");

    let requests = captured.requests.lock().unwrap();
    let (_, headers, _) = &requests[0];
    assert_eq!(
        headers.get("authorization").and_then(|v| v.to_str().ok()),
        Some("Bearer public")
    );
}

#[tokio::test]
async fn responses_fallback_is_used_when_chat_is_refused() {
    // A Zen that refuses chat completions for the tool payload, so the ladder
    // must fall through to the Responses API.
    let captured = Captured::default();
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(
                |State(captured): State<Captured>, headers: HeaderMap, body: Bytes| async move {
                    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    captured.requests.lock().unwrap().push((
                        "/v1/chat/completions".to_string(),
                        headers,
                        parsed,
                    ));
                    (
                        axum::http::StatusCode::BAD_REQUEST,
                        r#"{"error":{"message":"tools: invalid function declarations"}}"#,
                    )
                },
            ),
        )
        .route(
            "/v1/responses",
            post(
                |State(captured): State<Captured>, headers: HeaderMap, body: Bytes| async move {
                    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                    captured.requests.lock().unwrap().push((
                        "/v1/responses".to_string(),
                        headers,
                        parsed,
                    ));
                    ([("content-type", "text/event-stream")], responses_body())
                },
            ),
        )
        .with_state(captured.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let adapter = Adapter::new();
    let resp = adapter
        .complete(
            &Request {
                model: "zen-model".to_string(),
                messages: vec![message("hi")],
                ..Request::default()
            },
            &provider_for(&base, vec!["k".to_string()]),
            "k",
        )
        .await
        .expect("fallback complete");
    assert_eq!(resp.content, "Hello there");
    assert_eq!(resp.finish_reason, "stop");

    let requests = captured.requests.lock().unwrap();
    assert!(requests.len() >= 2, "chat then responses");
    assert!(requests[0].0.ends_with("/chat/completions"));
    assert!(requests[1].0.ends_with("/responses"));
    assert!(has_stream(&requests[1].2));
}
