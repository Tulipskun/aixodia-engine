//! Root-capable SFTP-style filesystem API.
//!
//! All paths are absolute paths on the engine host. The API is mounted behind
//! the same bearer-token middleware as the rest of the gateway. File contents
//! are streamed in both directions; they are never buffered as a whole.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use axum::body::Body;
use axum::extract::Query;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::gateway::{write_error, write_json};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub fn router() -> Router {
    Router::new()
        .route("/api/files", get(list).delete(remove))
        .route("/api/files/download", get(download))
        .route("/api/files/upload", post(upload))
        .route("/api/files/mkdir", post(mkdir))
        .route("/api/files/rename", post(rename))
}

#[derive(Debug, Default, Deserialize)]
struct PathQuery {
    #[serde(default)]
    path: String,
}

#[derive(Debug, Default, Deserialize)]
struct UploadQuery {
    #[serde(default)]
    path: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    overwrite: bool,
}

#[derive(Debug, Default, Deserialize)]
struct CreateDirectory {
    #[serde(default)]
    path: String,
}

#[derive(Debug, Default, Deserialize)]
struct RenameRequest {
    #[serde(default)]
    from: String,
    #[serde(default)]
    to: String,
}

#[derive(Debug, Default, Deserialize)]
struct DeleteQuery {
    #[serde(default)]
    path: String,
    #[serde(default)]
    recursive: bool,
}

fn bad_path(message: impl std::fmt::Display) -> Response {
    write_error(StatusCode::BAD_REQUEST, "invalid_path", &message.to_string())
}

fn absolute_path(raw: &str) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("path is required".into());
    }
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err("path must be absolute".into());
    }
    Ok(path.to_path_buf())
}

fn canonical_directory(raw: &str) -> Result<PathBuf, String> {
    let path = absolute_path(raw)?;
    let real = std::fs::canonicalize(&path)
        .map_err(|err| format!("{}: {err}", path.display()))?;
    if !real.is_dir() {
        return Err(format!("{} is not a directory", real.display()));
    }
    Ok(real)
}

fn parent_and_name(raw: &str) -> Result<(PathBuf, std::ffi::OsString), String> {
    let path = absolute_path(raw)?;
    let name = path
        .file_name()
        .filter(|name| *name != ".")
        .ok_or_else(|| "filesystem root cannot be used as a file target".to_string())?
        .to_os_string();
    if name == ".." {
        return Err("parent traversal is not a filename".into());
    }
    let parent = path
        .parent()
        .ok_or_else(|| "path has no parent directory".to_string())?;
    let parent = canonical_directory(&parent.display().to_string())?;
    Ok((parent, name))
}

async fn list(Query(query): Query<PathQuery>) -> Response {
    let requested = if query.path.trim().is_empty() { "/" } else { &query.path };
    let dir = match canonical_directory(requested) {
        Ok(dir) => dir,
        Err(err) => return bad_path(err),
    };
    let mut read = match tokio::fs::read_dir(&dir).await {
        Ok(read) => read,
        Err(err) => return write_error(StatusCode::FORBIDDEN, "filesystem_error", &err.to_string()),
    };
    let mut entries = Vec::new();
    loop {
        let item = match read.next_entry().await {
            Ok(Some(item)) => item,
            Ok(None) => break,
            Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, "filesystem_error", &err.to_string()),
        };
        let name = item.file_name().to_string_lossy().into_owned();
        let path = item.path();
        let metadata = match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        let kind = metadata.file_type();
        let modified = metadata.modified().ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
        #[cfg(unix)]
        let (mode, uid, gid) = {
            use std::os::unix::fs::MetadataExt;
            (Some(format!("{:04o}", metadata.mode() & 0o7777)), Some(metadata.uid()), Some(metadata.gid()))
        };
        #[cfg(not(unix))]
        let (mode, uid, gid): (Option<String>, Option<u32>, Option<u32>) = (None, None, None);
        entries.push(json!({
            "name": name,
            "path": path.to_string_lossy(),
            "type": if kind.is_dir() { "directory" } else if kind.is_symlink() { "symlink" } else if kind.is_file() { "file" } else { "other" },
            "is_dir": kind.is_dir(),
            "is_file": kind.is_file(),
            "is_symlink": kind.is_symlink(),
            "size": if kind.is_file() { Some(metadata.len()) } else { None },
            "modified": modified,
            "permissions": mode,
            "uid": uid,
            "gid": gid
        }));
    }
    entries.sort_by(|a, b| {
        let ad = a["is_dir"].as_bool().unwrap_or(false);
        let bd = b["is_dir"].as_bool().unwrap_or(false);
        bd.cmp(&ad).then_with(|| {
            a["name"].as_str().unwrap_or("").to_lowercase()
                .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
        })
    });
    write_json(StatusCode::OK, json!({
        "path": dir.to_string_lossy(),
        "parent": dir.parent().map(|p| p.to_string_lossy().into_owned()),
        "entries": entries,
        "count": entries.len()
    }))
}

