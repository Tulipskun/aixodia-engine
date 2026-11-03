//! The outbound half of RFC 6455: the engine dials the relay with it.
//!
//! Port of the Go `io/wsock/client.go`. A client masks every frame it sends and
//! may not receive masked frames, which is the mirror of the server half.
//!
//! The handshake and the masking are delegated to `tokio-tungstenite` here, and
//! only here: the server half keeps the raw framing the endpoints' behaviour is
//! defined in terms of, while an outbound socket only has to be a correct RFC
//! 6455 client. What this wrapper adds is the engine's shape — one message-level
//! API, a close frame surfaced as an end, and a liveness clock that every frame
//! refreshes.

use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::{Error, Result, OP_BINARY, OP_CLOSE, OP_PING, OP_PONG, OP_TEXT};

type Stream = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// A WebSocket the engine dialled.
///
/// Writes go through a channel to a task that owns the sink, so `write_message`
/// takes `&self`: a reader parked in `read_message` does not block a ping loop
/// from sending, which is exactly what the relay needs.
pub struct Client {
    stream: futures_util::stream::SplitStream<Stream>,
    sink: mpsc::UnboundedSender<Message>,
    liveness: Option<Duration>,
}

impl Client {
    /// Opens a WebSocket to a `ws://` or `wss://` URL. The extra headers are
    /// sent on the handshake, which is where the bearer token goes.
    pub async fn dial(url: &str, headers: &[(String, String)]) -> Result<Client> {
        let mut request = url
            .into_client_request()
            .map_err(|err| Error::Protocol(err.to_string()))?;
        for (name, value) in headers {
            let name = name
                .parse::<tokio_tungstenite::tungstenite::http::HeaderName>()
                .map_err(|err| Error::Protocol(err.to_string()))?;
            // The caller's headers may not override the handshake ones: a caller
            // that sets Host or the key by accident would break the exchange
            // silently, so those are skipped rather than merged.
            if matches!(
                name.as_str(),
                "upgrade" | "connection" | "sec-websocket-key" | "sec-websocket-version" | "host"
            ) {
                continue;
            }
            let value =
                HeaderValue::from_str(value).map_err(|err| Error::Protocol(err.to_string()))?;
            request.headers_mut().insert(name, value);
        }

        let (stream, _response) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(map_tungstenite)?;
        let (sink, source) = stream.split();
        let (tx, rx) = mpsc::unbounded_channel::<Message>();
        tokio::spawn(write_loop(sink, rx));
        Ok(Client {
            stream: source,
            sink: tx,
            liveness: None,
        })
    }

    /// Makes `read_message` give up when no frame of any kind arrives within
    /// `d`. Every frame — data, ping or pong — resets the clock, so a peer that
    /// is only answering pings still counts as alive. `None` disables the bound.
    pub fn set_liveness(&mut self, d: Option<Duration>) {
        self.liveness = d;
    }

    /// The next text or binary message, answering pings and surfacing a close
    /// frame as [`Error::Closed`] so callers have one way to notice the end.
    pub async fn read_message(&mut self) -> Result<(u8, Vec<u8>)> {
        loop {
            // Refresh the deadline on every frame, not once per message: a pong
            // answers the ping and says the peer is still there, and it must
            // count.
            let next = self.stream.next();
            let frame = match self.liveness {
                Some(d) => match tokio::time::timeout(d, next).await {
                    Ok(frame) => frame,
                    Err(_) => return Err(Error::Timeout),
                },
                None => next.await,
            };
            let Some(frame) = frame else {
                return Err(Error::Closed);
            };
            let message = frame.map_err(map_tungstenite)?;
            match message {
                Message::Text(text) => return Ok((OP_TEXT, text.into_bytes())),
                Message::Binary(data) => return Ok((OP_BINARY, data.to_vec())),
                Message::Ping(data) => {
                    self.send(Message::Pong(data))?;
                }
                Message::Pong(_) => {}
                Message::Close(_) => {
                    let _ = self.send(Message::Close(None));
                    return Err(Error::Closed);
                }
                Message::Frame(_) => {}
            }
        }
    }

    /// Sends one masked text or binary frame. A client must mask, so the server
    /// half's writer cannot be reused here.
    pub fn write_message(&self, op: u8, payload: &[u8]) -> Result<()> {
        let message = match op {
            OP_TEXT => Message::Text(String::from_utf8_lossy(payload).into_owned()),
            OP_BINARY => Message::Binary(payload.to_vec()),
            OP_PING => Message::Ping(payload.to_vec()),
            OP_PONG => Message::Pong(payload.to_vec()),
            OP_CLOSE => Message::Close(None),
            other => return Err(Error::UnknownOpcode(other)),
        };
        self.send(message)
    }

    fn send(&self, message: Message) -> Result<()> {
        self.sink
            .send(message)
            .map_err(|_| Error::Protocol("the socket is closed".to_string()))
    }

    /// Asks the peer to close and stops the writer task.
    pub fn close(&self) -> Result<()> {
        let _ = self.sink.send(Message::Close(None));
        Ok(())
    }
}

async fn write_loop(
    mut sink: futures_util::stream::SplitSink<Stream, Message>,
    mut rx: mpsc::UnboundedReceiver<Message>,
) {
    while let Some(message) = rx.recv().await {
        if sink.send(message).await.is_err() {
            return;
        }
    }
    let _ = sink.close().await;
}

fn map_tungstenite(err: tokio_tungstenite::tungstenite::Error) -> Error {
    use tokio_tungstenite::tungstenite::Error as Te;
    match err {
        Te::ConnectionClosed | Te::AlreadyClosed => Error::Closed,
        Te::Http(response) => {
            let status = response.status();
            let body = response
                .into_body()
                .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
                .unwrap_or_default();
            // The Go client answers with "<status>: <body>", and a caller that
            // wants to notice a rejected handshake looks for the status in it.
            if body.is_empty() {
                Error::Http(status.to_string())
            } else {
                Error::Http(format!("{status}: {body}"))
            }
        }
        Te::Io(err) => Error::Io(err),
        other => Error::Protocol(other.to_string()),
    }
}
