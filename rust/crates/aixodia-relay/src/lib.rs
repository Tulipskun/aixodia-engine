//! The phone's multiplexed socket: chat, terminal streams and every HTTP call
//! as rpc frames over one connection.
//!
//! Port of `io/relay`. The Go package also carries an outbound mode that dials
//! the Cloudflare relay; this deployment puts a quick tunnel in front of the
//! engine and phones connect straight to [`handler`], so only that transport is
//! built here.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use base64::Engine as _;
use serde_json::{json, Map, Value};
use tokio::io::{DuplexStream, WriteHalf};
use tokio::sync::{mpsc, Mutex};
use tower::ServiceExt;

use aixodia_mobile::ChatFunc;
use aixodia_terminal::Server as TerminalServer;
use aixodia_wsock::{MessageWriter, ServerConn, OP_BINARY, OP_CLOSE, OP_PING, OP_TEXT};

/// Terminal traffic rides binary frames prefixed with this channel byte and a
/// two-byte stream id.
pub const CH_TERM: u8 = 0x02;

/// What the relay needs from the engine.
#[derive(Clone)]
pub struct Config {
    pub token: String,
    pub api_token: String,
    pub hub: Arc<aixodia_mobile::Hub>,
    pub chat: ChatFunc,
    pub api: Router,
    pub term: Arc<TerminalServer>,
}

type Out = mpsc::UnboundedSender<(u8, Vec<u8>)>;

struct TermStream {
    writer: MessageWriter<WriteHalf<DuplexStream>>,
}

struct Session {
    cfg: Config,
    out: Out,
    chat: Mutex<Option<MessageWriter<WriteHalf<DuplexStream>>>>,
    terms: Mutex<HashMap<u16, TermStream>>,
    uploads: Mutex<HashMap<String, Vec<u8>>>,
}