async fn download(Query(query): Query<PathQuery>) -> Response {
    let path = match absolute_path(&query.path) {
        Ok(path) => path,
        Err(err) => return bad_path(err),
    };
    let real = match tokio::fs::canonicalize(&path).await {
        Ok(path) => path,
        Err(err) => return write_error(StatusCode::NOT_FOUND, "not_found", &err.to_string()),
    };
    let metadata = match tokio::fs::metadata(&real).await {
        Ok(metadata) if metadata.is_file() => metadata,
        Ok(_) => return bad_path("download target is not a regular file"),
        Err(err) => return write_error(StatusCode::NOT_FOUND, "not_found", &err.to_string()),
    };
    let name = real.file_name().unwrap_or_default().to_string_lossy().replace(['"', '\\r', '\\n'], "_");
    let file = match tokio::fs::File::open(&real).await {
        Ok(file) => file,
        Err(err) => return write_error(StatusCode::FORBIDDEN, "filesystem_error", &err.to_string()),
    };
    let stream = ReaderStream::new(file);
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    response.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from_str(&metadata.len().to_string()).unwrap());
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=\"{}\"; filename*=UTF-8''{}", name, percent_encode(&name))).unwrap(),
    );
    response
}

fn percent_encode(value: &str) -> String {
    value.bytes().map(|byte| {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            (byte as char).to_string()
        } else {
            format!("%{byte:02X}")
        }
    }).collect()
}

async fn upload(Query(query): Query<UploadQuery>, headers: HeaderMap, body: Body) -> Response {
    if query.name.is_empty() || query.name == "." || query.name == ".."
        || query.name.contains('/') || query.name.contains('\\') || query.name.contains('\0')
    {
        return bad_path("name must be one filename, without path separators");
    }
    let parent = match canonical_directory(&query.path).or_else(|err| Err(err)) {
        Ok(parent) => parent,
        Err(err) => return bad_path(err),
    };
    let destination = parent.join(&query.name);
    if !query.overwrite && tokio::fs::symlink_metadata(&destination).await.is_ok() {
        return write_error(StatusCode::CONFLICT, "already_exists", "destination already exists");
    }
    let seq = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(format!(".aixodia-upload-{}-{seq}.part", std::process::id()));
    let mut file = match tokio::fs::OpenOptions::new().write(true).create_new(true).open(&temp).await {
        Ok(file) => file,
        Err(err) => return write_error(StatusCode::INTERNAL_SERVER_ERROR, "filesystem_error", &err.to_string()),
    };
    let mut written = 0u64;
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(err) => {
                drop(file);
                let _ = tokio::fs::remove_file(&temp).await;
                return write_error(StatusCode::BAD_GATEWAY, "upload_error", &err.to_string());
            }
        };
        written = match written.checked_add(chunk.len() as u64) {
            Some(size) => size,
            None => {
                drop(file);
                let _ = tokio::fs::remove_file(&temp).await;
                return write_error(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "file size overflow");
            }
        };
        if let Err(err) = file.write_all(&chunk).await {
            drop(file);
            let _ = tokio::fs::remove_file(&temp).await;
            return write_error(StatusCode::INSUFFICIENT_STORAGE, "filesystem_error", &err.to_string());
        }
    }
    if let Err(err) = file.flush().await {
        drop(file);
        let _ = tokio::fs::remove_file(&temp).await;
        return write_error(StatusCode::INSUFFICIENT_STORAGE, "filesystem_error", &err.to_string());
    }
    drop(file);
    if !query.overwrite && tokio::fs::symlink_metadata(&destination).await.is_ok() {
        let _ = tokio::fs::remove_file(&temp).await;
        return write_error(StatusCode::CONFLICT, "already_exists", "destination appeared during upload");
    }
    if let Err(err) = tokio::fs::rename(&temp, &destination).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return write_error(StatusCode::INSUFFICIENT_STORAGE, "filesystem_error", &err.to_string());
    }
    let mime = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("application/octet-stream");
    write_json(StatusCode::CREATED, json!({
        "path": destination.to_string_lossy(),
        "name": query.name,
        "size": written,
        "content_type": mime
    }))
}

