//! The parts of RFB that can be decided without a socket.
//!
//! RFB is the protocol a VNC viewer speaks (RFC 6143). What is here is the
//! arithmetic — which version to agree on, which encoding to use out of the list
//! a viewer offers, and how a picture is packed for the pixel format it asked
//! for. Everything that waits on a socket lives in [`crate::rfb`].
//!
//! Nothing here is copied from another implementation: the wire format is the
//! published one, and the tests are its statements written down.

/// The version this server speaks.
pub const VERSION: (u8, u8) = (3, 8);

/// The protocol version string as it travels, twelve bytes with the newline.
pub const GREETING: &[u8; 12] = b"RFB 003.008\n";

/// Security type 1: no authentication.
pub const SECURITY_NONE: u8 = 1;

/// The largest protocol version this server will agree to, as the minor byte of
/// a version a client sent.
///
/// A client is answered with the lower of the two versions, so a 3.3 viewer gets
/// the 3.3 handshake it understands and a 3.8 viewer gets the modern one. A
/// client speaking a major version other than 3 is refused: 3 is the only family
/// RFB has had since 1998, and guessing at the rest would be inventing a wire
/// format.
pub fn agreed_version(greeting: &[u8]) -> Option<(u8, u8)> {
    if greeting.len() < 12 || !greeting.starts_with(b"RFB ") {
        return None;
    }
    let digits = &greeting[4..11];
    let major: u16 = std::str::from_utf8(&digits[0..3]).ok()?.parse().ok()?;
    let minor: u16 = std::str::from_utf8(&digits[4..7]).ok()?.parse().ok()?;
    if major != 3 {
        return None;
    }
    // A minor version larger than this server's own is answered with this
    // server's own, which is what "the lower of the two" means. It is not an
    // error: 3.889 is a 3.8 viewer with a vendor tag.
    Some((3, minor.min(VERSION.1 as u16) as u8))
}

/// The encodings a viewer may ask for, as they are numbered on the wire.
pub mod encoding {
    /// Uncompressed pixels, in the client's pixel format.
    pub const RAW: i32 = 0;
    /// A rectangle copied from elsewhere in the same framebuffer.
    pub const COPY_RECT: i32 = 1;
    /// Tiles with JPEG or zlib inside, segmented into fewer rectangles.
    pub const TIGHT: i32 = 7;
    /// The cursor's shape, so the viewer can draw the pointer itself.
    ///
    /// A pseudo-encoding, and a negative number for that reason: it is not a
    /// rectangle of the framebuffer. The rectangle's position is the cursor's hot
    /// spot and its size is the cursor's size, and the pixels that follow are the
    /// cursor's own, not the screen's.
    pub const CURSOR: i32 = -239;
}

/// What this server will send, in the order it prefers them.
///
/// Tight because it is the only one that can carry JPEG, and a JPEG tile of a
/// browser window is a fraction of the same tile raw. Raw last, because every
/// viewer must accept it. Hextile is not offered: this server has nothing that
/// compresses a Hextile tile, and claiming an encoding it does not use would be
/// a promise in a header that the pixels do not keep.
pub const PREFERENCE: [i32; 2] = [encoding::TIGHT, encoding::RAW];

/// Which encoding to use for a viewer that offered `offered`.
///
/// A viewer that offered nothing at all — an old client that never sends
/// `SetEncodings` — gets Raw, which is the only encoding every viewer is
/// required to understand.
/// The bitmask of a cursor: one bit per pixel, 1 where the pixel is visible.
///
/// Scanlines are left to right, top to bottom, each padded to a whole number of
/// bytes, and within a byte the most significant bit is the leftmost pixel.
pub fn cursor_mask(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    let stride = width.div_ceil(8) as usize;
    let mut mask = vec![0u8; stride * height as usize];
    for row in 0..height as usize {
        for column in 0..width as usize {
            let at = (row * width as usize + column) * 4;
            if rgba.get(at + 3).copied().unwrap_or(0) == 0 {
                continue;
            }
            mask[row * stride + column / 8] |= 0x80 >> (column % 8);
        }
    }
    mask
}

