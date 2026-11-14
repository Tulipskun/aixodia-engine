//! Turning two frames into the smallest thing that still looks right on a
//! phone.
//!
//! A phone on a phone network cannot be sent a 1080p frame thirty times a
//! second — that is roughly six megabytes each, and the link is well under one.
//! So the screen is cut into tiles, each tile is compared with the previous
//! frame, and only the tiles that changed are encoded and sent. Typing in a
//! terminal changes a handful of tiles; scrolling a browser changes most of the
//! page, which is exactly when the extra bytes are worth it.
//!
//! Each changed tile is JPEG-encoded on its own rather than the whole frame
//! being re-encoded: a tile is self-contained, so a client can paint one without
//! waiting for anything around it, and a dropped frame costs one tile instead of
//! the picture.

use rayon::prelude::*;
use std::io::Cursor;

use image::{ExtendedColorType, ImageEncoder};

/// Side of one tile, in pixels. A multiple of 16 keeps the JPEG chroma
/// subsampling aligned and the comparison cheap. The live value is part of
/// [`Settings`]; this is only the default.
pub const TILE: u32 = 64;

/// JPEG quality for a changed tile. High enough that text stays readable at a
/// phone's zoom, low enough that a full-screen change still fits a few hundred
/// kilobytes. The live value is part of [`Settings`].
const QUALITY: u8 = 62;

/// How a frame is cut up and encoded. Every field is adjustable while the
/// engine is running: the client sends a new set and the next frame obeys it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeSettings {
    /// Side of a tile, in pixels. Smaller means less to resend per change and
    /// more per-tile overhead; 64 is a good middle for text and pages.
    pub tile: u32,
    /// JPEG quality, 1..=95. Lower is smaller and blurrier.
    pub quality: u8,
}

impl Default for EncodeSettings {
    fn default() -> Self {
        Self {
            tile: TILE,
            quality: QUALITY,
        }
    }
}

impl EncodeSettings {
    /// Clamps what a client asked for into something the encoder can honour.
    pub fn sanitized(self) -> Self {
        Self {
            tile: self.tile.clamp(16, 256),
            quality: self.quality.clamp(10, 95),
        }
    }
}

/// One changed tile, ready to travel.
pub struct TileUpdate {
    /// Tile column and row on the grid (not pixels).
    pub col: u32,
    pub row: u32,
    /// Tile width and height in pixels — the last column and row of a frame are
    /// clipped, so a client cannot assume `TILE`.
    pub width: u32,
    pub height: u32,
    pub jpeg: Vec<u8>,
}

/// Compares a frame with the one before it and encodes what changed.
///
/// `previous` is updated in place to the new frame, so the next call has
/// something to compare against without the caller keeping two copies.
pub fn diff_and_encode(
    frame: &[u8],
    width: u32,
    height: u32,
    previous: &mut [u8],
    force_all: bool,
    settings: EncodeSettings,
) -> Vec<TileUpdate> {
    let tile_side = settings.tile.max(1);
    let stride = (width as usize) * 4;
    let columns = width.div_ceil(tile_side);
    let rows = height.div_ceil(tile_side);

    // The tiles are independent and the work is mostly JPEG compression, so a
    // whole-screen frame — 240 tiles, 166 ms on one thread here — is spread
    // across the cores instead. Without this a frame that changes everything
    // (a scroll, a new window) costs ~180 ms, which is five frames a second no
    // matter what rate the settings ask for.
    let encode = |row: u32, col: u32| -> Option<(usize, TileUpdate)> {
        let x = col * tile_side;
        let y = row * tile_side;
        let tile_width = tile_side.min(width - x);
        let tile_height = tile_side.min(height - y);

        let changed = force_all || {
            let mut differs = false;
            for line in 0..tile_height as usize {
                let start = (y as usize + line) * stride + (x as usize) * 4;
                let end = start + (tile_width as usize) * 4;
                if frame[start..end] != previous[start..end] {
                    differs = true;
                    break;
                }
            }
            differs
        };
        if !changed {
            return None;
        }

        // Copy the tile into a contiguous buffer, in the order the JPEG encoder
        // wants to see it.
        let mut tile = Vec::with_capacity((tile_width as usize) * (tile_height as usize) * 4);
        for line in 0..tile_height as usize {
            let start = (y as usize + line) * stride + (x as usize) * 4;
            let end = start + (tile_width as usize) * 4;
            tile.extend_from_slice(&frame[start..end]);
        }

        let jpeg = encode_tile(&tile, tile_width, tile_height, settings.quality)?;
        Some((
            (row * columns + col) as usize,
            TileUpdate {
                col,
                row,
                width: tile_width,
                height: tile_height,
                jpeg,
            },
        ))
    };

    // Each tile is read from the frame and written to the baseline at its own
    // offsets, so the passes never overlap and the baseline can be updated in
    // place rather than in a second sweep.
    let rows_wanted: Vec<u32> = (0..rows).collect();
    let mut found: Vec<(usize, TileUpdate)> = rows_wanted
        .par_iter()
        .flat_map_iter(|&row| (0..columns).filter_map(move |col| encode(row, col)))
        .collect();

    // The baseline is written after the fact, from the frame itself, so the
    // pass above stays read-only and can run in parallel.
    for (_, update) in &found {
        let x = update.col * tile_side;
        let y = update.row * tile_side;
        for line in 0..update.height as usize {
            let start = (y as usize + line) * stride + (x as usize) * 4;
            let end = start + (update.width as usize) * 4;
            previous[start..end].copy_from_slice(&frame[start..end]);
        }
    }

    // Ordered by position so the picture is painted in the same order every
    // time, which keeps a frame of tiles from arriving in a random arrangement.
    found.sort_by_key(|(order, _)| *order);
    found.into_iter().map(|(_, update)| update).collect()
}

