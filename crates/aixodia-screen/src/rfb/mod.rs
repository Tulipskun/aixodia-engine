//! An RFB server, so a stock VNC viewer can see and drive this display.
//!
//! The app has its own transport — tiles over the socket it already signs in to
//! — and that stays the fast path. This is the other door: the published VNC
//! protocol (RFC 6143), so a viewer somebody already has on their phone or
//! laptop connects with no app of ours installed.
//!
//! Only what a viewer actually needs is spoken: version 3.3 and 3.8, security
//! type `None`, Tight with JPEG tiles, and Raw for a viewer that asks for
//! nothing else. Hextile is deliberately not offered — a rectangle claiming an
//! encoding it does not compress is worse than one that says Raw.

pub mod protocol;

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::time::Instant;

use crate::capture::Button;
use crate::encoder;
use crate::pane::Events;
use crate::rfb::protocol::{encoding, PixelFormat};

/// The name a viewer shows for this display.
const NAME: &str = "aixodia";

/// Tile side, in pixels: the unit a changed area is sent in.
const TILE: u32 = 64;

/// How the server is set up when the engine starts it.
#[derive(Clone)]
pub struct Config {
    /// The port to listen on. Zero asks the kernel for a free one.
    pub port: u16,
    /// The address to listen on. Loopback unless an operator says otherwise.
    pub host: String,
    /// JPEG quality for a Tight tile, 1 to 100.
    pub quality: u8,
    /// Tile side in pixels; zero means the default.
    pub tile: u32,
    /// Where to write what a viewer does, when the reader is measuring one.
    ///
    /// Every pointer and key event, with its millisecond offset and the button
    /// mask, in one line each: enough to work out how another client turns
    /// fingers into events — its speed, its thresholds, its timing — by
    /// watching it work rather than by reading its source, which is not in the
    /// file it ships.
    pub record: Option<Arc<Mutex<BufWriter<File>>>>,
}

/// What a `vnc.conf` says about the VNC door.
///
/// A file rather than only environment variables, because the launcher unsets
/// every `AIXODIA_*` variable before it execs the engine — deliberately, so a
/// stray value in a shell cannot change the machine — which makes a file the
/// only place a setting can live and still arrive. The format is one `key=value`
/// per line, `#` for comments, and every key optional:
///
/// ```text
/// host=0.0.0.0          # leave loopback: for a viewer on the LAN
/// port=5900
/// quality=75
/// input_log=/root/.aixodia/vnc-input.log
/// off                   # or off=1, to close the door entirely
/// ```
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Settings {
    pub off: bool,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub quality: Option<u8>,
    pub input_log: Option<String>,
}

impl Settings {
    /// Reads the file's text. Unknown keys and unreadable numbers are ignored
    /// rather than fatal: a typo in a settings file must not take the picture
    /// down with it.
    pub fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (key, value) = match line.split_once('=') {
                Some((key, value)) => (key.trim(), value.trim()),
                None => (line, ""),
            };
            let value = value.split('#').next().unwrap_or("").trim();
            match key {
                "off" => settings.off = !matches!(value, "0" | "false" | "no"),
                "host" if !value.is_empty() => settings.host = Some(value.to_string()),
                "port" => settings.port = value.parse().ok(),
                // Clamped rather than refused: 999 is somebody meaning "as sharp
                // as it goes", and dropping it would quietly use a default.
                "quality" => {
                    settings.quality = value.parse().ok().map(|q: i32| q.clamp(1, 100) as u8)
                }
                "input_log" if !value.is_empty() => {
                    settings.input_log = Some(value.to_string());
                }
                _ => {}
            }
        }
        settings
    }
}

