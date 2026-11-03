//! Port of the Go `io/gateway` tests.
//!
//! The Go tests live in the same package as the code they drive, so they reach
//! private helpers (`complete`, `persist`, `mergeHistory`, …) directly. These
//! are a `#[cfg(test)]` module for the same reason: a port that could only see
//! the public API would not be testing the same behaviour.
//!
//! The provider list is a process-wide global, so tests that install one hold
//! `config_lock()` for their whole body — across awaits on purpose.
//! `#[tokio::test]` runs on a current-thread runtime, so the guard is never
//! required to be `Send` and cannot deadlock a worker.

#![allow(clippy::await_holding_lock)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use aixodia_provider::{APIError, Message, Response, ToolCall, Usage};
use aixodia_session::{
    messages_from_turns, text_block, AgentPatch, Error as SessionError, NewSession, Session, Turn,
    TurnInput, UnfinishedTurn,
};
use aixodia_tools::tools_with;
use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use axum::response::IntoResponse;
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt as _;

use crate::chat::{
    append_delta, attach_tool_durations, has_unfinished_question, history_window,
    is_repeat_of_tail, merge_history, select_tools, to_internal_provider, ChatSink,
    CompletionOptions, NullSink, Route, SinkRef, TrailWriter,
};
use crate::gateway::{Backups, Gateway, SessionStore, StateStore, REQUEST_MAX_BODY};

// ---------------------------------------------------------------------------
// The store the Go tests drive the gateway against.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct MemStore {
    sessions: Mutex<HashMap<String, Session>>,
    turns: Mutex<HashMap<String, Vec<Turn>>>,
}

/// The saved settings, which live in D1's `system_state` in production.
#[derive(Default)]
struct MemState {
    values: Mutex<HashMap<String, String>>,
}

#[async_trait::async_trait]
impl StateStore for MemState {
    async fn get_state(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self.values.lock().unwrap().get(key).cloned())
    }

    async fn put_state(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.values
            .lock()
            .unwrap()
            .insert(key.to_string(), value.to_string());
        Ok(())
    }
}

impl MemStore {
    fn new() -> Self {
        Self::default()
    }

    fn stored_turns(&self, id: &str) -> Vec<Turn> {
        self.turns
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl SessionStore for MemStore {
    async fn create(&self, params: NewSession) -> Result<Session, SessionError> {
        let mut sessions = self.sessions.lock().unwrap();
        let id = if params.id.is_empty() {
            "s1".to_string()
        } else {
            params.id.clone()
        };
        if sessions.contains_key(&id) {
            return Err(SessionError::NotFound(format!(
                "session {id:?} already exists"
            )));
        }
        let created = Session {
            id: id.clone(),
            title: params.title,
            provider: params.provider,
            model: params.model,
            reasoning: params.reasoning,
            temperature: params.temperature,
            top_p: params.top_p,
            max_tokens: params.max_tokens,
            ..Session::default()
        };
        sessions.insert(id, created.clone());
        Ok(created)
    }

    async fn get(&self, id: &str) -> Result<Session, SessionError> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| SessionError::NotFound(id.to_string()))
    }

    async fn list(&self) -> Result<Vec<Session>, SessionError> {
        Ok(self.sessions.lock().unwrap().values().cloned().collect())
    }

    async fn delete(&self, id: &str) -> Result<(), SessionError> {
        self.sessions.lock().unwrap().remove(id);
        self.turns.lock().unwrap().remove(id);
        Ok(())
    }

    async fn save_config(&self, id: &str, saved: &Session) -> Result<(), SessionError> {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(target) = sessions.get_mut(id) else {
            return Err(SessionError::NotFound(id.to_string()));
        };
        target.provider = saved.provider.clone();
        target.model = saved.model.clone();
        if !saved.title.is_empty() {
            target.title = saved.title.clone();
        }
        Ok(())
    }

    async fn save_agent(&self, id: &str, patch: &AgentPatch) -> Result<(), SessionError> {
        let mut sessions = self.sessions.lock().unwrap();
        let Some(target) = sessions.get_mut(id) else {
            return Err(SessionError::NotFound(id.to_string()));
        };
        if let Some(provider) = &patch.provider {
            target.provider = provider.clone();
        }
        if let Some(model) = &patch.model {
            target.model = model.clone();
        }
        Ok(())
    }

    async fn turns(&self, id: &str) -> Result<Vec<Turn>, SessionError> {
        Ok(self.stored_turns(id))
    }

    async fn history(&self, id: &str) -> Result<Vec<Message>, SessionError> {
        Ok(messages_from_turns(&self.stored_turns(id)))
    }

    async fn append_turns(&self, id: &str, turns: &[TurnInput]) -> Result<Vec<Turn>, SessionError> {
        let mut all = self.turns.lock().unwrap();
        let list = all.entry(id.to_string()).or_default();
        let start = list.len();
        for turn in turns {
            let seq = list.len() as i64 + 1;
            list.push(Turn {
                session_id: id.to_string(),
                id: seq,
                seq,
                role: turn.role.clone(),
                text: turn.text.clone(),
                model: turn.model.clone(),
                input_tokens: turn.input_tokens,
                output_tokens: turn.output_tokens,
                cache_read_tokens: turn.cache_read_tokens,
                cache_write_tokens: turn.cache_write_tokens,
                reasoning_tokens: turn.reasoning_tokens,
                input_includes_cache: turn.input_includes_cache,
                duration_ms: turn.duration_ms,
                generation_ms: turn.generation_ms,
                reasoning_content: turn.reasoning_content.clone(),
                block: turn.block.clone(),
                ..Turn::default()
            });
        }
        Ok(list[start..].to_vec())
    }

    async fn unfinished(&self, _limit: i64) -> Result<Vec<UnfinishedTurn>, SessionError> {
        Ok(Vec::new())
    }
}

// ---------------------------------------------------------------------------
// The fake upstream, and the provider list the gateway routes through.
// ---------------------------------------------------------------------------

/// The provider list is a package global, so tests that install one must not
/// overlap. Go's tests set `config.Providers` directly and rely on `go test`
/// running them in one goroutine at a time per package; Rust runs them in
/// parallel threads, so they take a lock instead.
fn config_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Installs one provider pointing at `url`, the way `config.Providers = [...]`
/// does in the Go tests.
async fn install_provider(name: &str, url: &str, keys: &[&str], adapter: &str) {
    let kv = Arc::new(aixodia_db::MemoryKv::new());
    let store = Arc::new(aixodia_config::Store::new(kv));
    store
        .add(aixodia_config::Provider {
            id: String::new(),
            name: name.to_string(),
            api_url: url.to_string(),
            keys: keys.iter().map(|key| key.to_string()).collect(),
            adapter: adapter.to_string(),
            ..aixodia_config::Provider::default()
        })
        .await
        .expect("add provider");
    aixodia_config::set_store(Some(store));
    aixodia_config::load_config().await.expect("load config");
}

fn stream_fake(content: &str, tool_calls: &[&str], finish: &str) -> String {
    let mut out = String::new();
    if !content.is_empty() || !tool_calls.is_empty() {
        let mut delta = json!({"role": "assistant"});
        if !content.is_empty() {
            delta["content"] = Value::String(content.to_string());
        }
        if !tool_calls.is_empty() {
            let calls: Vec<Value> = tool_calls
                .iter()
                .map(|name| {
                    json!({
                        "id": "call_1", "type": "function", "index": 0,
                        "function": {"name": name, "arguments": "{}"},
                    })
                })
                .collect();
            delta["tool_calls"] = Value::Array(calls);
        }
        let chunk = json!({
            "id": "chatcmpl-fake",
            "choices": [{"index": 0, "delta": delta, "finish_reason": Value::Null}],
        });
        out.push_str(&format!("data: {chunk}\n\n"));
    }
    let final_chunk = json!({
        "id": "chatcmpl-fake",
        "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
    });
    out.push_str(&format!("data: {final_chunk}\n\n"));
    out.push_str("data: [DONE]\n\n");
    out
}

fn write_fake(content: &str, tool_calls: &[&str], finish: &str) -> String {
    let mut message = json!({"role": "assistant", "content": content});
    let mut finish = finish.to_string();
    if !tool_calls.is_empty() {
        let calls: Vec<Value> = tool_calls
            .iter()
            .enumerate()
            .map(|(index, name)| {
                json!({
                    "id": "call_1", "type": "function", "index": index,
                    "function": {"name": name, "arguments": "{}"},
                })
            })
            .collect();
        message["tool_calls"] = Value::Array(calls);
        finish = "tool_calls".to_string();
    }
    json!({
        "id": "chatcmpl-fake",
        "model": "test-model",
        "choices": [{"message": message, "finish_reason": finish}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 4, "total_tokens": 7},
    })
    .to_string()
}

