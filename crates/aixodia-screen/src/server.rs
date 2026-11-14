//! The socket: frames out, fingers in.
//!
//! One connection carries both directions. The server sends a small JSON hello
//! and then binary frames, each a batch of changed tiles preceded by a four-byte
//! count — a batch rather than a frame per tile, so a redraw does not cost one
//! write syscall per square. The client sends JSON in the other direction:
//! pointer moves, button presses, wheel notches, key presses, and settings.
//!
//! The display is opened once per hub, not once per connection: two clients
//! watching the same screen share one capture, and a second phone does not pay
//! for the first one's setup. Only one capture runs at a time — see
//! [`ScreenHub::capture`].

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::capture::{Button, Screen};
use crate::encoder::{self, EncodeSettings};

/// How the picture is produced. Every field can change while the engine runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// Width the screen is captured at. The display is read at this size, so a
    /// smaller number is less to encode as well as less to send.
    pub width: u32,
    /// Height the screen is captured at.
    pub height: u32,
    /// Frames per second ceiling. The capture loop sleeps between frames.
    pub fps: u32,
    pub encode: EncodeSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            // 1280x720 is two thirds of a 1080p page in each direction and a
            // quarter of the pixels: readable at a phone's size, and small
            // enough that a full-page change still fits a slow link.
            width: 1280,
            height: 720,
            fps: 15,
            encode: EncodeSettings::default(),
        }
    }
}

impl Settings {
    fn sanitized(self) -> Self {
        Self {
            width: self.width.clamp(320, 3840),
            height: self.height.clamp(240, 2160),
            fps: self.fps.clamp(1, 60),
            encode: self.encode.sanitized(),
        }
    }
}

/// One frame as it crosses the socket: the tiles that changed, in one message.
#[derive(Serialize)]
struct FrameHeader {
    #[serde(rename = "type")]
    kind: &'static str,
    width: u32,
    height: u32,
    /// Monotonic counter, so a client can notice a frame it never saw.
    seq: u64,
}

/// What the client is told when it connects.
#[derive(Serialize)]
struct Hello {
    #[serde(rename = "type")]
    kind: &'static str,
    width: u32,
    height: u32,
    fps: u32,
    tile: u32,
    quality: u8,
    /// True when the capture path is the fast one, so a client can show it.
    shm: bool,
}

/// What the client sends while it is connected.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    /// Pointer moved to an absolute position on the captured screen.
    Move { x: i32, y: i32 },
    /// Button pressed at a position, in one message: a tap is a move and a
    /// press with no gap for the machine to see the pointer jump.
    Down { x: i32, y: i32, button: String },
    Up { x: i32, y: i32, button: String },
    /// A drag, in one step: press, move, release in that order.
    Drag {
        from_x: i32,
        from_y: i32,
        to_x: i32,
        to_y: i32,
        button: String,
        /// Milliseconds to spread the movement over. The machine's own drag and
        /// drop needs the pointer to pass over the drop target, so a single jump
        /// is not the same thing.
        #[serde(default)]
        duration_ms: u64,
    },
    /// One wheel notch; positive scrolls down.
    Wheel { x: i32, y: i32, delta: i32 },
    /// A key, by X keysym name, with modifiers held around it.
    Key {
        keysym: String,
        #[serde(default)]
        mods: Vec<String>,
    },
    /// New picture settings, applied from the next frame.
    Settings {
        #[serde(default)]
        width: Option<u32>,
        #[serde(default)]
        height: Option<u32>,
        #[serde(default)]
        fps: Option<u32>,
        #[serde(default)]
        tile: Option<u32>,
        #[serde(default)]
        quality: Option<u8>,
    },
}

fn button_from(name: &str) -> Button {
    match name {
        "right" => Button::Right,
        "middle" => Button::Middle,
        _ => Button::Left,
    }
}

