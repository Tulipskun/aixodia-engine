//! `POST /api/update` — the phone's "update this engine".
//!
//! `aixodia update` reaches an engine as `kill -USR1 <pid>`, which a phone has
//! no way to send: it is not on the machine and has no shell there. The same
//! request, spoken over HTTP, is the whole of this module.
//!
//! Two things the answer and the signal agree on:
//!
//! * It asks the release page first. "Already the published build" is a real
//!   answer, and replacing a process for nothing costs every connection to it.
//! * It answers before anything is installed. An install is an `execve`: the
//!   process stops being the one that accepted the request, so a response
//!   written after the swap would never be sent. The body therefore says
//!   `pending`, and the engine swaps when it is idle — seconds later if nothing
//!   is running, or at the end of a chat that is.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use tokio::sync::Notify;

use aixodia_selfupdate as selfupdate;

/// What an up-to-date engine answers.
pub fn current(published: &str) -> Value {
    json!({"ok": true, "update": false, "pending": false, "published": published})
}

/// What an engine that queued an install answers.
pub fn queued(published: &str) -> Value {
    json!({"ok": true, "update": true, "pending": true, "published": published})
}

/// The failure body: the release page could not be read, so nothing is known
/// about the published build and nothing is installed on a guess.
pub fn failure(reason: &str) -> Value {
    json!({"ok": false, "error": reason})
}

/// Asks the release page, then either says so or queues the install.
///
/// `Release` carries no tag of its own — GitHub's asset is named by the
/// architecture, not the build — so the timestamp of the published asset is
/// the version a reader can compare: it is the same value the state file
/// records, which is what `aixodia update` compares too.
async fn handler(notify: Arc<Notify>) -> Response {
    match selfupdate::need_update().await {
        Ok((false, release)) => {
            (StatusCode::OK, Json(current(&release.updated_at))).into_response()
        }
        Ok((true, release)) => {
            selfupdate::mark_pending();
            // The engine's own loop wakes on this instead of waiting out its
            // half-minute tick, so a button press swaps as soon as it is idle.
            notify.notify_one();
            (StatusCode::OK, Json(queued(&release.updated_at))).into_response()
        }
        Err(err) => {
            tracing::warn!(%err, "update: the release page did not answer");
            (StatusCode::BAD_GATEWAY, Json(failure(&err.to_string()))).into_response()
        }
    }
}

/// The route, carrying the wake-up the engine's install loop waits on.
pub fn router(notify: Arc<Notify>) -> axum::Router {
    use axum::routing::post;
    axum::Router::new().route(
        "/api/update",
        post(move || {
            let notify = notify.clone();
            async move { handler(notify).await }
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_engine_that_is_current_says_so_and_installs_nothing() {
        let body = current("2026-11-24T00:22:31Z");
        assert_eq!(body["ok"], json!(true));
        assert_eq!(body["update"], json!(false));
        assert_eq!(body["pending"], json!(false));
        assert_eq!(body["published"], json!("2026-11-24T00:22:31Z"));
    }

    #[test]
    fn an_engine_with_a_newer_build_on_the_shelf_queues_it() {
        let body = queued("2026-11-24T01:00:00Z");
        assert_eq!(body["update"], json!(true));
        // The swap happens after this answer: the process that sends it is the
        // one the exec replaces.
        assert_eq!(body["pending"], json!(true));
    }

    #[test]
    fn a_release_page_that_did_not_answer_is_not_an_install() {
        let body = failure("github release: 503");
        assert_eq!(body["ok"], json!(false));
        assert_eq!(body["update"], Value::Null);
        assert_eq!(body["error"], json!("github release: 503"));
    }
}
