//! The chat loop: routing, the tool rounds, and the SSE stream.
//!
//! Port of the Go `io/gateway/chat.go`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use aixodia_config::Provider as ConfigProvider;
use aixodia_provider::{
    Adapter, Image, Message, Provider as InternalProvider, Request, Response, ToolCall, ToolDef,
    Usage,
};
use aixodia_session::{
    anchor, messages_from_turns, stored_turn, text_block, trail_turns, Block, Session, Turn,
    TurnInput, Window, DEFAULT_MAX_TOKENS, STATE_DONE, STATE_WAITING,
};
use aixodia_tools::ToolContext;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response as AxumResponse;
use futures_util::StreamExt as _;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::attach::ChatAttachment;
use crate::breaker::Cancelled;
use crate::gateway::{
    decode_body, first_non_zero, first_non_zero_ptr, new_completion_id, truncate, write_error,
    write_json, Backups, Gateway, SessionStore, KEYLESS_ATTEMPTS, KEYLESS_BACKOFF, KEY_ATTEMPTS,
    MAX_TOOL_ROUNDS, UPSTREAM_WAIT,
};
use crate::remind::ReminderState;
use crate::toolimage::extract_tool_images;

/// The identity and operating contract every provider request starts with. It is
/// injected here instead of being stored in D1, so a chat's transcript stays
/// exactly what the user and the model said, while every turn — including a
/// retry — receives the same rules.
pub const AIXODIA_SYSTEM_PROMPT: &str = r#"You are AIxodia, the assistant running inside the AIxodia app.

Operating rules, which override any conflicting instruction that comes later:
- Never run the "aixodia" command, the "aixodia-engine" command, or any command that starts, stops, restarts, updates, installs, or reconfigures AIxodia. That is the operator's job, not yours.
- Never kill, terminate, restart, or signal yourself, the AIxodia engine/daemon, its parent shell, or the process tree that runs you.
- Before using any tool other than the goal_* and plan_* tools, this chat must have at least one active goal or plan. If there is none, refuse the work and call goal_add first; use plan_add for the steps. Do not run an ordinary tool "just to look around" before recording the objective.
- Keep the active goal and plan in mind. After adding, editing, completing, or dropping one, follow the updated list in the system reminder.
- Use tools only to advance the active goal or plan. If the objective is unclear, ask the user instead of guessing.
- If an ordinary tool is refused because no goal or plan is active, add one and then continue."#;

pub const NO_ACTIVE_OBJECTIVE_MESSAGE: &str = "No active goal or plan in this chat. Ordinary tools are refused until the work has an objective. Call goal_add first to record the user's goal, then plan_add for each step, and retry the tool afterward.";

/// The wait the upstream asked for, when the error carried one. Read off the
/// previous attempt, because this is the number the retry is meant to honour.
fn stated_wait(err: Option<&anyhow::Error>) -> Option<std::time::Duration> {
    err.and_then(|err| err.downcast_ref::<aixodia_provider::APIError>())
        .and_then(|api| api.retry_after)
}

/// The last thing the model reads in a turn that ran out of tool rounds. It is
/// written for the model and never stored: it is added to the request
/// conversation only, so the trail on record is what really happened.
pub const OUT_OF_ROUNDS: &str = "This turn has no tool calls left, so no further tool can run. \
Answer now in the language of the question: say what you actually did and found, \
and, if the task is not finished, list what is still left. \
Do not describe what you are about to look at, and do not ask for another tool.";

/// How much of a reasoning blob or a tool result is kept.
const REASONING_CEILING: usize = 32 * 1024;
const TOOL_RESULT_CEILING: usize = 32 * 1024;
const TOOL_EVENT_CEILING: usize = 4096;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatMessage {
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub content: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChatRequest {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub max_tokens: i64,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub user: String,
    /// Says the messages are the new turns only, not a transcript.
    #[serde(default)]
    pub append: bool,
    /// Resumes the last question already stored in the session instead of
    /// appending messages.
    #[serde(default)]
    pub retry: bool,
    #[serde(default)]
    pub attachments: Vec<ChatAttachment>,
}

/// One resolved provider/model pair, with the adapter that will be called.
#[derive(Clone)]
pub(crate) struct Route {
    pub(crate) adapter: Arc<dyn Adapter>,
    pub(crate) prov: InternalProvider,
    pub(crate) model: String,
}

/// A sink that receives everything a turn shows while it runs.
///
/// The Go code threads a fistful of callbacks through the loop; they all belong
/// to one watcher, so they are one trait here.
pub trait ChatSink: Send {
    fn emit(&mut self, delta: &str) -> anyhow::Result<()> {
        let _ = delta;
        Ok(())
    }
    fn on_request(&mut self, round: i64, state: &str) {
        let _ = (round, state);
    }
    fn on_usage(&mut self, usage: &Usage) {
        let _ = usage;
    }
    fn on_tool_call(&mut self, call: &ToolCall) {
        let _ = call;
    }
    fn on_tool_result(&mut self, call: &ToolCall, text: &str, duration_ms: i64, is_error: bool) {
        let _ = (call, text, duration_ms, is_error);
    }
    fn on_reasoning(&mut self, text: &str) -> anyhow::Result<()> {
        let _ = text;
        Ok(())
    }
    /// The window dropped whole turns to make this request fit.
    fn on_compact(&mut self, report: &aixodia_session::Compacted) {
        let _ = report;
    }
    fn on_stored(&mut self, rows: &[Turn], error: Option<&str>) {
        let _ = (rows, error);
    }
}

/// A watcher that shows nothing, which is the non-streaming API's answer.
pub struct NullSink;

impl ChatSink for NullSink {}

pub type SinkRef = Arc<Mutex<Box<dyn ChatSink>>>;

/// The same watcher seen from a write that may outlive its stream.
type SinkWeak = std::sync::Weak<std::sync::Mutex<Box<dyn ChatSink>>>;

pub(crate) fn lock_sink(sink: &SinkRef) -> std::sync::MutexGuard<'_, Box<dyn ChatSink>> {
    sink.lock().unwrap_or_else(|e| e.into_inner())
}

/// The watcher a row write reports to.
///
/// A turn that waits for its own write holds the stream anyway, so it hands the
/// strong reference straight over. A write that runs behind the turn holds only a
/// weak one and upgrades at the moment it reports — never across the store call,
/// which would keep a stream the reader already left open for as long as D1 takes.
#[derive(Clone)]
pub(crate) enum Reporter {
    Live(SinkRef),
    Behind(SinkWeak),
}

impl Reporter {
    fn report(&self, rows: &[Turn], error: Option<&str>) {
        match self {
            Reporter::Live(sink) => lock_sink(sink).on_stored(rows, error),
            Reporter::Behind(sink) => {
                if let Some(sink) = sink.upgrade() {
                    lock_sink(&sink).on_stored(rows, error);
                }
            }
        }
    }
}

/// Hands the adapter the sink and counts what it emitted, which is what decides
/// whether a retry is still allowed.
struct SinkEmitter {
    sink: SinkRef,
    emitted: Arc<AtomicUsize>,
}

impl aixodia_provider::types::Emitter for SinkEmitter {
    fn emit(&mut self, delta: &str) -> anyhow::Result<()> {
        self.emitted.fetch_add(1, Ordering::SeqCst);
        lock_sink(&self.sink).emit(delta)
    }
}

/// Everything one turn needs besides the conversation.
#[derive(Clone)]
pub struct CompletionOptions {
    pub max_tokens: i64,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub tools: Vec<ToolDef>,
    pub stream: bool,
    pub user: String,
    pub session_id: String,
    /// Names the conversation to the provider's prompt cache, so every turn of
    /// one chat carries the same cache key.
    pub window: Window,
    pub cancel: CancellationToken,
    pub reused_user: Option<Turn>,
}