/// An H.264 encoder for whole frames.
///
/// The tile path compresses each tile on its own, so a frame that changes
/// everywhere costs one JPEG per tile — measured here at 160 ms for a whole
/// screen, of which the pixel gathering is under a millisecond. H.264 encodes
/// the frame once and, more importantly, codes only the differences between
/// frames, so scrolling costs a fraction of what a grid of independent JPEGs
/// costs.
pub struct H264Encoder {
    encoder: openh264::encoder::Encoder,
    width: u32,
    height: u32,
    /// Kept as a field so a frame's pixels are not gathered twice.
    #[allow(dead_code)]
    packed: Vec<u8>,
    /// The YUV frame, kept between frames so its buffers are reused.
    yuv: openh264::formats::YUVBuffer,
}

impl H264Encoder {
    /// Builds an encoder for frames of `width` by `height`.
    pub fn new(width: u32, height: u32) -> Result<Self, String> {
        let config = openh264::encoder::EncoderConfig::new()
            .usage_type(openh264::encoder::UsageType::ScreenContentRealTime);
        let encoder = openh264::encoder::Encoder::with_api_config(
            openh264::OpenH264API::from_source(),
            config,
        )
        .map_err(|e| format!("h264 encoder: {e}"))?;
        Ok(Self {
            encoder,
            width,
            height,
            packed: Vec::new(),
            yuv: openh264::formats::YUVBuffer::new(width as usize, height as usize),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Encodes one frame of BGRX pixels into an H.264 access unit.
    pub fn encode(&mut self, bgrx: &[u8]) -> Result<Vec<u8>, String> {
        let pixels = (self.width as usize) * (self.height as usize);
        if bgrx.len() < pixels * 4 {
            return Err("frame is smaller than its size claims".to_string());
        }

        // The X frame is four bytes a pixel, blue first. That is exactly what
        // the library's BGRA source reads, so the pixels go in as they are and
        // the unused pad byte is ignored — no per-pixel copy at all.
        let source = openh264::formats::BgraSliceU8::new(
            bgrx,
            (self.width as usize, self.height as usize),
        );
        // Colour conversion is one call; doing it by hand would duplicate the
        // library's own and gain nothing.
        self.yuv.read_rgb(source);
        let bitstream = self
            .encoder
            .encode(&self.yuv)
            .map_err(|e| format!("h264 encode: {e}"))?;
        Ok(bitstream.to_vec())
    }
}

/// Encodes one tile as JPEG.
///
/// The pixels arrive as BGRX — blue, green, red, pad — because that is what the
/// X server hands over and converting the whole frame first would cost more
/// than the encode. Only the three meaningful bytes of each pixel are copied
/// into the RGB buffer the encoder takes.
fn encode_tile(pixels: &[u8], width: u32, height: u32, quality: u8) -> Option<Vec<u8>> {
    let count = (width as usize) * (height as usize);
    let mut rgb = Vec::with_capacity(count * 3);
    for pixel in pixels.chunks_exact(4) {
        rgb.push(pixel[2]);
        rgb.push(pixel[1]);
        rgb.push(pixel[0]);
    }

    let mut out = Cursor::new(Vec::new());
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    encoder
        .write_image(&rgb, width, height, ExtendedColorType::Rgb8)
        .ok()?;
    Some(out.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, value: u8) -> Vec<u8> {
        vec![value; (width as usize) * (height as usize) * 4]
    }

    #[test]
    fn an_unchanged_frame_sends_nothing() {
        let width = 128;
        let height = 128;
        let frame = solid(width, height, 0x40);
        let mut previous = frame.clone();
        let updates = diff_and_encode(&frame, width, height, &mut previous, false, EncodeSettings::default());
        assert!(updates.is_empty(), "nothing changed, so nothing should travel");
    }

    #[test]
    fn only_the_tile_that_changed_travels() {
        let width = 128;
        let height = 128;
        let mut frame = solid(width, height, 0x40);
        let mut previous = frame.clone();

        // Paint the second tile of the top row: columns 64..128, rows 0..64.
        for y in 0..64usize {
            let start = y * (width as usize) * 4 + 64 * 4;
            for x in 0..64usize {
                frame[start + x * 4] = 0xFF;
            }
        }

        let updates = diff_and_encode(&frame, width, height, &mut previous, false, EncodeSettings::default());
        assert_eq!(updates.len(), 1, "one tile changed");
        assert_eq!((updates[0].col, updates[0].row), (1, 0));
        assert!(!updates[0].jpeg.is_empty());
    }

    #[test]
    fn a_clipped_edge_tile_keeps_its_real_size() {
        // 100 wide is one full 64 tile and a 36-pixel remainder.
        let width = 100;
        let height = 70;
        let frame = solid(width, height, 0x20);
        let mut previous = frame.clone();
        let updates = diff_and_encode(&frame, width, height, &mut previous, true, EncodeSettings::default());

        let last_column = updates.iter().filter(|t| t.col == 1).next().expect("edge column");
        assert_eq!(last_column.width, 36);
        let last_row = updates.iter().filter(|t| t.row == 1).next().expect("edge row");
        assert_eq!(last_row.height, 6);
    }

    #[test]
    fn a_forced_frame_sends_every_tile() {
        let width = 128;
        let height = 128;
        let frame = solid(width, height, 0x10);
        let mut previous = frame.clone();
        let updates = diff_and_encode(&frame, width, height, &mut previous, true, EncodeSettings::default());
        // 128 / 64 = 2 columns and 2 rows.
        assert_eq!(updates.len(), 4);
    }

    #[test]
    fn a_smaller_tile_cuts_the_frame_more_finely() {
        let width = 128;
        let height = 128;
        let frame = solid(width, height, 0x30);
        let mut previous = frame.clone();
        let settings = EncodeSettings { tile: 32, quality: 50 }.sanitized();
        let updates = diff_and_encode(&frame, width, height, &mut previous, true, settings);
        // 128 / 32 = 4 columns by 4 rows.
        assert_eq!(updates.len(), 16);
    }

    #[test]
    fn a_quality_outside_the_range_is_clamped() {
        let wild = EncodeSettings { tile: 5, quality: 250 }.sanitized();
        assert_eq!(wild.tile, 16);
        assert_eq!(wild.quality, 95);
    }

    #[test]
    fn an_encoded_tile_is_a_jpeg() {
        let width = 64;
        let height = 64;
        let mut pixels = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let b = if (x / 8 + y / 8) % 2 == 0 { 0x20 } else { 0xE0 };
                pixels.extend_from_slice(&[b, b, b, 0]);
            }
        }
        let jpeg = encode_tile(&pixels, width, height, QUALITY).expect("a tile encodes");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "JPEG start-of-image marker");
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9], "JPEG end-of-image marker");
    }
}

