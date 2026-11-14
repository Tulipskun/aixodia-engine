//! The small RFC 6455 implementation the engine's WebSocket endpoints share:
//! the handshake and the frame reader and writer.
//!
//! Port of the Go `io/wsock` package. It deliberately carries no policy — the
//! token check, the meaning of a frame, and what happens on disconnect stay with
//! the endpoint that owns them — because the phone socket and the terminal
//! socket want different things from the same wire format.
//!
//! The Go package hijacks the connection away from `net/http`; the Rust
//! equivalent is hyper's connection upgrade, so [`accept`] performs the
//! handshake and hands the upgraded stream to a caller's task as a
//! [`ServerConn`]. The framing itself is implemented here rather than delegated
//! to a WebSocket library, because the endpoints' behaviour is defined in terms
//! of frames (the mask bit, unmasking, extended lengths) and the Go tests assert
//! exactly those.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;

/// The magic value RFC 6455 mixes with the client's key.
pub const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// Caps one frame: a client that asks for a bigger one is cut off rather than
/// allowed to make the daemon allocate whatever it asks for.
pub const MAX_FRAME: usize = 1 << 20;

/// Opcodes, as RFC 6455 numbers them.
pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xA;

/// Everything this layer can fail with. `Timeout` is separate from the rest
/// because a liveness timeout is a liveness timeout: a caller has to be able to
/// tell a silent peer from a broken one, and the Go code tests for a
/// `net.Error` with `Timeout()` true.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("websocket upgrade required")]
    NotUpgrade,
    #[error("hijack unsupported")]
    NoHijack,
    #[error("wsock: frame too large")]
    FrameTooLarge,
    #[error("wsock: message too large")]
    MessageTooLarge,
    #[error("wsock: unknown opcode {0}")]
    UnknownOpcode(u8),
    #[error("wsock: opcode changed mid-message")]
    OpcodeChanged,
    #[error("wsock: {0}")]
    Protocol(String),
    #[error("wsock: {0}")]
    Http(String),
    #[error("wsock: closed")]
    Closed,
    /// No frame of any kind arrived inside the liveness window.
    #[error("wsock: read timed out")]
    Timeout,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Whether this failure is the liveness clock running out.
    pub fn is_timeout(&self) -> bool {
        matches!(self, Error::Timeout)
    }

    /// Whether this failure is the peer saying it is finished.
    pub fn is_eof(&self) -> bool {
        matches!(self, Error::Closed)
    }
}

/// Whether the handshake carries the engine token. An empty configured token
/// never matches, so an engine started without `CF_TOKEN` cannot end up serving
/// everyone who knocks.
/// The header the engine's auth middleware sets once it has accepted the
/// caller's credential.
///
/// Handlers below the middleware still ask `bearer_ok` themselves, and those
/// checks only knew about the master token — so a perfectly good Worker-issued
/// token was refused by the terminal, relay and mobile routes even though the
/// middleware had just let it through. The middleware stamps this header after
/// it verifies, so an accepted token is accepted everywhere.
pub const VERIFIED_HEADER: &str = "x-aixodia-verified";

pub fn bearer_ok(headers: &HeaderMap, token: &str) -> bool {
    // Already vouched for upstream (the engine's auth middleware). That check
    // knows the Worker's token set; this one only knows the master token.
    if headers.get(VERIFIED_HEADER).is_some() {
        return true;
    }
    let auth = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim()
        .trim_start_matches("Bearer ")
        .trim();
    !token.is_empty() && constant_time_eq(auth.as_bytes(), token.as_bytes())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

/// Whether the request carries the whole RFC 6455 handshake.
pub fn is_upgrade(headers: &HeaderMap) -> bool {
    let get = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
    };
    get("upgrade").eq_ignore_ascii_case("websocket")
        && get("connection").to_ascii_lowercase().contains("upgrade")
        && get("sec-websocket-version") == "13"
        && !get("sec-websocket-key").is_empty()
}

/// The value of `Sec-WebSocket-Accept` for one client key.
pub fn accept_value(key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(key.as_bytes());
    hasher.update(GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

/// The stream an upgraded connection becomes: hyper's upgraded connection
/// adapted to tokio's `AsyncRead`/`AsyncWrite`, which is what the framing here
/// is written against.
pub type UpgradedStream = TokioIo<Upgraded>;

/// Completes the handshake and hands the upgraded connection to `serve`.
///
/// This is Go's `Accept`: the request must already be an upgrade (the caller
/// checks that and the bearer token first, so it can still answer with a plain
/// error), and the returned response is the 101. The connection belongs to
/// `serve` from there, and `serve` closes it.
pub fn accept<F, Fut>(mut request: axum::extract::Request, serve: F) -> Response
where
    F: FnOnce(ServerConn<UpgradedStream>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    if !is_upgrade(request.headers()) {
        return (StatusCode::BAD_REQUEST, "websocket upgrade required").into_response();
    }
    let Some(key) = request
        .headers()
        .get("sec-websocket-key")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
    else {
        return (StatusCode::BAD_REQUEST, "websocket upgrade required").into_response();
    };
    let on_upgrade = hyper::upgrade::on(&mut request);
    let response = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header("Sec-WebSocket-Accept", accept_value(&key))
        .body(Body::empty())
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "cannot upgrade").into_response());
    tokio::spawn(async move {
        match on_upgrade.await {
            Ok(upgraded) => serve(ServerConn::new(TokioIo::new(upgraded))).await,
            Err(err) => tracing::warn!("wsock: upgrade failed: {err}"),
        }
    });
    response
}

