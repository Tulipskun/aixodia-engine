//! Turns a picture into something a vision model can be sent.
//!
//! Port of the Go `imageprep` package. Phone screenshots are 3–8 MB and 3000 px
//! wide, which is both more pixels than a model samples and more bytes than a
//! request wants to carry as base64. Everything here exists to make that number
//! small without making the picture unreadable.

use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, GenericImageView, ImageFormat, RgbImage, RgbaImage};

/// The long edge a vision model actually samples.
pub const MAX_EDGE: u32 = 1568;

/// A ceiling on the encoded result.
pub const MAX_BYTES: usize = 1200 * 1024;

/// Quality of the first attempt.
pub const QUALITY: u8 = 82;

/// The point below which shrinking stops being worth it.
const MIN_EDGE: u32 = 512;

/// The result: the bytes, the MIME type they now are, and what was done to get
/// there.
#[derive(Debug, Clone, Default)]
pub struct Prepared {
    pub data: Vec<u8>,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub original_width: u32,
    pub original_height: u32,
    /// False only when the input was already a small JPEG, in which case the
    /// bytes are passed through untouched.
    pub resized: bool,
    pub reencoded: bool,
}

impl Prepared {
    /// The factor the long edge was reduced by, 1 when untouched.
    pub fn scale(&self) -> f64 {
        let longest = self.original_width.max(self.original_height);
        if longest == 0 {
            return 1.0;
        }
        f64::from(self.width.max(self.height)) / f64::from(longest)
    }
}

/// Decodes a picture and returns a JPEG no larger than [`MAX_EDGE`] on its long
/// side and no bigger than [`MAX_BYTES`].
pub fn prepare(raw: &[u8]) -> Result<Prepared, String> {
    prepare_within(raw, MAX_BYTES)
}

/// [`prepare`] with a caller-chosen byte budget.
pub fn prepare_within(raw: &[u8], max_bytes: usize) -> Result<Prepared, String> {
    let format =
        image::guess_format(raw).map_err(|err| format!("not a picture this can decode: {err}"))?;
    let src = image::load_from_memory_with_format(raw, format)
        .map_err(|err| format!("not a picture this can decode: {err}"))?;
    let (width, height) = src.dimensions();
    if width == 0 || height == 0 {
        return Err("picture has no pixels".to_string());
    }
    let media_type = match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Png => "image/png",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        ImageFormat::Bmp => "image/bmp",
        ImageFormat::Tiff => "image/tiff",
        _ => "image/octet-stream",
    };
    let mut out = Prepared {
        data: raw.to_vec(),
        media_type: media_type.to_string(),
        width,
        height,
        original_width: width,
        original_height: height,
        resized: false,
        reencoded: false,
    };
    let max_bytes = if max_bytes == 0 { MAX_BYTES } else { max_bytes };
    let is_jpeg = format == ImageFormat::Jpeg;
    if is_jpeg && fits(width, height) && raw.len() <= max_bytes {
        return Ok(out);
    }

    // Each pass is a smaller box; within a box the quality steps down.
    let mut edge = MAX_EDGE;
    loop {
        let target = fit_to(width, height, edge);
        let scaled: DynamicImage = if target.0 != width || target.1 != height {
            DynamicImage::ImageRgba8(resize(&src, target.0, target.1))
        } else {
            src.clone()
        };
        for quality in [QUALITY, 70, 58, 45] {
            let encoded = encode_jpeg(&scaled, quality)?;
            out.reencoded = true;
            out.media_type = "image/jpeg".to_string();
            out.width = target.0;
            out.height = target.1;
            out.resized = target.0 != width || target.1 != height;
            let fits_budget = encoded.len() <= max_bytes;
            out.data = encoded;
            if fits_budget {
                return Ok(out);
            }
        }
        if edge <= MIN_EDGE {
            return Ok(out);
        }
        edge = edge * 3 / 4;
    }
}

fn fits(width: u32, height: u32) -> bool {
    width <= MAX_EDGE && height <= MAX_EDGE
}