/// Stores tool steps as each round finishes instead of once the whole reply is
/// done. A turn that fails or is cut off after a tool already ran would
/// otherwise leave no record of it.
pub struct TrailWriter {
    sessions: Arc<dyn SessionStore>,
    id: String,
    pub(crate) saved: usize,
    pub(crate) pending: Option<Vec<TurnInput>>,
    pub(crate) pending_end: usize,
    /// How long each tool call ran, keyed by call id. The trail has no room for
    /// it — a tool result is a string — so it is stamped onto the stored row
    /// here, where the phone can still read it after the turn closes.
    durations: Arc<Mutex<HashMap<String, i64>>>,
    /// The chat's write queue, used when the rows go behind the turn.
    backups: Backups,
    /// True when the turn must not wait for D1. The round's tool steps then
    /// queue behind the chat instead of holding the next request.
    deferred: bool,
}

impl TrailWriter {
    pub fn new(sessions: Arc<dyn SessionStore>, stored: &Session) -> Self {
        Self::with_durations(sessions, stored, Arc::new(Mutex::new(HashMap::new())))
    }

    /// The same writer over a duration map somebody else also writes: the
    /// streaming watcher stamps a call's time as it happens.
    pub fn with_durations(
        sessions: Arc<dyn SessionStore>,
        stored: &Session,
        durations: Arc<Mutex<HashMap<String, i64>>>,
    ) -> Self {
        Self {
            sessions,
            id: stored.id.clone(),
            saved: 0,
            pending: None,
            pending_end: 0,
            durations,
            backups: Backups::default(),
            deferred: false,
        }
    }

    /// Stores this trail's rows behind the turn, on the chat's own queue, so a
    /// round's tool steps never hold up the request that follows them.
    pub fn behind(mut self, backups: Backups) -> Self {
        self.deferred = true;
        self.backups = backups;
        self
    }

    pub fn durations(&self) -> Arc<Mutex<HashMap<String, i64>>> {
        self.durations.clone()
    }

    /// Remembers how long a call ran so the next flush can stamp it on the
    /// tool_result row it stores.
    pub fn record_duration(&self, call_id: &str, ms: i64) {
        if call_id.is_empty() {
            return;
        }
        self.durations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(call_id.to_string(), ms);
    }

    /// The part of `trail` that a flush has not stored.
    pub(crate) fn rest(&self, trail: &[Message]) -> Vec<Message> {
        if self.saved > trail.len() {
            return trail.to_vec();
        }
        trail[self.saved..].to_vec()
    }

    /// Stores the steps of `trail` that are not stored yet. A failed write
    /// leaves them unmarked, so the next round, or persist at the end, retries.
    pub async fn flush(&mut self, trail: &[Message], sink: &SinkRef) {
        if self.deferred {
            self.queue_flush(trail, sink);
            return;
        }
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 64 {
                return;
            }
            if self.saved >= trail.len() {
                return;
            }
            if self.pending.is_none() {
                let mut pending = trail_turns(&trail[self.saved..]);
                attach_tool_durations(&mut pending, &self.durations);
                self.pending = Some(pending);
                self.pending_end = trail.len();
            }
            let pending = self.pending.take().unwrap_or_default();
            if !pending.is_empty() {
                let appended = self.sessions.append_turns_partial(&self.id, &pending).await;
                let rows = appended.rows;
                let error = appended.error.map(|err| err.to_string());
                let written = rows.len().min(pending.len());
                self.pending = Some(pending[written..].to_vec());
                lock_sink(sink).on_stored(&rows, error.as_deref());
                if error.is_some() {
                    tracing::warn!("gateway: save tool steps: {}", error.unwrap_or_default());
                    return;
                }
            }
            self.saved = self.pending_end;
            self.pending = None;
        }
    }

    /// Marks the round's steps as ours to store and hands them to the chat's
    /// queue. The next request goes out now; D1 catches up behind the turn, and
    /// the rows still land in order because the queue is one at a time.
    fn queue_flush(&mut self, trail: &[Message], sink: &SinkRef) {
        if self.saved >= trail.len() {
            return;
        }
        let mut pending = trail_turns(&trail[self.saved..]);
        attach_tool_durations(&mut pending, &self.durations);
        self.saved = trail.len();
        self.pending_end = self.saved;
        self.pending = None;
        if pending.is_empty() {
            return;
        }
        let sessions = self.sessions.clone();
        let queue_id = self.id.clone();
        let job_id = self.id.clone();
        let sink = Reporter::Behind(Arc::downgrade(sink));
        self.backups.queue(&queue_id, async move {
            save_trail(&sessions, &job_id, pending, &sink).await;
        });
    }
}

/// Writes a round's tool steps, giving back to the next write whatever the store
/// refused after its own tries.
async fn save_trail(
    sessions: &Arc<dyn SessionStore>,
    id: &str,
    mut rows: Vec<TurnInput>,
    sink: &Reporter,
) {
    for _attempt in 0..3 {
        if rows.is_empty() {
            return;
        }
        let appended = sessions.append_turns_partial(id, &rows).await;
        let error = appended.error.map(|err| err.to_string());
        let written = appended.rows.len().min(rows.len());
        rows.drain(..written);
        sink.report(&appended.rows, error.as_deref());
        if let Some(err) = error {
            tracing::warn!("gateway: save tool steps: {err}");
        }
    }
    if !rows.is_empty() {
        tracing::warn!("gateway: {} tool steps never reached the store", rows.len());
    }
}

/// Writes each stored tool_result row the figure of the call it answers. Without
/// it the time is a live-only flicker: the phone shows it while the tool runs
/// and loses it the moment the stored rows replace the live thread.
pub(crate) fn attach_tool_durations(
    rows: &mut [TurnInput],
    durations: &Arc<Mutex<HashMap<String, i64>>>,
) {
    let durations = durations.lock().unwrap_or_else(|e| e.into_inner());
    if durations.is_empty() {
        return;
    }
    for row in rows.iter_mut() {
        if row.role != "tool_result" {
            continue;
        }
        #[derive(Deserialize)]
        struct Row {
            #[serde(default)]
            tool_call_id: String,
        }
        let Ok(parsed) = serde_json::from_str::<Row>(&row.text) else {
            continue;
        };
        let Some(ms) = durations.get(&parsed.tool_call_id) else {
            continue;
        };
        row.duration_ms = *ms;
        // The stored tool block carries the same figure, so the row draws
        // settled even where the reader never sees the columns.
        if let Some(mut block) = Block::parse(&row.block) {
            if block.kind == aixodia_session::block::BLOCK_TOOL {
                block.duration_ms = *ms;
                row.block = block.json();
            }
        }
    }
}