/// A cursor's pixels in the viewer's own format.
///
/// The cursor arrives as straight RGBA and every viewer has its own idea of a
/// pixel, so it goes through the same packing as a frame: one conversion, not
/// two, so a 16-bit viewer sees a cursor the colour of its own pictures.
pub fn cursor_pixels(rgba: &[u8], width: u32, height: u32, format: &PixelFormat) -> Vec<u8> {
    // The frame layout the packer speaks is blue, green, red, pad.
    let mut bgrx = Vec::with_capacity((width * height * 4) as usize);
    for pixel in rgba.chunks_exact(4) {
        bgrx.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
    }
    extract(&bgrx, width, (0, 0, width, height), format)
}

pub fn choose(offered: &[i32]) -> i32 {
    PREFERENCE
        .iter()
        .copied()
        .find(|candidate| offered.contains(candidate))
        .unwrap_or(encoding::RAW)
}

/// One pixel, in the format a viewer asked for.
///
/// RFB lets a viewer choose its own format, including a 16-bit one and
/// byte-swapped channel order. The frame here is always 32 bits per pixel, blue
/// first in memory (what X hands over on a little-endian machine), so every
/// other arrangement is a conversion.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelFormat {
    pub bits_per_pixel: u8,
    pub depth: u8,
    pub big_endian: bool,
    pub true_colour: bool,
    pub red_max: u16,
    pub green_max: u16,
    pub blue_max: u16,
    pub red_shift: u8,
    pub green_shift: u8,
    pub blue_shift: u8,
}

impl PixelFormat {
    /// The format this server announces, and the one its own frames are already
    /// in: 32 bits, red in the top byte of three.
    pub const NATIVE: PixelFormat = PixelFormat {
        bits_per_pixel: 32,
        depth: 24,
        big_endian: false,
        true_colour: true,
        red_max: 255,
        green_max: 255,
        blue_max: 255,
        red_shift: 16,
        green_shift: 8,
        blue_shift: 0,
    };

    /// The sixteen bytes of `ServerInit` or `SetPixelFormat`.
    pub fn to_wire(self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[0] = self.bits_per_pixel;
        out[1] = self.depth;
        out[2] = self.big_endian as u8;
        out[3] = self.true_colour as u8;
        out[4..6].copy_from_slice(&self.red_max.to_be_bytes());
        out[6..8].copy_from_slice(&self.green_max.to_be_bytes());
        out[8..10].copy_from_slice(&self.blue_max.to_be_bytes());
        out[10] = self.red_shift;
        out[11] = self.green_shift;
        out[12] = self.blue_shift;
        out
    }

    pub fn from_wire(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 16 {
            return None;
        }
        Some(PixelFormat {
            bits_per_pixel: bytes[0],
            depth: bytes[1],
            big_endian: bytes[2] != 0,
            true_colour: bytes[3] != 0,
            red_max: u16::from_be_bytes([bytes[4], bytes[5]]),
            green_max: u16::from_be_bytes([bytes[6], bytes[7]]),
            blue_max: u16::from_be_bytes([bytes[8], bytes[9]]),
            red_shift: bytes[10],
            green_shift: bytes[11],
            blue_shift: bytes[12],
        })
    }

    /// Whether pixels can be copied out of a frame untouched.
    fn is_native(self) -> bool {
        self.bits_per_pixel == 32
            && !self.big_endian
            && self.red_shift == 16
            && self.green_shift == 8
            && self.blue_shift == 0
            && self.red_max == 255
            && self.green_max == 255
            && self.blue_max == 255
    }

    /// The number of bytes one row of pixels takes in this format.
    pub fn bytes_per_pixel(self) -> usize {
        (self.bits_per_pixel as usize) / 8
    }

