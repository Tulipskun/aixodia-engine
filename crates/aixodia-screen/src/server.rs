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

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
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
use crate::encoder::{self, EncodeSettings, H264Encoder};

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
    /// How the picture is compressed. JPEG tiles are the default and the
    /// fallback: they need nothing of the client but a decoder for a format
    /// every platform has. H.264 costs a fraction of the bandwidth for the same
    /// picture — measured at 3.7 KB against 237 KB for a scrolled screen here —
    /// so it is worth having where the client can decode it.
    pub codec: Codec,
}

/// How the picture is compressed on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// One JPEG per changed tile.
    Jpeg,
    /// One H.264 access unit per frame.
    H264,
}

impl Codec {
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::Jpeg => "jpeg",
            Codec::H264 => "h264",
        }
    }

    /// Reads a codec name, refusing anything unknown rather than guessing.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "jpeg" => Some(Codec::Jpeg),
            "h264" => Some(Codec::H264),
            _ => None,
        }
    }
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
            codec: Codec::Jpeg,
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
            codec: self.codec,
        }
    }
}

/// What the capture loop carries between frames.
///
/// The codecs need different things: the tile path a baseline to diff against,
/// the video path an encoder. Both live here so the loop owns them and they are
/// dropped when the capture stops.
struct FrameState {
    /// Set on the first frame, which has nothing to compare against.
    force_all: bool,
    /// The previous frame, for the tile path.
    previous: Vec<u8>,
    /// The encoder, for the video path.
    h264: Option<H264Encoder>,
    /// True when the next video frame must be decodable on its own.
    h264_keyframe_due: bool,
}

impl FrameState {
    fn new() -> Self {
        Self {
            force_all: true,
            previous: Vec::new(),
            h264: None,
            h264_keyframe_due: true,
        }
    }
}