/// Reads one frame and returns its opcode and unmasked payload. What an opcode
/// means is the caller's business; frame size is capped here.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 2];
    reader.read_exact(&mut head).await?;
    let op = head[0] & 0x0f;
    let masked = head[1] & 0x80 != 0;
    let mut len = u64::from(head[1] & 0x7f);
    match len {
        126 => {
            let mut buf = [0u8; 2];
            reader.read_exact(&mut buf).await?;
            len = u64::from(u16::from_be_bytes(buf));
        }
        127 => {
            let mut buf = [0u8; 8];
            reader.read_exact(&mut buf).await?;
            len = u64::from_be_bytes(buf);
        }
        _ => {}
    }
    if len > MAX_FRAME as u64 {
        return Err(Error::FrameTooLarge);
    }
    let mut mask = [0u8; 4];
    if masked {
        reader.read_exact(&mut mask).await?;
    }
    let mut payload = vec![0u8; len as usize];
    reader.read_exact(&mut payload).await?;
    if masked {
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte ^= mask[index % 4];
        }
    }
    Ok((op, payload))
}

/// Writes one unmasked frame, which is what a server sends; only clients mask.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    op: u8,
    payload: &[u8],
) -> Result<()> {
    let mut header = Vec::with_capacity(10);
    header.push(0x80 | op);
    let len = payload.len();
    if len < 126 {
        header.push(len as u8);
    } else if len <= 0xFFFF {
        header.push(126);
        header.extend_from_slice(&(len as u16).to_be_bytes());
    } else {
        header.push(127);
        header.extend_from_slice(&(len as u64).to_be_bytes());
    }
    writer.write_all(&header).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

/// The inbound half of RFC 6455: a connection the engine accepted itself.
///
/// It is the mirror of [`client::Client`] — nothing it sends is masked — and it
/// carries the same message-level API, so a handler written against one runs on
/// the other.
pub struct ServerConn<S> {
    stream: S,
    /// How long a read waits for any frame. `None` waits forever.
    liveness: Option<Duration>,
}

impl<S> ServerConn<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            liveness: None,
        }
    }

    /// Makes `read_message` give up when no frame of any kind arrives within
    /// `d`. A phone that vanishes without a close frame leaves the reader parked
    /// until TCP keepalive gives up, which is minutes; the peer pings on a
    /// shorter interval, so a silent socket is treated as gone.
    pub fn set_liveness(&mut self, d: Duration) {
        self.liveness = Some(d);
    }

    pub fn stream_mut(&mut self) -> &mut S {
        &mut self.stream
    }

    pub fn into_inner(self) -> S {
        self.stream
    }
}

/// The two halves of a split [`ServerConn`].
pub type SplitConn<S> = (
    MessageReader<tokio::io::ReadHalf<S>, tokio::io::WriteHalf<S>>,
    MessageWriter<tokio::io::WriteHalf<S>>,
);

impl<S: AsyncRead + AsyncWrite + Unpin + Send> ServerConn<S> {
    /// The next text or binary frame, answering pings and surfacing a close
    /// frame as [`Error::Closed`] so callers have one way to notice the end.
    pub async fn read_message(&mut self) -> Result<(u8, Vec<u8>)> {
        let liveness = self.liveness;
        let stream = &mut self.stream;
        let read = async move {
            loop {
                let (op, payload) = read_frame(stream).await?;
                match op {
                    OP_PING => {
                        write_frame(stream, OP_PONG, &payload).await?;
                    }
                    OP_PONG => {}
                    OP_CLOSE => {
                        let _ = write_frame(stream, OP_CLOSE, &[]).await;
                        return Err(Error::Closed);
                    }
                    OP_TEXT | OP_BINARY => return Ok((op, payload)),
                    other => return Err(Error::UnknownOpcode(other)),
                }
            }
        };
        match liveness {
            Some(d) => match tokio::time::timeout(d, read).await {
                Ok(result) => result,
                Err(_) => Err(Error::Timeout),
            },
            None => read.await,
        }
    }

    /// Sends one unmasked text or binary frame, as a server must.
    pub async fn write_message(&mut self, op: u8, payload: &[u8]) -> Result<()> {
        write_frame(&mut self.stream, op, payload).await
    }

    pub async fn close(&mut self) -> Result<()> {
        self.stream.shutdown().await?;
        Ok(())
    }

    /// Splits the connection so a reader and a writer can live in different
    /// tasks — which is what a server that broadcasts to its peers needs. The
    /// reader keeps a handle on the writer so it can still answer a ping and
    /// acknowledge a close, exactly as the unsplit form does.
    pub fn split(self) -> SplitConn<S> {
        let (read, write) = tokio::io::split(self.stream);
        let writer = MessageWriter {
            half: Arc::new(Mutex::new(write)),
        };
        (
            MessageReader {
                half: read,
                writer: writer.clone(),
                liveness: self.liveness,
            },
            writer,
        )
    }
}