/// The largest box with the same shape as the input that fits inside `edge`.
fn fit_to(width: u32, height: u32, edge: u32) -> (u32, u32) {
    if width <= edge && height <= edge {
        return (width, height);
    }
    if width >= height {
        (edge, (height * edge / width).max(1))
    } else {
        ((width * edge / height).max(1), edge)
    }
}

fn encode_jpeg(image: &DynamicImage, quality: u8) -> Result<Vec<u8>, String> {
    // JPEG has no alpha channel; dropping it (rather than blending) is what the
    // Go encoder does with an RGBA source too.
    let rgb: RgbImage = image.to_rgb8();
    let mut data = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut data, quality);
    encoder
        .encode_image(&DynamicImage::ImageRgb8(rgb))
        .map_err(|err| format!("cannot encode picture: {err}"))?;
    Ok(data)
}

/// Averages the source pixels each destination pixel covers. Every source pixel
/// contributes, so thin lines and small text do not fall between samples.
fn resize(src: &DynamicImage, width: u32, height: u32) -> RgbaImage {
    let mut dst = RgbaImage::new(width, height);
    let (src_w, src_h) = src.dimensions();
    for y in 0..height {
        let y0 = y * src_h / height;
        let mut y1 = (y + 1) * src_h / height;
        if y1 <= y0 {
            y1 = y0 + 1;
        }
        for x in 0..width {
            let x0 = x * src_w / width;
            let mut x1 = (x + 1) * src_w / width;
            if x1 <= x0 {
                x1 = x0 + 1;
            }
            let mut sum = [0u64; 4];
            let mut n = 0u64;
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let pixel = src.get_pixel(sx, sy);
                    for (i, value) in pixel.0.iter().enumerate() {
                        sum[i] += u64::from(*value);
                    }
                    n += 1;
                }
            }
            let n = n.max(1);
            dst.put_pixel(
                x,
                y,
                image::Rgba([
                    (sum[0] / n) as u8,
                    (sum[1] / n) as u8,
                    (sum[2] / n) as u8,
                    (sum[3] / n) as u8,
                ]),
            );
        }
    }
    dst
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut img = RgbaImage::new(width, height);
        for (x, y, pixel) in img.enumerate_pixels_mut() {
            *pixel = Rgba([(x % 255) as u8, (y % 255) as u8, 128, 255]);
        }
        let mut out = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut out, ImageFormat::Png)
            .expect("encode png");
        out.into_inner()
    }

    #[test]
    fn a_large_picture_shrinks_to_the_long_edge() {
        let raw = png(3000, 1000);
        let prepared = prepare(&raw).expect("prepare");
        assert_eq!(prepared.media_type, "image/jpeg");
        assert_eq!(prepared.width, MAX_EDGE);
        assert!(prepared.height < MAX_EDGE);
        assert!(prepared.resized);
        assert_eq!(prepared.original_width, 3000);
        assert!(prepared.scale() < 1.0);
    }

    #[test]
    fn a_small_jpeg_is_passed_through() {
        let small = DynamicImage::ImageRgba8(RgbaImage::new(40, 30));
        let mut out = std::io::Cursor::new(Vec::new());
        small
            .write_to(&mut out, ImageFormat::Jpeg)
            .expect("encode jpeg");
        let raw = out.into_inner();
        let prepared = prepare(&raw).expect("prepare");
        assert!(!prepared.reencoded);
        assert!(!prepared.resized);
        assert_eq!(prepared.data, raw);
        assert_eq!((prepared.width, prepared.height), (40, 30));
    }

    #[test]
    fn a_flat_colour_budget_is_met_by_lowering_quality_or_pixels() {
        let mut img = RgbaImage::new(900, 900);
        for pixel in img.pixels_mut() {
            *pixel = Rgba([20, 40, 60, 255]);
        }
        let mut raw = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut raw, ImageFormat::Png)
            .expect("encode png");
        let prepared = prepare_within(&raw.into_inner(), 20_000).expect("prepare");
        assert!(prepared.data.len() <= 20_000);
        assert!(prepared.reencoded);
    }

    #[test]
    fn nonsense_is_refused() {
        assert!(prepare(b"not a picture").is_err());
    }
}