    /// Packs one RGB colour into this format's bits.
    fn pack(self, red: u8, green: u8, blue: u8) -> u32 {
        let red = ((red as u32 * self.red_max as u32) / 255) << self.red_shift;
        let green = ((green as u32 * self.green_max as u32) / 255) << self.green_shift;
        let blue = ((blue as u32 * self.blue_max as u32) / 255) << self.blue_shift;
        red | green | blue
    }
}

/// Whether the viewer wants exactly what the frame holds.
pub fn is_native(format: &PixelFormat) -> bool {
    format.is_native()
}

/// Copies a rectangle out of a frame into a viewer's pixel format.
///
/// `frame` is the whole screen as X hands it over: four bytes a pixel, blue
/// first. `region` is `(x, y, width, height)` inside it.
pub fn extract(
    frame: &[u8],
    frame_width: u32,
    region: (u32, u32, u32, u32),
    format: &PixelFormat,
) -> Vec<u8> {
    let (x, y, width, height) = region;
    let stride = frame_width as usize * 4;
    let bytes = format.bytes_per_pixel();
    let mut out = vec![0u8; width as usize * height as usize * bytes];
    for row in 0..height as usize {
        for column in 0..width as usize {
            let source = (y as usize + row) * stride + (x as usize + column) * 4;
            let pixel = if format.is_native() {
                // Blue, green, red, pad — which is what a little-endian machine
                // means by a 32-bit pixel with red in the top byte.
                u32::from_le_bytes([
                    frame[source],
                    frame[source + 1],
                    frame[source + 2],
                    frame[source + 3],
                ])
            } else {
                format.pack(frame[source + 2], frame[source + 1], frame[source])
            };
            let at = (row * width as usize + column) * bytes;
            match bytes {
                4 => {
                    let packed = if format.big_endian {
                        pixel.to_be_bytes()
                    } else {
                        pixel.to_le_bytes()
                    };
                    out[at..at + 4].copy_from_slice(&packed);
                }
                2 => {
                    let packed = if format.big_endian {
                        (pixel as u16).to_be_bytes()
                    } else {
                        (pixel as u16).to_le_bytes()
                    };
                    out[at..at + 2].copy_from_slice(&packed);
                }
                _ => {
                    out[at] = pixel as u8;
                }
            }
        }
    }
    out
}

/// The tiles of changed pixels, as rectangles a viewer can be sent.
///
/// A whole-screen rectangle is what a viewer asks for, but sending one back
/// means sending the screen's worth of bytes for a cursor that moved. This cuts
/// it into tiles and keeps only the ones that differ from the previous frame,
/// which is the same idea the app's own transport uses.
pub fn changed_tiles(
    frame: &[u8],
    previous: &[u8],
    width: u32,
    height: u32,
    tile: u32,
) -> Vec<(u32, u32, u32, u32)> {
    let stride = width as usize * 4;
    let mut tiles = Vec::new();
    let mut y = 0;
    while y < height {
        let mut x = 0;
        while x < width {
            let w = tile.min(width - x);
            let h = tile.min(height - y);
            let mut differs = false;
            for row in 0..h as usize {
                let start = (y as usize + row) * stride + x as usize * 4;
                let end = start + w as usize * 4;
                if frame[start..end] != previous[start..end] {
                    differs = true;
                    break;
                }
            }
            if differs {
                tiles.push((x, y, w, h));
            }
            x += tile;
        }
        y += tile;
    }
    tiles
}

/// A Tight rectangle's control byte for a JPEG tile.
///
/// The top four bits are the subencoding and the bottom four are the state of
/// the zlib stream, which a JPEG tile does not use. Quality is carried in the
/// low bits of the subencoding, 1 to 9: 9 is the best picture.
pub fn tight_jpeg_control(quality: u8) -> u8 {
    let quality = quality.clamp(1, 9);
    0x90 | quality
}

