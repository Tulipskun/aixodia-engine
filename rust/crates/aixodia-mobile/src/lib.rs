//! The WebSocket the AIxodia app talks to: one socket per phone, frames are
//! JSON, auth is the bearer token in the handshake only.
//!
//! Port of the Go `io/mobile` package. The RFC 6455 handshake and framing come
//! from `aixodia-wsock`, shared with the terminal socket.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use aixodia_session::{Block, Turn};
use aixodia_wsock::{Error as WsError, MessageWriter, ServerConn, OP_CLOSE, OP_TEXT};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::WriteHalf;
use tokio_util::sync::CancellationToken;

/// Answers one user message in a session, calling `on_delta` for each streamed
/// piece and `on_tool` for each live tool activity, and returns the committed
/// turn data. It must stop when `cancel` fires.
///
/// Attachments are the files the phone uploaded before sending it: the pictures
/// among them go to the model as pictures, and every one of them is still named
/// in the text for the model that cannot take them or for a later turn that
/// reads this back from the database.
///
/// `retry` asks the engine to resume the last unfinished question already stored
/// in the session instead of appending text as a new user turn.
#[derive(Clone)]
pub struct ChatCall {
    pub session_id: String,
    pub text: String,
    pub retry: bool,
    pub attachments: Vec<Attachment>,
    pub cancel: CancellationToken,
    pub on_delta: DeltaFn,
    pub on_tool: ToolFn,
}

/// Receives each streamed piece of the answer.
pub type DeltaFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Receives each live tool activity.
pub type ToolFn = Arc<dyn Fn(ToolEvent) + Send + Sync>;

/// The chat implementation the hub calls. `main.rs` builds one that drives the
/// gateway's HTTP chat route in-process.
pub type ChatFunc = Arc<
    dyn Fn(ChatCall) -> Pin<Box<dyn Future<Output = Result<ChatResult, String>> + Send>>
        + Send
        + Sync,
>;

/// Committed rows, not a socket-local sequence counter.
#[derive(Debug, Clone, Default)]
pub struct ChatResult {
    pub answer: Turn,
    pub turns: Vec<Turn>,
    pub turns_synced: bool,
}

/// One uploaded file as the phone describes it.
#[derive(Debug, Clone, Default)]
pub struct Attachment {
    pub name: String,
    pub path: String,
    pub mime: String,
}

/// One live tool activity inside a turn.
#[derive(Debug, Clone, Default)]
pub struct ToolEvent {
    pub stage: String,
    pub name: String,
    pub call_id: String,
    pub arguments: String,
    pub text: String,
    pub is_error: bool,
    pub duration_ms: i64,
    /// The provider request round the event belongs to.
    pub round: i64,
    /// Where that round or step stands (waiting/thinking/running/done/error).
    pub state: String,
    /// The turn's running token total, on the `usage` stage only.
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub reasoning_tokens: i64,
    pub input_includes_cache: bool,
}

/// One part of an incoming message. `type` rides the wire but the engine only
/// ever reads the text, exactly as the Go frame does.
#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
struct InContent {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    text: String,
}

/// One uploaded file as the frame describes it. `size` is on the wire and is
/// deliberately dropped: the engine reads the file itself.
#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
struct InAttachment {
    #[serde(default)]
    name: String,
    #[serde(default)]
    path: String,
    #[serde(default)]
    mime: String,
    #[serde(default)]
    size: i64,
}