#[cfg(test)]
mod cost_probe {
    //! How long one whole-screen encode actually takes.
    //!
    //! The frame period the settings ask for is meaningless if encoding a frame
    //! costs more than it. This measures the real path — a full-size frame with
    //! one changed pixel — and prints the number, so the settings can be given
    //! a limit that this machine can meet.

    use super::*;

    #[test]
    fn a_whole_screen_encode_is_timed() {
        let width = 1280u32;
        let height = 720u32;
        let frame: Vec<u8> = (0..(width as usize * height as usize * 4))
            .map(|index| (index % 251) as u8)
            .collect();
        let mut previous = vec![0u8; frame.len()];

        let started = std::time::Instant::now();
        let updates = diff_and_encode(
            &frame,
            width,
            height,
            &mut previous,
            true,
            EncodeSettings::default(),
        );
        let whole = started.elapsed();

        // A tick where one tile changed: the case the loop meets most often.
        let mut one = frame.clone();
        one[0] ^= 0xFF;
        let mut baseline = frame.clone();
        let started = std::time::Instant::now();
        let partial = diff_and_encode(
            &one,
            width,
            height,
            &mut baseline,
            false,
            EncodeSettings::default(),
        );
        let single = started.elapsed();

        println!(
            "whole frame: {} tiles in {:?}; one changed tile: {} in {:?}",
            updates.len(),
            whole,
            partial.len(),
            single
        );
        assert!(!updates.is_empty(), "the first frame must send something");
    }
}