pub async fn chat(State(g): State<Arc<Gateway>>, body: Bytes) -> AxumResponse {
    let body: ChatRequest = match decode_body(body) {
        Ok(body) => body,
        Err(err) => {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &err.to_string(),
            )
        }
    };
    if body.messages.is_empty() && !body.retry {
        return write_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "messages must not be empty",
        );
    }
    // An earlier turn of this chat left its rows to be written behind it. The
    // history read below has to see them, so a message typed the moment its
    // answer appeared still reads that answer; a chat whose writes landed while
    // its reader was still looking at the answer waits on nothing.
    g.await_backups(&body.session_id).await;
    // The session and its transcript key off the same id and neither one feeds
    // the other, and each is a separate HTTPS call to the store — so asking
    // both at once takes a round trip off the time before the model is asked.
    let reads = if body.session_id.is_empty() {
        None
    } else {
        let session_id = body.session_id.clone();
        Some(tokio::join!(
            g.sessions.get(&session_id),
            g.sessions.turns(&session_id)
        ))
    };
    let stored: Option<Session> = match &reads {
        None => None,
        Some((Ok(found), _)) => Some(found.clone()),
        Some((Err(err), _)) => {
            return write_error(StatusCode::NOT_FOUND, "not_found", &err.to_string());
        }
    };
    let preloaded = reads.map(|(_, turns)| turns);
    if let Some(found) = &stored {
        if !g.begin_turn(&found.id) {
            return write_error(
                StatusCode::CONFLICT,
                "turn_in_progress",
                "a turn is already running for this chat; wait for it to finish",
            );
        }
    }
    // A transcript the store would not name is no answer the model can be asked
    // for, and the turn claim taken above has to be given back first.
    let preloaded = match preloaded {
        Some(Ok(turns)) => turns,
        Some(Err(err)) => {
            if let Some(found) = &stored {
                g.end_turn(&found.id);
            }
            return write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string());
        }
        None => Vec::new(),
    };

    let incoming: Vec<Message> = body
        .messages
        .iter()
        .map(|message| {
            let role = message.role.trim();
            Message {
                role: if role.is_empty() {
                    "user".to_string()
                } else {
                    role.to_string()
                },
                content: message.content.clone(),
                ..Message::default()
            }
        })
        .collect();
    // Pictures ride on the message that asked about them. The text note the
    // phone sends alongside still names every file, so a model that cannot take
    // pictures, and every later turn that reads this back from the database, is
    // no worse off than before.
    let (incoming, skipped) = g.attach_images(&incoming, &body.attachments);
    for reason in skipped {
        tracing::info!("gateway: attachment not sent as a picture: {reason}");
    }

    let mut history = incoming.clone();
    let mut delta = incoming.clone();
    let mut saved_turns: Vec<Turn> = Vec::new();
    let mut saved: Vec<Message> = Vec::new();
    if body.retry {
        let Some(found) = &stored else {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "retry requires a session_id",
            );
        };
        saved_turns = preloaded;
        saved = messages_from_turns(&saved_turns);
        if !has_unfinished_question(&saved_turns) {
            g.end_turn(&found.id);
            return write_error(
                StatusCode::CONFLICT,
                "invalid_request_error",
                "no unfinished turn to retry",
            );
        }
        // No delta: the question and every tool step already stored belong to
        // the turn being resumed. Persist will only append what this attempt
        // produces from here on.
        history = saved.clone();
        delta = Vec::new();
    } else if stored.is_some() {
        saved_turns = preloaded;
        saved = messages_from_turns(&saved_turns);
        if body.append {
            let (full, new_delta) = append_delta(&saved, &incoming);
            history = full;
            delta = new_delta;
        } else {
            let (full, new_delta) = merge_history(&saved, &incoming);
            history = full;
            delta = new_delta;
        }
    }

    let primary = match g.resolve_route(stored.as_ref(), &body).await {
        Ok(route) => route,
        Err(err) => {
            if let Some(found) = &stored {
                g.end_turn(&found.id);
            }
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &err.to_string(),
            );
        }
    };

    let toolset = match select_tools(&g, body.tools.as_ref()) {
        Ok(toolset) => toolset,
        Err(err) => {
            if let Some(found) = &stored {
                g.end_turn(&found.id);
            }
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &err.to_string(),
            );
        }
    };

    let mut reused_user: Option<Turn> = None;
    if delta.is_empty() && incoming.len() == 1 && incoming[0].role == "user" {
        for turn in saved_turns.iter().rev() {
            if turn.role == "user" && turn.text == incoming[0].content {
                reused_user = Some(turn.clone());
                break;
            }
        }
    }

    let options = CompletionOptions {
        max_tokens: first_non_zero(
            body.max_tokens,
            stored.as_ref().map(|s| s.max_tokens).unwrap_or(0),
        ),
        temperature: first_non_zero_ptr(
            body.temperature,
            stored.as_ref().and_then(|s| s.temperature),
        ),
        top_p: first_non_zero_ptr(body.top_p, stored.as_ref().and_then(|s| s.top_p)),
        tools: toolset,
        stream: body.stream,
        user: body.user,
        session_id: stored.as_ref().map(|s| s.id.clone()).unwrap_or_default(),
        window: history_window(&saved_turns, &saved, &history),
        cancel: CancellationToken::new(),
        reused_user,
    };

    if body.stream {
        return chat_stream(g, stored, primary, history, delta, options).await;
    }
    chat_once(g, stored, primary, history, delta, options).await
}

async fn chat_once(
    g: Arc<Gateway>,
    stored: Option<Session>,
    primary: Route,
    history: Vec<Message>,
    delta: Vec<Message>,
    options: CompletionOptions,
) -> AxumResponse {
    let started = Instant::now();
    let sink: SinkRef = Arc::new(Mutex::new(Box::new(NullSink)));
    let sessions = g.sessions();
    let mut trail = stored.as_ref().map(|s| TrailWriter::new(sessions, s));
    g.persist_user_before(stored.as_ref(), &history, &delta, &sink)
        .await;

    let result = g
        .complete(&primary, &history, &options, &sink, trail.as_mut())
        .await;
    let resp = match result {
        Ok(resp) => resp,
        Err(err) => {
            tracing::warn!(
                "gateway: {}/{} turn failed: {err}",
                primary.prov.name,
                primary.model
            );
            if let Some(found) = &stored {
                g.end_turn(&found.id);
            }
            return write_upstream_error(&err);
        }
    };
    let answer_duration = started.elapsed().as_millis() as i64;
    let _ = g
        .persist(
            stored.as_ref(),
            &delta,
            &resp,
            trail.as_mut(),
            &sink,
            answer_duration,
        )
        .await;
    if let Some(found) = &stored {
        g.end_turn(&found.id);
    }

    let id = if resp.id.is_empty() {
        new_completion_id()
    } else {
        resp.id.clone()
    };
    let finish = if resp.finish_reason.is_empty() {
        "stop".to_string()
    } else {
        resp.finish_reason.clone()
    };
    write_json(
        StatusCode::OK,
        json!({
            "id": id,
            "object": "chat.completion",
            "created": unix_now(),
            "model": resp.model,
            "provider": resp.provider,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": resp.content},
                "finish_reason": finish,
            }],
            "usage": {
                "prompt_tokens": resp.usage.prompt_tokens,
                "completion_tokens": resp.usage.completion_tokens,
                "total_tokens": resp.usage.total_tokens,
            },
        }),
    )
}

