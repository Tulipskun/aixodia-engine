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
    let mut updates = Vec::new();
    let stride = (width as usize) * 4;
    let columns = width.div_ceil(tile_side);
    let rows = height.div_ceil(tile_side);

    for row in 0..rows {
        for col in 0..columns {
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
                continue;
            }

            // Copy the tile into a contiguous buffer, in the order the JPEG
            // encoder wants to see it, and remember it as the new baseline.
            let mut tile = Vec::with_capacity((tile_width as usize) * (tile_height as usize) * 4);
            for line in 0..tile_height as usize {
                let start = (y as usize + line) * stride + (x as usize) * 4;
                let end = start + (tile_width as usize) * 4;
                tile.extend_from_slice(&frame[start..end]);
                previous[start..end].copy_from_slice(&frame[start..end]);
            }

            if let Some(jpeg) = encode_tile(&tile, tile_width, tile_height, settings.quality) {
                updates.push(TileUpdate {
                    col,
                    row,
                    width: tile_width,
                    height: tile_height,
                    jpeg,
                });
            }
        }
    }

    updates
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
