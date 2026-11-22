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

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use crate::capture::{Button, Input, Screen};
use crate::encoder;
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
    /// JPEG quality for a Tight tile, 1 to 100.
    pub quality: u8,
    /// Tile side in pixels; zero means the default.
    pub tile: u32,
}

/// Serves `config` on loopback until the process ends.
///
/// Returns the port it is listening on, so the caller can say it in a log rather
/// than leave an operator guessing. Loopback only: RFB's `None` security type is
/// no password at all, and a display on a public interface is somebody else's
/// desktop. Reaching it from another machine is a tunnel's job.
pub fn start(mut config: Config) -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", config.port))?;
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

    let display_name = match crate::display::ensure() {
        Ok(display) => display,
        Err(err) => {
            tracing::warn!("rfb: no display to serve: {}", err.0);
            return Ok(());
        }
    };
    let mut input = match Input::open(&display_name) {
        Ok(input) => input,
        Err(err) => {
            tracing::warn!("rfb: cannot open {display_name}: {err}");
            return Ok(());
        }
    };
    let (width, height) = input.display_size();
    let mut screen = match Screen::open(&display_name, width, height) {
        Ok(screen) => screen,
        Err(err) => {
            tracing::warn!("rfb: cannot capture {display_name}: {err}");
            return Ok(());
        }
    };

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
    let mut previous = vec![0u8; (width as usize) * (height as usize) * 4];
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
                tracing::debug!("rfb: frame failed: {err}");
                continue;
            }
        };
        let tiles = protocol::changed_tiles(&frame.pixels, &previous, width, height, config.tile);
        if !whole && tiles.is_empty() {
            // Nothing has changed, and the viewer asked only for changes: the
            // next request is the next chance to send anything.
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
            &frame.pixels,
            width,
            &tiles,
            &format,
            encoding,
            config.quality,
        )?;
        previous.copy_from_slice(&frame.pixels);
        requested = false;
        whole = false;
    }
}

/// Turns a viewer's button mask into presses and releases.
///
/// RFB sends the state of all eight buttons together, so what has changed since
/// the last message is what has to be sent to X. The wheel is two of those bits,
/// and X wants a notch as a press and a release.
fn press_buttons(input: &Input, held: &mut u8, mask: u8) {
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

/// Sends one `FramebufferUpdate` covering `tiles`.
///
/// Written through a `Write` so the bytes can be checked without a socket.
fn write_update<W: Write>(
    out: &mut W,
    frame: &[u8],
    frame_width: u32,
    tiles: &[(u32, u32, u32, u32)],
    format: &PixelFormat,
    encoding: i32,
    quality: u8,
) -> std::io::Result<()> {
    out.write_all(&[0, 0])?;
    out.write_all(&(tiles.len() as u16).to_be_bytes())?;
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
    fn an_update_says_how_many_rectangles_it_carries() {
        let (frame, tiles) = frame_and_tiles();
        let mut out = Vec::new();
        write_update(
            &mut out,
            &frame,
            8,
            &tiles,
            &PixelFormat::NATIVE,
            encoding::RAW,
            70,
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
            &frame,
            8,
            &tiles[..1],
            &PixelFormat::NATIVE,
            encoding::TIGHT,
            70,
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
        let display = match crate::display::ensure() {
            Ok(display) => display,
            Err(_) => return,
        };
        let Ok(input) = Input::open(&display) else {
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