/// One frame from the phone. `job_id` is carried for the phone's own bookkeeping
/// and is not read here.
#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
struct InFrame {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    content: Vec<InContent>,
    #[serde(default)]
    client_msg_id: String,
    #[serde(default)]
    job_id: String,
    #[serde(default)]
    attachments: Vec<InAttachment>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct ToolCallJson {
    id: String,
    name: String,
    arguments: String,
}

#[derive(Debug, Clone, Default, Serialize)]
struct ToolResultJson {
    id: String,
    name: String,
    text: String,
    is_error: bool,
}

/// The frame the phone reads. Field names and `omitempty` decisions are the wire
/// format, so they are transcribed from the Go struct one for one.
#[derive(Debug, Clone, Default, Serialize)]
struct OutFrame {
    source: String,
    session_id: String,
    kind: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    text: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    role: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    agent: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    client_msg_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    stage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call: Option<ToolCallJson>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_result: Option<ToolResultJson>,
    #[serde(skip_serializing_if = "is_zero")]
    tool_duration_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    block: Option<Block>,
    #[serde(skip_serializing_if = "is_zero")]
    seq: i64,
    #[serde(skip_serializing_if = "is_zero")]
    input_tokens: i64,
    #[serde(skip_serializing_if = "is_zero")]
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    reasoning_tokens: i64,
    #[serde(skip_serializing_if = "String::is_empty")]
    reasoning_content: String,
    input_includes_cache: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    model: String,
    #[serde(skip_serializing_if = "is_zero")]
    duration_ms: i64,
    #[serde(skip_serializing_if = "is_zero")]
    generation_ms: i64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    turns: Vec<Turn>,
    turns_synced: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    daemon_url: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    daemon_version: String,
    #[serde(skip_serializing_if = "is_zero")]
    daemon_heartbeat: i64,
    daemon_started: i64,
    daemon_online: bool,
}

fn is_zero(value: &i64) -> bool {
    *value == 0
}

/// Everything the hub needs of a peer's socket. It is a trait rather than a
/// concrete writer so the hub does not have to name the upgraded stream type.
#[async_trait::async_trait]
pub trait FrameWriter: Send + Sync {
    async fn write_frame(&mut self, op: u8, payload: &[u8]) -> Result<(), WsError>;
}

#[async_trait::async_trait]
impl<S> FrameWriter for MessageWriter<WriteHalf<S>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    async fn write_frame(&mut self, op: u8, payload: &[u8]) -> Result<(), WsError> {
        self.write_message(op, payload).await
    }
}

/// Tracks every connected phone so the daemon can push a frame to all of them at
/// once, with no polling and no second connection per phone.
#[derive(Default)]
pub struct Hub {
    peers: Mutex<Vec<Arc<Peer>>>,
}

struct Peer {
    writer: tokio::sync::Mutex<Box<dyn FrameWriter>>,
    /// The turns this peer is running, keyed by client message id: the chat they
    /// belong to, and the token that stops them.
    cancels: Mutex<HashMap<String, (String, CancellationToken)>>,
}

impl Peer {
    async fn send(&self, mut frame: OutFrame) -> Result<(), WsError> {
        frame.source = "ai".to_string();
        let data = serde_json::to_vec(&frame).unwrap_or_else(|_| b"{}".to_vec());
        self.writer.lock().await.write_frame(OP_TEXT, &data).await
    }

    fn register(&self, id: &str, session_id: &str, cancel: CancellationToken) {
        self.cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), (session_id.to_string(), cancel));
    }

    fn unregister(&self, id: &str) {
        self.cancels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    /// Stops the turns running for `session_id` and returns how many it stopped.
    /// An empty `session_id` stops every turn on the peer, which is what a
    /// dropped socket does. A turn whose own session is unknown is stopped too:
    /// it is better to stop a turn the phone asked to stop than to leave it
    /// running.
    fn cancel_session(&self, session_id: &str) -> usize {
        let mut cancels = self.cancels.lock().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<String> = cancels.keys().cloned().collect();
        let mut stopped = 0;
        for key in keys {
            let Some((turn_session, cancel)) = cancels.get(&key) else {
                continue;
            };
            if !session_id.is_empty() && !turn_session.is_empty() && turn_session != session_id {
                continue;
            }
            cancel.cancel();
            cancels.remove(&key);
            stopped += 1;
        }
        stopped
    }
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    fn add(&self, peer: Arc<Peer>) {
        self.peers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(peer);
    }

    fn remove(&self, peer: &Arc<Peer>) {
        self.peers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|other| !Arc::ptr_eq(other, peer));
    }

    /// Pushes one daemon frame to every connected phone, so discovery listens for
    /// the daemon's own announcements instead of reading the D1 row on a timer.
    /// No phones connected, no frame sent.
    pub async fn broadcast_daemon(
        &self,
        url: &str,
        version: &str,
        heartbeat: i64,
        started: i64,
        online: bool,
    ) -> Option<WsError> {
        let peers: Vec<Arc<Peer>> = self.peers.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let frame = OutFrame {
            kind: "daemon".to_string(),
            daemon_url: url.to_string(),
            daemon_version: version.to_string(),
            daemon_heartbeat: heartbeat,
            daemon_started: started,
            daemon_online: online,
            ..OutFrame::default()
        };
        let mut first_err = None;
        for peer in peers {
            if let Err(err) = peer.send(frame.clone()).await {
                if first_err.is_none() {
                    first_err = Some(err);
                }
            }
        }
        first_err
    }

    /// Serves an already-upgraded connection. The socket policy (bearer,
    /// upgrade) has happened by now and the hub only speaks frames.
    pub async fn serve<S>(self: Arc<Self>, conn: ServerConn<S>, chat: ChatFunc)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut reader, writer) = conn.split();
        let peer = Arc::new(Peer {
            writer: tokio::sync::Mutex::new(Box::new(writer)),
            cancels: Mutex::new(HashMap::new()),
        });
        self.add(peer.clone());

        // A socket that simply stops talking is a network event, not a decision
        // by the phone: the tunnel drops, the screen locks, the phone changes
        // network, and the turn running on it is cancelled with it — so the work
        // stops and the question is left unanswered. Only a close frame is the
        // phone saying it is finished, and that one cancels.
        loop {
            match reader.read_message().await {
                Err(_) => break,
                Ok((OP_TEXT, payload)) => match serde_json::from_slice::<InFrame>(&payload) {
                    Err(_) => {
                        let _ = peer
                            .send(OutFrame {
                                kind: "error".to_string(),
                                text: "frame ที่อ่านไม่ได้".to_string(),
                                ..OutFrame::default()
                            })
                            .await;
                    }
                    Ok(frame) => handle(&self, &peer, frame, &chat).await,
                },
                Ok((OP_CLOSE, _)) => {
                    peer.cancel_session("");
                    break;
                }
                Ok(_) => {}
            }
        }

        self.remove(&peer);
        peer.cancel_session("");
    }
}