/// A length, in Tight's compact form: one to three bytes.
pub fn tight_length(len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![len as u8]
    } else if len < 0x4000 {
        vec![((len & 0x7f) as u8) | 0x80, (len >> 7) as u8]
    } else {
        vec![
            ((len & 0x7f) as u8) | 0x80,
            (((len >> 7) & 0x7f) as u8) | 0x80,
            (len >> 14) as u8,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_viewer_version_is_answered_with_the_lower_of_the_two() {
        assert_eq!(agreed_version(b"RFB 003.008\n"), Some((3, 8)));
        assert_eq!(agreed_version(b"RFB 003.003\n"), Some((3, 3)));
        assert_eq!(agreed_version(b"RFB 003.889\n"), Some((3, 8)));
    }

    #[test]
    fn a_greeting_that_is_not_rfb_is_refused() {
        assert_eq!(agreed_version(b"HTTP/1.1 200"), None);
        assert_eq!(agreed_version(b"RFB 004.000\n"), None);
        assert_eq!(agreed_version(b"RFB xx"), None);
    }

    #[test]
    fn the_encoding_chosen_is_the_best_one_offered() {
        // A modern viewer offers everything and gets Tight.
        assert_eq!(
            choose(&[encoding::RAW, 5, encoding::TIGHT]),
            encoding::TIGHT
        );
        // One that offers no Tight gets Raw, which it must accept.
        assert_eq!(choose(&[encoding::RAW, 5]), encoding::RAW);
        // A viewer that said nothing gets Raw.
        assert_eq!(choose(&[]), encoding::RAW);
        // An encoding this server does not implement is never chosen.
        assert_eq!(choose(&[2, 6, 16]), encoding::RAW);
    }

    #[test]
    fn the_native_format_is_the_frames_own() {
        assert!(is_native(&PixelFormat::NATIVE));
        let mut sixteen = PixelFormat::NATIVE;
        sixteen.bits_per_pixel = 16;
        assert!(!is_native(&sixteen));
    }

    #[test]
    fn a_pixel_format_survives_the_wire() {
        let bytes = PixelFormat::NATIVE.to_wire();
        assert_eq!(PixelFormat::from_wire(&bytes), Some(PixelFormat::NATIVE));
        // Red must come out in the top byte of three, whose shift is 16.
        assert_eq!(bytes[10], 16);
        assert_eq!(u16::from_be_bytes([bytes[4], bytes[5]]), 255);
    }

    #[test]
    fn a_native_extract_is_the_frame_itself() {
        // Two pixels, stored the way X gives them: blue, green, red, pad.
        let frame = vec![0x10, 0x20, 0x30, 0, 0x11, 0x21, 0x31, 0];
        let out = extract(&frame, 2, (0, 0, 2, 1), &PixelFormat::NATIVE);
        assert_eq!(out, frame);
    }

    #[test]
    fn a_sixteen_bit_viewer_gets_its_own_colour_shifts() {
        let frame = vec![0x00, 0x00, 0xff, 0]; // pure red
        let format = PixelFormat {
            bits_per_pixel: 16,
            depth: 16,
            big_endian: true,
            true_colour: true,
            red_max: 31,
            green_max: 63,
            blue_max: 31,
            red_shift: 11,
            green_shift: 5,
            blue_shift: 0,
        };
        let out = extract(&frame, 1, (0, 0, 1, 1), &format);
        assert_eq!(out, vec![0xf8, 0x00]);
    }

    #[test]
    fn a_region_is_cut_out_of_the_right_place() {
        // Four pixels in a row: only the third is white.
        let frame = vec![0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0xff, 0, 0, 0, 0, 0];
        let out = extract(&frame, 4, (2, 0, 1, 1), &PixelFormat::NATIVE);
        assert_eq!(out, vec![0xff, 0xff, 0xff, 0]);
    }

    #[test]
    fn only_tiles_that_changed_are_listed() {
        // Two tiles across, so a change in the second one can be told apart from
        // a change in the first.
        let width = 128;
        let height = 32;
        let black = vec![0u8; width as usize * height as usize * 4];
        let mut changed = black.clone();
        // One pixel in the second tile of the first row.
        let at = 68 * 4;
        changed[at] = 0xff;
        let tiles = changed_tiles(&changed, &black, width, height, 64);
        assert_eq!(tiles, vec![(64, 0, 64, 32)]);
        assert!(changed_tiles(&black, &black, width, height, 64).is_empty());
    }

    #[test]
    fn the_last_tiles_are_clipped_to_the_screen() {
        let width = 100;
        let height = 40;
        let frame = vec![1u8; width as usize * height as usize * 4];
        let previous = vec![0u8; width as usize * height as usize * 4];
        let tiles = changed_tiles(&frame, &previous, width, height, 64);
        // 2 columns (64 + 36) by 1 row (40).
        assert_eq!(tiles, vec![(0, 0, 64, 40), (64, 0, 36, 40)]);
    }

    #[test]
    fn a_tight_control_byte_says_jpeg_and_its_quality() {
        assert_eq!(tight_jpeg_control(7), 0x97);
        assert_eq!(tight_jpeg_control(0), 0x91);
        assert_eq!(tight_jpeg_control(99), 0x99);
    }

    #[test]
    fn a_cursor_mask_marks_the_visible_pixels() {
        // Two pixels across, the first and the third visible: the top bit of
        // each row's byte, twice.
        let rgba = [
            255, 255, 255, 255, 0, 0, 0, 0, 255, 255, 255, 255, 0, 0, 0, 0,
        ];
        assert_eq!(cursor_mask(&rgba, 2, 2), vec![0b1000_0000, 0b1000_0000]);
    }

    #[test]
    fn a_cursor_wider_than_eight_pixels_pads_its_mask() {
        // Nine pixels needs two bytes a row, and the ninth lands in the second.
        let mut rgba = vec![0u8; 9 * 4];
        rgba[8 * 4 + 3] = 255;
        assert_eq!(cursor_mask(&rgba, 9, 1), vec![0b0000_0000, 0b1000_0000]);
    }

    #[test]
    fn a_transparent_cursor_has_an_empty_mask() {
        // An invisible cursor reads as nothing visible; drawing the dot that
        // stands in for it is the viewer's business, not the wire's.
        assert_eq!(cursor_mask(&[0u8; 4 * 4], 2, 2), vec![0, 0]);
    }

    #[test]
    fn cursor_pixels_take_the_viewers_format() {
        // The native format is blue, green, red, pad in memory.
        let red = [255u8, 0, 0, 255];
        assert_eq!(
            cursor_pixels(&red, 1, 1, &PixelFormat::NATIVE),
            vec![0, 0, 255, 255]
        );
        // A sixteen-bit viewer gets two bytes a pixel, not four.
        let mut sixteen = PixelFormat::NATIVE;
        sixteen.bits_per_pixel = 16;
        sixteen.depth = 16;
        sixteen.red_max = 31;
        sixteen.green_max = 63;
        sixteen.blue_max = 31;
        sixteen.red_shift = 11;
        sixteen.green_shift = 5;
        sixteen.blue_shift = 0;
        assert_eq!(cursor_pixels(&red, 1, 1, &sixteen).len(), 2);
    }

    #[test]
    fn a_tight_length_uses_as_few_bytes_as_it_can() {
        assert_eq!(tight_length(0x7f), vec![0x7f]);
        assert_eq!(tight_length(0x80), vec![0x80, 0x01]);
        assert_eq!(tight_length(0x3fff), vec![0xff, 0x7f]);
        assert_eq!(tight_length(0x4000), vec![0x80, 0x80, 0x01]);
    }
}