/// The Go `fakeUpstream`: one route, a bearer check, and a two-round answer
/// (a tool call, then the answer) once the tool result comes back.
async fn fake_upstream() -> String {
    let app = Router::new().fallback(|request: Request<Body>| async move {
        if request.uri().path() != "/v1/chat/completions" {
            return (StatusCode::NOT_FOUND, "").into_response();
        }
        let auth = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        if auth != "Bearer test-key" {
            return (
                StatusCode::UNAUTHORIZED,
                r#"{"error":{"message":"bad key"}}"#,
            )
                .into_response();
        }
        let body = axum::body::to_bytes(request.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&body);
        let stream = text.contains("\"stream\":true");
        let answered = text.contains("\"tool_call_id\"");
        if stream {
            let sse = if answered {
                stream_fake("done", &[], "stop")
            } else {
                stream_fake("", &["current_time"], "tool_calls")
            };
            return (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "text/event-stream")],
                sse,
            )
                .into_response();
        }
        let json = if answered {
            write_fake("done", &[], "")
        } else {
            write_fake("", &["current_time"], "thinking")
        };
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// The Go `newTestGateway`: the mock provider plus a builtin toolset.
async fn new_test_gateway(store: Arc<MemStore>) -> (Arc<Gateway>, String) {
    let upstream = fake_upstream().await;
    install_provider("mock", &upstream, &["test-key"], "openai").await;
    let gateway = Arc::new(Gateway::new(
        store,
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    ));
    (gateway, upstream)
}

// ---------------------------------------------------------------------------
// Driving the router.
// ---------------------------------------------------------------------------

async fn send(router: &Router, method: Method, path: &str, body: Vec<u8>) -> (StatusCode, String) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .expect("request");
    let response = router.clone().oneshot(request).await.expect("router");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

async fn post_json(router: &Router, path: &str, body: &str) -> (StatusCode, String) {
    send(router, Method::POST, path, body.as_bytes().to_vec()).await
}

async fn get_json(router: &Router, path: &str) -> (StatusCode, String) {
    send(router, Method::GET, path, Vec::new()).await
}

fn body_json(body: &str) -> Value {
    serde_json::from_str(body).unwrap_or(Value::Null)
}

/// The SSE events of a streaming reply, in order, as parsed JSON (the `[DONE]`
/// sentinel is dropped, exactly as the Go test loops do).
fn sse_events(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|event| {
            let payload = event.strip_prefix("data: ")?;
            if payload == "[DONE]" {
                return None;
            }
            serde_json::from_str(payload).ok()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// gateway_test.go
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_completion_runs_a_tool_and_returns_the_answer() {
    let _guard = config_lock().lock().unwrap();
    let (gateway, _upstream) = new_test_gateway(Arc::new(MemStore::new())).await;

    let (status, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"model":"mock/test-model","messages":[{"role":"user","content":"what time is it"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body = {body}");

    let payload = body_json(&body);
    assert_eq!(payload["provider"], "mock");
    assert_eq!(payload["choices"][0]["message"]["content"], "done");
    assert_eq!(payload["usage"]["total_tokens"], 14, "two rounds of 7");
}

#[tokio::test]
async fn chat_completion_streams() {
    let _guard = config_lock().lock().unwrap();
    let (gateway, _upstream) = new_test_gateway(Arc::new(MemStore::new())).await;

    let (status, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"model":"mock/test-model","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert!(
        body.contains("chat.completion.chunk"),
        "missing chunk objects:\n{body}"
    );
    assert!(body.contains("data: [DONE]"), "missing [DONE]:\n{body}");
    assert!(
        body.matches("\n\n").count() >= 3,
        "expected several SSE events:\n{body}"
    );
}

/// The stream no longer waits for the store: it names the answer it has in
/// memory, says the rows are not synced yet, and pushes only rows that really
/// landed while it stayed open.
#[tokio::test]
async fn the_stream_pushes_only_rows_the_store_has() {
    let _guard = config_lock().lock().unwrap();
    let store = Arc::new(MemStore::new());
    let (gateway, _upstream) = new_test_gateway(store.clone()).await;
    post_json(
        &gateway.router(),
        "/v1/sessions",
        r#"{"title":"t","model":"mock/test-model"}"#,
    )
    .await;
    let (_, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"session_id":"s1","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    )
    .await;

    let mut pushed: Vec<Turn> = Vec::new();
    let mut answer = Turn::default();
    let mut synced = true;
    for event in sse_events(&body) {
        if let Some(turns) = event.get("turns").and_then(Value::as_array) {
            for turn in turns {
                pushed.push(serde_json::from_value(turn.clone()).expect("turn"));
            }
        }
        if event.get("turn").is_some() {
            let parsed: Turn = serde_json::from_value(event["turn"].clone()).expect("turn");
            answer = parsed;
            synced = event
                .get("turns_synced")
                .and_then(Value::as_bool)
                .unwrap_or(true);
        }
    }

    // The rows went to the chat's backup queue, so the turn closed without
    // waiting for them. Reading the queue is what the next turn of that chat
    // does before it asks the model anything.
    gateway.await_backups("s1").await;
    let stored = store.stored_turns("s1");
    assert!(stored.len() >= 4, "stored={stored:?}");
    // Every row the stream did name is a stored row, in the stored order: the
    // reader never sees a row the store does not have.
    assert!(
        pushed.len() <= stored.len(),
        "pushed={pushed:?} stored={stored:?}"
    );
    for (index, pushed_turn) in pushed.iter().enumerate() {
        assert_eq!(
            pushed_turn.seq, stored[index].seq,
            "row {index} mismatch: {pushed_turn:?} != {:?}",
            stored[index]
        );
        assert_eq!(pushed_turn.text, stored[index].text, "row {index} text");
    }
    let last = stored.last().expect("stored row");
    assert_eq!(answer.text, last.text, "final answer text");
    assert_eq!(answer.model, last.model, "final answer model");
    assert_eq!(answer.seq, 0, "the closing row names no seq yet");
    assert!(!synced, "an unwritten row must not claim it is synced");
}

/// A store whose writes answer only when the test lets them through — the shape
/// of a D1 call that is taking its time. The turn must answer anyway: what it
/// owes the store is a backup, and the reader is not waiting for it.
struct SlowStore {
    inner: MemStore,
    gate: tokio::sync::Mutex<()>,
}

impl SlowStore {
    fn new() -> Self {
        Self {
            inner: MemStore::new(),
            gate: tokio::sync::Mutex::new(()),
        }
    }

    fn stored(&self, id: &str) -> Vec<Turn> {
        self.inner.stored_turns(id)
    }
}

#[async_trait::async_trait]
impl SessionStore for SlowStore {
    async fn create(&self, params: NewSession) -> Result<Session, SessionError> {
        self.inner.create(params).await
    }
    async fn get(&self, id: &str) -> Result<Session, SessionError> {
        self.inner.get(id).await
    }
    async fn list(&self) -> Result<Vec<Session>, SessionError> {
        self.inner.list().await
    }
    async fn delete(&self, id: &str) -> Result<(), SessionError> {
        self.inner.delete(id).await
    }
    async fn save_config(&self, id: &str, saved: &Session) -> Result<(), SessionError> {
        self.inner.save_config(id, saved).await
    }
    async fn save_agent(&self, id: &str, patch: &AgentPatch) -> Result<(), SessionError> {
        self.inner.save_agent(id, patch).await
    }
    async fn turns(&self, id: &str) -> Result<Vec<Turn>, SessionError> {
        self.inner.turns(id).await
    }
    async fn history(&self, id: &str) -> Result<Vec<Message>, SessionError> {
        self.inner.history(id).await
    }
    async fn unfinished(&self, limit: i64) -> Result<Vec<UnfinishedTurn>, SessionError> {
        self.inner.unfinished(limit).await
    }

    async fn append_turns(&self, id: &str, turns: &[TurnInput]) -> Result<Vec<Turn>, SessionError> {
        let _parked = self.gate.lock().await;
        self.inner.append_turns(id, turns).await
    }

    async fn append_turns_partial(
        &self,
        id: &str,
        turns: &[TurnInput],
    ) -> aixodia_session::Appended {
        let _parked = self.gate.lock().await;
        self.inner.append_turns_partial(id, turns).await
    }
}

/// The turn answers with its rows still owed, and the rows then land in the
/// order the turn made them: the question, this round's tool steps, the answer.
#[tokio::test]
async fn a_store_that_takes_its_time_does_not_hold_the_answer() {
    let _guard = config_lock().lock().unwrap();
    let upstream = fake_upstream().await;
    install_provider("mock", &upstream, &["test-key"], "openai").await;
    let store = Arc::new(SlowStore::new());
    let gateway = Arc::new(Gateway::new(
        store.clone(),
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    ));
    post_json(
        &gateway.router(),
        "/v1/sessions",
        r#"{"title":"t","model":"mock/test-model"}"#,
    )
    .await;
    let parked = store.gate.lock().await;

    let (status, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"session_id":"s1","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert!(
        body.contains("data: [DONE]"),
        "the stream closed while the store was parked:\n{body}"
    );
    assert!(
        body.contains(r#""turns_synced":false"#),
        "the closing frame claimed a write that had not landed: {body}"
    );
    assert!(
        store.stored("s1").is_empty(),
        "rows landed through a parked store: {:?}",
        store.stored("s1")
    );
    assert!(
        gateway.owes_backups(),
        "an update would have replaced the process mid-write"
    );

    drop(parked);
    gateway.await_backups("s1").await;
    assert!(
        !gateway.owes_backups(),
        "the write was still owed after its queue drained"
    );
    let stored = store.stored("s1");
    assert!(stored.len() >= 4, "stored={stored:?}");
    assert_eq!(stored[0].role, "user", "the question is stored first");
    assert_eq!(
        stored.last().expect("row").role,
        "assistant",
        "the answer closes the turn"
    );
    for (index, row) in stored.iter().enumerate() {
        assert_eq!(row.seq, index as i64 + 1, "row {index}: {row:?}");
    }
}

/// An exit drains every chat's queue, so a write a turn left behind is not lost
/// with the process that owed it.
#[tokio::test]
async fn an_exit_waits_for_every_owed_write() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let backups = Backups::default();
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    let landed = Arc::new(AtomicUsize::new(0));
    for chat in ["a", "b"] {
        let gate = gate.clone();
        let landed = landed.clone();
        backups.queue(chat, async move {
            let _parked = gate.lock().await;
            landed.fetch_add(1, Ordering::SeqCst);
        });
    }
    let parked = gate.lock().await;
    let draining = {
        let queue = backups.clone();
        let landed = landed.clone();
        tokio::spawn(async move {
            queue.drain_all().await;
            landed.load(Ordering::SeqCst)
        })
    };
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        landed.load(Ordering::SeqCst),
        0,
        "a write landed through a parked store"
    );
    drop(parked);
    assert_eq!(
        draining.await.unwrap(),
        2,
        "the exit left an owed write behind"
    );
}

#[tokio::test]
async fn the_stream_pushes_cache_usage_and_actual_reasoning() {
    let _guard = config_lock().lock().unwrap();
    let upstream = Router::new().fallback(|| async {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"real thought\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}],",
            "\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":5,\"total_tokens\":105,",
            "\"prompt_tokens_details\":{\"cached_tokens\":80},",
            "\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\n"
        );
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/event-stream")],
            body,
        )
            .into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, upstream).await;
    });
    install_provider("mock", &format!("http://{addr}"), &["key"], "openai").await;

    let store = Arc::new(MemStore::new());
    let gateway = Arc::new(Gateway::new(
        store.clone(),
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    ));
    post_json(
        &gateway.router(),
        "/v1/sessions",
        r#"{"model":"mock/test-model"}"#,
    )
    .await;
    let (_, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"session_id":"s1","messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    )
    .await;
    assert!(
        body.contains(r#""stage":"reasoning""#)
            && body.contains(r#""cache_read_tokens":80"#)
            && body.contains(r#""turns_synced":false"#),
        "missing live fields: {body}"
    );

    // The rows are a backup written behind the stream, so the reader asks for
    // them here the way the app's recovery read does.
    gateway.await_backups("s1").await;
    let turns = store.stored_turns("s1");
    let last = turns.last().expect("stored row");
    assert_eq!(last.reasoning_content, "real thought");
    assert_eq!(last.cache_read_tokens, 80);
    assert_eq!(last.reasoning_tokens, 2);
    assert!(last.input_includes_cache);
}

/// The frame the phone's cost strip climbs on: a `stage="usage"` event after
/// every provider round, carrying the turn's running total (not the round's
/// own), so a missed frame self-corrects.
#[tokio::test]
async fn the_stream_reports_running_usage_each_round() {
    let _guard = config_lock().lock().unwrap();
    let upstream = Router::new().fallback(|request: Request<Body>| async move {
        let body = axum::body::to_bytes(request.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&body);
        let sse = if text.contains("\"tool_call_id\"") {
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}],",
                "\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":6,\"total_tokens\":126,",
                "\"prompt_tokens_details\":{\"cached_tokens\":100},",
                "\"completion_tokens_details\":{\"reasoning_tokens\":1}}}\n\n",
                "data: [DONE]\n\n"
            )
            .to_string()
        } else {
            concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"id\":\"call_1\",\"type\":\"function\",\"index\":0,",
                "\"function\":{\"name\":\"current_time\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}],",
                "\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":5,\"total_tokens\":105,",
                "\"prompt_tokens_details\":{\"cached_tokens\":80},",
                "\"completion_tokens_details\":{\"reasoning_tokens\":2}}}\n\n",
                "data: [DONE]\n\n"
            )
            .to_string()
        };
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/event-stream")],
            sse,
        )
            .into_response()
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, upstream).await;
    });
    install_provider("mock", &format!("http://{addr}"), &["test-key"], "openai").await;

    let store = Arc::new(MemStore::new());
    let gateway = Arc::new(Gateway::new(
        store,
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    ));
    post_json(
        &gateway.router(),
        "/v1/sessions",
        r#"{"title":"t","model":"mock/test-model"}"#,
    )
    .await;
    let (_, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"session_id":"s1","model":"mock/test-model","messages":[{"role":"user","content":"กี่โมง"}],"stream":true}"#,
    )
    .await;

    let usages: Vec<Value> = sse_events(&body)
        .into_iter()
        .filter_map(|event| event.get("tool_event").cloned())
        .filter(|event| event["stage"] == "usage")
        .collect();
    assert_eq!(
        usages.len(),
        2,
        "usage frames = {}, want 2 (one per round):\n{body}",
        usages.len()
    );
    let last = usages.last().expect("usage frame");
    assert_eq!(last["prompt_tokens"], 220, "running total: {last}");
    assert_eq!(last["completion_tokens"], 11);
    assert_eq!(last["cache_read_tokens"], 180);
    assert_eq!(last["input_includes_cache"], true);
    assert!(
        last.get("block").is_none(),
        "usage frame must carry no thread block: {last}"
    );
}