async fn chat_stream(
    g: Arc<Gateway>,
    stored: Option<Session>,
    primary: Route,
    history: Vec<Message>,
    delta: Vec<Message>,
    options: CompletionOptions,
) -> AxumResponse {
    use tokio_stream::wrappers::UnboundedReceiverStream;

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Bytes>();
    let body = axum::body::Body::from_stream(
        UnboundedReceiverStream::new(rx).map(Ok::<Bytes, std::convert::Infallible>),
    );
    let response = AxumResponse::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "text/event-stream")
        .header(axum::http::header::CACHE_CONTROL, "no-cache")
        .header(axum::http::header::CONNECTION, "keep-alive")
        .body(body)
        .unwrap_or_else(|_| {
            write_error(StatusCode::INTERNAL_SERVER_ERROR, "stream_error", "stream")
        });

    let created = unix_now();
    let id = new_completion_id();
    let model_id = primary.model.clone();
    let durations: Arc<Mutex<HashMap<String, i64>>> = Arc::new(Mutex::new(HashMap::new()));
    let sink: SinkRef = Arc::new(Mutex::new(Box::new(SseSink {
        tx: tx.clone(),
        id: id.clone(),
        created,
        model: model_id,
        first: true,
        open_round: 0,
        durations: durations.clone(),
    })));

    let task_g = g.clone();
    let task_sink = sink.clone();
    let mut task_options = options.clone();
    task_options.stream = true;
    let backups = g.backups.clone();
    tokio::spawn(async move {
        let started = Instant::now();
        let sessions = task_g.sessions();
        let mut trail = stored
            .as_ref()
            .map(|s| TrailWriter::with_durations(sessions, s, durations.clone()).behind(backups));

        if let Some(reused) = task_options.reused_user.clone() {
            lock_sink(&task_sink).on_stored(std::slice::from_ref(&reused), None);
        }
        // The question is written now — just not waited on. D1 answers one
        // HTTPS call per read and per insert, and a turn that waits for its own
        // bookkeeping makes the reader wait for it; the chat's queue keeps the
        // rows in order behind the answer instead.
        let rows = task_g.user_rows(stored.as_ref(), &history, &delta);
        if !rows.is_empty() {
            if let Some(found) = &stored {
                let g = task_g.clone();
                // A backup must not keep the stream open: a strong sink owns a
                // sender, and the body ends only when the last one drops. The
                // job reaches the watcher only once a write has landed, and only
                // while the reader is still there.
                let sink = Reporter::Behind(Arc::downgrade(&task_sink));
                let queue_id = found.id.clone();
                let session_id = found.id.clone();
                task_g.queue_backup(&queue_id, async move {
                    g.save_turns(&session_id, &rows, &sink).await;
                });
            }
        }

        let result = task_g
            .complete(
                &primary,
                &history,
                &task_options,
                &task_sink,
                trail.as_mut(),
            )
            .await;
        let resp = match result {
            Ok(resp) => resp,
            Err(err) => {
                tracing::warn!(
                    "gateway: {}/{} turn failed: {err}",
                    primary.prov.name,
                    primary.model
                );
                let payload =
                    json!({"error": {"message": err.to_string(), "type": "upstream_error"}});
                write_sse(&tx, &payload);
                write_sse_done(&tx);
                if let Some(found) = &stored {
                    task_g.end_turn(&found.id);
                }
                return;
            }
        };
        let duration = started.elapsed().as_millis() as i64;
        // The rows are built from the answer in memory, so the frame that ends
        // the turn needs no write to have landed. Only its seq is unknown until
        // D1 keeps it, which is what `turns_synced: false` tells the reader.
        let (rows, title) = task_g.answer_rows(&delta, &resp, trail.as_ref(), duration);
        let answer = stored_turn(
            stored.as_ref().map(|s| s.id.as_str()).unwrap_or(""),
            0,
            unix_now(),
            &rows[rows.len() - 1],
        );
        if let Some(found) = &stored {
            let g = task_g.clone();
            let sink = Reporter::Behind(Arc::downgrade(&task_sink));
            let stored_session = found.clone();
            let model = resp.model.clone();
            let provider = resp.provider.clone();
            let id = found.id.clone();
            task_g.queue_backup(&id, async move {
                g.persist_answer(
                    Some(&stored_session),
                    &rows,
                    &title,
                    &provider,
                    &model,
                    &sink,
                )
                .await;
            });
        }
        if let Some(found) = &stored {
            task_g.end_turn(&found.id);
        }

        let finish = if resp.finish_reason.is_empty() {
            "stop".to_string()
        } else {
            resp.finish_reason.clone()
        };
        let final_frame = json!({
            "turn": serde_json::to_value(&answer).unwrap_or(Value::Null),
            // The rows this turn made are still on their way to the store, so
            // the reader cannot trust the seq it is handed: false is the app's
            // cue to read the chat back once it is ready.
            "turns_synced": false,
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": resp.model,
            "provider": resp.provider,
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": finish,
            }],
            "usage": {
                "prompt_tokens": resp.usage.prompt_tokens,
                "completion_tokens": resp.usage.completion_tokens,
                "total_tokens": resp.usage.total_tokens,
                "cache_read_tokens": resp.usage.cache_read_tokens,
                "cache_write_tokens": resp.usage.cache_write_tokens,
                "reasoning_tokens": resp.usage.reasoning_tokens,
                "input_includes_cache": resp.usage.input_includes_cache,
            },
        });
        write_sse(&tx, &final_frame);
        write_sse_done(&tx);
    });

    response
}

/// The streaming watcher: every frame the phone reads while a turn runs.
struct SseSink {
    tx: tokio::sync::mpsc::UnboundedSender<Bytes>,
    id: String,
    created: i64,
    model: String,
    first: bool,
    /// The round the loop is spending now. Reasoning and tool events carry no
    /// round of their own — they fire inside the provider call — so the handler
    /// stamps the round that is open when each one arrives.
    open_round: i64,
    durations: Arc<Mutex<HashMap<String, i64>>>,
}

impl ChatSink for SseSink {
    fn emit(&mut self, delta: &str) -> anyhow::Result<()> {
        let mut body = json!({"content": delta});
        if self.first {
            body["role"] = Value::String("assistant".to_string());
            self.first = false;
        }
        let chunk = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": body, "finish_reason": Value::Null}],
        });
        write_sse(&self.tx, &chunk);
        Ok(())
    }

    fn on_request(&mut self, round: i64, state: &str) {
        self.open_round = round;
        let block = aixodia_session::request_block(round, state);
        write_sse(
            &self.tx,
            &json!({"tool_event": {
                "stage": "request", "round": round, "state": state,
                "block": block_json(&block),
            }}),
        );
    }

    fn on_usage(&mut self, usage: &Usage) {
        write_sse(
            &self.tx,
            &json!({"tool_event": {
                "stage": "usage", "round": self.open_round,
                "prompt_tokens": usage.prompt_tokens,
                "completion_tokens": usage.completion_tokens,
                "total_tokens": usage.total_tokens,
                "cache_read_tokens": usage.cache_read_tokens,
                "cache_write_tokens": usage.cache_write_tokens,
                "reasoning_tokens": usage.reasoning_tokens,
                "input_includes_cache": usage.input_includes_cache,
            }}),
        );
    }

    fn on_tool_call(&mut self, call: &ToolCall) {
        let block =
            aixodia_session::tool_block(&call.id, &call.name, &call.arguments, "", false, 0, false);
        write_sse(
            &self.tx,
            &json!({"tool_event": {
                "stage": "tool_call", "name": call.name, "call_id": call.id,
                "arguments": call.arguments, "round": self.open_round,
                "block": block_json(&block),
            }}),
        );
    }

    fn on_reasoning(&mut self, text: &str) -> anyhow::Result<()> {
        let block = aixodia_session::reasoning_block(self.open_round, text);
        write_sse(
            &self.tx,
            &json!({"tool_event": {
                "stage": "reasoning", "round": self.open_round, "text": text,
                "block": block_json(&block),
            }}),
        );
        Ok(())
    }

    fn on_compact(&mut self, report: &aixodia_session::Compacted) {
        write_sse(
            &self.tx,
            &json!({"tool_event": {
                "stage": "compact", "round": self.open_round,
                "kept_turns": report.kept_turns as i64,
                "dropped_turns": report.dropped_turns as i64,
                "summary": report.summary,
            }}),
        );
    }

    fn on_tool_result(&mut self, call: &ToolCall, text: &str, duration_ms: i64, is_error: bool) {
        self.durations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(call.id.clone(), duration_ms);
        let text = truncate(text, TOOL_EVENT_CEILING);
        let block = aixodia_session::tool_block(
            &call.id,
            &call.name,
            &call.arguments,
            &text,
            is_error,
            duration_ms,
            true,
        );
        write_sse(
            &self.tx,
            &json!({"tool_event": {
                "stage": "tool_result", "name": call.name, "call_id": call.id,
                "text": text, "is_error": is_error, "duration_ms": duration_ms,
                "round": self.open_round,
                "block": block_json(&block),
            }}),
        );
    }

    fn on_stored(&mut self, rows: &[Turn], _error: Option<&str>) {
        if !rows.is_empty() {
            write_sse(
                &self.tx,
                &json!({"turns": rows.iter().map(turn_value).collect::<Vec<_>>()}),
            );
        }
    }
}