/// The shared state behind the screen socket.
pub struct ScreenHub {
    display: String,
    /// The engine's master token, so the ticket endpoint can refuse a caller
    /// that has not signed in. The socket itself is opened with a ticket.
    token: String,
    tickets: aixodia_wsock::Tickets,
    settings: Mutex<Settings>,
    screen: Mutex<Option<Screen>>,
    /// Set while one task is capturing, so two connections never read the same
    /// display at once (which would hand each other half-frames).
    capturing: AtomicBool,
    /// Broadcast so every connected client sees the same frame.
    frames: broadcast::Sender<Arc<Message>>,
    sequence: AtomicU32,
}

impl ScreenHub {
    /// Builds a hub for `display` (empty means `$DISPLAY`), guarded by the
    /// engine's master `token`.
    pub fn new(display: impl Into<String>, token: impl Into<String>) -> Arc<Self> {
        let (frames, _) = broadcast::channel(8);
        Arc::new(Self {
            display: display.into(),
            token: token.into(),
            tickets: aixodia_wsock::Tickets::new(),
            settings: Mutex::new(Settings::default()),
            screen: Mutex::new(None),
            capturing: AtomicBool::new(false),
            frames,
            sequence: AtomicU32::new(0),
        })
    }

    /// The platform facts the client is told once: the real captured size and
    /// whether the fast path is in use.
    fn describe(&self) -> (u32, u32, bool) {
        let settings = *self.settings.lock().unwrap_or_else(|e| e.into_inner());
        let screen = self.screen.lock().unwrap_or_else(|e| e.into_inner());
        match screen.as_ref() {
            Some(s) => (s.width(), s.height(), s.using_shm()),
            None => (settings.width, settings.height, false),
        }
    }

    /// Applies a client's request, keeping what it did not mention.
    fn update(&self, message: &ClientMessage) {
        let ClientMessage::Settings {
            width,
            height,
            fps,
            tile,
            quality,
        } = message
        else {
            return;
        };
        let mut settings = self.settings.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(value) = width {
            settings.width = *value;
        }
        if let Some(value) = height {
            settings.height = *value;
        }
        if let Some(value) = fps {
            settings.fps = *value;
        }
        if let Some(value) = tile {
            settings.encode.tile = *value;
        }
        if let Some(value) = quality {
            settings.encode.quality = *value;
        }
        *settings = settings.sanitized();
        // The next frame is read at the new size, so the captured image has to
        // be rebuilt; dropping it makes the next `frame()` reopen at the size.
        if width.is_some() || height.is_some() {
            let mut screen = self.screen.lock().unwrap_or_else(|e| e.into_inner());
            *screen = None;
        }
    }