/// The reading half of a split [`ServerConn`].
pub struct MessageReader<R, W> {
    half: R,
    writer: MessageWriter<W>,
    liveness: Option<Duration>,
}

impl<R, W> MessageReader<R, W> {
    pub fn set_liveness(&mut self, d: Duration) {
        self.liveness = Some(d);
    }
}

impl<R: AsyncRead + Unpin + Send, W: AsyncWrite + Unpin + Send> MessageReader<R, W> {
    pub async fn read_message(&mut self) -> Result<(u8, Vec<u8>)> {
        let liveness = self.liveness;
        let half = &mut self.half;
        let writer = &self.writer;
        let read = async move {
            loop {
                let (op, payload) = read_frame(half).await?;
                match op {
                    OP_PING => {
                        writer.write_message(OP_PONG, &payload).await?;
                    }
                    OP_PONG => {}
                    OP_CLOSE => {
                        let _ = writer.write_message(OP_CLOSE, &[]).await;
                        return Err(Error::Closed);
                    }
                    OP_TEXT | OP_BINARY => return Ok((op, payload)),
                    other => return Err(Error::UnknownOpcode(other)),
                }
            }
        };
        match liveness {
            Some(d) => match tokio::time::timeout(d, read).await {
                Ok(result) => result,
                Err(_) => Err(Error::Timeout),
            },
            None => read.await,
        }
    }
}

/// The writing half of a split [`ServerConn`]. It is `Send` and cheap to clone,
/// so a hub can push a frame to a phone while the reader loop is parked.
pub struct MessageWriter<W> {
    half: Arc<Mutex<W>>,
}

impl<W> Clone for MessageWriter<W> {
    fn clone(&self) -> Self {
        Self {
            half: self.half.clone(),
        }
    }
}

impl<W: AsyncWrite + Unpin + Send> MessageWriter<W> {
    pub async fn write_message(&self, op: u8, payload: &[u8]) -> Result<()> {
        let mut half = self.half.lock().await;
        write_frame(&mut *half, op, payload).await
    }
}

pub mod client;

pub use client::Client;

/// Short-lived, single-use tickets for socket dials.
///
/// A WebSocket upgrade from a phone cannot carry an `Authorization` header — the
/// platform's client does not let one be set on the handshake. So a client asks
/// over an ordinary authenticated POST for a ticket and puts it in the query
/// string instead. The ticket is good once and for a few seconds: enough to
/// dial, useless to anyone who reads it later out of a log.
///
/// Both the terminal and the screen use one of these, which is why it lives
/// here rather than in either of them.
#[derive(Default)]
pub struct Tickets {
    live: StdMutex<HashMap<String, Instant>>,
}

impl Tickets {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mints a ticket. The returned second value is when it stops being good,
    /// as a Unix timestamp, so a client can log it.
    pub fn issue(&self) -> (String, i64) {
        let ticket = uuid::Uuid::new_v4().simple().to_string();
        let now = Instant::now();
        let expires = now + TICKET_TTL;
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        // Spent entries are dropped as we go: a long-lived engine would
        // otherwise keep every ticket ever issued.
        live.retain(|_, at| *at > now);
        live.insert(ticket.clone(), expires);
        (ticket, now_unix() + TICKET_TTL.as_secs() as i64)
    }

    /// Spends a ticket. True only the first time, for a ticket that has not
    /// expired.
    pub fn consume(&self, value: &str) -> bool {
        let value = value.trim();
        if value.is_empty() {
            return false;
        }
        let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
        match live.remove(value) {
            Some(expires) => Instant::now() < expires,
            None => false,
        }
    }

    /// Whether a request should be let in: a bearer token, or a ticket in the
    /// query string that this store accepts.
    pub fn admits(&self, request: &Request, token: &str) -> bool {
        if bearer_ok(request.headers(), token) {
            return true;
        }
        self.consume(&ticket_of(request))
    }
}

/// The `ticket` query parameter of a request, decoded, or an empty string.
pub fn ticket_of<B>(request: &Request<B>) -> String {
    request
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .find_map(|pair| pair.strip_prefix("ticket="))
        .map(|value| {
            let mut out = String::with_capacity(value.len());
            let bytes = value.as_bytes();
            let mut at = 0;
            while at < bytes.len() {
                match bytes[at] {
                    b'%' if at + 2 < bytes.len() => {
                        let hex = &value[at + 1..at + 3];
                        if let Ok(byte) = u8::from_str_radix(hex, 16) {
                            out.push(byte as char);
                            at += 3;
                            continue;
                        }
                        out.push('%');
                        at += 1;
                    }
                    b'+' => {
                        out.push(' ');
                        at += 1;
                    }
                    other => {
                        out.push(other as char);
                        at += 1;
                    }
                }
            }
            out
        })
        .unwrap_or_default()
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// How long a ticket stays good. One dial is all it is for.
pub const TICKET_TTL: Duration = Duration::from_secs(45);