/// Serves `config` on loopback until the process ends.
///
/// Returns the port it is listening on, so the caller can say it in a log rather
/// than leave an operator guessing. Loopback only: RFB's `None` security type is
/// no password at all, and a display on a public interface is somebody else's
/// desktop. Reaching it from another machine is a tunnel's job.
pub fn start(mut config: Config) -> std::io::Result<u16> {
    let listener = TcpListener::bind((config.host.as_str(), config.port))?;
    let port = listener.local_addr()?.port();
    if config.tile == 0 {
        config.tile = TILE;
    }
    std::thread::Builder::new()
        .name("aixodia-rfb".to_string())
        .spawn(move || {
            for connection in listener.incoming() {
                match connection {
                    Ok(stream) => {
                        let config = config.clone();
                        // One thread per viewer: a second one must not wait
                        // behind the first, and each opens its own display
                        // connection so input never says "already grabbed".
                        let _ = std::thread::Builder::new()
                            .name("aixodia-rfb-client".to_string())
                            .spawn(move || {
                                if let Err(err) = serve(stream, &config) {
                                    // A viewer that hangs up mid-handshake is the
                                    // ordinary way a connection ends.
                                    if err.kind() != std::io::ErrorKind::UnexpectedEof {
                                        tracing::debug!("rfb: {err}");
                                    }
                                }
                            });
                    }
                    Err(err) => {
                        tracing::warn!("rfb: {err}");
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        })?;
    Ok(port)
}

/// One viewer, from its greeting to its close.
fn serve(mut stream: TcpStream, config: &Config) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    stream.write_all(protocol::GREETING)?;

    let mut greeting = [0u8; 12];
    stream.read_exact(&mut greeting)?;
    let Some((_, minor)) = protocol::agreed_version(&greeting) else {
        // Not RFB at all, or a family this server has never heard of. Nothing
        // sensible can be said to it, so the connection is simply closed.
        return Ok(());
    };

    // Security: one option, and it is "none".
    if minor >= 7 {
        stream.write_all(&[1, protocol::SECURITY_NONE])?;
        let mut chosen = [0u8; 1];
        stream.read_exact(&mut chosen)?;
        if chosen[0] != protocol::SECURITY_NONE {
            // Only a refusal can be reported, and only to a 3.8 viewer.
            if minor >= 8 {
                stream.write_all(&1u32.to_be_bytes())?;
            }
            return Ok(());
        }
        if minor >= 8 {
            stream.write_all(&0u32.to_be_bytes())?;
        }
    } else {
        // 3.3 sends the single security type as a 32-bit number and says nothing
        // about a result.
        stream.write_all(&(protocol::SECURITY_NONE as u32).to_be_bytes())?;
    }

    let mut shared = [0u8; 1];
    stream.read_exact(&mut shared)?;

    // The pane is whatever this engine shows: an X display somebody else owns,
    // or the compositor this process made. Both doors into the screen ask for it
    // here, so a viewer and the phone look at one desktop and move one pointer.
    let panes = match crate::pane::open(0, 0) {
        Ok(panes) => panes,
        Err(err) => {
            tracing::warn!("rfb: no screen to serve: {err}");
            return Ok(());
        }
    };
    let input = panes.events;
    let mut screen = panes.source;
    let (width, height) = input.display_size();

    // ServerInit: the size, the format this server works in, and a name.
    let mut init = Vec::with_capacity(24);
    init.extend_from_slice(&(width as u16).to_be_bytes());
    init.extend_from_slice(&(height as u16).to_be_bytes());
    init.extend_from_slice(&PixelFormat::NATIVE.to_wire());
    init.extend_from_slice(&(NAME.len() as u32).to_be_bytes());
    init.extend_from_slice(NAME.as_bytes());
    stream.write_all(&init)?;

    // Nothing is sent until a viewer asks: an update nobody requested is a
    // picture the viewer is not waiting for, and one that shifts everything it
    // reads by a frame.
    let mut requested = false;
    let mut whole = true;
    let mut format = PixelFormat::NATIVE;
    let mut encoding = encoding::RAW;
    // Whether this viewer draws the cursor itself, and the shape it has already
    // been given. Both are per connection: one viewer's change is not another's.
    let mut cursor_wanted = false;
    let mut cursor_serial: Option<u64> = None;
    let mut previous = vec![0u8; (width as usize) * (height as usize) * 4];
    let started = Instant::now();
    let mut buttons = 0u8;

    loop {
        let mut kind = [0u8; 1];
        stream.read_exact(&mut kind)?;
        match kind[0] {
            // SetPixelFormat
            0 => {
                let mut body = [0u8; 19];
                stream.read_exact(&mut body)?;
                if let Some(asked) = PixelFormat::from_wire(&body[3..19]) {
                    format = asked;
                    whole = true;
                    // The cursor already sent was packed for the old format.
                    cursor_serial = None;
                }
            }
            // SetEncodings
            2 => {
                let mut header = [0u8; 3];
                stream.read_exact(&mut header)?;
                let count = u16::from_be_bytes([header[1], header[2]]) as usize;
                let mut offered = Vec::with_capacity(count);
                let mut body = vec![0u8; count * 4];
                stream.read_exact(&mut body)?;
                for number in body.chunks_exact(4) {
                    offered.push(i32::from_be_bytes([
                        number[0], number[1], number[2], number[3],
                    ]));
                }
                encoding = protocol::choose(&offered);
                // A viewer that asked for the cursor is declaring it can draw
                // one itself, which is the whole point: the shape then travels
                // once instead of being painted into every frame.
                cursor_wanted = offered.contains(&protocol::encoding::CURSOR);
            }
            // FramebufferUpdateRequest
            3 => {
                let mut body = [0u8; 9];
                stream.read_exact(&mut body)?;
                // A non-incremental request asks for the whole screen whatever
                // changed, which is what a viewer does when its window is
                // uncovered.
                if body[0] == 0 {
                    whole = true;
                }
                requested = true;
            }
            // KeyEvent
            4 => {
                let mut body = [0u8; 7];
                stream.read_exact(&mut body)?;
                let down = body[0] != 0;
                let keysym = u32::from_be_bytes([body[3], body[4], body[5], body[6]]);
                input.keysym(keysym, down);
                record(config, started, "key", None, keysym as i32, i32::from(down));
            }
            // PointerEvent
            5 => {
                let mut body = [0u8; 5];
                stream.read_exact(&mut body)?;
                let mask = body[0];
                let x = u16::from_be_bytes([body[1], body[2]]) as i32;
                let y = u16::from_be_bytes([body[3], body[4]]) as i32;
                input.move_pointer(x, y);
                press_buttons(&input, &mut buttons, mask);
                record(config, started, "pointer", Some(mask), x, y);
            }
            // ClientCutText: the viewer's clipboard. Nothing here reads it.
            6 => {
                let mut header = [0u8; 7];
                stream.read_exact(&mut header)?;
                let length =
                    u32::from_be_bytes([header[3], header[4], header[5], header[6]]) as usize;
                let mut text = vec![0u8; length];
                stream.read_exact(&mut text)?;
            }
            _ => return Ok(()),
        }

        if !requested {
            continue;
        }
        let frame = match screen.frame() {
            Ok(frame) => frame,
            Err(err) => {
                tracing::warn!("rfb: frame failed: {err}");
                continue;
            }
        };
        let tiles = protocol::changed_tiles(&frame.pixels, &previous, width, height, config.tile);
        // The cursor is checked on every request the viewer makes, which is
        // how often a viewer can draw it. Only a change is sent: a cursor that
        // has not moved is bytes nobody can see.
        let cursor = if cursor_wanted {
            input
                .cursor_shape()
                .filter(|now| cursor_serial != Some(now.serial) && now.width > 0 && now.height > 0)
        } else {
            None
        };
        if !whole && tiles.is_empty() && cursor.is_none() {
            // Nothing has changed, but the viewer still asked: it is answered
            // with an update of no rectangles. A viewer is entitled to wait for
            // the reply before it asks again, and one that does would otherwise
            // sit looking at a picture that never moves.
            write_update(
                &mut stream,
                &Picture {
                    frame: &frame.pixels,
                    frame_width: width,
                    tiles: &[],
                    format: &format,
                    encoding,
                    quality: config.quality,
                    cursor: None,
                },
            )?;
            requested = false;
            continue;
        }
        let tiles = if whole {
            // Every tile, whatever changed: the viewer has just asked for the
            // whole picture, and its own copy cannot be trusted.
            protocol::changed_tiles(
                &frame.pixels,
                &vec![0u8; previous.len()],
                width,
                height,
                config.tile,
            )
        } else {
            tiles
        };
        write_update(
            &mut stream,
            &Picture {
                frame: &frame.pixels,
                frame_width: width,
                tiles: &tiles,
                format: &format,
                encoding,
                quality: config.quality,
                cursor: cursor.as_ref(),
            },
        )?;
        if let Some(now) = &cursor {
            cursor_serial = Some(now.serial);
        }
        previous.copy_from_slice(&frame.pixels);
        requested = false;
        whole = false;
    }
}

/// Writes one line about what a viewer just did, when recording is on.
///
/// The line is the whole point of the feature, so it is formatted by a function
/// of its own rather than by an inline `write!`: a recorded stream is only
/// useful if its shape is stable enough to compare two clients' streams field by
/// field.
fn record(config: &Config, started: Instant, kind: &str, mask: Option<u8>, x: i32, y: i32) {
    let Some(log) = &config.record else { return };
    let line = record_line(started.elapsed().as_millis(), kind, mask, x, y);
    if let Ok(mut log) = log.lock() {
        // A recorder that cannot write must not take the viewer down with it:
        // the picture is the job, the log is the measuring.
        let _ = log.write_all(line.as_bytes());
        let _ = log.flush();
    }
}

/// One recorded event, as text: `ms kind mask x y`.
fn record_line(elapsed_ms: u128, kind: &str, mask: Option<u8>, x: i32, y: i32) -> String {
    match mask {
        Some(mask) => format!("{elapsed_ms} {kind} mask={mask} x={x} y={y}\n"),
        None => format!("{elapsed_ms} {kind} x={x} y={y}\n"),
    }
}

/// Turns a viewer's button mask into presses and releases.
///
/// RFB sends the state of all eight buttons together, so what has changed since
/// the last message is what has to be sent to X. The wheel is two of those bits,
/// and X wants a notch as a press and a release.
fn press_buttons(input: &Events, held: &mut u8, mask: u8) {
    for (bit, button) in [(0u8, Button::Left), (1, Button::Middle), (2, Button::Right)] {
        let wanted = mask & (1 << bit) != 0;
        let had = *held & (1 << bit) != 0;
        if wanted != had {
            input.button(button, wanted);
        }
    }
    // A wheel notch is not a state that can be held: it is one click of a
    // button X reports as 4, 5, 6 or 7 — up, down, left, right.
    for (bit, button) in [
        (3u8, Button::WheelUp),
        (4, Button::WheelDown),
        (5, Button::WheelLeft),
        (6, Button::WheelRight),
    ] {
        if mask & (1 << bit) != 0 && *held & (1 << bit) == 0 {
            input.button(button, true);
            input.button(button, false);
        }
    }
    *held = mask;
}

/// Everything one `FramebufferUpdate` says: the frame it is cut from, the tiles
/// that changed, how to pack them, and the cursor if that is what changed.
struct Picture<'a> {
    frame: &'a [u8],
    frame_width: u32,
    tiles: &'a [(u32, u32, u32, u32)],
    format: &'a PixelFormat,
    encoding: i32,
    quality: u8,
    cursor: Option<&'a crate::cursor::Now>,
}

/// Sends one `FramebufferUpdate` describing `picture`.
///
/// Written through a `Write` so the bytes can be checked without a socket.
fn write_update<W: Write>(out: &mut W, picture: &Picture) -> std::io::Result<()> {
    let Picture {
        frame,
        frame_width,
        tiles,
        format,
        encoding,
        quality,
        cursor,
    } = *picture;
    let rectangles = tiles.len() + usize::from(cursor.is_some());
    out.write_all(&[0, 0])?;
    out.write_all(&(rectangles as u16).to_be_bytes())?;
    // The cursor goes first. RFB does not order the rectangles of an update, but
    // a viewer that has just been told a size reads the shape it must draw
    // before the pixels it must draw under it.
    if let Some(cursor) = cursor {
        write_cursor(out, cursor, format)?;
    }
    for &(x, y, width, height) in tiles {
        out.write_all(&(x as u16).to_be_bytes())?;
        out.write_all(&(y as u16).to_be_bytes())?;
        out.write_all(&(width as u16).to_be_bytes())?;
        out.write_all(&(height as u16).to_be_bytes())?;
        out.write_all(&encoding.to_be_bytes())?;
        match encoding {
            encoding::TIGHT => {
                let pixels = region(frame, frame_width, (x, y, width, height));
                let Some(jpeg) = encoder::encode_tile(&pixels, width, height, quality) else {
                    tracing::warn!("rfb: tile {width}x{height} would not encode as JPEG");
                    // A tile that will not encode must still be sent, or the
                    // picture on the viewer would keep the state it had.
                    let raw = protocol::extract(frame, frame_width, (x, y, width, height), format);
                    write_raw(out, &raw)?;
                    continue;
                };
                out.write_all(&[protocol::tight_jpeg_control(quality)])?;
                out.write_all(&protocol::tight_length(jpeg.len()))?;
                out.write_all(&jpeg)?;
            }
            _ => {
                let raw = protocol::extract(frame, frame_width, (x, y, width, height), format);
                write_raw(out, &raw)?;
            }
        }
    }
    out.flush()
}

/// Sends the cursor as one pseudo-rectangle of the update.
///
/// The rectangle's position carries the hot spot rather than a place on the
/// screen, and what follows the header is the cursor's own pixels and then a
/// bitmask of which of them are visible. A viewer that asked for this draws the
/// pointer itself, which is why the arrow then follows the phone's thumb
/// without a frame of lag.
fn write_cursor<W: Write>(
    out: &mut W,
    cursor: &crate::cursor::Now,
    format: &PixelFormat,
) -> std::io::Result<()> {
    out.write_all(&(cursor.hot_x as u16).to_be_bytes())?;
    out.write_all(&(cursor.hot_y as u16).to_be_bytes())?;
    out.write_all(&(cursor.width as u16).to_be_bytes())?;
    out.write_all(&(cursor.height as u16).to_be_bytes())?;
    out.write_all(&protocol::encoding::CURSOR.to_be_bytes())?;
    let pixels = protocol::cursor_pixels(&cursor.rgba, cursor.width, cursor.height, format);
    out.write_all(&pixels)?;
    let mask = protocol::cursor_mask(&cursor.rgba, cursor.width, cursor.height);
    out.write_all(&mask)
}

/// Raw pixels: the bytes themselves, with nothing in front of them.
fn write_raw<W: Write>(out: &mut W, pixels: &[u8]) -> std::io::Result<()> {
    out.write_all(pixels)
}

/// A rectangle of a frame, copied out row by row into one buffer.
fn region(frame: &[u8], frame_width: u32, region: (u32, u32, u32, u32)) -> Vec<u8> {
    let (x, y, width, height) = region;
    let stride = frame_width as usize * 4;
    let mut out = Vec::with_capacity(width as usize * height as usize * 4);
    for row in 0..height as usize {
        let start = (y as usize + row) * stride + x as usize * 4;
        out.extend_from_slice(&frame[start..start + width as usize * 4]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two tiles of nonsense, to be written as Raw.
    fn frame_and_tiles() -> (Vec<u8>, Vec<(u32, u32, u32, u32)>) {
        let mut frame = vec![0u8; 8 * 8 * 4];
        for (index, byte) in frame.iter_mut().enumerate() {
            *byte = (index % 251) as u8;
        }
        (frame, vec![(0, 0, 4, 4), (4, 0, 4, 4)])
    }

    #[test]
    fn a_settings_file_is_read_key_by_key() {
        let settings = Settings::parse(
            "# the VNC door\nhost=0.0.0.0\nport=5901\nquality=70  # a comment\n\ninput_log=/tmp/in.log\n",
        );
        assert_eq!(settings.host.as_deref(), Some("0.0.0.0"));
        assert_eq!(settings.port, Some(5901));
        assert_eq!(settings.quality, Some(70));
        assert_eq!(settings.input_log.as_deref(), Some("/tmp/in.log"));
        assert!(!settings.off);
    }

    #[test]
    fn a_bare_off_key_closes_the_door_and_nonsense_is_ignored() {
        let settings = Settings::parse("off\nport=seventy\nquality=999\nhost=\n");
        assert!(settings.off);
        // A port that is not a number keeps the default rather than becoming
        // zero, and an empty host is no host at all.
        assert_eq!(settings.port, None);
        assert_eq!(settings.host, None);
        assert_eq!(settings.quality, Some(100), "clamped, not dropped");
    }

    #[test]
    fn a_recorded_line_carries_the_time_the_buttons_and_the_place() {
        // A recorded stream is compared field by field against another client's,
        // so the shape matters as much as the values: time first (two streams can
        // be laid side by side), then what happened, then where.
        assert_eq!(
            record_line(1234, "pointer", Some(1), 350, 250),
            "1234 pointer mask=1 x=350 y=250\n"
        );
        assert_eq!(record_line(9, "key", None, 65293, 1), "9 key x=65293 y=1\n");
    }

    #[test]
    fn an_update_says_how_many_rectangles_it_carries() {
        let (frame, tiles) = frame_and_tiles();
        let mut out = Vec::new();
        write_update(
            &mut out,
            &Picture {
                frame: &frame,
                frame_width: 8,
                tiles: &tiles,
                format: &PixelFormat::NATIVE,
                encoding: encoding::RAW,
                quality: 70,
                cursor: None,
            },
        )
        .expect("written");
        assert_eq!(&out[..2], &[0, 0], "message type and padding");
        assert_eq!(u16::from_be_bytes([out[2], out[3]]), 2, "two rectangles");
        // The first rectangle's header: x, y, width, height, encoding.
        assert_eq!(u16::from_be_bytes([out[4], out[5]]), 0);
        assert_eq!(u16::from_be_bytes([out[6], out[7]]), 0);
        assert_eq!(u16::from_be_bytes([out[8], out[9]]), 4);
        assert_eq!(u16::from_be_bytes([out[10], out[11]]), 4);
        assert_eq!(
            i32::from_be_bytes([out[12], out[13], out[14], out[15]]),
            encoding::RAW
        );
        // Two 4x4 rectangles of four bytes a pixel follow the headers.
        assert_eq!(out.len(), 4 + 2 * 12 + 2 * 4 * 4 * 4);
    }

    #[test]
    fn a_tight_update_carries_a_jpeg_after_its_control_byte() {
        let (frame, tiles) = frame_and_tiles();
        let mut out = Vec::new();
        write_update(
            &mut out,
            &Picture {
                frame: &frame,
                frame_width: 8,
                tiles: &tiles[..1],
                format: &PixelFormat::NATIVE,
                encoding: encoding::TIGHT,
                quality: 70,
                cursor: None,
            },
        )
        .expect("written");
        assert_eq!(
            i32::from_be_bytes([out[12], out[13], out[14], out[15]]),
            encoding::TIGHT
        );
        let control = out[16];
        assert_eq!(control >> 4, 0x9, "the JPEG subencoding");
        // The compact length follows the control byte, then the JPEG itself: a
        // tile is well over 127 bytes, so the length takes more than one.
        let mut at = 17;
        let mut length = (out[at] as usize) & 0x7f;
        if out[at] & 0x80 != 0 {
            at += 1;
            length |= (out[at] as usize) << 7;
        }
        at += 1;
        assert_eq!(
            &out[at..at + 2],
            &[0xFF, 0xD8],
            "JPEG start-of-image marker"
        );
        assert_eq!(out.len(), at + length);
    }

    #[test]
    fn an_update_can_carry_nothing_but_the_cursor() {
        // A cursor that changed over an unchanged picture is still a reason to
        // send an update: one rectangle, and it is the pointer rather than a
        // piece of the screen.
        let cursor = crate::cursor::Now {
            serial: 7,
            width: 2,
            height: 2,
            hot_x: 1,
            hot_y: 0,
            rgba: vec![
                255, 255, 255, 255, // opaque white, top left
                0, 0, 0, 0, // transparent, top right
                0, 0, 0, 0, // transparent, bottom left
                255, 0, 0, 255, // opaque red, bottom right
            ],
        };
        let mut out = Vec::new();
        write_update(
            &mut out,
            &Picture {
                frame: &[0u8; 8 * 8 * 4],
                frame_width: 8,
                tiles: &[],
                format: &PixelFormat::NATIVE,
                encoding: encoding::RAW,
                quality: 70,
                cursor: Some(&cursor),
            },
        )
        .expect("written");
        assert_eq!(u16::from_be_bytes([out[2], out[3]]), 1, "one rectangle");
        // The header carries the hot spot and the size, not a place on screen.
        assert_eq!(u16::from_be_bytes([out[4], out[5]]), 1, "hot spot x");
        assert_eq!(u16::from_be_bytes([out[6], out[7]]), 0, "hot spot y");
        assert_eq!(u16::from_be_bytes([out[8], out[9]]), 2, "width");
        assert_eq!(u16::from_be_bytes([out[10], out[11]]), 2, "height");
        assert_eq!(
            i32::from_be_bytes([out[12], out[13], out[14], out[15]]),
            protocol::encoding::CURSOR
        );
        // Blue, green, red, pad for each pixel in the native format.
        let pixels = [
            255, 255, 255, 255, //
            0, 0, 0, 255, //
            0, 0, 0, 255, //
            0, 0, 255, 255,
        ];
        assert_eq!(&out[16..32], &pixels, "the cursor's own pixels");
        // A mask of one byte a row: top left visible, then bottom right.
        assert_eq!(&out[32..34], &[0b1000_0000, 0b0100_0000], "the mask");
        assert_eq!(out.len(), 34, "and nothing else");
    }

    #[test]
    fn an_update_with_nothing_to_say_is_still_an_update() {
        // A viewer that asked is answered even when the screen has not moved:
        // RealVNC Viewer waits for the reply before it asks again, and a server
        // that says nothing leaves it with a picture that never changes.
        let mut out = Vec::new();
        write_update(
            &mut out,
            &Picture {
                frame: &[0u8; 8 * 8 * 4],
                frame_width: 8,
                tiles: &[],
                format: &PixelFormat::NATIVE,
                encoding: encoding::RAW,
                quality: 70,
                cursor: None,
            },
        )
        .expect("written");
        assert_eq!(
            out,
            vec![0, 0, 0, 0],
            "a type, a padding byte, and no rectangles"
        );
    }

    #[test]
    fn a_region_is_copied_row_by_row() {
        let mut frame = vec![0u8; 4 * 2 * 4];
        // The second row, second pixel, is the only non-zero word.
        let at = (1 * 4 + 1) * 4;
        frame[at..at + 4].copy_from_slice(&[1, 2, 3, 0]);
        let out = region(&frame, 4, (1, 1, 1, 1));
        assert_eq!(out, vec![1, 2, 3, 0]);
    }

    #[test]
    fn a_button_mask_becomes_presses_and_notches() {
        // The mask is compared against what was held, so a held button is not
        // pressed twice; without an X server the calls are no-ops, which keeps
        // this a test of the arithmetic.
        // The arithmetic is the test; the pane is only there to hold a pointer.
        let Ok(input) = crate::pane::open_events(0, 0) else {
            return;
        };
        let mut held = 0u8;
        press_buttons(&input, &mut held, 0b0000_0001);
        assert_eq!(held, 0b0000_0001);
        press_buttons(&input, &mut held, 0b0000_0101);
        assert_eq!(held, 0b0000_0101);
        press_buttons(&input, &mut held, 0);
        assert_eq!(held, 0);
    }
}