fn block_json(block: &Block) -> Value {
    serde_json::from_str(&block.json()).unwrap_or(Value::Null)
}

fn turn_value(turn: &Turn) -> Value {
    serde_json::to_value(turn).unwrap_or(Value::Null)
}

fn write_sse(tx: &tokio::sync::mpsc::UnboundedSender<Bytes>, payload: &Value) {
    let encoded = serde_json::to_string(payload).unwrap_or_else(|_| "{}".to_string());
    let _ = tx.send(Bytes::from(format!("data: {encoded}\n\n")));
}

fn write_sse_done(tx: &tokio::sync::mpsc::UnboundedSender<Bytes>) {
    let _ = tx.send(Bytes::from_static(b"data: [DONE]\n\n"));
}

impl Gateway {
    pub(crate) async fn resolve_route(
        &self,
        stored: Option<&Session>,
        body: &ChatRequest,
    ) -> anyhow::Result<Route> {
        let mut provider_name = body.provider.clone();
        let mut model_id = body.model.clone();
        if let Some(stored) = stored {
            if provider_name.is_empty() {
                provider_name = stored.provider.clone();
            }
            if model_id.is_empty() {
                model_id = stored.model.clone();
            }
        }
        if provider_name.trim().is_empty() && model_id.trim().is_empty() {
            // A new chat is created with no route and the phone leaves its
            // picker empty until the user chooses, so fall back to the saved
            // main: the first message answers instead of failing for a missing
            // model that the settings already name.
            let (provider, model) = self.saved_main_model().await;
            provider_name = provider;
            model_id = model;
        }
        let model_id = model_id.trim().to_string();
        if model_id.is_empty() {
            anyhow::bail!("model is required (use provider/model or the provider field)");
        }

        if !provider_name.is_empty() {
            let Some(cfg) = aixodia_config::get_provider_by_name(&provider_name) else {
                anyhow::bail!(
                    "unknown provider {provider_name:?} (known: {})",
                    provider_names()
                );
            };
            let adapter = self.registry().require(&cfg.adapter)?;
            let model = trim_provider_prefix(&model_id, &cfg.name);
            if model.is_empty() {
                anyhow::bail!("model is required for provider {:?}", cfg.name);
            }
            return Ok(Route {
                adapter,
                prov: to_internal_provider(&cfg),
                model,
            });
        }

        if let Some((prefix, rest)) = model_id.split_once('/') {
            if let Some(cfg) = aixodia_config::get_provider_by_name(prefix) {
                let adapter = self.registry().require(&cfg.adapter)?;
                return Ok(Route {
                    adapter,
                    prov: to_internal_provider(&cfg),
                    model: rest.to_string(),
                });
            }
        }
        anyhow::bail!(
            "unknown model {model_id:?}: prefix it with a provider, for example provider/{model_id} (known: {})",
            provider_names()
        )
    }

    pub(crate) async fn complete(
        &self,
        primary: &Route,
        history: &[Message],
        options: &CompletionOptions,
        sink: &SinkRef,
        mut trail: Option<&mut TrailWriter>,
    ) -> anyhow::Result<Response> {
        // The identity block is a fixed prefix, which also keeps the provider's
        // prompt cache useful across turns.
        let mut conversation: Vec<Message> = Vec::with_capacity(history.len() + 1);
        conversation.push(Message {
            role: "system".to_string(),
            content: AIXODIA_SYSTEM_PROMPT.to_string(),
            ..Message::default()
        });
        conversation.extend_from_slice(history);

        let current = primary.clone();
        let mut window = options.window;
        if window.max <= 0 {
            window.max = DEFAULT_MAX_TOKENS;
        }

        let emitted = Arc::new(AtomicUsize::new(0));
        let can_retry = {
            let emitted = emitted.clone();
            let stream = options.stream;
            move || !stream || emitted.load(Ordering::SeqCst) == 0
        };

        let mut final_response = Response::default();
        let mut served = String::new();
        let mut pending = false;
        let mut remind = ReminderState::default();

        let mut round: i64 = 0;
        while round < MAX_TOOL_ROUNDS {
            lock_sink(sink).on_request(round + 1, STATE_WAITING);
            let (messages, whole, compaction) = self
                .prepare_messages(&conversation, window, &options.session_id, &mut remind)
                .await;
            if let Some(report) = compaction {
                lock_sink(sink).on_compact(&report);
            }
            conversation = messages.clone();
            let request = build_request(&current, &messages, options, sink, true);

            let call_started = Instant::now();
            let result = self
                .call(&current, &request, options, sink, &emitted, &can_retry)
                .await;
            final_response.generation_ms += call_started.elapsed().as_millis() as i64;
            let resp = match result {
                Ok(resp) => resp,
                Err(err) => {
                    lock_sink(sink).on_request(round + 1, STATE_DONE);
                    return Err(err);
                }
            };
            round += 1;
            if served.is_empty() {
                served = current.prov.name.clone();
            }
            // The provider counted exactly the prompt it received. When that
            // prompt was the whole conversation, the count describes the head of
            // the conversation and anchors the next round; when it was a
            // compacted window, it describes messages that are no longer a
            // prefix, so it is dropped.
            if !whole {
                window = Window::new(window.max);
            } else if resp.usage.prompt_tokens > 0 {
                window = Window {
                    max: window.max,
                    usage: resp.usage.prompt_tokens,
                    messages: messages.len(),
                };
            }

            final_response.id = resp.id.clone();
            final_response.model = current.model.clone();
            final_response.provider = served.clone();
            // The last round's text is the answer. Earlier rounds are narration
            // before a tool call; they stay with that tool step in the trail.
            final_response.content = resp.content.clone();
            final_response.reasoning_content = truncate(&resp.reasoning_content, REASONING_CEILING);
            add_usage(&mut final_response.usage, &resp.usage);
            lock_sink(sink).on_usage(&final_response.usage);
            final_response.finish_reason = resp.finish_reason.clone();

            pending = !resp.tool_calls.is_empty();
            if !pending {
                lock_sink(sink).on_request(round, STATE_DONE);
                break;
            }

            let assistant = Message {
                role: "assistant".to_string(),
                content: resp.content.clone(),
                tool_calls: resp.tool_calls.clone(),
                reasoning_content: final_response.reasoning_content.clone(),
                ..Message::default()
            };
            conversation.push(assistant.clone());
            final_response.trail.push(assistant);

            for call in &resp.tool_calls {
                lock_sink(sink).on_tool_call(call);
                let started = Instant::now();
                let (output, images) = self.run_tool(call, options).await;
                // A successful goal or plan change means the next model round
                // must see the fresh list.
                if !is_tool_error(&output)
                    && (call.name.starts_with("goal_") || call.name.starts_with("plan_"))
                {
                    remind.dirty = true;
                }
                let duration_ms = started.elapsed().as_millis() as i64;
                let is_error = is_tool_error(&output);
                lock_sink(sink).on_tool_result(call, &output, duration_ms, is_error);
                let result_message = Message {
                    role: "tool".to_string(),
                    content: output,
                    tool_call_id: call.id.clone(),
                    tool_name: call.name.clone(),
                    ..Message::default()
                };
                conversation.push(result_message.clone());
                final_response.trail.push(result_message);
                // A picture the tool returned cannot ride on the tool result:
                // both wire shapes take only a string there. It goes in its own
                // message so the model can see what the tool actually produced.
                // It stays out of the trail, so it is neither stored nor
                // replayed on the next turn.
                if !images.is_empty() {
                    conversation.push(Message {
                        role: "user".to_string(),
                        content: format!(
                            "The picture the {} tool returned is attached below.",
                            call.name
                        ),
                        images,
                        ..Message::default()
                    });
                }
            }
            if let Some(writer) = trail.as_mut() {
                writer.flush(&final_response.trail, sink).await;
            }
            lock_sink(sink).on_request(round, STATE_DONE);
            final_response.finish_reason = "tool_calls".to_string();
        }

        if pending {
            // The tool budget for this turn is spent. Taking the tools away
            // alone is not enough: the model keeps narrating and that narration
            // is stored as the answer, so the session stops in the middle of the
            // work looking like it was about to continue.
            conversation.push(Message {
                role: "user".to_string(),
                content: OUT_OF_ROUNDS.to_string(),
                ..Message::default()
            });
            let extra_round = MAX_TOOL_ROUNDS + 1;
            lock_sink(sink).on_request(extra_round, STATE_WAITING);
            let (messages, _, compaction) = self
                .prepare_messages(&conversation, window, &options.session_id, &mut remind)
                .await;
            if let Some(report) = compaction {
                lock_sink(sink).on_compact(&report);
            }
            let request = build_request(&current, &messages, options, sink, false);

            let call_started = Instant::now();
            let result = self
                .call(&current, &request, options, sink, &emitted, &can_retry)
                .await;
            final_response.generation_ms += call_started.elapsed().as_millis() as i64;
            let resp = match result {
                Ok(resp) => resp,
                Err(err) => {
                    lock_sink(sink).on_request(extra_round, STATE_DONE);
                    return Err(err);
                }
            };
            final_response.content = resp.content.clone();
            final_response.reasoning_content = truncate(&resp.reasoning_content, REASONING_CEILING);
            add_usage(&mut final_response.usage, &resp.usage);
            lock_sink(sink).on_usage(&final_response.usage);
            final_response.finish_reason = resp.finish_reason.clone();
            lock_sink(sink).on_request(extra_round, STATE_DONE);
        }
        Ok(final_response)
    }

