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
/// us with the cursor: a widget that takes text sets an I-beam, and the arrow
/// every other widget sets is a different glyph.
///
/// Measured on this display, which has no cursor theme — so the names are all
/// empty and the shape itself has to answer: the arrow is 24×24 with its hot
/// spot at the tip, xterm's I-beam is 9×16, and Chromium's address bar, once it
/// has the focus, is 24×24 — the same glyph drawn bigger. Size alone cannot
/// separate those, which is why a pressed address bar used to leave the
/// keyboard down.
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
    pub fn role(&self) -> Role {
        if self.is_a_bar() {
            Role::Text
        } else {
            Role::Pointer
        }
    }

    /// Whether the shape is an I-beam: a narrow stem with wider bars above and
    /// below it, and the hot spot on the middle of the stem.
    ///
    /// Three things are true of an I-beam and false of an arrow, an hourglass
    /// and a hand, whatever their size:
    ///
    /// * the hot spot — the pixel that types — is the glyph's middle, not a tip
    ///   in a corner; the arrow's is three pixels in from the left and one down;
    /// * the middle rows are the *narrowest*, at most two fifths of the width,
    ///   and they are all the same width, because a stem is a stem and not a
    ///   taper (an hourglass narrows row by row);
    /// * the rows at the very top and bottom are at least twice as wide as the
    ///   stem, which is the bar of the I.
    fn is_a_bar(&self) -> bool {
        let (width, height) = (self.width as usize, self.height as usize);
        if width < 3 || height < 12 || self.rgba.len() != width * height * 4 {
            return false;
        }
        let centre = |hot: u32, of: usize| (hot as usize) * 4 >= of && (hot as usize) * 2 <= of * 3;
        if !centre(self.hot_x, width) || !centre(self.hot_y, height) {
            return false;
        }

        // Opaque pixels in each row: alpha, not colour, because a cursor's
        // colour says nothing about what it is.
        let mut rows = vec![0usize; height];
        for y in 0..height {
            for x in 0..width {
                if self.rgba[(y * width + x) * 4 + 3] >= 128 {
                    rows[y] += 1;
                }
            }
        }

        let stem = &rows[height / 3..height * 2 / 3];
        let stem_width = stem[0];
        let uniform = stem.iter().all(|&row| row == stem_width);
        if stem_width == 0 || stem_width * 5 > width * 2 || !uniform {
            return false;
        }

        let bar = rows[..height / 6]
            .iter()
            .chain(&rows[height * 5 / 6..])
            .copied()
            .max()
            .unwrap_or(0);
        bar >= stem_width * 2
    }
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

    /// A cursor drawn from `#` and `.`, with its hot spot where it types.
    fn drawn(art: &[&str], hot_x: u32, hot_y: u32) -> Now {
        let height = art.len() as u32;
        let width = art[0].chars().count() as u32;
        let mut rgba = vec![0u8; (width * height * 4) as usize];
        for (y, row) in art.iter().enumerate() {
            for (x, cell) in row.chars().enumerate() {
                if cell == '#' {
                    let at = (y * width as usize + x) * 4;
                    rgba[at..at + 4].copy_from_slice(&[255, 255, 255, 255]);
                }
            }
        }
        Now {
            serial: 1,
            width,
            height,
            hot_x,
            hot_y,
            rgba,
        }
    }

    /// The I-beam xterm asks for, read off the display at 9×16.
    const XTERM_I_BEAM: [&str; 16] = [
        "####.####",
        "#########",
        "#########",
        "..#####..",
        "...###...",
        "...###...",
        "...###...",
        "...###...",
        "...###...",
        "...###...",
        "...###...",
        "...###...",
        "..#####..",
        "#########",
        "#########",
        "####.####",
    ];

    /// The I-beam Chromium's address bar shows once it has the focus, read off
    /// the display at 24×24. Pressing the address bar is what this exists for:
    /// the keyboard has to come up on the biggest text widget on the machine.
    const CHROMIUM_I_BEAM: [&str; 24] = [
        "........................",
        "........................",
        "......#########.........",
        "......#########.........",
        "......#########.........",
        ".........####...........",
        ".........###............",
        ".........###............",
        ".........###............",
        ".........###............",
        ".........###............",
        ".........###............",
        ".........###............",
        ".........###............",
        ".........###............",
        ".........###............",
        ".........###............",
        ".........###............",
        ".........###............",
        "......#########.........",
        "......#########.........",
        "......#########.........",
        "........................",
        "........................",
    ];

    #[test]
    fn an_i_beam_is_text() {
        assert_eq!(Role::Text, drawn(&XTERM_I_BEAM, 4, 8).role());
        // The same glyph a size bigger, hot spot in the middle: this is the one
        // whose miss left the keyboard down when a reader pressed an address bar.
        assert_eq!(Role::Text, drawn(&CHROMIUM_I_BEAM, 11, 12).role());
    }

    #[test]
    fn an_arrow_is_not_text_however_large() {
        // The arrow this display shows, 24×24, hot spot at its tip.
        let arrow = [
            "##......................",
            "###.....................",
            "####....................",
            "#####...................",
            "######..................",
            "#######.................",
            "########................",
            "#########...............",
            "##########..............",
            "###########.............",
            "############............",
            "#####.......#...........",
            "####.........#..........",
            "###...........#.........",
            "##.............#........",
            "#...............#.......",
            ".................#......",
            ".................#......",
            "..................#.....",
            "..................#.....",
            "...................#....",
            "...................#....",
            "....................#...",
            "........................",
        ];
        assert_eq!(Role::Pointer, drawn(&arrow, 3, 1).role());
    }

    #[test]
    fn an_hourglass_is_not_text() {
        // A shape that also widens towards its ends, but tapers row by row:
        // a stem is a stem, and an hourglass is not one.
        let watch = [
            "#########",
            "#########",
            ".#######.",
            "..#####..",
            "...###...",
            "....#....",
            "...###...",
            "..#####..",
            ".#######.",
            "#########",
            "#########",
            "#########",
        ];
        assert_eq!(Role::Pointer, drawn(&watch, 4, 6).role());
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