/// Answers `hello` and `cancel` inline; a message runs in its own task so a
/// cancel frame can arrive while the reply is still streaming.
async fn handle(hub: &Arc<Hub>, peer: &Arc<Peer>, frame: InFrame, chat: &ChatFunc) {
    match frame.kind.as_str() {
        "hello" => {
            let _ = peer
                .send(OutFrame {
                    kind: "ack".to_string(),
                    session_id: frame.session_id.clone(),
                    ..OutFrame::default()
                })
                .await;
        }
        "cancel" => {
            // Nothing may be running for this chat — the turn already ended, or
            // the phone is holding a screen the daemon finished with. A stop
            // that gets no reply leaves the screen stuck showing a turn that is
            // not happening, so answer anyway.
            if peer.cancel_session(&frame.session_id) == 0 {
                let _ = peer
                    .send(OutFrame {
                        kind: "done".to_string(),
                        session_id: frame.session_id.clone(),
                        stage: "already_done".to_string(),
                        ..OutFrame::default()
                    })
                    .await;
            }
        }
        "retry" => {
            // The question is already in the session. Re-run the turn from what
            // the engine stored, without appending a twin user row.
            let _ = peer
                .send(OutFrame {
                    kind: "ack".to_string(),
                    session_id: frame.session_id.clone(),
                    client_msg_id: frame.client_msg_id.clone(),
                    ..OutFrame::default()
                })
                .await;
            let cancel = CancellationToken::new();
            let key = turn_key(&frame);
            peer.register(&key, &frame.session_id, cancel.clone());
            let call = ChatCall {
                session_id: frame.session_id.clone(),
                text: String::new(),
                retry: true,
                attachments: attachments_of(&frame.attachments),
                cancel,
                on_delta: delta_fn(peer, &frame),
                on_tool: tool_fn(peer, &frame),
            };
            spawn_answer(hub, peer, key, frame, call, chat);
        }
        _ => {
            let mut parts: Vec<String> = frame
                .content
                .iter()
                .filter(|part| !part.text.is_empty())
                .map(|part| part.text.clone())
                .collect();
            let mut text = parts.join("\n");
            parts.clear();
            let _ = peer
                .send(OutFrame {
                    kind: "ack".to_string(),
                    session_id: frame.session_id.clone(),
                    client_msg_id: frame.client_msg_id.clone(),
                    ..OutFrame::default()
                })
                .await;
            if let Some(note) = attachment_note(&frame.attachments) {
                // The note travels with the message, so the stored turn shows
                // what was attached and the agent is told where to find it.
                if text.trim().is_empty() {
                    text = note;
                } else {
                    text = format!("{text}\n\n{note}");
                }
            }
            if text.trim().is_empty() {
                let _ = peer
                    .send(OutFrame {
                        kind: "error".to_string(),
                        session_id: frame.session_id.clone(),
                        client_msg_id: frame.client_msg_id.clone(),
                        text: "ข้อความว่าง".to_string(),
                        ..OutFrame::default()
                    })
                    .await;
                return;
            }
            // Register the cancel token before answering, so a cancel frame that
            // arrives right behind this message always finds it.
            let cancel = CancellationToken::new();
            let key = turn_key(&frame);
            peer.register(&key, &frame.session_id, cancel.clone());
            let call = ChatCall {
                session_id: frame.session_id.clone(),
                text,
                retry: false,
                attachments: attachments_of(&frame.attachments),
                cancel,
                on_delta: delta_fn(peer, &frame),
                on_tool: tool_fn(peer, &frame),
            };
            spawn_answer(hub, peer, key, frame, call, chat);
        }
    }
}