    pub(crate) async fn call(
        &self,
        target: &Route,
        request: &Request,
        options: &CompletionOptions,
        sink: &SinkRef,
        emitted: &Arc<AtomicUsize>,
        can_retry: &(dyn Fn() -> bool + Send + Sync),
    ) -> anyhow::Result<Response> {
        let now = Instant::now();
        if self.circuit_open(&target.prov.name, now) {
            return Err(aixodia_provider::APIError::new(
                503,
                format!(
                    "provider {} is paused after repeated failures",
                    target.prov.name
                ),
            )
            .into_anyhow());
        }
        let result = self
            .call_keys(target, request, options, sink, emitted, can_retry)
            .await;
        if !options.cancel.is_cancelled() {
            self.record_outcome(&target.prov.name, result.as_ref().err(), now);
        }
        result
    }

    pub(crate) async fn call_keys(
        &self,
        target: &Route,
        request: &Request,
        options: &CompletionOptions,
        sink: &SinkRef,
        emitted: &Arc<AtomicUsize>,
        can_retry: &(dyn Fn() -> bool + Send + Sync),
    ) -> anyhow::Result<Response> {
        let deadline = tokio::time::Instant::now() + UPSTREAM_WAIT;

        // Non-login mode: a keyless provider is sent one empty key, so the
        // rotation math below still describes what is being tried.
        let keys: Vec<String> = if target.prov.keys.is_empty() {
            vec![String::new()]
        } else {
            target.prov.keys.clone()
        };
        let start = if target.prov.keys.is_empty() {
            0
        } else {
            self.next_key(&target.prov.name)
        };

        let keyless = target.prov.keys.is_empty();
        let attempts = if keyless {
            KEYLESS_ATTEMPTS
        } else {
            KEY_ATTEMPTS
        };
        let mut last_err: Option<anyhow::Error> = None;
        let mut emitter = SinkEmitter {
            sink: sink.clone(),
            emitted: emitted.clone(),
        };

        for attempt in 0..attempts {
            if attempt > 0 && !can_retry() {
                break;
            }
            if attempt > 0 {
                let wait = if keyless {
                    let scheduled = KEYLESS_BACKOFF[(attempt - 1).min(KEYLESS_BACKOFF.len() - 1)];
                    // Sleep the window the upstream named instead of the table: a
                    // 4-second throttle is gone in 4 seconds, and a 25-second one
                    // is not woken by asking again at 3. `retryable` only passes a
                    // wait the budget covers, so this cannot outlive the turn.
                    stated_wait(last_err.as_ref())
                        .unwrap_or(scheduled)
                        .max(scheduled)
                } else {
                    std::time::Duration::from_millis(750 * attempt as u64)
                };
                tokio::select! {
                    _ = options.cancel.cancelled() => return Err(Cancelled.into()),
                    _ = tokio::time::sleep_until(deadline) => {
                        return Err(anyhow::anyhow!("the upstream call timed out"));
                    }
                    _ = tokio::time::sleep(wait) => {}
                }
            }
            let key = keys[(start + attempt) % keys.len()].clone();
            let call = async {
                if request.stream {
                    target
                        .adapter
                        .stream(request, &target.prov, &key, &mut emitter)
                        .await
                } else {
                    target.adapter.complete(request, &target.prov, &key).await
                }
            };
            let result = tokio::select! {
                _ = options.cancel.cancelled() => Err(Cancelled.into()),
                _ = tokio::time::sleep_until(deadline) => {
                    Err(anyhow::anyhow!("the upstream call timed out"))
                }
                result = call => result,
            };
            match result {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    let asked_for_a_key = keyless
                        && err
                            .downcast_ref::<aixodia_provider::APIError>()
                            .is_some_and(|api| api.status_code == 401);
                    if asked_for_a_key
                        || !aixodia_provider::retryable(&err)
                        || options.cancel.is_cancelled()
                    {
                        last_err = Some(err);
                        break;
                    }
                    last_err = Some(err);
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("the upstream call failed")))
    }

    /// Executes one tool call and returns its text plus any pictures it produced.
    /// Pictures are lifted out of the text so the base64 never reaches the prompt
    /// and the model can be shown the image instead.
    pub(crate) async fn run_tool(
        &self,
        call: &ToolCall,
        options: &CompletionOptions,
    ) -> (String, Vec<Image>) {
        if !is_objective_tool(&call.name) {
            match self.has_active_objective(&options.session_id).await {
                Err(err) => {
                    return (
                        tool_error(&format!("cannot check the active goal or plan: {err}")),
                        Vec::new(),
                    )
                }
                Ok(false) => return (tool_error(NO_ACTIVE_OBJECTIVE_MESSAGE), Vec::new()),
                Ok(true) => {}
            }
        }
        let Some(tool) = self.tool(&call.name) else {
            return (
                tool_error(&format!("unknown tool {:?}", call.name)),
                Vec::new(),
            );
        };
        let context = ToolContext {
            session_id: options.session_id.clone(),
            store: self.objective_store(),
        };
        match tool.run(context, &call.arguments).await {
            Err(err) => (tool_error(&err.to_string()), Vec::new()),
            Ok(result) => {
                let (text, images) = extract_tool_images(&result);
                (truncate(&text, TOOL_RESULT_CEILING), images)
            }
        }
    }

