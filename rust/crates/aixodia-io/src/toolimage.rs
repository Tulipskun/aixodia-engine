//! Pictures a tool handed back as text.
//!
//! Port of the Go `io/gateway/toolimage.go`. A tool can hand a picture back only
//! as text, and the one form both tool authors and the wire shapes agree on is a
//! base64 data URI. Sent on as text it is worth nothing: a provider reads
//! picture bytes from an image part, never from a string, so the model is blind
//! to it and the base64 only fills the context.

use aixodia_provider::Image;
use base64::Engine as _;

/// Caps one tool round. Each picture costs image tokens, and a result carrying a
/// dozen is a mistake more often than an intent.
pub const MAX_TOOL_IMAGES: usize = 8;
/// Caps the decoded size a single round may add.
pub const MAX_TOOL_IMAGE_BYTES: usize = 4 << 20;

/// Replaces every image data URI in `text` with a marker and returns the decoded
/// pictures in order. Anything past a cap is marked as omitted rather than
/// attached, so one result cannot flood a request. Non-image data URIs and
/// malformed ones are left exactly as they were.
pub fn extract_tool_images(text: &str) -> (String, Vec<Image>) {
    let mut images: Vec<Image> = Vec::new();
    let mut out = String::new();
    let mut total = 0usize;
    let mut rest = text;

    while let Some(start) = rest.find("data:image/") {
        match parse_image_data_uri(&rest[start..]) {
            None => {
                // Not a picture URI (or not decodable): keep the text and move
                // past its "data:" so the next search cannot match the same spot
                // again.
                out.push_str(&rest[..start + "data:".len()]);
                rest = &rest[start + "data:".len()..];
            }
            Some((media, data, span)) => {
                out.push_str(&rest[..start]);
                if images.len() >= MAX_TOOL_IMAGES {
                    out.push_str("[picture omitted: this turn already carries enough]");
                } else if total + data.len() > MAX_TOOL_IMAGE_BYTES {
                    out.push_str("[picture omitted: too large for this turn]");
                } else {
                    total += data.len();
                    images.push(Image {
                        media_type: media.clone(),
                        data,
                    });
                    out.push_str(&format!("[picture attached below: {media}]"));
                }
                rest = &rest[start + span..];
            }
        }
    }
    out.push_str(rest);
    (out, images)
}

/// Parses a base64 image data URI at the start of `s` and reports the media
/// type, the decoded bytes, and how many bytes of `s` the URI spans. It refuses
/// anything but an image and anything whose payload does not decode, because a
/// data URI that claims to be a picture and holds something else is worse than
/// leaving the text alone.
pub fn parse_image_data_uri(s: &str) -> Option<(String, Vec<u8>, usize)> {
    const PREFIX: &str = "data:";
    let bytes = s.as_bytes();
    if !s.starts_with(PREFIX) {
        return None;
    }
    let comma = s.find(";base64,")?;
    let media = &s[PREFIX.len()..comma];
    if !media.starts_with("image/") {
        return None;
    }
    let payload = comma + ";base64,".len();
    let mut end = payload;
    while end < bytes.len() && is_base64_byte(bytes[end]) {
        end += 1;
    }
    if end == payload {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&bytes[payload..end])
        .ok()?;
    if decoded.is_empty() {
        return None;
    }
    Some((media.to_string(), decoded, end))
}

fn is_base64_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'=')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uri(media: &str, payload: &[u8]) -> String {
        format!(
            "data:{media};base64,{}",
            base64::engine::general_purpose::STANDARD.encode(payload)
        )
    }

    #[test]
    fn lifts_a_picture_out_of_the_text() {
        let text = format!("here it is {}", uri("image/png", b"hello"));
        let (out, images) = extract_tool_images(&text);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media_type, "image/png");
        assert_eq!(images[0].data, b"hello");
        assert_eq!(out, "here it is [picture attached below: image/png]");
    }

    #[test]
    fn leaves_non_image_data_uris_alone() {
        let text = "data:text/plain;base64,aGVsbG8=";
        let (out, images) = extract_tool_images(text);
        assert!(images.is_empty());
        assert_eq!(out, text);
    }

    #[test]
    fn omits_pictures_past_the_cap() {
        let mut text = String::new();
        for _ in 0..(MAX_TOOL_IMAGES + 1) {
            text.push_str(&uri("image/png", b"x"));
            text.push(' ');
        }
        let (out, images) = extract_tool_images(&text);
        assert_eq!(images.len(), MAX_TOOL_IMAGES);
        assert!(out.contains("[picture omitted: this turn already carries enough]"));
    }

    #[test]
    fn a_malformed_uri_stays_as_text_and_does_not_loop() {
        let text = "data:image/png;base64,!!!";
        let (out, images) = extract_tool_images(text);
        assert!(images.is_empty());
        assert_eq!(out, text);
    }
}