fn turn_key(frame: &InFrame) -> String {
    if frame.client_msg_id.is_empty() {
        frame.session_id.clone()
    } else {
        frame.client_msg_id.clone()
    }
}

fn delta_fn(peer: &Arc<Peer>, frame: &InFrame) -> DeltaFn {
    let peer = peer.clone();
    let session_id = frame.session_id.clone();
    let client_msg_id = frame.client_msg_id.clone();
    Arc::new(move |piece: &str| {
        let peer = peer.clone();
        let frame = OutFrame {
            kind: "delta".to_string(),
            session_id: session_id.clone(),
            client_msg_id: client_msg_id.clone(),
            text: piece.to_string(),
            role: "model".to_string(),
            agent: "main".to_string(),
            ..OutFrame::default()
        };
        tokio::spawn(async move {
            let _ = peer.send(frame).await;
        });
    })
}

fn tool_fn(peer: &Arc<Peer>, frame: &InFrame) -> ToolFn {
    let peer = peer.clone();
    let session_id = frame.session_id.clone();
    let client_msg_id = frame.client_msg_id.clone();
    Arc::new(move |event: ToolEvent| {
        let peer = peer.clone();
        let mut out = OutFrame {
            kind: "trace".to_string(),
            session_id: session_id.clone(),
            client_msg_id: client_msg_id.clone(),
            agent: "main".to_string(),
            stage: event.stage.clone(),
            tool_duration_ms: event.duration_ms,
            block: tool_block(&event),
            ..OutFrame::default()
        };
        match event.stage.as_str() {
            "tool_call" => {
                out.tool_call = Some(ToolCallJson {
                    id: event.call_id.clone(),
                    name: event.name.clone(),
                    arguments: event.arguments.clone(),
                });
            }
            "tool_result" => {
                out.tool_result = Some(ToolResultJson {
                    id: event.call_id.clone(),
                    name: event.name.clone(),
                    text: event.text.clone(),
                    is_error: event.is_error,
                });
                out.tool_call = Some(ToolCallJson {
                    id: event.call_id.clone(),
                    name: event.name.clone(),
                    arguments: String::new(),
                });
            }
            "usage" => {
                // The turn's running cost, re-stated after each provider round.
                // No text and no block: this frame moves the strip, not the
                // thread.
                out.input_tokens = event.prompt_tokens;
                out.output_tokens = event.completion_tokens;
                out.cache_read_tokens = event.cache_read_tokens;
                out.cache_write_tokens = event.cache_write_tokens;
                out.reasoning_tokens = event.reasoning_tokens;
                out.input_includes_cache = event.input_includes_cache;
            }
            _ => out.text = event.text.clone(),
        }
        tokio::spawn(async move {
            let _ = peer.send(out).await;
        });
    })
}

fn spawn_answer(
    hub: &Arc<Hub>,
    peer: &Arc<Peer>,
    key: String,
    frame: InFrame,
    call: ChatCall,
    chat: &ChatFunc,
) {
    let _ = hub;
    let peer = peer.clone();
    let chat = chat.clone();
    tokio::spawn(async move {
        answer(&peer, key, frame, call, chat).await;
    });
}