#[cfg(test)]
mod tile_size_probe {
    //! Which tile side gives the fastest whole-screen encode.
    //!
    //! A smaller tile finds changes more precisely but costs one encode call per
    //! tile, and the call overhead — not the compression — is what dominates
    //! here. This measures the trade rather than assuming 64 is right.

    use super::*;

    #[test]
    fn a_bigger_tile_encodes_a_whole_screen_faster() {
        let width = 1280u32;
        let height = 720u32;
        let frame: Vec<u8> = (0..(width as usize * height as usize * 4))
            .map(|index| (index % 251) as u8)
            .collect();

        for side in [32u32, 64, 128, 256] {
            let mut previous = vec![0u8; frame.len()];
            let settings = EncodeSettings {
                tile: side,
                quality: 62,
            };
            let started = std::time::Instant::now();
            let updates = diff_and_encode(&frame, width, height, &mut previous, true, settings);
            println!(
                "tile {side}: {} tiles in {:?}",
                updates.len(),
                started.elapsed()
            );
        }
    }
}

#[cfg(test)]
mod encode_split_probe {
    //! Where a whole-screen encode spends its time.
    //!
    //! Gathering a tile's pixels and compressing it are different costs with
    //! different fixes, so they are timed apart rather than assumed.

    use super::*;

    #[test]
    fn gathering_and_compressing_are_timed_apart() {
        let width = 1280u32;
        let height = 720u32;
        let tile = 64u32;
        let frame: Vec<u8> = (0..(width as usize * height as usize * 4))
            .map(|index| (index % 251) as u8)
            .collect();
        let stride = (width as usize) * 4;

        // Gathering: copy each tile's lines into a buffer.
        let started = std::time::Instant::now();
        let mut gathered = 0usize;
        for row in 0..(height / tile) {
            for col in 0..(width / tile) {
                let mut buffer = Vec::with_capacity((tile * tile * 4) as usize);
                for line in 0..tile {
                    let start = (((row * tile + line) * width + col * tile) as usize) * 4;
                    buffer.extend_from_slice(&frame[start..start + (tile * 4) as usize]);
                }
                gathered += buffer.len();
            }
        }
        let gather_time = started.elapsed();

        // Compressing: the same tiles, through the JPEG encoder.
        let started = std::time::Instant::now();
        let mut total = 0usize;
        for row in 0..(height / tile) {
            for col in 0..(width / tile) {
                let mut buffer = Vec::with_capacity((tile * tile * 4) as usize);
                for line in 0..tile {
                    let start = (((row * tile + line) * width + col * tile) as usize) * 4;
                    buffer.extend_from_slice(&frame[start..start + (tile * 4) as usize]);
                }
                if let Some(jpeg) = encode_tile(&buffer, tile, tile, 62) {
                    total += jpeg.len();
                }
            }
        }
        let full_time = started.elapsed();

        let _ = stride;
        println!(
            "gather {:?} ({gathered} bytes); gather+encode {:?} ({total} bytes)",
            gather_time, full_time
        );
        assert!(total > 0);
    }
}

#[cfg(test)]
mod codec_comparison {
    //! H.264 against JPEG tiles, measured on the same frames.
    //!
    //! The plan asks for numbers before choosing, so this is the measurement:
    //! one screenful, then the same screen with a scrolling band through it —
    //! the case the tile path is worst at, because every tile an image moves
    //! across changes and is compressed again from nothing.