impl Session {
    fn new(cfg: Config, out: Out) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            out,
            chat: Mutex::new(None),
            terms: Mutex::new(HashMap::new()),
            uploads: Mutex::new(HashMap::new()),
        })
    }

    fn send(&self, op: u8, payload: Vec<u8>) {
        let _ = self.out.send((op, payload));
    }

    fn send_text(&self, value: Value) {
        self.send(OP_TEXT, value.to_string().into_bytes());
    }

    async fn chat_writer(self: &Arc<Self>) -> Option<MessageWriter<WriteHalf<DuplexStream>>> {
        let mut slot = self.chat.lock().await;
        if let Some(writer) = slot.as_ref() {
            return Some(writer.clone());
        }
        let (hub_side, relay_side) = tokio::io::duplex(1 << 20);
        let (mut relay_reader, relay_writer) = ServerConn::new(relay_side).split();
        *slot = Some(relay_writer.clone());
        drop(slot);

        let hub = self.cfg.hub.clone();
        let chat = self.cfg.chat.clone();
        tokio::spawn(async move {
            hub.serve(ServerConn::new(hub_side), chat).await;
        });

        let this = self.clone();
        tokio::spawn(async move {
            while let Ok((op, payload)) = relay_reader.read_message().await {
                match op {
                    OP_TEXT => {
                        if let Ok(mut map) = serde_json::from_slice::<Map<String, Value>>(&payload)
                        {
                            map.insert("ch".to_string(), Value::String("chat".to_string()));
                            this.send_text(Value::Object(map));
                        } else {
                            this.send(OP_TEXT, payload);
                        }
                    }
                    OP_CLOSE => break,
                    _ => this.send(op, payload),
                }
            }
        });
        Some(relay_writer)
    }

    async fn text(self: &Arc<Self>, payload: &[u8]) {
        let envelope: Value = match serde_json::from_slice(payload) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!("relay: unreadable frame: {err}");
                return;
            }
        };
        let ctl = envelope.get("ctl").and_then(Value::as_str).unwrap_or("");
        if !ctl.is_empty() {
            return;
        }
        let ch = envelope.get("ch").and_then(Value::as_str).unwrap_or("");
        match ch {
            "" | "chat" => {
                if let Some(writer) = self.chat_writer().await {
                    let _ = writer.write_message(OP_TEXT, payload).await;
                }
            }
            "ctl" => {
                let act = envelope.get("act").and_then(Value::as_str).unwrap_or("");
                self.control(act, &envelope).await;
            }
            "term" => self.term(&envelope).await,
            "rpc" => {
                let more = envelope
                    .get("more")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if more {
                    self.append_chunk(&envelope).await;
                } else {
                    let this = self.clone();
                    let envelope = envelope.clone();
                    tokio::spawn(async move { this.rpc(envelope).await });
                }
            }
            other => tracing::warn!("relay: unknown channel {other:?}"),
        }
    }

    async fn binary(&self, payload: &[u8]) {
        if payload.len() < 3 || payload[0] != CH_TERM {
            return;
        }
        let sid = u16::from_be_bytes([payload[1], payload[2]]);
        let writer = {
            let terms = self.terms.lock().await;
            terms.get(&sid).map(|term| term.writer.clone())
        };
        if let Some(writer) = writer {
            let _ = writer.write_message(OP_BINARY, &payload[3..]).await;
        }
    }

    async fn control(self: &Arc<Self>, act: &str, envelope: &Value) {
        let sid = envelope.get("sid").and_then(Value::as_i64).unwrap_or(0) as u16;
        match act {
            "open" => {
                let id = envelope
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let cols = envelope.get("cols").and_then(Value::as_i64).unwrap_or(0);
                let rows = envelope.get("rows").and_then(Value::as_i64).unwrap_or(0);
                let (term_side, relay_side) = tokio::io::duplex(1 << 20);
                let (mut relay_reader, relay_writer) = ServerConn::new(relay_side).split();
                let this = self.clone();
                let term = self.cfg.term.clone();
                let attach_id = id.clone();
                tokio::spawn(async move {
                    let _ = term.attach(term_side, attach_id, cols, rows).await;
                });
                let forward = self.clone();
                let forward_id = id.clone();
                tokio::spawn(async move {
                    while let Ok((op, payload)) = relay_reader.read_message().await {
                        match op {
                            OP_BINARY => {
                                let mut frame = Vec::with_capacity(3 + payload.len());
                                frame.push(CH_TERM);
                                frame.extend_from_slice(&sid.to_be_bytes());
                                frame.extend_from_slice(&payload);
                                forward.send(OP_BINARY, frame);
                            }
                            OP_TEXT => {
                                if let Ok(p) = serde_json::from_slice::<Value>(&payload) {
                                    forward.send_text(json!({
                                        "ch": "term",
                                        "id": forward_id,
                                        "sid": sid,
                                        "k": "json",
                                        "p": p,
                                    }));
                                }
                            }
                            OP_CLOSE => break,
                            _ => {}
                        }
                    }
                });
                let old = {
                    let mut terms = self.terms.lock().await;
                    terms.insert(
                        sid,
                        TermStream {
                            writer: relay_writer,
                        },
                    )
                };
                if let Some(old) = old {
                    let _ = old.writer.write_message(OP_CLOSE, &[]).await;
                }
                let _ = this;
            }
            "close" => {
                let old = { self.terms.lock().await.remove(&sid) };
                if let Some(old) = old {
                    let _ = old.writer.write_message(OP_CLOSE, &[]).await;
                }
            }
            "bye" => {
                if let Some(writer) = self.chat_writer().await {
                    let _ = writer.write_message(OP_TEXT, br#"{"type":"cancel"}"#).await;
                }
            }
            _ => {}
        }
    }

    async fn term(self: &Arc<Self>, envelope: &Value) {
        let sid = envelope.get("sid").and_then(Value::as_i64).unwrap_or(0) as u16;
        let writer = {
            let terms = self.terms.lock().await;
            terms.get(&sid).map(|term| term.writer.clone())
        };
        let Some(writer) = writer else {
            return;
        };
        let kind = envelope.get("k").and_then(Value::as_str).unwrap_or("");
        match kind {
            "b64" => {
                let raw = envelope
                    .get("p")
                    .and_then(Value::as_str)
                    .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok());
                if let Some(raw) = raw {
                    let _ = writer.write_message(OP_BINARY, &raw).await;
                }
            }
            _ => {
                let p = envelope.get("p").cloned().unwrap_or(Value::Null);
                let _ = writer
                    .write_message(OP_TEXT, p.to_string().as_bytes())
                    .await;
            }
        }
    }

    async fn append_chunk(&self, envelope: &Value) {
        let rid = envelope.get("rid").and_then(Value::as_str).unwrap_or("");
        let chunk = envelope
            .get("b")
            .and_then(Value::as_str)
            .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok());
        let Some(chunk) = chunk else {
            return;
        };
        self.uploads
            .lock()
            .await
            .entry(rid.to_string())
            .or_default()
            .extend_from_slice(&chunk);
    }

    async fn rpc(self: &Arc<Self>, envelope: Value) {
        let rid = envelope.get("rid").and_then(Value::as_str).unwrap_or("");
        let reply = |status: StatusCode,
                     headers: Option<HeaderMap>,
                     body: Option<Vec<u8>>,
                     failure: &str| {
            let mut out = json!({"ch": "rpc", "rid": rid, "s": status.as_u16()});
            if !failure.is_empty() {
                out["e"] = Value::String(failure.to_string());
            }
            if let Some(headers) = headers {
                let mut flat = Map::new();
                for (key, value) in headers.iter() {
                    if let Ok(text) = value.to_str() {
                        flat.insert(key.as_str().to_string(), Value::String(text.to_string()));
                    }
                }
                if !flat.is_empty() {
                    out["h"] = Value::Object(flat);
                }
            }
            if let Some(body) = body {
                out["b"] = Value::String(base64::engine::general_purpose::STANDARD.encode(body));
            }
            self.send_text(out);
        };

        let chunk = envelope
            .get("b")
            .and_then(Value::as_str)
            .and_then(|value| base64::engine::general_purpose::STANDARD.decode(value).ok());
        let Some(chunk) = chunk else {
            reply(StatusCode::BAD_REQUEST, None, None, "body is not base64");
            return;
        };
        let mut body = chunk;
        if let Some(buffered) = self.uploads.lock().await.remove(rid) {
            body = buffered;
        }
        let method = envelope
            .get("m")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_string();
        let method = Method::from_bytes(method.as_bytes()).unwrap_or(Method::GET);
        let mut path = match envelope.get("p") {
            Some(Value::String(value)) => value.clone(),
            Some(other) => other.to_string(),
            None => "/".to_string(),
        };
        if path.is_empty() {
            path = "/".to_string();
        }
        if !path.starts_with('/') {
            path = format!("/{path}");
        }

        let mut request = Request::builder().method(method).uri(path);
        if let Some(Value::Object(headers)) = envelope.get("h") {
            for (key, value) in headers {
                if let Some(value) = value.as_str() {
                    request = request.header(key.as_str(), value);
                }
            }
        }
        let api_token = if self.cfg.api_token.is_empty() {
            &self.cfg.token
        } else {
            &self.cfg.api_token
        };
        let request = match request
            .header("authorization", format!("Bearer {api_token}"))
            .body(Body::from(body))
        {
            Ok(request) => request,
            Err(err) => {
                reply(StatusCode::BAD_REQUEST, None, None, &err.to_string());
                return;
            }
        };

        match self.cfg.api.clone().oneshot(request).await {
            Ok(response) => {
                let status = response.status();
                let headers = response.headers().clone();
                match axum::body::to_bytes(response.into_body(), usize::MAX).await {
                    Ok(bytes) => reply(status, Some(headers), Some(bytes.to_vec()), ""),
                    Err(err) => reply(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        None,
                        None,
                        &err.to_string(),
                    ),
                }
            }
            Err(err) => reply(
                StatusCode::INTERNAL_SERVER_ERROR,
                None,
                None,
                &err.to_string(),
            ),
        }
    }
}