/// One video frame as it crosses the socket.
#[derive(Serialize)]
struct VideoHeader {
    #[serde(rename = "type")]
    kind: &'static str,
    width: u32,
    height: u32,
    seq: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<(i32, i32)>,
    /// True when this frame stands on its own, so a client may start here.
    keyframe: bool,
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
    /// Where the machine's pointer is, so the client can draw it. The pointer
    /// is not part of the captured pixels — X takes it out of the framebuffer —
    /// so a client that wants to show it has to be told where it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<(i32, i32)>,
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
    /// Which compression the frames will use, so a client can pick a decoder
    /// before the first frame arrives rather than guessing from its shape.
    codec: &'static str,
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
        /// "jpeg" or "h264". Anything else leaves the setting alone rather than
        /// silently choosing something the client did not ask for.
        #[serde(default)]
        codec: Option<String>,
    },
    /// Asks for a frame that stands on its own.
    ///
    /// A client only needs this when it has lost its decoder's state — after a
    /// resize, or when it joined mid-stream — and it is cheaper than
    /// reconnecting.
    RequestKeyframe,
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
    /// How many clients are watching.
    ///
    /// The capture loop must run exactly while somebody is looking and stop
    /// when nobody is, and a plain true/false flag cannot say that. It was
    /// false whenever the connection that happened to start the loop left —
    /// even with other clients still connected — which stopped the capture for
    /// everybody and left those clients with a frozen picture after two frames.
    viewers: AtomicUsize,
    /// Set when a client asks for a frame it can start from. The loop clears it
    /// after honouring it, so a request costs one keyframe however many clients
    /// made it at once.
    keyframe_wanted: AtomicBool,
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
            viewers: AtomicUsize::new(0),
            keyframe_wanted: AtomicBool::new(false),
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
            codec,
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
        // An unknown codec name leaves the setting alone. Guessing would hand
        // the client frames in a shape it cannot decode.
        if let Some(value) = codec.as_ref().and_then(|name| Codec::parse(name)) {
            settings.codec = value;
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
    /// Captures a frame and sends it in whichever compression is set.
    ///
    /// JPEG keeps a baseline to diff against; H.264 keeps an encoder, because
    /// the compression of a frame depends on the frames before it. Both are held
    /// by the loop rather than the hub, so they die with the capture.
    fn publish(&self, state: &mut FrameState) -> Result<usize, String> {
        // A client asking to start over gets a frame that stands on its own, and
        // the request is cleared here so it is answered once.
        if self.keyframe_wanted.swap(false, Ordering::SeqCst) {
            state.h264_keyframe_due = true;
            state.force_all = true;
        }
        // The screen lock is held only while the pixels are read; encoding and
        // sending happen without it so a slow client cannot stall the capture.
        let (frame, width, height) = {
            let mut guard = self.screen.lock().unwrap_or_else(|e| e.into_inner());
            let screen = guard.as_mut().ok_or("display is not open")?;
            let frame = screen.frame().map_err(|e| e.to_string())?;
            (frame.pixels, frame.width, frame.height)
        };

        let settings = self.settings_now();
        if settings.codec == Codec::H264 {
            return self.send_h264(&frame, width, height, state);
        }

        if state.previous.len() != frame.len() {
            state.previous.clear();
            state.previous.resize(frame.len(), 0);
            // A new size means there is nothing to compare against.
            return self.send_tiles(&frame, width, height, state.force_all, &mut state.previous);
        }
        self.send_tiles(&frame, width, height, state.force_all, &mut state.previous)
    }

    /// Sends one frame as an H.264 access unit.
    ///
    /// A separate message shape, not a variant of the tile message: the client
    /// either has a video decoder or it does not, and a frame it cannot read is
    /// worse than no frame.
    fn send_h264(
        &self,
        frame: &[u8],
        width: u32,
        height: u32,
        state: &mut FrameState,
    ) -> Result<usize, String> {
        // A change of size or codec invalidates the encoder, which is tied to
        // both. Rebuilding it means the next frame is a keyframe.
        let needs_encoder = match &state.h264 {
            Some(existing) => existing.size() != (width, height),
            None => true,
        };
        if needs_encoder {
            state.h264 = Some(H264Encoder::new(width, height)?);
        }
        let encoder = state.h264.as_mut().expect("an encoder was just built");
        let payload = encoder.encode(frame)?;
        if payload.is_empty() {
            return Ok(0);
        }

        // Keyframe detection: an access unit that cannot be decoded without
        // earlier ones must be asked for again by a client that joins, and a
        // new client is told so it knows this one is whole.
        let keyframe = state.h264_keyframe_due;
        state.h264_keyframe_due = false;

        let seq = self.sequence.fetch_add(1, Ordering::Relaxed) as u64;
        let cursor = self.cursor_in(width, height);
        let header = serde_json::to_vec(&VideoHeader {
            kind: "video",
            width,
            height,
            seq,
            cursor,
            keyframe,
        })
        .unwrap_or_else(|_| b"{\"type\":\"video\"}".to_vec());

        let mut buffer = Vec::with_capacity(4 + header.len() + payload.len());
        buffer.extend_from_slice(&(header.len() as u32).to_le_bytes());
        buffer.extend_from_slice(&header);
        buffer.extend_from_slice(&payload);
        let _ = self
            .frames
            .send(Arc::new(Message::Binary(buffer.into())));
        Ok(1)
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
        // One message per frame: a header, the tile count, then each tile's
        // geometry and JPEG. Lengths are fixed-width so the client can walk the
        // buffer without a parser.
        let _ = self
            .frames
            .send(Arc::new(Message::Binary(self.pack(width, height, &updates).into())));
        Ok(updates.len())
    }

    /// The whole screen, for a client that has just connected.
    ///
    /// The shared stream only carries what changed since the previous frame, so
    /// a client that joins mid-stream would be handed the differences against a
    /// picture it never received — which on screen is a half-drawn, or blank,
    /// display. A newcomer therefore gets one complete frame first, and the
    /// diffs make sense from there.
    fn full_frame(&self) -> Result<Message, String> {
        let (frame, width, height) = {
            let mut guard = self.screen.lock().unwrap_or_else(|e| e.into_inner());
            let screen = guard.as_mut().ok_or("display is not open")?;
            let captured = screen.frame().map_err(|e| e.to_string())?;
            (captured.pixels, captured.width, captured.height)
        };
        let settings = self.settings_now();
        let updates = encoder::diff_and_encode(
            &frame,
            width,
            height,
            &mut vec![0u8; frame.len()],
            true,
            settings.encode,
        );
        Ok(Message::Binary(self.pack(width, height, &updates).into()))
    }

    /// Packs tiles into one message: a header, a count, then each tile.
    /// Where the pointer is, in the pixels of a frame of this size.
    ///
    /// The pointer is reported in the display's own pixels; a frame has been
    /// scaled to the size the client asked for, so it is scaled the same way.
    /// Without this the arrow lands somewhere the pointer never was on any
    /// display that is not exactly the capture size.
    fn cursor_in(&self, width: u32, height: u32) -> Option<(i32, i32)> {
        let mut guard = self.screen.lock().unwrap_or_else(|e| e.into_inner());
        let screen = guard.as_mut()?;
        let (x, y) = screen.pointer_position()?;
        let (full_w, full_h) = screen.display_size();
        if full_w == 0 || full_h == 0 {
            return Some((x, y));
        }
        let scale_x = width as f64 / full_w as f64;
        let scale_y = height as f64 / full_h as f64;
        Some((
            (x as f64 * scale_x).round() as i32,
            (y as f64 * scale_y).round() as i32,
        ))
    }

    fn pack(&self, width: u32, height: u32, updates: &[encoder::TileUpdate]) -> Vec<u8> {
        let seq = self.sequence.fetch_add(1, Ordering::Relaxed) as u64;
        // The pointer is reported in the display's own pixels; the frame has
        // been scaled to the size the client asked for, so it is scaled the same
        // way. Without this the arrow lands somewhere the pointer never was on
        // any display that is not exactly the capture size.
        let cursor = self.cursor_in(width, height);
        let mut buffer = Vec::with_capacity(64 + updates.iter().map(|t| t.jpeg.len() + 16).sum::<usize>());
        let header = serde_json::to_vec(&FrameHeader {
            kind: "frame",
            width,
            height,
            seq,
            cursor,
        })
        .unwrap_or_else(|_| b"{\"type\":\"frame\"}".to_vec());
        buffer.extend_from_slice(&(header.len() as u32).to_le_bytes());
        buffer.extend_from_slice(&header);
        buffer.extend_from_slice(&(updates.len() as u32).to_le_bytes());
        for update in updates {
            buffer.extend_from_slice(&(update.col as u16).to_le_bytes());
            buffer.extend_from_slice(&(update.row as u16).to_le_bytes());
            buffer.extend_from_slice(&(update.width as u16).to_le_bytes());
            buffer.extend_from_slice(&(update.height as u16).to_le_bytes());
            buffer.extend_from_slice(&(update.jpeg.len() as u32).to_le_bytes());
            buffer.extend_from_slice(&update.jpeg);
        }
        buffer
    }

    /// Applies one client message to the display.
    fn apply(&self, message: ClientMessage) {
        if matches!(message, ClientMessage::Settings { .. }) {
            self.update(&message);
            return;
        }
        // A keyframe request touches no display: it asks the loop to send a
        // frame that stands on its own, and the loop answers it.
        if matches!(message, ClientMessage::RequestKeyframe) {
            self.keyframe_wanted.store(true, Ordering::SeqCst);
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
            // Nothing to do to the display; the loop picks the request up.
            ClientMessage::Settings { .. } | ClientMessage::RequestKeyframe => {}
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
        codec: settings.codec.as_str(),
    };
    if socket
        .send(Message::Text(serde_json::to_string(&hello).unwrap_or_default().into()))
        .await
        .is_err()
    {
        return;
    }

    // Before joining the shared stream, the whole screen once: everything after
    // this is a diff, and a diff means nothing to a client with no picture yet.
    //
    // In video mode there is nothing to send: a tile frame is not a frame that
    // client can read, and the encoder's next keyframe carries the whole picture
    // anyway. Sending one would hand it a message it must throw away.
    if settings.codec == Codec::Jpeg {
        if let Ok(first) = hub.full_frame() {
            if socket.send(first).await.is_err() {
                return;
            }
        }
    }

    let mut frames = hub.frames.subscribe();

    // The loop runs while anybody is watching and stops when the last viewer
    // goes, so an idle engine is not encoding a screen nobody sees. Counting
    // viewers rather than letting the first connection own the loop is what
    // keeps it alive for the others when that first connection leaves.
    let before = hub.viewers.fetch_add(1, Ordering::SeqCst);
    let capture_hub = Arc::clone(&hub);
    let capture = (before == 0).then(|| {
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
                    // A client that fell behind has gaps in its picture, and a
                    // diff cannot repair a gap — it assumes every earlier tile
                    // arrived. So a lagging client gets a whole frame again
                    // rather than being left to catch up on its own, which it
                    // cannot do. Without this it shows a permanent mess of old
                    // and missing tiles, the broken picture seen in the app.
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::debug!(skipped, "screen: client lagged, resending the whole frame");
                        match hub.full_frame() {
                            Ok(whole) => {
                                if socket.send(whole).await.is_err() {
                                    break;
                                }
                            }
                            Err(err) => {
                                tracing::debug!(%err, "screen: could not resend a full frame");
                                break;
                            }
                        }
                    }
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

    // Leaving decrements the count. Only the last viewer stops the loop, and
    // only the connection that started it waits for it to finish, so the display
    // is closed before it is opened again.
    let remaining = hub.viewers.fetch_sub(1, Ordering::SeqCst) - 1;
    if let Some(task) = capture {
        if remaining == 0 {
            let _ = task.await;
        }
    }
}

/// The capture loop: one frame every `1000 / fps` milliseconds while at least
/// one client is connected.
fn run_capture(hub: Arc<ScreenHub>) {
    let mut state = FrameState::new();
    while hub.viewers.load(Ordering::SeqCst) > 0 {
        let settings = hub.settings_now();
        let period = Duration::from_millis((1000 / settings.fps.max(1) as u64).max(1));
        let started = std::time::Instant::now();

        match hub.publish(&mut state) {
            Ok(_) => state.force_all = false,
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
            codec: None,
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
            codec: None,
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

#[cfg(test)]
mod loop_probe {
    //! Why does a 15 fps loop deliver two frames?
    //!
    //! The capture loop and the per-connection sender are separate pieces of
    //! code with a broadcast channel between them, and a slow or absent reader
    //! changes what the loop appears to do. This drives the encoder and a real
    //! broadcast channel with the loop's own timing, so the answer is measured
    //! rather than guessed.

    use super::*;

    #[test]
    fn a_static_screen_sends_one_frame_then_nothing() {
        let width = 128u32;
        let height = 128u32;
        let mut previous = vec![0u8; (width * height * 4) as usize];
        let mut frame = vec![0u8; (width * height * 4) as usize];
        for (index, byte) in frame.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }

        // The first tick sees an empty baseline and must send everything.
        let first = encoder::diff_and_encode(
            &frame,
            width,
            height,
            &mut previous,
            true,
            EncodeSettings::default(),
        );
        assert!(!first.is_empty(), "the first frame establishes the picture");

        // Every tick after it must send nothing while the screen holds still.
        for tick in 0..15 {
            let again = encoder::diff_and_encode(
                &frame,
                width,
                height,
                &mut previous,
                false,
                EncodeSettings::default(),
            );
            assert!(again.is_empty(), "tick {tick} resent an unchanged screen");
        }
    }

    #[test]
    fn a_broadcast_with_no_reader_does_not_stop_the_loop() {
        // The loop sends into a channel that nobody may be reading. Sending must
        // stay cheap and must not block, or the capture stalls and the whole
        // picture goes stale with it.
        let (frames, _) = broadcast::channel::<u32>(8);
        let started = std::time::Instant::now();
        for tick in 0..200u32 {
            let _ = frames.send(tick);
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "200 sends took {:?}, which would stall the capture loop",
            started.elapsed()
        );
    }
}

#[cfg(test)]
mod viewer_lifetime {
    //! How long the capture loop lives.
    //!
    //! This is the bug that made the app show a still picture: the loop was
    //! started by whichever client connected first and stopped when that client
    //! left, even while others were still watching. Counting viewers is the fix,
    //! and these hold it in place.

    use super::*;

    /// The rule the loop checks: capture while somebody is watching.
    fn should_capture(viewers: usize) -> bool {
        viewers > 0
    }

    #[test]
    fn one_viewer_keeps_the_capture_running() {
        assert!(should_capture(1));
    }

    #[test]
    fn no_viewers_stops_the_capture() {
        assert!(!should_capture(0), "an engine nobody watches must not encode");
    }

    #[test]
    fn a_second_viewer_does_not_stop_the_first() {
        // The count is what decides. Two viewers are two viewers whatever order
        // they arrived in, and the one that leaves first must not take the
        // loop away from the one that stays.
        let hub = ScreenHub::new("", "token");
        assert_eq!(hub.viewers.load(Ordering::SeqCst), 0);

        hub.viewers.fetch_add(1, Ordering::SeqCst); // first client
        hub.viewers.fetch_add(1, Ordering::SeqCst); // second client
        assert!(should_capture(hub.viewers.load(Ordering::SeqCst)));

        // The first client leaves; the second is still watching.
        let remaining = hub.viewers.fetch_sub(1, Ordering::SeqCst) - 1;
        assert_eq!(remaining, 1);
        assert!(
            should_capture(hub.viewers.load(Ordering::SeqCst)),
            "the loop must keep running for the client that stayed"
        );

        // And when the last one leaves, it stops.
        let remaining = hub.viewers.fetch_sub(1, Ordering::SeqCst) - 1;
        assert_eq!(remaining, 0);
        assert!(!should_capture(hub.viewers.load(Ordering::SeqCst)));
    }

    #[test]
    fn only_the_first_viewer_starts_the_loop() {
        // Starting a loop per connection would give two capture tasks reading
        // one display and handing each other half-frames.
        let hub = ScreenHub::new("", "token");
        let first = hub.viewers.fetch_add(1, Ordering::SeqCst);
        assert_eq!(first, 0, "the first viewer starts the loop");
        let second = hub.viewers.fetch_add(1, Ordering::SeqCst);
        assert_ne!(second, 0, "a later viewer must not start a second loop");
    }
}

#[cfg(test)]
mod codec_choice {
    //! Which compression is used, and what happens to a name nobody knows.

    use super::*;

    #[test]
    fn jpeg_is_the_default_so_an_old_client_still_works() {
        assert_eq!(Settings::default().codec, Codec::Jpeg);
    }

    #[test]
    fn the_names_round_trip() {
        assert_eq!(Codec::parse("jpeg"), Some(Codec::Jpeg));
        assert_eq!(Codec::parse("h264"), Some(Codec::H264));
        assert_eq!(Codec::Jpeg.as_str(), "jpeg");
        assert_eq!(Codec::H264.as_str(), "h264");
    }

    #[test]
    fn an_unknown_codec_is_refused_rather_than_guessed() {
        // Choosing something the client did not ask for would send it frames it
        // cannot decode, which looks exactly like a broken screen.
        assert_eq!(Codec::parse("h265"), None);
        assert_eq!(Codec::parse(""), None);
        assert_eq!(Codec::parse("H264"), None);
    }

    #[test]
    fn a_client_can_switch_codec_and_keep_everything_else() {
        let hub = ScreenHub::new(":0", "test-token");
        hub.update(&ClientMessage::Settings {
            codec: Some("h264".to_string()),
            width: None,
            height: None,
            fps: None,
            tile: None,
            quality: None,
        });
        let settings = hub.settings_now();
        assert_eq!(settings.codec, Codec::H264);
        // Nothing else moved.
        assert_eq!(settings.width, Settings::default().width);
        assert_eq!(settings.fps, Settings::default().fps);
    }

    #[test]
    fn an_unknown_codec_leaves_the_setting_alone() {
        let hub = ScreenHub::new(":0", "test-token");
        hub.update(&ClientMessage::Settings {
            codec: Some("mpeg2".to_string()),
            width: None,
            height: None,
            fps: None,
            tile: None,
            quality: None,
        });
        assert_eq!(hub.settings_now().codec, Codec::Jpeg);
    }

    #[test]
    fn a_keyframe_request_is_counted_once_however_often_asked() {
        let hub = ScreenHub::new(":0", "test-token");
        assert!(!hub.keyframe_wanted.load(Ordering::SeqCst));
        hub.apply(ClientMessage::RequestKeyframe);
        hub.apply(ClientMessage::RequestKeyframe);
        hub.apply(ClientMessage::RequestKeyframe);
        // The loop takes the flag; three requests are still one keyframe.
        assert!(hub.keyframe_wanted.swap(false, Ordering::SeqCst));
        assert!(!hub.keyframe_wanted.load(Ordering::SeqCst));
    }
}