    use super::*;

    fn screen(width: usize, height: usize, shift: usize) -> Vec<u8> {
        // Text-like horizontal bands, moved down by `shift` pixels, which is
        // what scrolling looks like to a screen encoder.
        let mut pixels = vec![0u8; width * height * 4];
        for y in 0..height {
            let band = ((y + shift) / 14) % 7;
            let shade = (band * 30) as u8;
            for x in 0..width {
                let at = (y * width + x) * 4;
                pixels[at] = shade;
                pixels[at + 1] = shade.wrapping_add(60);
                pixels[at + 2] = shade.wrapping_add(120);
                pixels[at + 3] = 0;
            }
        }
        pixels
    }

    #[test]
    fn h264_is_measured_against_jpeg_tiles() {
        let width = 1280usize;
        let height = 720usize;

        // JPEG tiles: first frame, then one where everything moved.
        let first = screen(width, height, 0);
        let mut previous = vec![0u8; first.len()];
        let started = std::time::Instant::now();
        let whole = diff_and_encode(
            &first,
            width as u32,
            height as u32,
            &mut previous,
            true,
            EncodeSettings::default(),
        );
        let jpeg_first = started.elapsed();
        let jpeg_first_bytes: usize = whole.iter().map(|u| u.jpeg.len()).sum();

        let scrolled = screen(width, height, 6);
        let started = std::time::Instant::now();
        let moved = diff_and_encode(
            &scrolled,
            width as u32,
            height as u32,
            &mut previous,
            false,
            EncodeSettings::default(),
        );
        let jpeg_scroll = started.elapsed();
        let jpeg_scroll_bytes: usize = moved.iter().map(|u| u.jpeg.len()).sum();

        // H.264: the same two frames through one encoder.
        let mut encoder = H264Encoder::new(width as u32, height as u32)
            .expect("an h264 encoder for a normal screen size");
        let started = std::time::Instant::now();
        let h264_first = encoder.encode(&first).expect("first frame");
        let h264_first_time = started.elapsed();
        let started = std::time::Instant::now();
        let h264_scroll = encoder.encode(&scrolled).expect("second frame");
        let h264_scroll_time = started.elapsed();

        println!("JPEG  first: {:>7} B in {:?}", jpeg_first_bytes, jpeg_first);
        println!("JPEG  scroll:{:>7} B in {:?}", jpeg_scroll_bytes, jpeg_scroll);
        println!("H.264 first: {:>7} B in {:?}", h264_first.len(), h264_first_time);
        println!("H.264 scroll:{:>7} B in {:?}", h264_scroll.len(), h264_scroll_time);

        assert!(!h264_first.is_empty(), "the first frame must carry a picture");
        assert!(
            !h264_scroll.is_empty(),
            "a scrolled frame must carry its changes"
        );
    }
}

#[cfg(test)]
mod h264_stream_shape {
    //! What the access unit actually contains.
    //!
    //! Android's MediaCodec wants Annex B access units — start codes, not the
    //! length-prefixed form some encoders emit. Whether a client can decode the
    //! stream at all depends on this, so it is checked rather than assumed.

    use super::*;

    #[test]
    fn an_access_unit_starts_with_a_start_code() {
        let width = 320usize;
        let height = 240usize;
        let mut pixels = vec![0u8; width * height * 4];
        for (index, byte) in pixels.iter_mut().enumerate() {
            *byte = (index % 200) as u8;
        }

        let mut encoder = H264Encoder::new(width as u32, height as u32).expect("encoder");
        let first = encoder.encode(&pixels).expect("first frame");
        assert!(first.len() > 4, "a frame must carry something");
        let starts_with_start_code =
            first.starts_with(&[0, 0, 0, 1]) || first.starts_with(&[0, 0, 1]);
        println!(
            "first frame {} bytes, starts with {}",
            first.len(),
            if first.starts_with(&[0, 0, 0, 1]) {
                "a 4-byte start code"
            } else if first.starts_with(&[0, 0, 1]) {
                "a 3-byte start code"
            } else {
                "something else"
            }
        );
        assert!(
            starts_with_start_code,
            "a MediaCodec decoder reads Annex B, so the unit must be start-code framed"
        );
    }
}