async fn answer(peer: &Arc<Peer>, key: String, frame: InFrame, call: ChatCall, chat: ChatFunc) {
    let cancelled = call.cancel.clone();
    let result = chat(call).await;
    peer.unregister(&key);

    if cancelled.is_cancelled() {
        let _ = peer
            .send(OutFrame {
                kind: "done".to_string(),
                session_id: frame.session_id.clone(),
                client_msg_id: frame.client_msg_id.clone(),
                stage: "cancelled".to_string(),
                turns: result.as_ref().map(|r| r.turns.clone()).unwrap_or_default(),
                turns_synced: result.as_ref().map(|r| r.turns_synced).unwrap_or(false),
                ..OutFrame::default()
            })
            .await;
        return;
    }

    let result = match result {
        Ok(result) => result,
        Err(err) => {
            tracing::warn!("mobile: chat: {err}");
            let _ = peer
                .send(OutFrame {
                    kind: "error".to_string(),
                    session_id: frame.session_id.clone(),
                    client_msg_id: frame.client_msg_id.clone(),
                    text: err,
                    ..OutFrame::default()
                })
                .await;
            return;
        }
    };

    let answer = result.answer.clone();
    let block = Block::parse(&answer.block);
    let _ = peer
        .send(OutFrame {
            kind: "message".to_string(),
            session_id: frame.session_id.clone(),
            client_msg_id: frame.client_msg_id.clone(),
            text: answer.text.clone(),
            role: "model".to_string(),
            agent: "main".to_string(),
            seq: answer.seq,
            block,
            input_tokens: answer.input_tokens,
            output_tokens: answer.output_tokens,
            cache_read_tokens: answer.cache_read_tokens,
            cache_write_tokens: answer.cache_write_tokens,
            reasoning_tokens: answer.reasoning_tokens,
            reasoning_content: answer.reasoning_content.clone(),
            input_includes_cache: answer.input_includes_cache,
            model: answer.model.clone(),
            duration_ms: answer.duration_ms,
            generation_ms: answer.generation_ms,
            ..OutFrame::default()
        })
        .await;
    let _ = peer
        .send(OutFrame {
            kind: "done".to_string(),
            session_id: frame.session_id.clone(),
            client_msg_id: frame.client_msg_id.clone(),
            seq: answer.seq,
            turns: result.turns,
            turns_synced: result.turns_synced,
            ..OutFrame::default()
        })
        .await;
}

/// Describes a live tool event the way the stored row will: one block per event,
/// keyed the same, so the live thread and the stored thread it becomes are the
/// same blocks. Unknown stages carry no block and take the text path, the way an
/// older daemon's frames do.
fn tool_block(event: &ToolEvent) -> Option<Block> {
    match event.stage.as_str() {
        "tool_call" => Some(aixodia_session::tool_block(
            &event.call_id,
            &event.name,
            &event.arguments,
            "",
            false,
            0,
            false,
        )),
        "tool_result" => Some(aixodia_session::tool_block(
            &event.call_id,
            &event.name,
            &event.arguments,
            &event.text,
            event.is_error,
            event.duration_ms,
            true,
        )),
        "request" => Some(aixodia_session::request_block(event.round, &event.state)),
        "reasoning" => Some(aixodia_session::reasoning_block(event.round, &event.text)),
        _ => None,
    }
}

/// Carries the uploaded files from the socket frame into the answer call, where
/// the ones that are pictures can be sent to the model.
fn attachments_of(items: &[InAttachment]) -> Vec<Attachment> {
    items
        .iter()
        .map(|item| Attachment {
            name: item.name.clone(),
            path: item.path.clone(),
            mime: item.mime.clone(),
        })
        .collect()
}

/// The line appended to a message that carried files, so the agent knows what
/// was attached and where each file sits on disk.
fn attachment_note(items: &[InAttachment]) -> Option<String> {
    if items.is_empty() {
        return None;
    }
    let mut out = String::from("[ไฟล์แนบ]");
    for item in items {
        out.push_str("\n- ");
        if item.name.is_empty() {
            out.push_str("ไฟล์");
        } else {
            out.push_str(&item.name);
        }
        if !item.path.is_empty() {
            out.push_str(" → ");
            out.push_str(&item.path);
        }
        if !item.mime.is_empty() {
            out.push_str(" (");
            out.push_str(&item.mime);
            out.push(')');
        }
    }
    out.push_str("\nเปิดอ่านได้ด้วย tool read/bash ตามพาธด้านบน");
    Some(out)
}

/// Upgrades `/ws` to a WebSocket after checking the bearer token, and serves it
/// from the hub so the connection is registered for broadcasts.
pub fn handler(
    request: axum::extract::Request,
    hub: Arc<Hub>,
    token: String,
    chat: ChatFunc,
) -> Response {
    if !aixodia_wsock::bearer_ok(request.headers(), &token) {
        return (StatusCode::UNAUTHORIZED, "token rejected").into_response();
    }
    aixodia_wsock::accept(request, move |conn| async move {
        hub.serve(conn, chat).await;
    })
}

/// The frame counter the hub hands out when a caller wants a stable peer label.
pub fn next_peer_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

/// A JSON view of one frame, for callers that want to inspect what was sent.
pub fn frame_json(frame: &Value) -> String {
    frame.to_string()
}
