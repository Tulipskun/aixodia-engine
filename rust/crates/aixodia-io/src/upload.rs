//! Attachment uploads.
//!
//! Port of the Go `io/gateway/upload.go`. The phone sends the raw bytes of a
//! picked file and gets back the path the daemon saved it to. That path travels
//! with the user's message, so the agent — which already has shell and read —
//! can open the file itself. Nothing about the file's type needs to be
//! understood here; images, video and documents all take the same route.

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::gateway::{write_error, write_json, Gateway};

/// Caps one attachment. The body is streamed to disk, never held in memory
/// whole, and a file over the cap is refused rather than truncated.
pub const MAX_UPLOAD: u64 = 128 << 20;

/// The upload cap, for callers that only want the number.
pub fn max_upload() -> u64 {
    MAX_UPLOAD
}

#[derive(Debug, Default, Deserialize)]
pub struct UploadQuery {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub name: String,
}

pub async fn api_upload_attachment(
    State(g): State<std::sync::Arc<Gateway>>,
    Query(query): Query<UploadQuery>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let upload_dir = g.upload_dir();
    if upload_dir.trim().is_empty() {
        return write_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "uploads_disabled",
            "this daemon has no upload directory",
        );
    }
    let mut session = safe_segment(&query.session_id);
    if session.is_empty() {
        session = "shared".to_string();
    }
    let mut name = safe_file_name(&query.name);
    if name.is_empty() {
        name = "upload.bin".to_string();
    }

    let dir = Path::new(&upload_dir).join(&session);
    if let Err(err) = tokio::fs::create_dir_all(&dir).await {
        return write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_error",
            &err.to_string(),
        );
    }

    // Never overwrite: two files with the same name in one session are two
    // attachments, and the agent must be able to read both.
    let mut dst = dir.join(&name);
    let mut index = 1;
    while tokio::fs::try_exists(&dst).await.unwrap_or(false) {
        let ext = Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| format!(".{e}"))
            .unwrap_or_default();
        let stem = name.strip_suffix(&ext).unwrap_or(&name);
        dst = dir.join(format!("{stem}-{index}{ext}"));
        index += 1;
    }

    let mut file = match tokio::fs::File::create(&dst).await {
        Ok(file) => file,
        Err(err) => {
            return write_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "storage_error",
                &err.to_string(),
            )
        }
    };

    let mut written: u64 = 0;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(err) => {
                let _ = tokio::fs::remove_file(&dst).await;
                return write_error(StatusCode::BAD_GATEWAY, "storage_error", &err.to_string());
            }
        };
        written += chunk.len() as u64;
        if written > MAX_UPLOAD {
            let _ = tokio::fs::remove_file(&dst).await;
            return write_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                &format!("ไฟล์ใหญ่เกิน {} MB", MAX_UPLOAD >> 20),
            );
        }
        if let Err(err) = file.write_all(&chunk).await {
            let _ = tokio::fs::remove_file(&dst).await;
            return write_error(StatusCode::BAD_GATEWAY, "storage_error", &err.to_string());
        }
    }
    if let Err(err) = file.flush().await {
        let _ = tokio::fs::remove_file(&dst).await;
        return write_error(StatusCode::BAD_GATEWAY, "storage_error", &err.to_string());
    }
    drop(file);

    let mime = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim()
        .to_string();
    let file_name = dst
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("upload.bin")
        .to_string();
    write_json(
        StatusCode::CREATED,
        json!({
            "path": dst.to_string_lossy(),
            "name": file_name,
            "size": written,
            "mime": mime,
        }),
    )
}

/// Keeps one path element: letters, digits, dash and underscore. Anything else
/// (a slash, a dot-dot) is dropped, so a caller cannot walk out of the upload
/// directory.
pub fn safe_segment(raw: &str) -> String {
    let filtered: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    truncate_chars(&filtered, 64)
}

/// Keeps a readable file name while removing any directory part and characters
/// that are awkward on a shell or a filesystem.
pub fn safe_file_name(raw: &str) -> String {
    let raw = raw.trim().replace('\\', "/");
    let base = match raw.rsplit_once('/') {
        Some((_, tail)) => tail,
        None => raw.as_str(),
    };
    let cleaned: String = base
        .chars()
        .filter(|c| *c != '\0')
        .map(|c| {
            if c.is_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ' | '(' | ')') {
                c
            } else if c as u32 > 0x7F {
                // Non-ASCII letters are common in Thai file names; keep them.
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_start_matches('.');
    truncate_chars(trimmed, 120)
}

/// Cuts a string to a byte budget without splitting a character.
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// The directory an attachment lives in, for callers that build a path.
pub fn upload_path(dir: &str, session: &str, name: &str) -> PathBuf {
    Path::new(dir).join(session).join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_segment_keeps_one_path_element() {
        assert_eq!(safe_segment("  abc-123_x  "), "abc-123_x");
        assert_eq!(safe_segment("../../etc"), "etc");
        assert_eq!(safe_segment("a/b"), "ab");
    }

    #[test]
    fn a_file_name_drops_the_directory_and_awkward_characters() {
        assert_eq!(safe_file_name("dir/sub/report.pdf"), "report.pdf");
        assert_eq!(safe_file_name("a|b.txt"), "a_b.txt");
        assert_eq!(safe_file_name(".hidden"), "hidden");
        assert_eq!(safe_file_name("รูป ภาพ.png"), "รูป ภาพ.png");
    }
}