#[tokio::test]
async fn retry_stream_includes_the_recorded_user_without_reading_history_twice() {
    let _guard = config_lock().lock().unwrap();
    let store = Arc::new(MemStore::new());
    let (gateway, _upstream) = new_test_gateway(store.clone()).await;
    post_json(
        &gateway.router(),
        "/v1/sessions",
        r#"{"model":"mock/test-model"}"#,
    )
    .await;
    store
        .append_turns("s1", &[TurnInput::text("user", "hi")])
        .await
        .expect("append");

    let (_, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"session_id":"s1","append":true,"messages":[{"role":"user","content":"hi"}],"stream":true}"#,
    )
    .await;
    assert!(
        body.contains(r#""role":"user""#) && body.contains(r#""seq":1"#),
        "retry did not push the existing user: {body}"
    );
    let stored = store.stored_turns("s1");
    let users = stored.iter().filter(|turn| turn.role == "user").count();
    assert_eq!(users, 1, "retry duplicated the user: {stored:?}");
}

#[tokio::test]
async fn a_retry_resumes_the_stored_turn_without_appending_a_user() {
    let _guard = config_lock().lock().unwrap();
    let store = Arc::new(MemStore::new());
    let (gateway, _upstream) = new_test_gateway(store.clone()).await;
    post_json(
        &gateway.router(),
        "/v1/sessions",
        r#"{"model":"mock/test-model"}"#,
    )
    .await;
    store
        .append_turns("s1", &[TurnInput::text("user", "hi")])
        .await
        .expect("append");

    let (status, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"session_id":"s1","retry":true,"stream":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body = {body}");

    let turns = store.stored_turns("s1");
    let users: Vec<&Turn> = turns.iter().filter(|turn| turn.role == "user").collect();
    assert_eq!(
        users.len(),
        1,
        "retry appended a second user row: {turns:?}"
    );
    assert_eq!(users[0].text, "hi", "retry rewrote the question");
    let last = turns.last().expect("stored row");
    assert_eq!(last.role, "assistant");
    assert_eq!(last.text, "done");
}

#[test]
fn an_unfinished_question_survives_an_interrupted_tool_loop() {
    let mut turns = vec![
        Turn {
            role: "user".to_string(),
            text: "hi".to_string(),
            ..Turn::default()
        },
        Turn {
            role: "assistant".to_string(),
            text: "checking".to_string(),
            block: text_block("n1", "checking").json(),
            ..Turn::default()
        },
        Turn {
            role: "tool_call".to_string(),
            text: r#"{"id":"c1","name":"read","arguments":"{}"}"#.to_string(),
            ..Turn::default()
        },
        Turn {
            role: "tool_result".to_string(),
            text: r#"{"tool_call_id":"c1","name":"read","content":"ok"}"#.to_string(),
            ..Turn::default()
        },
    ];
    assert!(
        has_unfinished_question(&turns),
        "narration before a tool was mistaken for the answer: {turns:?}"
    );
    turns.push(Turn {
        role: "assistant".to_string(),
        text: "done".to_string(),
        block: text_block("answer", "done").json(),
        ..Turn::default()
    });
    assert!(
        !has_unfinished_question(&turns),
        "a finished turn still looks retryable: {turns:?}"
    );
}

#[tokio::test]
async fn a_retry_rejects_a_finished_turn() {
    let _guard = config_lock().lock().unwrap();
    let store = Arc::new(MemStore::new());
    let (gateway, _upstream) = new_test_gateway(store.clone()).await;
    post_json(
        &gateway.router(),
        "/v1/sessions",
        r#"{"model":"mock/test-model"}"#,
    )
    .await;
    store
        .append_turns(
            "s1",
            &[
                TurnInput::text("user", "hi"),
                TurnInput {
                    block: text_block("answer", "done").json(),
                    ..TurnInput::text("assistant", "done")
                },
            ],
        )
        .await
        .expect("append");

    let (status, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"session_id":"s1","retry":true,"stream":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body = {body}");
    assert_eq!(
        store.stored_turns("s1").len(),
        2,
        "retry changed a finished turn"
    );
}

/// A store whose first multi-row append writes only the first row and fails, the
/// way a D1 statement that hits the parameter cap does.
struct PartialStore {
    inner: MemStore,
    failed: Mutex<bool>,
}

#[async_trait::async_trait]
impl SessionStore for PartialStore {
    async fn create(&self, params: NewSession) -> Result<Session, SessionError> {
        self.inner.create(params).await
    }
    async fn get(&self, id: &str) -> Result<Session, SessionError> {
        self.inner.get(id).await
    }
    async fn list(&self) -> Result<Vec<Session>, SessionError> {
        self.inner.list().await
    }
    async fn delete(&self, id: &str) -> Result<(), SessionError> {
        self.inner.delete(id).await
    }
    async fn save_config(&self, id: &str, saved: &Session) -> Result<(), SessionError> {
        self.inner.save_config(id, saved).await
    }
    async fn save_agent(&self, id: &str, patch: &AgentPatch) -> Result<(), SessionError> {
        self.inner.save_agent(id, patch).await
    }
    async fn turns(&self, id: &str) -> Result<Vec<Turn>, SessionError> {
        self.inner.turns(id).await
    }
    async fn history(&self, id: &str) -> Result<Vec<Message>, SessionError> {
        self.inner.history(id).await
    }
    async fn unfinished(&self, limit: i64) -> Result<Vec<UnfinishedTurn>, SessionError> {
        self.inner.unfinished(limit).await
    }

    async fn append_turns(&self, id: &str, turns: &[TurnInput]) -> Result<Vec<Turn>, SessionError> {
        let appended = self.append_turns_partial(id, turns).await;
        match appended.error {
            Some(err) => Err(err),
            None => Ok(appended.rows),
        }
    }

    async fn append_turns_partial(
        &self,
        id: &str,
        turns: &[TurnInput],
    ) -> aixodia_session::Appended {
        let partial = {
            let mut failed = self.failed.lock().unwrap();
            if !*failed && turns.len() > 1 {
                *failed = true;
                true
            } else {
                false
            }
        };
        if partial {
            // The first row lands and the later chunk fails, exactly as a D1
            // statement that hits the parameter cap does.
            return match self.inner.append_turns(id, &turns[..1]).await {
                Ok(rows) => aixodia_session::Appended {
                    rows,
                    error: Some(SessionError::NotFound("later chunk failed".to_string())),
                },
                Err(err) => aixodia_session::Appended {
                    rows: Vec::new(),
                    error: Some(err),
                },
            };
        }
        match self.inner.append_turns(id, turns).await {
            Ok(rows) => aixodia_session::Appended { rows, error: None },
            Err(err) => aixodia_session::Appended {
                rows: Vec::new(),
                error: Some(err),
            },
        }
    }
}

#[tokio::test]
async fn a_partial_tool_write_retries_only_the_unwritten_suffix() {
    let store = Arc::new(PartialStore {
        inner: MemStore::new(),
        failed: Mutex::new(false),
    });
    let gateway = Gateway::new(
        store.clone(),
        Arc::new(aixodia_provider::Registry::new()),
        HashMap::new(),
    );
    let session = Session {
        id: "s1".to_string(),
        ..Session::default()
    };
    let mut writer = TrailWriter::new(gateway.sessions(), &session);

    let trail = vec![
        Message {
            role: "assistant".to_string(),
            content: "narration".to_string(),
            tool_calls: vec![
                ToolCall {
                    id: "c1".to_string(),
                    name: "read".to_string(),
                    arguments: "{}".to_string(),
                },
                ToolCall {
                    id: "c2".to_string(),
                    name: "read".to_string(),
                    arguments: "{}".to_string(),
                },
            ],
            ..Message::default()
        },
        Message {
            role: "tool".to_string(),
            tool_call_id: "c1".to_string(),
            tool_name: "read".to_string(),
            content: "first".to_string(),
            ..Message::default()
        },
        Message {
            role: "tool".to_string(),
            tool_call_id: "c2".to_string(),
            tool_name: "read".to_string(),
            content: "second".to_string(),
            ..Message::default()
        },
    ];
    let sink: SinkRef = Arc::new(Mutex::new(Box::new(NullSink)));
    writer.flush(&trail, &sink).await;
    assert_eq!(
        writer.pending.as_ref().map(Vec::len),
        Some(4),
        "pending rows = {:?}, want 4",
        writer.pending.as_ref().map(Vec::len)
    );

    let response = Response {
        content: "answer".to_string(),
        trail: trail.clone(),
        ..Response::default()
    };
    gateway
        .persist(
            Some(&session),
            &[],
            &response,
            Some(&mut writer),
            &sink,
            100,
        )
        .await;

    let rows = store.inner.stored_turns("s1");
    assert_eq!(
        rows.len(),
        6,
        "partial retry duplicated or lost rows: {rows:?}"
    );
    assert_eq!(rows[0].text, "narration");
    assert_eq!(rows[5].text, "answer");
}

#[tokio::test]
async fn chat_rejects_an_unknown_model() {
    let _guard = config_lock().lock().unwrap();
    let (gateway, _upstream) = new_test_gateway(Arc::new(MemStore::new())).await;

    let (status, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"model":"nope","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.contains("prefix it with a provider"), "body = {body}");
}

/// The phone opens a new chat locally and only asks for the server row when the
/// first message is sent. Repeating the create — a lost HTTP response, a
/// reconnect — must return the same row rather than make a second chat.
#[tokio::test]
async fn creating_a_session_keeps_the_client_draft_id_and_is_idempotent() {
    let _guard = config_lock().lock().unwrap();
    let store = Arc::new(MemStore::new());
    let (gateway, _upstream) = new_test_gateway(store.clone()).await;

    let (first_status, first_body) = post_json(
        &gateway.router(),
        "/v1/sessions",
        r#"{"id":"s-draft-42","title":"New chat"}"#,
    )
    .await;
    assert_eq!(first_status, StatusCode::CREATED, "body = {first_body}");
    let created = body_json(&first_body);
    assert_eq!(created["id"], "s-draft-42");

    let (second_status, second_body) = post_json(
        &gateway.router(),
        "/v1/sessions",
        r#"{"id":"s-draft-42","title":"Different"}"#,
    )
    .await;
    assert_eq!(second_status, StatusCode::OK, "body = {second_body}");
    let again = body_json(&second_body);
    assert_eq!(again["id"], created["id"]);
    assert_eq!(
        again["title"], "New chat",
        "the first row must be unchanged"
    );
    assert_eq!(store.sessions.lock().unwrap().len(), 1);

    let (bad_status, _) = post_json(
        &gateway.router(),
        "/v1/sessions",
        r#"{"id":"../outside","title":"bad"}"#,
    )
    .await;
    assert_eq!(bad_status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn chatting_in_a_session_persists_the_turns() {
    let _guard = config_lock().lock().unwrap();
    let store = Arc::new(MemStore::new());
    let (gateway, _upstream) = new_test_gateway(store.clone()).await;

    let (created_status, created_body) = post_json(
        &gateway.router(),
        "/v1/sessions",
        r#"{"title":"t","model":"mock/test-model"}"#,
    )
    .await;
    assert_eq!(created_status, StatusCode::CREATED, "body = {created_body}");

    let (status, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"session_id":"s1","messages":[{"role":"user","content":"hello"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body = {body}");

    let turns = store.stored_turns("s1");
    assert_eq!(
        turns.len(),
        4,
        "want user, tool_call, tool_result, assistant: {turns:?}"
    );
    assert_eq!(turns[0].role, "user");
    assert_eq!(turns[0].text, "hello");
    assert_eq!(turns[1].role, "tool_call");
    assert!(turns[1].text.contains("current_time"));
    assert_eq!(turns[2].role, "tool_result");
    assert_eq!(turns[3].role, "assistant");
    assert_eq!(turns[3].text, "done");
    assert_eq!(
        store.sessions.lock().unwrap().get("s1").unwrap().model,
        "test-model"
    );
}

/// The default offer carries "shell", not "bash": the real OpenCode client (v2)
/// sends "shell" — v1's "bash" was renamed — and it is the name the free-tier
/// gate recognises.
#[test]
fn the_default_toolset_offers_shell_not_bash() {
    let gateway = Gateway::new(
        Arc::new(MemStore::new()),
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    );
    let defs = select_tools(&gateway, None).expect("tools");
    let names: Vec<String> = defs.iter().map(|def| def.name.clone()).collect();
    assert!(names.iter().any(|name| name == "shell"), "{names:?}");
    assert!(!names.iter().any(|name| name == "bash"), "{names:?}");
}

/// A tool's duration has to reach the stored tool_result row: the phone shows it
/// while the step runs, but the stored thread replaces the live one when the
/// turn closes.
#[test]
fn attaching_tool_durations_stamps_the_result_rows() {
    let mut rows = vec![
        TurnInput::text(
            "tool_call",
            r#"{"id":"c1","name":"shell","arguments":"{}"}"#,
        ),
        TurnInput::text(
            "tool_result",
            r#"{"tool_call_id":"c1","name":"shell","content":"ok"}"#,
        ),
        TurnInput::text(
            "tool_result",
            r#"{"tool_call_id":"c2","name":"shell","content":"ok"}"#,
        ),
    ];
    let mut durations = HashMap::new();
    durations.insert("c1".to_string(), 2500i64);
    attach_tool_durations(&mut rows, &Arc::new(Mutex::new(durations)));
    assert_eq!(rows[1].duration_ms, 2500);
    assert_eq!(rows[2].duration_ms, 0, "a call with no recorded time");
}

#[test]
fn merging_history_appends_only_new_messages() {
    let stored = vec![
        Message {
            role: "user".to_string(),
            content: "one".to_string(),
            ..Message::default()
        },
        Message {
            role: "assistant".to_string(),
            content: "two".to_string(),
            ..Message::default()
        },
    ];
    let incoming = vec![
        Message {
            role: "user".to_string(),
            content: "one".to_string(),
            ..Message::default()
        },
        Message {
            role: "assistant".to_string(),
            content: "two".to_string(),
            ..Message::default()
        },
        Message {
            role: "user".to_string(),
            content: "three".to_string(),
            ..Message::default()
        },
    ];

    let (full, delta) = merge_history(&stored, &incoming);
    assert_eq!(full.len(), 3, "full = {full:?}");
    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0].content, "three");
}

#[tokio::test]
async fn tool_steps_replay_on_the_next_turn() {
    let _guard = config_lock().lock().unwrap();
    let store = Arc::new(MemStore::new());
    let (gateway, _upstream) = new_test_gateway(store.clone()).await;
    post_json(
        &gateway.router(),
        "/v1/sessions",
        r#"{"title":"t","model":"mock/test-model"}"#,
    )
    .await;

    post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"session_id":"s1","messages":[{"role":"user","content":"hello"}]}"#,
    )
    .await;
    // The client only knows visible text, so it sends the answer back without
    // the tool steps.
    let (status, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"session_id":"s1","messages":[{"role":"user","content":"hello"},{"role":"assistant","content":"done"},{"role":"user","content":"again"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body = {body}");

    let turns = store.stored_turns("s1");
    assert_eq!(
        turns.len(),
        6,
        "the second turn adds only user + tool steps + assistant: {turns:?}"
    );

    let history = store.history("s1").await.expect("history");
    let saw_tool_call = history.iter().any(|message| !message.tool_calls.is_empty());
    let saw_tool_result = history
        .iter()
        .any(|message| message.role == "tool" && message.tool_call_id == "call_1");
    assert!(
        saw_tool_call && saw_tool_result,
        "history lost tool steps: {history:?}"
    );
}

#[test]
fn merging_history_keeps_the_tool_steps_between_visible_messages() {
    let stored = vec![
        Message {
            role: "user".to_string(),
            content: "q".to_string(),
            ..Message::default()
        },
        Message {
            role: "assistant".to_string(),
            tool_calls: vec![ToolCall {
                id: "c".to_string(),
                name: "current_time".to_string(),
                arguments: "{}".to_string(),
            }],
            ..Message::default()
        },
        Message {
            role: "tool".to_string(),
            tool_call_id: "c".to_string(),
            content: "10:00".to_string(),
            ..Message::default()
        },
        Message {
            role: "assistant".to_string(),
            content: "it is 10:00".to_string(),
            ..Message::default()
        },
    ];
    let incoming = vec![
        Message {
            role: "user".to_string(),
            content: "q".to_string(),
            ..Message::default()
        },
        Message {
            role: "assistant".to_string(),
            content: "it is 10:00".to_string(),
            ..Message::default()
        },
        Message {
            role: "user".to_string(),
            content: "thanks".to_string(),
            ..Message::default()
        },
    ];

    let (full, delta) = merge_history(&stored, &incoming);
    assert_eq!(full.len(), 5, "stored steps kept: {full:?}");
    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0].content, "thanks");
}

#[test]
fn the_history_window_anchors_on_the_stored_prompt() {
    let turns = vec![
        Turn {
            role: "user".to_string(),
            text: "q1".to_string(),
            ..Turn::default()
        },
        Turn {
            role: "assistant".to_string(),
            text: "a1".to_string(),
            input_tokens: 40,
            ..Turn::default()
        },
    ];
    let saved = messages_from_turns(&turns);
    let mut history = saved.clone();
    history.push(Message {
        role: "user".to_string(),
        content: "q2".to_string(),
        ..Message::default()
    });

    let window = history_window(&turns, &saved, &history);
    assert_eq!(window.usage, 40, "window = {window:?}");
    assert_eq!(window.messages, 1);
    assert_eq!(window.max, aixodia_session::DEFAULT_MAX_TOKENS);

    // A client that rewrote the history the count was taken for cannot anchor
    // on it.
    let rewritten = vec![Message {
        role: "user".to_string(),
        content: "something else".to_string(),
        ..Message::default()
    }];
    let got = history_window(&turns, &saved, &rewritten);
    assert_eq!(got.messages, 0, "rewritten window = {got:?}");
    assert_eq!(got.usage, 0);

    // A count that reaches past what is being sent describes something else.
    let got = history_window(&turns, &saved, &[]);
    assert_eq!(got.messages, 0, "short window = {got:?}");
    assert_eq!(got.usage, 0);
}

#[test]
fn the_circuit_breaker_pauses_then_recovers() {
    let gateway = Gateway::new(
        Arc::new(MemStore::new()),
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    );
    let now = Instant::now();
    let outage = APIError::new(503, "down").into_anyhow();

    for _ in 0..crate::breaker::BREAKER_THRESHOLD {
        gateway.record_outcome("mock", Some(&outage), now);
    }
    assert!(gateway.circuit_open("mock", now + Duration::from_secs(1)));
    assert!(!gateway.circuit_open(
        "mock",
        now + crate::breaker::BREAKER_COOLDOWN + Duration::from_secs(1)
    ));

    gateway.record_outcome("mock", Some(&outage), now);
    gateway.record_outcome("mock", None, now);
    gateway.record_outcome("mock", Some(&outage), now);
    gateway.record_outcome("mock", Some(&outage), now);
    assert!(
        !gateway.circuit_open("mock", now),
        "a success should reset the failure count"
    );
}

#[test]
fn the_circuit_breaker_ignores_client_errors() {
    let gateway = Gateway::new(
        Arc::new(MemStore::new()),
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    );
    let now = Instant::now();
    let bad_key = APIError::new(400, "bad request").into_anyhow();
    for _ in 0..(crate::breaker::BREAKER_THRESHOLD + 2) {
        gateway.record_outcome("mock", Some(&bad_key), now);
    }
    assert!(
        !gateway.circuit_open("mock", now),
        "400 responses must not pause the provider"
    );
}

#[test]
fn merging_history_with_no_common_prefix_keeps_only_the_incoming() {
    let stored = vec![
        Message {
            role: "user".to_string(),
            content: "old question".to_string(),
            ..Message::default()
        },
        Message {
            role: "assistant".to_string(),
            tool_calls: vec![ToolCall {
                id: "c".to_string(),
                name: "current_time".to_string(),
                arguments: "{}".to_string(),
            }],
            ..Message::default()
        },
        Message {
            role: "tool".to_string(),
            tool_call_id: "c".to_string(),
            content: "10:00".to_string(),
            ..Message::default()
        },
        Message {
            role: "assistant".to_string(),
            content: "it is 10:00".to_string(),
            ..Message::default()
        },
    ];
    let incoming = vec![Message {
        role: "user".to_string(),
        content: "a different first message".to_string(),
        ..Message::default()
    }];

    let (full, delta) = merge_history(&stored, &incoming);
    assert_eq!(full.len(), 1, "full = {full:?}");
    assert_eq!(full[0].content, "a different first message");
    assert_eq!(delta.len(), 1);
}

// --- part 2 -----------------------------------------------------------------

/// A watcher that records the live tool activity, the way the Go test's
/// `onToolCall`/`onToolResult` callbacks do.
#[derive(Default)]
struct RecordingSink {
    calls: Arc<Mutex<Vec<String>>>,
    results: Arc<Mutex<Vec<String>>>,
}

impl ChatSink for RecordingSink {
    fn on_tool_call(&mut self, call: &ToolCall) {
        self.calls.lock().unwrap().push(call.name.clone());
    }

    fn on_tool_result(&mut self, call: &ToolCall, _text: &str, _duration_ms: i64, _is_error: bool) {
        self.results.lock().unwrap().push(call.name.clone());
    }
}

fn options(max_tokens: i64, tools: Vec<aixodia_provider::ToolDef>) -> CompletionOptions {
    CompletionOptions {
        max_tokens,
        temperature: None,
        top_p: None,
        tools,
        stream: false,
        user: String::new(),
        session_id: String::new(),
        window: aixodia_session::Window::new(0),
        cancel: tokio_util::sync::CancellationToken::new(),
        reused_user: None,
    }
}

fn builtin_def(name: &str) -> aixodia_provider::ToolDef {
    tools_with(None)
        .get(name)
        .expect("builtin tool")
        .def
        .clone()
}

/// Spawns a one-route upstream and returns its URL.
async fn spawn_upstream(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}")
}

async fn route_to(upstream: &str) -> Route {
    install_provider("mock", upstream, &["test-key"], "openai").await;
    let cfg = aixodia_config::get_provider_by_name("mock").expect("provider");
    let registry = aixodia_provider::Registry::new();
    let adapter = registry.require(&cfg.adapter).expect("adapter");
    Route {
        adapter,
        prov: to_internal_provider(&cfg),
        model: "test-model".to_string(),
    }
}

/// When the model keeps asking for tools past the round limit, it must still
/// answer: the gateway asks once more without tools.
#[tokio::test]
async fn the_tool_round_limit_still_gives_an_answer() {
    let _guard = config_lock().lock().unwrap();
    let upstream = spawn_upstream(Router::new().fallback(|request: Request<Body>| async move {
        let body = axum::body::to_bytes(request.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&body);
        let json = if text.contains("\"tools\"") {
            write_fake("", &["current_time"], "tool_calls")
        } else {
            write_fake("here is the answer", &[], "stop")
        };
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response()
    }))
    .await;
    install_provider("mock", &upstream, &["test-key"], "openai").await;
    let gateway = Arc::new(Gateway::new(
        Arc::new(MemStore::new()),
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    ));

    let (status, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"model":"mock/test-model","messages":[{"role":"user","content":"what time"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert!(
        body.contains("here is the answer"),
        "reply had no answer after the tool limit: {body}"
    );
}

#[tokio::test]
async fn rejects_an_invalid_utf8_body() {
    let _guard = config_lock().lock().unwrap();
    let (gateway, _upstream) = new_test_gateway(Arc::new(MemStore::new())).await;
    let mut bad = br#"{"model":"mock/test-model","messages":[{"role":"user","content":""#.to_vec();
    bad.extend_from_slice(&[0xe0, 0xb8]); // a truncated Thai character
    bad.extend_from_slice(br#""}]}"#);

    let (status, _) = send(&gateway.router(), Method::POST, "/v1/chat/completions", bad).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn rejects_an_oversized_request_body() {
    let _guard = config_lock().lock().unwrap();
    let (gateway, _upstream) = new_test_gateway(Arc::new(MemStore::new())).await;
    let body = vec![b' '; REQUEST_MAX_BODY + 1];
    let (status, reply) = send(
        &gateway.router(),
        Method::POST,
        "/v1/chat/completions",
        body,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        reply.contains("exceeds the"),
        "body = {reply}, want the request-size error"
    );
}

#[test]
fn a_repeated_tail_is_recognised() {
    let user = |text: &str| Message {
        role: "user".to_string(),
        content: text.to_string(),
        ..Message::default()
    };
    let assistant = |text: &str| Message {
        role: "assistant".to_string(),
        content: text.to_string(),
        ..Message::default()
    };
    assert!(is_repeat_of_tail(&[user("hi")], &[user("hi")]));
    assert!(!is_repeat_of_tail(&[user("hi")], &[user("other")]));
    assert!(!is_repeat_of_tail(
        &[user("hi"), assistant("yo")],
        &[user("hi")]
    ));
    assert!(!is_repeat_of_tail(&[], &[user("hi")]));
}

#[tokio::test]
async fn the_tool_hooks_fire_during_the_rounds() {
    let _guard = config_lock().lock().unwrap();
    let upstream = fake_upstream().await;
    install_provider("mock", &upstream, &["test-key"], "openai").await;
    let gateway = Arc::new(Gateway::new(
        Arc::new(MemStore::new()),
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    ));
    let primary = route_to(&upstream).await;

    let recording = Arc::new(RecordingSink::default());
    let sink: SinkRef = Arc::new(Mutex::new(Box::new(RecordingSink {
        calls: recording.calls.clone(),
        results: recording.results.clone(),
    })));
    let opts = options(50, vec![builtin_def("current_time")]);
    let response = gateway
        .complete(
            &primary,
            &[Message {
                role: "user".to_string(),
                content: "what time is it".to_string(),
                ..Message::default()
            }],
            &opts,
            &sink,
            None,
        )
        .await
        .expect("complete");

    assert_eq!(response.content, "done");
    assert_eq!(
        *recording.calls.lock().unwrap(),
        vec!["current_time".to_string()]
    );
    assert_eq!(
        *recording.results.lock().unwrap(),
        vec!["current_time".to_string()]
    );
}

#[test]
fn a_file_name_is_sanitised() {
    for (input, want) in [
        ("../../etc/passwd", "passwd"),
        ("/abs/photo.jpg", "photo.jpg"),
        ("my file (1).png", "my file (1).png"),
        ("...", ""),
        ("รูปถ่าย.jpg", "รูปถ่าย.jpg"),
    ] {
        assert_eq!(
            crate::upload::safe_file_name(input),
            want,
            "safe_file_name({input:?})"
        );
    }
    assert_eq!(crate::upload::safe_segment("ses-1/../../x"), "ses-1x");
}

#[test]
fn appending_a_delta_keeps_the_stored_history() {
    let user = |text: &str| Message {
        role: "user".to_string(),
        content: text.to_string(),
        ..Message::default()
    };
    let assistant = |text: &str| Message {
        role: "assistant".to_string(),
        content: text.to_string(),
        ..Message::default()
    };
    let stored = vec![
        user("คำถามแรก"),
        assistant("คำตอบแรก"),
        user("คำถามที่สอง"),
        assistant("คำตอบที่สอง"),
    ];
    let incoming = vec![user("คำถามที่สาม")];

    // What the WebSocket path used to do: a single new message shares no prefix
    // with the stored transcript, so everything before it was cut and the model
    // answered as if this were a fresh conversation.
    let (_, merged) = merge_history(&stored, &incoming);
    assert_eq!(
        merged.len(),
        1,
        "mergeHistory is expected to drop history for a single new turn"
    );

    let (full, delta) = append_delta(&stored, &incoming);
    assert_eq!(full.len(), 5, "the whole conversation must be kept");
    assert_eq!(full[0].content, "คำถามแรก");
    assert_eq!(delta.len(), 1);
    assert_eq!(delta[0].content, "คำถามที่สาม");
}

#[test]
fn appending_a_delta_reuses_the_recorded_question_on_a_retry() {
    let user = |text: &str| Message {
        role: "user".to_string(),
        content: text.to_string(),
        ..Message::default()
    };
    // The failed attempt already recorded the question; a retry must not stack a
    // twin the model would read as the user asking twice.
    let stored = vec![user("ก่อนหน้า"), user("คำถามที่ล้มแล้ว")];
    let (full, delta) = append_delta(&stored, &[user("คำถามที่ล้มแล้ว")]);
    assert_eq!(full.len(), 2, "retry stacked a twin");
    assert!(delta.is_empty(), "retry should persist nothing new");
}

#[tokio::test]
async fn the_final_answer_holds_only_the_last_tool_round() {
    let _guard = config_lock().lock().unwrap();
    let upstream = spawn_upstream(Router::new().fallback(|request: Request<Body>| async move {
        let body = axum::body::to_bytes(request.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&body);
        let json = if text.contains("\"tool_call_id\"") {
            write_fake("คำตอบสุดท้าย", &[], "")
        } else {
            write_fake("กำลังดูครับ", &["current_time"], "tool_calls")
        };
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response()
    }))
    .await;
    install_provider("mock", &upstream, &["test-key"], "openai").await;
    let gateway = Arc::new(Gateway::new(
        Arc::new(MemStore::new()),
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    ));
    let primary = route_to(&upstream).await;
    let sink: SinkRef = Arc::new(Mutex::new(Box::new(NullSink)));

    let response = gateway
        .complete(
            &primary,
            &[Message {
                role: "user".to_string(),
                content: "เวลาเท่าไหร่".to_string(),
                ..Message::default()
            }],
            &options(100, vec![builtin_def("current_time")]),
            &sink,
            None,
        )
        .await
        .expect("complete");
    assert_eq!(
        response.content, "คำตอบสุดท้าย",
        "want only the last round's text"
    );
}

/// The footer shows two different numbers from one turn: how long the reader
/// waited (the whole turn) and how fast the model wrote (the provider window).
#[tokio::test]
async fn a_turn_stores_both_the_wait_and_the_write() {
    let store = Arc::new(MemStore::new());
    let gateway = Gateway::new(
        store.clone(),
        Arc::new(aixodia_provider::Registry::new()),
        HashMap::new(),
    );
    let session = Session {
        id: "s1".to_string(),
        ..Session::default()
    };
    let sink: SinkRef = Arc::new(Mutex::new(Box::new(NullSink)));
    let question = vec![Message {
        role: "user".to_string(),
        content: "ถาม".to_string(),
        ..Message::default()
    }];

    gateway
        .persist_user_before(Some(&session), &[], &question, &sink)
        .await;
    let response = Response {
        content: "ตอบแล้ว".to_string(),
        model: "muse-spark".to_string(),
        generation_ms: 4200,
        usage: Usage {
            prompt_tokens: 10,
            completion_tokens: 84,
            ..Usage::default()
        },
        ..Response::default()
    };
    gateway
        .persist(Some(&session), &question, &response, None, &sink, 9000)
        .await;

    let turns = store.stored_turns("s1");
    assert_eq!(turns.len(), 2, "want the question and the answer");
    assert_eq!(turns[1].duration_ms, 9000, "the whole turn");
    assert_eq!(turns[1].generation_ms, 4200, "the provider window");
}

/// A provider that reports nothing usable must not produce a rate window longer
/// than the turn itself, which would divide the token count by a time that never
/// happened.
#[tokio::test]
async fn generation_falls_back_to_the_turn() {
    let store = Arc::new(MemStore::new());
    let gateway = Gateway::new(
        store.clone(),
        Arc::new(aixodia_provider::Registry::new()),
        HashMap::new(),
    );
    let session = Session {
        id: "s2".to_string(),
        ..Session::default()
    };
    let sink: SinkRef = Arc::new(Mutex::new(Box::new(NullSink)));
    let question = vec![Message {
        role: "user".to_string(),
        content: "ถาม".to_string(),
        ..Message::default()
    }];

    gateway
        .persist_user_before(Some(&session), &[], &question, &sink)
        .await;
    let response = Response {
        content: "ตอบแล้ว".to_string(),
        generation_ms: 60000,
        usage: Usage {
            completion_tokens: 10,
            ..Usage::default()
        },
        ..Response::default()
    };
    gateway
        .persist(Some(&session), &question, &response, None, &sink, 2500)
        .await;

    assert_eq!(
        store.stored_turns("s2")[1].generation_ms,
        2500,
        "want the turn rather than the impossible 60000ms"
    );
}

/// A tool that already ran has to be on record even when the turn then fails:
/// its steps used to be written only together with the final answer.
#[tokio::test]
async fn tool_steps_survive_a_failed_turn() {
    let _guard = config_lock().lock().unwrap();
    for stream in [false, true] {
        let upstream =
            spawn_upstream(Router::new().fallback(|request: Request<Body>| async move {
                let body = axum::body::to_bytes(request.into_body(), usize::MAX)
                    .await
                    .unwrap_or_default();
                let text = String::from_utf8_lossy(&body);
                if text.contains("\"tool_call_id\"") {
                    return (
                        StatusCode::BAD_REQUEST,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"error":{"message":"rejected"}}"#,
                    )
                        .into_response();
                }
                if text.contains("\"stream\":true") {
                    return (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        stream_fake("", &["current_time"], "tool_calls"),
                    )
                        .into_response();
                }
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    write_fake("", &["current_time"], "tool_calls"),
                )
                    .into_response()
            }))
            .await;
        install_provider("mock", &upstream, &["test-key"], "openai").await;
        let store = Arc::new(MemStore::new());
        let gateway = Arc::new(Gateway::new(
            store.clone(),
            Arc::new(aixodia_provider::Registry::new()),
            tools_with(None),
        ));

        let (created, body) = post_json(
            &gateway.router(),
            "/v1/sessions",
            r#"{"title":"t","model":"mock/test-model"}"#,
        )
        .await;
        assert_eq!(created, StatusCode::CREATED, "body = {body}");
        post_json(
            &gateway.router(),
            "/v1/chat/completions",
            &format!(
                r#"{{"session_id":"s1","stream":{stream},"messages":[{{"role":"user","content":"what time"}}]}}"#
            ),
        )
        .await;

        let turns = store.stored_turns("s1");
        let roles: Vec<&str> = turns.iter().map(|turn| turn.role.as_str()).collect();
        assert_eq!(
            roles,
            vec!["user", "tool_call", "tool_result"],
            "stream={stream}"
        );
        assert!(turns[1].text.contains("current_time"));
    }
}

/// A task that needs more tool rounds than the old four-round cap was cut off in
/// the middle of the work. Two things are pinned here: the loop keeps going for
/// the whole budget, and the request that ends it says the budget is spent so
/// the answer is a result.
#[tokio::test]
async fn out_of_rounds_keeps_going_and_ends_with_a_result() {
    let _guard = config_lock().lock().unwrap();
    let rounds = Arc::new(Mutex::new(0usize));
    let last = Arc::new(Mutex::new(String::new()));
    let rounds_in = rounds.clone();
    let last_in = last.clone();
    let upstream = spawn_upstream(Router::new().fallback(move |request: Request<Body>| {
        let rounds = rounds_in.clone();
        let last = last_in.clone();
        async move {
            let body = axum::body::to_bytes(request.into_body(), usize::MAX)
                .await
                .unwrap_or_default();
            let text = String::from_utf8_lossy(&body).to_string();
            *last.lock().unwrap() = text.clone();
            let json = if text.contains("\"tools\"") {
                *rounds.lock().unwrap() += 1;
                write_fake("กำลังดูอยู่", &["current_time"], "tool_calls")
            } else {
                write_fake("นี่คือสิ่งที่ทำได้แล้ว", &[], "stop")
            };
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                json,
            )
                .into_response()
        }
    }))
    .await;
    install_provider("mock", &upstream, &["test-key"], "openai").await;
    let gateway = Arc::new(Gateway::new(
        Arc::new(MemStore::new()),
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    ));

    let (status, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"model":"mock/test-model","messages":[{"role":"user","content":"งานยาว"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let seen = *rounds.lock().unwrap();
    assert_eq!(
        seen,
        crate::gateway::MAX_TOOL_ROUNDS as usize,
        "want the whole budget"
    );
    assert!(seen > 4, "want more than the old four-round cap");
    assert!(
        body.contains("นี่คือสิ่งที่ทำได้แล้ว"),
        "want the result of the closing round: {body}"
    );
    let closing = last.lock().unwrap().clone();
    assert!(
        closing.contains("no tool calls left"),
        "the closing request does not say the budget is spent: {closing}"
    );
    assert!(
        !closing.contains("\"tools\""),
        "the closing round must not offer tools any more"
    );
}

/// The reasoning a provider writes before its answer is stored with the turn and
/// replayed on the next request.
#[tokio::test]
async fn reasoning_is_stored_and_replayed_on_the_next_turn() {
    let _guard = config_lock().lock().unwrap();
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let bodies_in = bodies.clone();
    let upstream = spawn_upstream(Router::new().fallback(move |request: Request<Body>| {
        let bodies = bodies_in.clone();
        async move {
            let body = axum::body::to_bytes(request.into_body(), usize::MAX)
                .await
                .unwrap_or_default();
            bodies
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&body).to_string());
            let json = concat!(
                r#"{"id":"c","model":"test-model","choices":[{"index":0,"message":"#,
                r#"{"role":"assistant","content":"ตอบแล้ว","reasoning_content":"คิดก่อนตอบ"},"finish_reason":"stop"}],"#,
                r#""usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#
            );
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                json,
            )
                .into_response()
        }
    }))
    .await;
    install_provider("mock", &upstream, &["test-key"], "openai").await;
    let gateway = Arc::new(Gateway::new(
        Arc::new(MemStore::new()),
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    ));

    post_json(
        &gateway.router(),
        "/v1/sessions",
        r#"{"title":"t","model":"mock/test-model"}"#,
    )
    .await;
    post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"session_id":"s1","append":true,"messages":[{"role":"user","content":"ข้อความแรก"}]}"#,
    )
    .await;
    post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"session_id":"s1","append":true,"messages":[{"role":"user","content":"ข้อความที่สอง"}]}"#,
    )
    .await;

    let seen = bodies.lock().unwrap();
    assert!(seen.len() >= 2, "requests = {}, want two turns", seen.len());
    assert!(
        seen[1].contains(r#""reasoning_content":"คิดก่อนตอบ""#),
        "the second turn does not replay the reasoning: {}",
        seen[1]
    );
}

/// The phone opens a chat on its newest page and asks for one page per scroll. A
/// store that only knows how to return everything must still be served the
/// requested slice, never the whole transcript for a page request.
#[tokio::test]
async fn session_turns_honour_before_and_limit() {
    let _guard = config_lock().lock().unwrap();
    let store = Arc::new(MemStore::new());
    {
        let mut turns = store.turns.lock().unwrap();
        let list = turns.entry("s1".to_string()).or_default();
        for seq in 1..=250i64 {
            list.push(Turn {
                session_id: "s1".to_string(),
                seq,
                ..Turn::default()
            });
        }
    }
    let (gateway, _upstream) = new_test_gateway(store).await;
    let router = gateway.router();

    let page = |body: &str| -> Vec<Turn> {
        body_json(body)["turns"]
            .as_array()
            .map(|turns| {
                turns
                    .iter()
                    .map(|turn| serde_json::from_value(turn.clone()).expect("turn"))
                    .collect()
            })
            .unwrap_or_default()
    };

    let (status, body) = get_json(&router, "/v1/sessions/s1/turns?limit=60").await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    let newest = page(&body);
    assert_eq!(newest.len(), 60);
    assert_eq!(newest[0].seq, 191);
    assert_eq!(newest[59].seq, 250);

    let (_, body) = get_json(&router, "/v1/sessions/s1/turns?before_seq=191&limit=60").await;
    let older = page(&body);
    assert_eq!(older.len(), 60);
    assert_eq!(older[0].seq, 131);
    assert_eq!(older[59].seq, 190);

    let (_, body) = get_json(&router, "/v1/sessions/s1/turns").await;
    assert_eq!(
        page(&body).len(),
        250,
        "an unpaged request is the whole list"
    );
}

/// A request without the right bearer token is turned away; the right one gets
/// through. The comparison is constant-time, so this only pins the behaviour.
#[tokio::test]
async fn the_token_guard_compares_the_whole_token() {
    let guarded = Router::new()
        .fallback(|| async { StatusCode::NO_CONTENT })
        .layer(axum::middleware::from_fn_with_state(
            "secret".to_string(),
            crate::gateway::require_token,
        ));

    for (header_value, want) in [
        ("Bearer secret", StatusCode::NO_CONTENT),
        ("Bearer secre", StatusCode::UNAUTHORIZED),
        ("Bearer secrets", StatusCode::UNAUTHORIZED),
        ("Bearer ", StatusCode::UNAUTHORIZED),
        ("", StatusCode::UNAUTHORIZED),
    ] {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri("/v1/sessions")
            .body(Body::empty())
            .expect("request");
        if !header_value.is_empty() {
            request
                .headers_mut()
                .insert(header::AUTHORIZATION, header_value.parse().expect("value"));
        }
        let response = guarded.clone().oneshot(request).await.expect("router");
        assert_eq!(
            response.status(),
            want,
            "Authorization {header_value:?} should be {want}"
        );
    }

    let request = Request::builder()
        .method(Method::GET)
        .uri("/healthz")
        .body(Body::empty())
        .expect("request");
    let response = guarded.clone().oneshot(request).await.expect("router");
    assert_eq!(
        response.status(),
        StatusCode::NO_CONTENT,
        "/healthz must stay open"
    );
}

/// What the prompt cache served must reach the stored turn. The figures live in
/// the usage details, not the top-level counts.
#[tokio::test]
async fn the_cache_read_from_usage_details_reaches_the_stored_turn() {
    let _guard = config_lock().lock().unwrap();
    let upstream = spawn_upstream(Router::new().fallback(|| async {
        let json = concat!(
            r#"{"id":"chatcmpl-cache","model":"test-model","#,
            r#""choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"#,
            r#""usage":{"prompt_tokens":100,"completion_tokens":5,"total_tokens":105,"#,
            r#""prompt_tokens_details":{"cached_tokens":80},"#,
            r#""completion_tokens_details":{"reasoning_tokens":2}}}"#
        );
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            json,
        )
            .into_response()
    }))
    .await;
    install_provider("mock", &upstream, &["test-key"], "openai").await;
    let store = Arc::new(MemStore::new());
    let gateway = Arc::new(Gateway::new(
        store.clone(),
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    ));

    post_json(
        &gateway.router(),
        "/v1/sessions",
        r#"{"title":"t","model":"mock/test-model"}"#,
    )
    .await;
    let (status, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"session_id":"s1","messages":[{"role":"user","content":"hi"}]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body = {body}");

    let turns = store.stored_turns("s1");
    let last = turns.last().expect("stored row");
    assert_eq!(last.role, "assistant");
    assert_eq!(last.cache_read_tokens, 80);
    assert_eq!(last.reasoning_tokens, 2);
    assert!(
        last.input_includes_cache,
        "input_includes_cache must be set: prompt_tokens already contains the cached part"
    );
}

/// Every provider request round announces itself — waiting when it starts, done
/// when it ends — and every displayable unit rides the stream as a block the
/// phone renders generically.
#[tokio::test]
async fn the_request_rounds_and_blocks_ride_the_stream() {
    let _guard = config_lock().lock().unwrap();
    let upstream = spawn_upstream(Router::new().fallback(|request: Request<Body>| async move {
        let body = axum::body::to_bytes(request.into_body(), usize::MAX)
            .await
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&body);
        let sse = if text.contains("\"tool_call_id\"") {
            stream_fake("เสร็จแล้ว", &[], "stop")
        } else {
            stream_fake("", &["current_time"], "tool_calls")
        };
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/event-stream")],
            sse,
        )
            .into_response()
    }))
    .await;
    install_provider("mock", &upstream, &["test-key"], "openai").await;
    let store = Arc::new(MemStore::new());
    let gateway = Arc::new(Gateway::new(
        store.clone(),
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    ));
    post_json(
        &gateway.router(),
        "/v1/sessions",
        r#"{"title":"t","model":"mock/test-model"}"#,
    )
    .await;

    let (_, body) = post_json(
        &gateway.router(),
        "/v1/chat/completions",
        r#"{"session_id":"s1","model":"mock/test-model","messages":[{"role":"user","content":"กี่โมง"}],"stream":true}"#,
    )
    .await;
    for want in [
        r#""stage":"request""#,
        r#""round":1"#,
        r#""round":2"#,
        r#""type":"request""#,
        r#""type":"tool""#,
        r#""stage":"tool_call""#,
        r#""stage":"tool_result""#,
    ] {
        assert!(body.contains(want), "the stream misses {want}:\n{body}");
    }
    // Two rounds, each announced once waiting and once done.
    assert_eq!(
        body.matches(r#""type":"request""#).count(),
        4,
        "want waiting+done for each of two rounds:\n{body}"
    );

    let turns = store.stored_turns("s1");
    let mut blocks: HashMap<String, String> = HashMap::new();
    for turn in &turns {
        // The question draws as a bubble and needs no block; everything the
        // engine says back does.
        if turn.role == "user" {
            continue;
        }
        let block = aixodia_session::Block::parse(&turn.block)
            .unwrap_or_else(|| panic!("stored {} row has no block: {turn:?}", turn.role));
        blocks.insert(format!("{} {}", turn.role, block.kind), turn.block.clone());
    }
    assert!(blocks.contains_key("tool_call tool"), "{blocks:?}");
    assert!(blocks.contains_key("tool_result tool"), "{blocks:?}");
    let last = turns.last().expect("stored row");
    let block = aixodia_session::Block::parse(&last.block).expect("closing block");
    assert_eq!(block.kind, "text", "closing answer block = {}", last.block);
}

/// Self-update was removed: updates happen by re-running the install script, so
/// the daemon must no longer expose a route that swaps its own binary.
#[tokio::test]
async fn the_engine_update_route_is_gone() {
    let gateway = Arc::new(Gateway::new(
        Arc::new(MemStore::new()),
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    ));
    let (status, _) = post_json(&gateway.router(), "/api/engine/update", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "self-update must be gone");
}

// --- restart_test.go --------------------------------------------------------

/// `POST /restart` is how the phone ends a session: the notebook watches
/// `~/.exit`, backs the filesystem up and starts the next run. Nothing goes to
/// D1.
#[tokio::test]
async fn restart_writes_the_exit_file_in_home() {
    let _guard = config_lock().lock().unwrap();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("HOME", dir.path());
    let (gateway, _upstream) = new_test_gateway(Arc::new(MemStore::new())).await;

    let (status, body) = post_json(&gateway.router(), "/restart", "{}").await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert!(
        dir.path().join(".exit").exists(),
        "no ~/.exit after POST /restart"
    );
}

/// A keyless provider that throttles the turn is asked again instead of failing
/// it: the free tiers throttle per IP for a fixed window, so the phone should
/// see the answer after the window rather than an outage. The wording matters —
/// a plain rate limit paces, a spent allowance is terminal (see
/// `a_spent_free_quota_fails_fast_instead_of_stalling` in the provider).
#[tokio::test]
async fn a_keyless_turn_retries_after_a_throttle_answer() {
    let _guard = config_lock().lock().unwrap();
    let hits = Arc::new(Mutex::new(0usize));
    let seen = hits.clone();
    let app = Router::new().fallback(move |request: Request<Body>| {
        let seen = seen.clone();
        async move {
            if request.uri().path() != "/v1/chat/completions" {
                return (StatusCode::NOT_FOUND, "").into_response();
            }
            let mut hits = seen.lock().unwrap();
            *hits += 1;
            if *hits == 1 {
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"error":{"message":"too many requests"}}"#,
                )
                    .into_response();
            }
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                write_fake("after wait", &[], "stop"),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    install_provider("free", &format!("http://{addr}"), &[], "openai").await;

    let gateway = Arc::new(Gateway::new(
        Arc::new(MemStore::new()),
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    ));
    let body = json!({
        "model": "free/m1",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let started = Instant::now();
    let (status, body) =
        post_json(&gateway.router(), "/v1/chat/completions", &body.to_string()).await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert!(body.contains("after wait"), "body = {body}");
    assert_eq!(*hits.lock().unwrap(), 2);
    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "the retry must wait before asking again"
    );
}

/// A brand-new chat is created with no route, and the phone leaves its picker
/// empty until the user chooses. The daemon must fall back to the saved main
/// instead of failing the first message for a model nobody had to pick.
#[tokio::test]
async fn a_new_session_without_a_route_uses_the_saved_main() {
    let _guard = config_lock().lock().unwrap();
    let (gateway, _upstream) = new_test_gateway(Arc::new(MemStore::new())).await;
    let state = Arc::new(MemState::default());
    state
        .put_state(
            crate::api::SETTINGS_STATE_KEY,
            r#"{"main":{"provider":"mock","model":"test-model","generation":{}}}"#,
        )
        .await
        .unwrap();
    *gateway.state.write().unwrap() = Some(state);

    let (status, body) = post_json(&gateway.router(), "/v1/sessions", r#"{"title":"new"}"#).await;
    assert_eq!(status, StatusCode::CREATED, "body = {body}");
    let session: Value = serde_json::from_str(&body).expect("session json");
    let id = session["id"].as_str().expect("session id").to_string();

    let body = json!({
        "session_id": id,
        "messages": [{"role": "user", "content": "hi"}],
    });
    let (status, body) =
        post_json(&gateway.router(), "/v1/chat/completions", &body.to_string()).await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    assert!(body.contains("done"), "body = {body}");
}

#[tokio::test]
async fn a_daemon_with_no_stored_settings_defaults_to_the_keyless_opencode() {
    let _guard = config_lock().lock().unwrap();
    let (gateway, _upstream) = new_test_gateway(Arc::new(MemStore::new())).await;
    // An empty state store has never been told a main, so the built-in
    // non-login provider is the route.
    *gateway.state.write().unwrap() = Some(Arc::new(MemState::default()));
    assert_eq!(
        gateway.saved_main_model().await,
        ("Opencode".to_string(), "big-pickle".to_string()),
        "a fresh daemon must answer on the keyless built-in"
    );
}

#[tokio::test]
async fn the_objectives_endpoint_splits_goals_from_plans() {
    // The app's goals card reads this one route, so the split by kind and the
    // status of each item are the contract: a plan must never arrive among the
    // goals, and a completed goal must not read as still open.
    use aixodia_goals::{Kind, MemoryStore, Store};
    let store = Arc::new(MemoryStore::default());
    let goal = store
        .add("s1", Kind::Goal, "ship the card")
        .await
        .expect("add");
    store.add("s1", Kind::Plan, "render it").await.expect("add");
    store
        .complete("s1", Kind::Goal, &goal)
        .await
        .expect("complete");

    let gateway = Arc::new(Gateway::new(
        Arc::new(MemStore::default()),
        Arc::new(aixodia_provider::Registry::new()),
        tools_with(None),
    ));
    gateway.set_objective_store(store);

    let (status, body) = get_json(&gateway.router(), "/v1/sessions/s1/objectives").await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    let v: Value = serde_json::from_str(&body).expect("json");
    assert_eq!(
        v["goals"].as_array().expect("goals").len(),
        1,
        "body = {body}"
    );
    assert_eq!(v["goals"][0]["status"], "done", "body = {body}");
    assert_eq!(
        v["plans"].as_array().expect("plans").len(),
        1,
        "body = {body}"
    );
    assert_eq!(v["plans"][0]["content"], "render it", "body = {body}");

    // A chat nothing was ever recorded for answers with empty lists, and a
    // daemon with no store at all answers the same way — never an error the
    // app would have to tell apart from "no goals yet".
    let (status, body) = get_json(&gateway.router(), "/v1/sessions/other/objectives").await;
    assert_eq!(status, StatusCode::OK, "body = {body}");
    let v: Value = serde_json::from_str(&body).expect("json");
    assert!(
        v["goals"].as_array().expect("goals").is_empty(),
        "body = {body}"
    );
    assert!(
        v["plans"].as_array().expect("plans").is_empty(),
        "body = {body}"
    );
}

/// `RIDEABLE_WAIT` is the budget the paced keyless retry spends, and the two
/// constants live in different crates. This pins them: if `KEYLESS_BACKOFF`
/// grows and the budget does not, a throttle the retry has now learned to ride
/// out is still read as a spent allowance and the turn dies on the first answer.
#[test]
fn the_rideable_wait_equals_the_keyless_retry_budget() {
    let budget: Duration = crate::gateway::KEYLESS_BACKOFF.iter().sum();
    assert_eq!(
        budget,
        aixodia_provider::RIDEABLE_WAIT,
        "KEYLESS_BACKOFF sums to {budget:?} but RIDEABLE_WAIT is {:?}",
        aixodia_provider::RIDEABLE_WAIT
    );
}
