//! What the machine's pointer looks like right now.
//!
//! A phone has no pointer of its own, and the pointer it draws for the machine
//! has to change the way the machine's does: a hand over a link, an I-beam over
//! text, a watch over something that is loading. X keeps that in the cursor the
//! window under the pointer defined, and XFixes can read it back.
//!
//! Read back as pixels, not as a name. Naming was tried first and is not there:
//! measured on the engine's own display, every cursor — the root's arrow and the
//! I-beam an xterm asks for — came back with an empty name, because a display
//! without a cursor theme never records one. Guessing a shape from a name that
//! is always empty would have left every pointer an arrow, which is exactly the
//! complaint this answers. The pixels are always there, and they are what a VNC
//! viewer sends for the same reason.
//!
//! So the cursor travels as a bitmap and its hot spot. It is small — a cursor is
//! sixteen or twenty-four pixels square — and it is only sent when the serial
//! changes, which is when the pointer crosses into another window.

use x11::{xfixes, xlib};

/// The cursor the display is showing: its pixels, its hot spot, its serial.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Now {
    pub serial: u64,
    pub width: u32,
    pub height: u32,
    /// The pixel of the image that sits *on* the pointer.
    pub hot_x: u32,
    pub hot_y: u32,
    /// Straight RGBA, one byte per channel, row by row.
    pub rgba: Vec<u8>,
}

/// What the pointer is over, as far as its shape can say.
///
/// A viewer wants to know whether a tap lands in something text can be typed
/// into, because that is when the phone's keyboard belongs on screen. X tells
/// us with the cursor: a widget that takes text sets an I-beam, and nothing
/// else on the display is a glyph that is much taller than it is wide.
///
/// Measured on this display, which has no cursor theme — so the names are all
/// empty and the shape itself has to answer: the arrow is 24×24, the hourglass
/// 16×16, and the I-beam an xterm asks for is 9×16.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Text can be typed where the pointer is.
    Text,
    /// Anything else: an arrow, a hand, a watch.
    Pointer,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Text => "text",
            Role::Pointer => "pointer",
        }
    }
}

impl Now {
    /// The role this shape plays.
    ///
    /// Three halves of the width is the line the I-beam crosses and an arrow
    /// does not: 16 ≥ 13.5 for the I-beam, 24 < 36 for the arrow. A shape too
    /// small to be either is left as a pointer rather than guessed at.
    pub fn role(&self) -> Role {
        if self.height >= 12 && self.height * 2 >= self.width * 3 {
            Role::Text
        } else {
            Role::Pointer
        }
    }
}

/// Whether this display can report its cursor at all.
///
/// XFixes is an extension: a display without it simply never changes the arrow,
/// which is what happened before this existed.
pub unsafe fn available(display: *mut xlib::Display) -> bool {
    if display.is_null() {
        return false;
    }
    let mut event_base = 0;
    let mut error_base = 0;
    xfixes::XFixesQueryExtension(display, &mut event_base, &mut error_base) != 0
}

/// Reads the cursor the display is showing.
///
/// The image XFixes returns is allocated by the library and must be freed with
/// `XFree`. Its pixels are premultiplied ARGB, which is not what an image viewer
/// wants; [`straighten`] turns them into the RGBA everything else speaks.
pub unsafe fn read(display: *mut xlib::Display) -> Option<Now> {
    if display.is_null() {
        return None;
    }
    let image = unsafe { xfixes::XFixesGetCursorImage(display) };
    if image.is_null() {
        return None;
    }
    let (serial, width, height, hot_x, hot_y, pixels) = unsafe {
        (
            (*image).cursor_serial as u64,
            (*image).width as u32,
            (*image).height as u32,
            (*image).xhot as u32,
            (*image).yhot as u32,
            (*image).pixels,
        )
    };
    let count = (width * height) as usize;
    let argb: Vec<u32> = if pixels.is_null() {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(pixels, count) }
            .iter()
            .map(|pixel| *pixel as u32)
            .collect()
    };
    unsafe { xlib::XFree(image as *mut std::ffi::c_void) };
    if argb.len() != count {
        return None;
    }
    Some(Now {
        serial,
        width,
        height,
        hot_x,
        hot_y,
        rgba: straighten(&argb),
    })
}

/// Premultiplied ARGB, as X stores a cursor, to straight RGBA as an image is.
///
/// A premultiplied pixel has already been multiplied by its own alpha, so a half
/// transparent white is 128, not 255. Handing those numbers to an image viewer
/// draws a dark halo once the viewer multiplies by alpha a second time: the rim
/// of every anti-aliased cursor came out grey. Dividing it back out is the fix,
/// and zero alpha has no colour to recover at all.
pub fn straighten(argb: &[u32]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(argb.len() * 4);
    for pixel in argb {
        let alpha = ((pixel >> 24) & 0xff) as u8;
        let r = ((pixel >> 16) & 0xff) as u8;
        let g = ((pixel >> 8) & 0xff) as u8;
        let b = (pixel & 0xff) as u8;
        let undo = |channel: u8| -> u8 {
            if alpha == 0 {
                0
            } else if alpha == 255 {
                channel
            } else {
                ((channel as u32 * 255 + alpha as u32 / 2) / alpha as u32).min(255) as u8
            }
        };
        rgba.push(undo(r));
        rgba.push(undo(g));
        rgba.push(undo(b));
        rgba.push(alpha);
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(width: u32, height: u32) -> Now {
        Now {
            serial: 1,
            width,
            height,
            hot_x: 0,
            hot_y: 0,
            rgba: vec![0; (width * height * 4) as usize],
        }
    }

    #[test]
    fn an_i_beam_is_text() {
        // Measured on this display over an xterm.
        assert_eq!(Role::Text, shape(9, 16).role());
    }

    #[test]
    fn an_arrow_and_an_hourglass_are_not_text() {
        assert_eq!(Role::Pointer, shape(24, 24).role());
        assert_eq!(Role::Pointer, shape(16, 16).role());
        // A hand over a link is square too.
        assert_eq!(Role::Pointer, shape(24, 26).role());
    }

    #[test]
    fn a_shape_with_no_size_is_not_text() {
        assert_eq!(Role::Pointer, shape(0, 0).role());
    }

    #[test]
    fn an_opaque_pixel_is_unchanged() {
        // 0xFF_FF_FF_FF: white, all there.
        assert_eq!(straighten(&[0xFFFF_FFFF]), vec![255, 255, 255, 255]);
        assert_eq!(straighten(&[0xFF00_0000]), vec![0, 0, 0, 255]);
    }

    #[test]
    fn a_half_transparent_pixel_gets_its_colour_back() {
        // Premultiplied white at half alpha is 128, not 255. Left alone, an
        // anti-aliased cursor rim comes out grey.
        let straight = straighten(&[0x80_808080]);
        assert_eq!(straight, vec![255, 255, 255, 128]);
    }

    #[test]
    fn nothing_hidden_has_no_colour_to_recover() {
        // Dividing by zero alpha is where a fast implementation panics.
        assert_eq!(straighten(&[0x00_123456]), vec![0, 0, 0, 0]);
    }

    #[test]
    fn channels_keep_their_places() {
        // Red, green and blue must not swap on the way to the viewer.
        assert_eq!(straighten(&[0xFF_112233]), vec![0x11, 0x22, 0x33, 0xff]);
    }

    #[test]
    fn a_cursor_of_two_pixels_has_eight_bytes() {
        let bytes = straighten(&[0xFF_FFFFFF, 0x00_000000]);
        assert_eq!(bytes.len(), 8);
    }
}