fn is_role_note(payload: &[u8]) -> bool {
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(payload) else {
        return false;
    };
    map.contains_key("role") && !map.contains_key("ch") && !map.contains_key("type")
}

/// The `/room` route, mounted by the engine's mux.
pub fn router(cfg: Arc<Config>) -> Router {
    Router::new()
        .route("/room", axum::routing::get(handler))
        .with_state(cfg)
}

async fn handler(
    axum::extract::State(cfg): axum::extract::State<Arc<Config>>,
    request: Request,
) -> Response {
    serve(request, cfg).await
}

async fn serve(request: Request, cfg: Arc<Config>) -> Response {
    if !aixodia_wsock::bearer_ok(request.headers(), &cfg.token) {
        return (StatusCode::UNAUTHORIZED, "token rejected").into_response();
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<(u8, Vec<u8>)>();
    aixodia_wsock::accept(request, move |conn| async move {
        let mut conn = conn;
        conn.set_liveness(std::time::Duration::from_secs(90));
        let (mut reader, writer) = conn.split();
        let session = Session::new((*cfg).clone(), tx.clone());
        let writer_task = tokio::spawn(async move {
            while let Some((op, payload)) = rx.recv().await {
                if writer.write_message(op, &payload).await.is_err() {
                    return;
                }
            }
        });
        if session.chat_writer().await.is_none() {
            writer_task.abort();
            return;
        }
        if session
            .out
            .send((OP_TEXT, br#"{"ctl":"presence","engine":true}"#.to_vec()))
            .is_err()
        {
            writer_task.abort();
            return;
        }
        let ping_session = session.clone();
        let ping = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(20));
            loop {
                ticker.tick().await;
                if ping_session.out.send((OP_PING, Vec::new())).is_err() {
                    return;
                }
            }
        });
        loop {
            match reader.read_message().await {
                Ok((OP_TEXT, payload)) => {
                    if is_role_note(&payload) {
                        continue;
                    }
                    session.text(&payload).await;
                }
                Ok((OP_BINARY, payload)) => session.binary(&payload).await,
                Ok((OP_CLOSE, _)) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        ping.abort();
        writer_task.abort();
    })
}
