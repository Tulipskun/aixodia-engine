//! `POST /restart` asks the machine this engine runs on to restart the session.
//!
//! Port of the Go `io/gateway/restart.go`. Nothing goes to D1: the engine runs
//! beside the notebook, and the notebook watches for `~/.exit`, backs the
//! filesystem up to the snapshot repo, and starts the next run itself. Creating
//! the file is the whole handshake.

use axum::http::StatusCode;
use axum::response::Response;
use serde_json::json;

use crate::gateway::{write_error, write_json};

pub async fn api_restart() -> Response {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "restart_failed",
            "cannot find the home directory",
        );
    }
    let path = std::path::Path::new(&home).join(".exit");
    match std::fs::File::create(&path) {
        Ok(_) => {
            tracing::info!("restart: wrote {}", path.display());
            write_json(
                StatusCode::OK,
                json!({"ok": true, "file": path.to_string_lossy()}),
            )
        }
        Err(err) => {
            tracing::warn!("restart: cannot create {}: {err}", path.display());
            write_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "restart_failed",
                &err.to_string(),
            )
        }
    }
}