    /// The gate for ordinary tools. Goal and plan tools are exempt so the model
    /// can record the objective it is missing. A chat without a session id, or
    /// an engine without objective storage, keeps the old behaviour.
    pub(crate) async fn has_active_objective(&self, session_id: &str) -> anyhow::Result<bool> {
        let Some(store) = self.objective_store() else {
            return Ok(true);
        };
        if session_id.trim().is_empty() {
            return Ok(true);
        }
        let items = store.open(session_id).await?;
        Ok(!items.is_empty())
    }

    /// The rows the new messages add to the transcript, or none when there is
    /// nothing new — a question already recorded by an earlier failed attempt is
    /// not stacked again.
    pub(crate) fn user_rows(
        &self,
        stored: Option<&Session>,
        history: &[Message],
        delta: &[Message],
    ) -> Vec<TurnInput> {
        if stored.is_none() || delta.is_empty() {
            return Vec::new();
        }
        let recorded: Vec<Message> = if history.len() >= delta.len() {
            history[..history.len() - delta.len()].to_vec()
        } else {
            history.to_vec()
        };
        if is_repeat_of_tail(&recorded, delta) {
            return Vec::new();
        }
        delta
            .iter()
            .filter(|message| message.role == "user")
            .map(|message| TurnInput::text(message.role.clone(), message.content.clone()))
            .collect()
    }

    /// Appends rows and tells the watcher what the store gave back.
    pub(crate) async fn save_turns(&self, session_id: &str, turns: &[TurnInput], sink: &Reporter) {
        let appended = self.sessions.append_turns_partial(session_id, turns).await;
        let rows = appended.rows;
        let error = appended.error.map(|err| err.to_string());
        sink.report(&rows, error.as_deref());
        if let Some(err) = error {
            tracing::warn!("gateway: save turns: {err}");
        }
    }

    /// Records the user's new turns before the upstream call, so a failed send
    /// still leaves its question in history instead of vanishing.
    pub(crate) async fn persist_user_before(
        &self,
        stored: Option<&Session>,
        history: &[Message],
        delta: &[Message],
        sink: &SinkRef,
    ) {
        let Some(stored) = stored else {
            return;
        };
        let turns = self.user_rows(Some(stored), history, delta);
        if turns.is_empty() {
            return;
        }
        self.save_turns(&stored.id, &turns, &Reporter::Live(sink.clone()))
            .await;
    }

    /// The rows this turn made that the store does not have yet: the tool steps
    /// the trail still owes, then the answer that closes them.
    pub(crate) fn answer_rows(
        &self,
        delta: &[Message],
        resp: &Response,
        trail: Option<&TrailWriter>,
        duration: i64,
    ) -> (Vec<TurnInput>, String) {
        // The incoming question has already been persisted before upstream was
        // called, so here we only append the assistant reply (and the tool trail
        // that belongs to it). Appending it again would duplicate it.
        let mut title = String::new();
        for message in delta {
            if title.is_empty() && message.role == "user" && !message.content.trim().is_empty() {
                title = truncate(message.content.trim(), 60);
            }
        }

        let mut turns: Vec<TurnInput> = Vec::new();
        let mut pending_snapshot: Option<(Vec<TurnInput>, usize)> = None;
        if let Some(writer) = trail {
            turns.extend(trail_turns(&writer.rest(&resp.trail)));
            if let Some(pending) = &writer.pending {
                pending_snapshot = Some((pending.clone(), writer.pending_end));
            }
        }
        if let Some((pending, pending_end)) = pending_snapshot {
            let rest = if pending_end <= resp.trail.len() {
                trail_turns(&resp.trail[pending_end..])
            } else {
                Vec::new()
            };
            turns = pending;
            turns.extend(rest);
        }

        let mut generation = resp.generation_ms;
        if generation <= 0 || generation > duration {
            // Either the provider reported no time of its own, or it reported
            // more than the turn took. Either way the wall clock is the honest
            // fallback.
            generation = duration;
        }
        turns.push(TurnInput {
            role: "assistant".to_string(),
            text: resp.content.clone(),
            block: answer_block(&resp.content),
            model: resp.model.clone(),
            reasoning_content: resp.reasoning_content.clone(),
            input_tokens: resp.usage.prompt_tokens,
            output_tokens: resp.usage.completion_tokens,
            cache_read_tokens: resp.usage.cache_read_tokens,
            cache_write_tokens: resp.usage.cache_write_tokens,
            reasoning_tokens: resp.usage.reasoning_tokens,
            input_includes_cache: resp.usage.input_includes_cache,
            duration_ms: duration,
            generation_ms: generation,
        });
        (turns, title)
    }

    /// Writes a finished turn's rows and the route it answered on, and answers
    /// with the stored reply. The local copy is the answer when the store gave
    /// no row back: it is what the reader already saw, only unnumbered.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn persist_answer(
        &self,
        stored: Option<&Session>,
        turns: &[TurnInput],
        title: &str,
        provider: &str,
        model: &str,
        sink: &Reporter,
    ) -> Turn {
        let session_id = stored.map(|s| s.id.clone()).unwrap_or_default();
        let mut answer = stored_turn(&session_id, 0, unix_now(), &turns[turns.len() - 1]);
        let Some(stored) = stored else {
            return answer;
        };
        let appended = self.sessions.append_turns_partial(&stored.id, turns).await;
        let rows = appended.rows;
        let error = appended.error;
        sink.report(&rows, error.as_ref().map(|err| err.to_string()).as_deref());
        if rows.len() == turns.len() {
            answer = rows[rows.len() - 1].clone();
        }
        if let Some(err) = error {
            tracing::warn!("gateway: save turns: {err}");
        }

        let mut saved = Session {
            provider: provider.to_string(),
            model: model.to_string(),
            ..Session::default()
        };
        if stored.title.is_empty() {
            saved.title = title.to_string();
        }
        for attempt in 0..3 {
            match self.sessions.save_config(&stored.id, &saved).await {
                Ok(()) => break,
                Err(err) => {
                    if attempt == 2 {
                        tracing::warn!("gateway: save session config: {err}");
                    }
                }
            }
        }
        answer
    }

    /// The non-streaming turn's answer: build its rows and wait for the store to
    /// keep them, since there is no stream to hand back until they are kept.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn persist(
        &self,
        stored: Option<&Session>,
        delta: &[Message],
        resp: &Response,
        trail: Option<&mut TrailWriter>,
        sink: &SinkRef,
        duration: i64,
    ) -> Turn {
        let (turns, title) = self.answer_rows(delta, resp, trail.as_deref(), duration);
        self.persist_answer(
            stored,
            &turns,
            &title,
            &resp.provider,
            &resp.model,
            &Reporter::Live(sink.clone()),
        )
        .await
    }
}

fn build_request(
    current: &Route,
    messages: &[Message],
    options: &CompletionOptions,
    sink: &SinkRef,
    include_tools: bool,
) -> Request {
    let on_reasoning = {
        let sink = sink.clone();
        let callback: aixodia_provider::ReasoningFn =
            Arc::new(move |text: &str| lock_sink(&sink).on_reasoning(text));
        Some(callback)
    };
    Request {
        model: current.model.clone(),
        messages: messages.to_vec(),
        session_id: options.session_id.clone(),
        max_tokens: options.max_tokens,
        temperature: options.temperature,
        top_p: options.top_p,
        stream: options.stream,
        tools: if include_tools {
            options.tools.clone()
        } else {
            Vec::new()
        },
        user: options.user.clone(),
        on_reasoning,
    }
}