    /// The current settings, for the capture loop.
    fn settings_now(&self) -> Settings {
        *self.settings.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Opens the display if it is not open yet, at the current settings.
    fn ensure_screen(&self) -> Result<(), String> {
        let mut guard = self.screen.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_some() {
            return Ok(());
        }
        let settings = self.settings_now();
        let screen = Screen::open(&self.display, settings.width, settings.height)
            .map_err(|e| e.to_string())?;
        *guard = Some(screen);
        Ok(())
    }

    /// Runs one frame: capture, diff, encode, publish. Returns how many tiles
    /// went out, which the loop uses to decide how long to wait.
    fn publish_frame(&self, force_all: bool, previous: &mut Vec<u8>) -> Result<usize, String> {
        // The screen lock is held only while the pixels are read; encoding and
        // sending happen without it so a slow client cannot stall the capture.
        let (frame, width, height) = {
            let mut guard = self.screen.lock().unwrap_or_else(|e| e.into_inner());
            let screen = guard.as_mut().ok_or("display is not open")?;
            let frame = screen.frame().map_err(|e| e.to_string())?;
            (frame.pixels, frame.width, frame.height)
        };

        if previous.len() != frame.len() {
            previous.clear();
            previous.resize(frame.len(), 0);
            // A new size means there is nothing to compare against.
            return self.send_tiles(&frame, width, height, true, previous);
        }
        self.send_tiles(&frame, width, height, force_all, previous)
    }

    fn send_tiles(
        &self,
        frame: &[u8],
        width: u32,
        height: u32,
        force_all: bool,
        previous: &mut [u8],
    ) -> Result<usize, String> {
        let settings = self.settings_now();
        let updates = encoder::diff_and_encode(frame, width, height, previous, force_all, settings.encode);
        if updates.is_empty() {
            return Ok(0);
        }
        let seq = self.sequence.fetch_add(1, Ordering::Relaxed) as u64;

        // One message per frame: a header, the tile count, then each tile's
        // geometry and JPEG. Lengths are fixed-width so the client can walk the
        // buffer without a parser.
        let mut buffer = Vec::with_capacity(64 + updates.iter().map(|t| t.jpeg.len() + 16).sum::<usize>());
        let header = serde_json::to_vec(&FrameHeader {
            kind: "frame",
            width,
            height,
            seq,
        })
        .map_err(|e| e.to_string())?;
        buffer.extend_from_slice(&(header.len() as u32).to_le_bytes());
        buffer.extend_from_slice(&header);
        buffer.extend_from_slice(&(updates.len() as u32).to_le_bytes());
        for update in &updates {
            buffer.extend_from_slice(&(update.col as u16).to_le_bytes());
            buffer.extend_from_slice(&(update.row as u16).to_le_bytes());
            buffer.extend_from_slice(&(update.width as u16).to_le_bytes());
            buffer.extend_from_slice(&(update.height as u16).to_le_bytes());
            buffer.extend_from_slice(&(update.jpeg.len() as u32).to_le_bytes());
            buffer.extend_from_slice(&update.jpeg);
        }

        // Nobody listening is not an error: the capture loop keeps the baseline
        // fresh so the next client's first frame is a diff, not a full repaint.
        let _ = self.frames.send(Arc::new(Message::Binary(buffer.into())));
        Ok(updates.len())
    }

    /// Applies one client message to the display.
    fn apply(&self, message: ClientMessage) {
        if matches!(message, ClientMessage::Settings { .. }) {
            self.update(&message);
            return;
        }
        if let Err(err) = self.ensure_screen() {
            tracing::warn!(%err, "screen input dropped: no display");
            return;
        }
        let screen = self.screen.lock().unwrap_or_else(|e| e.into_inner());
        let Some(screen) = screen.as_ref() else { return };

        // The client works in the captured coordinate space; the display is
        // whatever size it actually is. One multiply per event is cheaper than
        // making every client know the real size.
        let (captured_w, captured_h) = (screen.width(), screen.height());
        let settings = self.settings_now();
        let scale_x = if settings.width == 0 { 1.0 } else { captured_w as f64 / settings.width as f64 };
        let scale_y = if settings.height == 0 { 1.0 } else { captured_h as f64 / settings.height as f64 };
        let to_x = |x: i32| (x as f64 * scale_x).round() as i32;
        let to_y = |y: i32| (y as f64 * scale_y).round() as i32;

        match message {
            ClientMessage::Move { x, y } => screen.move_pointer(to_x(x), to_y(y)),
            ClientMessage::Down { x, y, button } => {
                screen.move_pointer(to_x(x), to_y(y));
                screen.button(button_from(&button), true);
            }
            ClientMessage::Up { x, y, button } => {
                screen.move_pointer(to_x(x), to_y(y));
                screen.button(button_from(&button), false);
            }
            ClientMessage::Drag {
                from_x,
                from_y,
                to_x: dest_x,
                to_y: dest_y,
                button,
                duration_ms,
            } => {
                let button = button_from(&button);
                screen.move_pointer(to_x(from_x), to_y(from_y));
                screen.button(button, true);
                // A drag the machine can follow: the pointer walks to the target
                // so hover states and drop targets see it pass over them.
                let steps = (duration_ms / 16).clamp(1, 60);
                for step in 1..=steps {
                    let ratio = step as f64 / steps as f64;
                    let x = from_x as f64 + (dest_x - from_x) as f64 * ratio;
                    let y = from_y as f64 + (dest_y - from_y) as f64 * ratio;
                    screen.move_pointer(to_x(x.round() as i32), to_y(y.round() as i32));
                    std::thread::sleep(Duration::from_millis(16));
                }
                screen.button(button, false);
            }
            ClientMessage::Wheel { x, y, delta } => {
                screen.move_pointer(to_x(x), to_y(y));
                let button = if delta > 0 { Button::WheelDown } else { Button::WheelUp };
                for _ in 0..delta.unsigned_abs().min(10) {
                    screen.button(button, true);
                    screen.button(button, false);
                }
            }
            ClientMessage::Key { keysym, mods } => screen.key(&keysym, &mods),
            ClientMessage::Settings { .. } => {}
        }
    }
}

/// The screen socket, and the one endpoint that says whether it will work.
pub fn router(hub: Arc<ScreenHub>) -> Router {
    Router::new()
        .route("/api/screen", get(socket))
        .route("/api/screen/info", get(info))
        .route("/api/screen/ticket", axum::routing::post(ticket))
        .with_state(hub)
}

/// Issues a ticket for a socket dial. A WebSocket upgrade cannot carry a header
/// from the clients this serves, so the signed-in request happens here and the
/// dial carries only the ticket.
async fn ticket(State(hub): State<Arc<ScreenHub>>, headers: axum::http::HeaderMap) -> Response {
    if !aixodia_wsock::bearer_ok(&headers, &hub.token) {
        return (axum::http::StatusCode::UNAUTHORIZED, "token rejected").into_response();
    }
    let (ticket, expires_at) = hub.tickets.issue();
    axum::Json(serde_json::json!({ "ticket": ticket, "expires_at": expires_at })).into_response()
}

#[derive(Serialize)]
struct Info {
    available: bool,
    width: u32,
    height: u32,
    fps: u32,
    tile: u32,
    quality: u8,
    shm: bool,
    /// Why the screen cannot be shown, when it cannot.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// Answers whether this machine has a display the app can look at, without
/// opening a socket. The app asks once so it can hide the feature instead of
/// showing a screen that will never fill.
async fn info(State(hub): State<Arc<ScreenHub>>) -> axum::Json<Info> {
    let settings = hub.settings_now();
    match hub.ensure_screen() {
        Ok(()) => {
            let (width, height, shm) = hub.describe();
            axum::Json(Info {
                available: true,
                width,
                height,
                fps: settings.fps,
                tile: settings.encode.tile,
                quality: settings.encode.quality,
                shm,
                reason: None,
            })
        }
        Err(reason) => axum::Json(Info {
            available: false,
            width: settings.width,
            height: settings.height,
            fps: settings.fps,
            tile: settings.encode.tile,
            quality: settings.encode.quality,
            shm: false,
            reason: Some(reason),
        }),
    }
}

async fn socket(ws: WebSocketUpgrade, State(hub): State<Arc<ScreenHub>>, request: axum::extract::Request) -> Response {
    // Either a bearer (a browser or the CLI) or the single-use ticket the app
    // fetched a moment ago. The middleware in front of this has already let a
    // ticket-bearing dial through, so this is where it is actually spent.
    if !hub.tickets.admits(&request, &hub.token) {
        return (axum::http::StatusCode::UNAUTHORIZED, "token rejected").into_response();
    }
    ws.on_upgrade(move |socket| connection(socket, hub))
}

async fn connection(mut socket: WebSocket, hub: Arc<ScreenHub>) {
    if let Err(reason) = hub.ensure_screen() {
        // Nothing to show. Say why once and stop, rather than streaming a black
        // rectangle the client cannot tell from a broken socket.
        let message = serde_json::json!({ "type": "error", "reason": reason });
        let _ = socket.send(Message::Text(message.to_string().into())).await;
        return;
    }

    let (width, height, shm) = hub.describe();
    let settings = hub.settings_now();
    let hello = Hello {
        kind: "hello",
        width,
        height,
        fps: settings.fps,
        tile: settings.encode.tile,
        quality: settings.encode.quality,
        shm,
    };
    if socket
        .send(Message::Text(serde_json::to_string(&hello).unwrap_or_default().into()))
        .await
        .is_err()
    {
        return;
    }

    let mut frames = hub.frames.subscribe();

    // The capture loop is started by the first connection and stops when the
    // last one goes, so an idle engine is not encoding a screen nobody sees.
    let may_capture = hub
        .capturing
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok();
    let capture_hub = Arc::clone(&hub);
    let capture = may_capture.then(|| {
        tokio::task::spawn_blocking(move || run_capture(capture_hub))
    });

    loop {
        tokio::select! {
            // Frames to the client.
            frame = frames.recv() => {
                match frame {
                    Ok(message) => {
                        if socket.send((*message).clone()).await.is_err() {
                            break;
                        }
                    }
                    // A client that fell behind loses frames rather than the
                    // connection: the next frame is a diff against the state
                    // everyone else already has, so it catches up on its own.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Input from the client.
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { break };
                match message {
                    Message::Text(text) => match serde_json::from_str::<ClientMessage>(&text) {
                        Ok(parsed) => hub.apply(parsed),
                        Err(err) => tracing::debug!(%err, "screen: unreadable input"),
                    },
                    Message::Binary(_) => {}
                    Message::Close(_) => break,
                    // Ping and pong are answered by the transport.
                    _ => {}
                }
            }
        }
    }

    if let Some(task) = capture {
        // The loop stops on its own the next time it checks the flag; await it
        // so the display is closed before the next client opens it.
        hub.capturing.store(false, Ordering::SeqCst);
        let _ = task.await;
    }
}

/// The capture loop: one frame every `1000 / fps` milliseconds while at least
/// one client is connected.
fn run_capture(hub: Arc<ScreenHub>) {
    let mut previous: Vec<u8> = Vec::new();
    let mut first = true;
    while hub.capturing.load(Ordering::SeqCst) {
        let settings = hub.settings_now();
        let period = Duration::from_millis((1000 / settings.fps.max(1) as u64).max(1));
        let started = std::time::Instant::now();

        match hub.publish_frame(first, &mut previous) {
            Ok(_) => first = false,
            Err(err) => {
                // The display went away (the machine's session ended). Stop
                // rather than spinning on a dead handle.
                tracing::warn!(%err, "screen capture stopped");
                break;
            }
        }

        let spent = started.elapsed();
        if spent < period {
            std::thread::sleep(period - spent);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_from_a_client_are_clamped_not_trusted() {
        let hub = ScreenHub::new(":0", "test-token");
        hub.update(&ClientMessage::Settings {
            width: Some(99_999),
            height: Some(1),
            fps: Some(1000),
            tile: Some(4),
            quality: Some(0),
        });
        let settings = hub.settings_now();
        assert_eq!(settings.width, 3840);
        assert_eq!(settings.height, 240);
        assert_eq!(settings.fps, 60);
        assert_eq!(settings.encode.tile, 16);
        assert_eq!(settings.encode.quality, 10);
    }

    #[test]
    fn settings_keep_what_the_client_did_not_mention() {
        let hub = ScreenHub::new(":0", "test-token");
        hub.update(&ClientMessage::Settings {
            width: None,
            height: None,
            fps: Some(30),
            tile: None,
            quality: Some(80),
        });
        let settings = hub.settings_now();
        assert_eq!(settings.fps, 30);
        assert_eq!(settings.encode.quality, 80);
        // Untouched fields keep their defaults.
        assert_eq!(settings.width, Settings::default().width);
        assert_eq!(settings.encode.tile, EncodeSettings::default().tile);
    }

    #[test]
    fn a_button_name_that_is_not_known_is_the_left_button() {
        assert_eq!(button_from("left"), Button::Left);
        assert_eq!(button_from(""), Button::Left);
        assert_eq!(button_from("nonsense"), Button::Left);
        assert_eq!(button_from("right"), Button::Right);
        assert_eq!(button_from("middle"), Button::Middle);
    }
}