async fn mkdir(Json(request): Json<CreateDirectory>) -> Response {
    let (parent, name) = match parent_and_name(&request.path) {
        Ok(parts) => parts,
        Err(err) => return bad_path(err),
    };
    let target = parent.join(name);
    match tokio::fs::create_dir(&target).await {
        Ok(()) => write_json(StatusCode::CREATED, json!({"ok": true, "path": target.to_string_lossy()})),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => write_error(StatusCode::CONFLICT, "already_exists", &err.to_string()),
        Err(err) => write_error(StatusCode::FORBIDDEN, "filesystem_error", &err.to_string()),
    }
}

async fn rename(Json(request): Json<RenameRequest>) -> Response {
    let (source_parent, source_name) = match parent_and_name(&request.from) {
        Ok(parts) => parts,
        Err(err) => return bad_path(err),
    };
    let (target_parent, target_name) = match parent_and_name(&request.to) {
        Ok(parts) => parts,
        Err(err) => return bad_path(err),
    };
    let source = source_parent.join(source_name);
    let target = target_parent.join(target_name);
    if tokio::fs::symlink_metadata(&source).await.is_err() {
        return write_error(StatusCode::NOT_FOUND, "not_found", "source does not exist");
    }
    if tokio::fs::symlink_metadata(&target).await.is_ok() {
        return write_error(StatusCode::CONFLICT, "already_exists", "destination already exists");
    }
    match tokio::fs::rename(&source, &target).await {
        Ok(()) => write_json(StatusCode::OK, json!({"ok": true, "from": source.to_string_lossy(), "to": target.to_string_lossy()})),
        Err(err) => write_error(StatusCode::FORBIDDEN, "filesystem_error", &err.to_string()),
    }
}

async fn remove(Query(query): Query<DeleteQuery>) -> Response {
    let (parent, name) = match parent_and_name(&query.path) {
        Ok(parts) => parts,
        Err(err) => return bad_path(err),
    };
    let target = parent.join(name);
    let metadata = match tokio::fs::symlink_metadata(&target).await {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return write_error(StatusCode::NOT_FOUND, "not_found", &err.to_string()),
        Err(err) => return write_error(StatusCode::FORBIDDEN, "filesystem_error", &err.to_string()),
    };
    let result = if metadata.file_type().is_dir() {
        if !query.recursive {
            tokio::fs::remove_dir(&target).await
        } else {
            tokio::fs::remove_dir_all(&target).await
        }
    } else {
        tokio::fs::remove_file(&target).await
    };
    match result {
        Ok(()) => write_json(StatusCode::OK, json!({"ok": true, "path": target.to_string_lossy()})),
        Err(err) => write_error(StatusCode::FORBIDDEN, "filesystem_error", &err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_must_be_absolute() {
        assert!(absolute_path("/etc").is_ok());
        assert!(absolute_path("etc").is_err());
        assert!(absolute_path("").is_err());
    }

    #[test]
    fn root_cannot_be_a_mutation_target() {
        assert!(parent_and_name("/").is_err());
    }

    #[test]
    fn percent_encoding_is_header_safe() {
        assert_eq!(percent_encode("a b.txt"), "a%20b.txt");
        assert_eq!(percent_encode("ไฟล์.txt").contains('%'), true);
    }

    #[tokio::test]
    async fn upload_is_streamed_and_does_not_overwrite_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let query = UploadQuery { path: dir.path().display().to_string(), name: "data.bin".into(), overwrite: false };
        let response = upload(Query(query.clone()), HeaderMap::new(), Body::from("payload")).await;
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(tokio::fs::read(dir.path().join("data.bin")).await.unwrap(), b"payload");
        let response = upload(Query(query), HeaderMap::new(), Body::from("other")).await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
    }
}