/// Folds one round's usage into the turn's running total. The cache and thinking
/// counts are summed like the token counts they come from: a turn that ran tools
/// made several calls, and the prompt the cache served on each is part of what
/// the turn cost. `input_includes_cache` is a property of the provider, not of
/// one round, so any round that says the prompt contains its cache half makes the
/// whole total say so.
pub fn add_usage(total: &mut Usage, round: &Usage) {
    total.prompt_tokens += round.prompt_tokens;
    total.completion_tokens += round.completion_tokens;
    total.total_tokens += round.total_tokens;
    total.cache_read_tokens += round.cache_read_tokens;
    total.cache_write_tokens += round.cache_write_tokens;
    total.reasoning_tokens += round.reasoning_tokens;
    if round.input_includes_cache {
        total.input_includes_cache = true;
    }
}

pub fn is_objective_tool(name: &str) -> bool {
    name.starts_with("goal_") || name.starts_with("plan_")
}

/// Whether tool output is a `toolError` envelope (a failed execution), as
/// opposed to genuine command output.
pub fn is_tool_error(output: &str) -> bool {
    let trimmed = output.trim_start();
    trimmed.starts_with("{\"error\":") || trimmed.starts_with("{\"error\" :")
}

pub fn tool_error(message: &str) -> String {
    json!({"error": message}).to_string()
}

/// The tools one turn offers. `None` means every builtin; `Some([])` means none.
pub fn select_tools(g: &Gateway, names: Option<&Vec<String>>) -> anyhow::Result<Vec<ToolDef>> {
    match names {
        None => {
            let mut all: Vec<ToolDef> = g.tools.values().map(|tool| tool.def.clone()).collect();
            all.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(all)
        }
        Some(names) => {
            let mut selected = Vec::with_capacity(names.len());
            for name in names {
                let Some(tool) = g.tool(name) else {
                    anyhow::bail!("unknown tool {name:?}");
                };
                selected.push(tool.def);
            }
            Ok(selected)
        }
    }
}

/// Whether the newest user turn has no closing answer after it. Narration stored
/// before a tool call is an assistant row too, so a plain role check would call
/// an interrupted tool loop finished: the closing row is the one whose display
/// block is the answer, recognised here the same way the phone recognises it.
pub fn has_unfinished_question(turns: &[Turn]) -> bool {
    let mut last_user: Option<usize> = None;
    for (index, turn) in turns.iter().enumerate() {
        if turn.role == "user" {
            last_user = Some(index);
        }
    }
    let Some(last_user) = last_user else {
        return false;
    };
    for turn in &turns[last_user + 1..] {
        if is_stored_answer(turn) {
            return false;
        }
    }
    true
}

pub fn is_stored_answer(turn: &Turn) -> bool {
    if let Some(block) = Block::parse(&turn.block) {
        if block.kind == aixodia_session::BLOCK_TEXT && block.id == "answer" {
            return true;
        }
    }
    // Rows written before display blocks still carry the final answer's footer;
    // narration before a tool call carries neither model nor usage.
    turn.role == "assistant"
        && (!turn.model.is_empty()
            || turn.duration_ms > 0
            || turn.input_tokens > 0
            || turn.output_tokens > 0)
}

/// The closing answer as a display block. Blank when the turn answered with
/// tools alone: an empty text block would draw nothing, and the row stays on the
/// role-based path where the empty-answer filter removes it.
pub fn answer_block(content: &str) -> String {
    if content.trim().is_empty() {
        return String::new();
    }
    text_block("answer", content).json()
}

/// Whether `delta` is a single user message identical to the last visible
/// recorded turn (which then necessarily has no answer yet).
pub fn is_repeat_of_tail(recorded: &[Message], delta: &[Message]) -> bool {
    if delta.len() != 1 || delta[0].role != "user" {
        return false;
    }
    let want = delta[0].content.trim();
    if want.is_empty() {
        return false;
    }
    for message in recorded.iter().rev() {
        if aixodia_session::trail::is_tool_message(message) {
            continue;
        }
        return message.role == "user" && message.content.trim() == want;
    }
    false
}

/// Seeds the size of a request from the provider's last measurement of this
/// history. The count is used only while the messages it covered are still the
/// head of what will be sent.
pub fn history_window(turns: &[Turn], saved: &[Message], history: &[Message]) -> Window {
    let mut window = Window::new(DEFAULT_MAX_TOKENS);
    let (messages, tokens) = anchor(turns);
    if tokens <= 0 || messages == 0 || messages > saved.len() || messages > history.len() {
        return window;
    }
    for index in 0..messages {
        if saved[index].role != history[index].role
            || saved[index].content != history[index].content
        {
            return window;
        }
    }
    window.usage = tokens;
    window.messages = messages;
    window
}

/// Matches the client's messages against the stored conversation. Clients only
/// ever see visible text, so matching skips stored tool steps; the steps that sit
/// between matched messages are kept in full.
pub fn merge_history(stored: &[Message], incoming: &[Message]) -> (Vec<Message>, Vec<Message>) {
    let visible: Vec<usize> = stored
        .iter()
        .enumerate()
        .filter(|(_, message)| !aixodia_session::trail::is_tool_message(message))
        .map(|(index, _)| index)
        .collect();

    let mut common = 0;
    while common < visible.len() && common < incoming.len() {
        let matched = &stored[visible[common]];
        if matched.role != incoming[common].role || matched.content != incoming[common].content {
            break;
        }
        common += 1;
    }

    if common == visible.len() {
        let mut full = stored.to_vec();
        full.extend_from_slice(&incoming[common..]);
        return (full, incoming[common..].to_vec());
    }
    if common == incoming.len() {
        return (stored.to_vec(), Vec::new());
    }
    let cut = if common > 0 {
        visible[common - 1] + 1
    } else {
        0
    };
    let mut full = stored[..cut].to_vec();
    full.extend_from_slice(&incoming[common..]);
    (full, incoming[common..].to_vec())
}

/// Keeps the stored conversation and adds only what is new.
///
/// It exists because `merge_history` matches a common prefix, which is right for
/// a client that resends its whole transcript but wrong for one that sends a
/// single new turn.
pub fn append_delta(stored: &[Message], incoming: &[Message]) -> (Vec<Message>, Vec<Message>) {
    let mut full = stored.to_vec();
    // A retry after a failed attempt reuses the question that was already
    // recorded, instead of stacking a twin the model would see twice.
    if let Some(last) = stored.last() {
        if incoming.len() == 1
            && !aixodia_session::trail::is_tool_message(last)
            && last.role == incoming[0].role
            && last.content == incoming[0].content
        {
            return (full, Vec::new());
        }
    }
    full.extend_from_slice(incoming);
    (full, incoming.to_vec())
}

pub fn to_internal_provider(cfg: &ConfigProvider) -> InternalProvider {
    InternalProvider {
        name: cfg.name.clone(),
        endpoint: cfg.api_url.clone(),
        keys: cfg.keys.clone(),
        adapter: cfg.adapter.clone(),
        model: String::new(),
    }
}

pub fn trim_provider_prefix(model: &str, provider: &str) -> String {
    let prefix = format!("{provider}/");
    if model.len() > prefix.len() && model[..prefix.len()].eq_ignore_ascii_case(&prefix) {
        return model[prefix.len()..].to_string();
    }
    model.to_string()
}

pub fn provider_names() -> String {
    let names: Vec<String> = aixodia_config::get_providers()
        .into_iter()
        .map(|provider| provider.name)
        .collect();
    if names.is_empty() {
        return "none".to_string();
    }
    names.join(", ")
}

pub fn write_upstream_error(err: &anyhow::Error) -> AxumResponse {
    write_error(StatusCode::BAD_GATEWAY, "upstream_error", &err.to_string())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
