//! Streamed tool calls against an OpenAI-compatible upstream.
//!
//! A provider names a function once, in the fragment that opens the call, and
//! sends the arguments as bare fragments after it. The fold has to keep both, or
//! the tool arrives with the right name and nothing to act on.

use aixodia_provider::openai;
use aixodia_provider::{Adapter as AdapterTrait, Message, Provider, Request, ToolCall};
use axum::body::Bytes;
use axum::extract::State;
use axum::routing::post;
use axum::Router;
use std::sync::{Arc, Mutex};

/// gpt-oss spelled exactly as it streams: id and name in the opening fragment,
/// then argument text with neither.
const STREAM: &str = r#"data: {"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"write_file","arguments":""}}]}}]}

data: {"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"arguments":"{\"path\""}}]}}]}

data: {"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"function":{"arguments":": \"/tmp/x\"}"}}]}}]}

data: {"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":"tool_calls"}]}

data: [DONE]

"#;

async fn upstream(_: Bytes) -> impl axum::response::IntoResponse {
    ([("content-type", "text/event-stream")], STREAM)
}

type Captured = Arc<Mutex<Vec<serde_json::Value>>>;

async fn capture(
    State(captured): State<Captured>,
    body: Bytes,
) -> impl axum::response::IntoResponse {
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) {
        captured.lock().unwrap().push(value);
    }
    ([("content-type", "text/event-stream")], STREAM)
}

/// An assistant turn whose call arguments never parsed — the stream was cut
/// mid-JSON, or the model named the function and filled nothing — is replayed on
/// every later round. Sent as-is, the upstream refuses the whole request with
/// `function.arguments must be valid JSON` and the turn cannot continue, so the
/// wire copy is repaired while the stored call keeps what the model wrote.
#[tokio::test]
async fn a_replayed_call_with_broken_arguments_is_repaired() {
    let captured: Captured = Arc::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(capture))
        .with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let mut emitted = String::new();
    let mut emit = |delta: &str| {
        emitted.push_str(delta);
        Ok(())
    };
    openai::Adapter::new()
        .stream(
            &Request {
                model: "m".to_string(),
                messages: vec![Message {
                    role: "assistant".to_string(),
                    tool_calls: vec![
                        ToolCall {
                            id: "call-empty".to_string(),
                            name: "read".to_string(),
                            arguments: String::new(),
                        },
                        ToolCall {
                            id: "call-cut".to_string(),
                            name: "write_file".to_string(),
                            arguments: r#"{"path": "/tmp/x""#.to_string(),
                        },
                        ToolCall {
                            id: "call-ok".to_string(),
                            name: "read".to_string(),
                            arguments: r#"{"path": "/tmp/y"}"#.to_string(),
                        },
                    ],
                    ..Message::default()
                }],
                ..Request::default()
            },
            &Provider {
                name: "openai-compat".to_string(),
                endpoint: format!("http://{address}"),
                keys: vec!["k".to_string()],
                ..Provider::default()
            },
            "k",
            &mut emit,
        )
        .await
        .expect("stream");

    let sent = captured.lock().unwrap();
    let calls = sent[0]["messages"][0]["tool_calls"].as_array().unwrap();
    assert_eq!(calls.len(), 3);
    assert_eq!(calls[0]["function"]["arguments"], "{}");
    assert_eq!(calls[1]["function"]["arguments"], "{}");
    assert_eq!(calls[2]["function"]["arguments"], r#"{"path": "/tmp/y"}"#);
}

/// The gateway spells absent delta fields as null, on the opening chunk and on
/// the finish chunk. A bare `String`/`Vec` would reject those chunks and lose
/// the text and the finish reason with them.
const STREAM_WITH_NULLS: &str = r#"data: {"id":"c2","model":"m","choices":[{"index":0,"finish_reason":null,"delta":{"role":"assistant","content":null,"reasoning":null,"tool_calls":null}}]}

data: {"id":"c2","model":"m","choices":[{"index":0,"finish_reason":null,"delta":{"role":"assistant","content":"All "}}]}

data: {"id":"c2","model":"m","choices":[{"index":0,"finish_reason":null,"delta":{"role":"assistant","content":"good"}}]}

data: {"id":"c2","model":"m","choices":[{"index":0,"finish_reason":"stop","delta":{"role":"assistant","content":null,"tool_calls":null}}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}

data: [DONE]

"#;

async fn upstream_with_nulls(_: Bytes) -> impl axum::response::IntoResponse {
    ([("content-type", "text/event-stream")], STREAM_WITH_NULLS)
}

#[tokio::test]
async fn null_delta_fields_do_not_drop_the_turn() {
    let app = Router::new().route("/v1/chat/completions", post(upstream_with_nulls));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let mut emitted = String::new();
    let mut emit = |delta: &str| {
        emitted.push_str(delta);
        Ok(())
    };
    let response = openai::Adapter::new()
        .stream(
            &Request {
                model: "m".to_string(),
                messages: vec![Message {
                    role: "user".to_string(),
                    content: "hi".to_string(),
                    ..Message::default()
                }],
                ..Request::default()
            },
            &Provider {
                name: "openai-compat".to_string(),
                endpoint: format!("http://{address}"),
                keys: vec!["k".to_string()],
                ..Provider::default()
            },
            "k",
            &mut emit,
        )
        .await
        .expect("stream");

    assert_eq!(response.content, "All good");
    assert_eq!(response.finish_reason, "stop");
    assert_eq!(response.usage.total_tokens, 3);
    assert_eq!(emitted, "All good");
}

#[tokio::test]
async fn streamed_tool_arguments_fold_into_one_call() {
    let app = Router::new().route("/v1/chat/completions", post(upstream));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let adapter = openai::Adapter::new();
    let request = Request {
        model: "m".to_string(),
        messages: vec![Message {
            role: "user".to_string(),
            content: "write it".to_string(),
            ..Message::default()
        }],
        ..Request::default()
    };

    let mut emitted = String::new();
    let mut emit = |delta: &str| {
        emitted.push_str(delta);
        Ok(())
    };
    let response = adapter
        .stream(
            &request,
            &Provider {
                name: "openai-compat".to_string(),
                endpoint: format!("http://{address}"),
                keys: vec!["k".to_string()],
                ..Provider::default()
            },
            "k",
            &mut emit,
        )
        .await
        .expect("stream");

    assert_eq!(response.tool_calls.len(), 1, "one call folded");
    assert_eq!(response.tool_calls[0].id, "call-1");
    assert_eq!(response.tool_calls[0].name, "write_file");
    assert_eq!(response.tool_calls[0].arguments, r#"{"path": "/tmp/x"}"#);
    assert_eq!(response.finish_reason, "tool_calls");
    assert!(emitted.is_empty(), "a tool-only turn emits no text");
}
