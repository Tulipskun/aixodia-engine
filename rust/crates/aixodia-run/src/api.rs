//! The run API: `POST /v1/runs`, `GET /v1/runs/{id}`, its event stream and
//! cancellation.
//!
//! Port of the Go `run/api.go`.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use crate::event::{Event, StartRequest, KIND_STATUS};
use crate::manager::{sort_events, Manager};

pub fn router(manager: Arc<Manager>) -> Router {
    Router::new()
        .route("/v1/runs", post(handle_create))
        .route("/v1/runs/:id", get(handle_get))
        .route("/v1/runs/:id/events", get(handle_events))
        .route("/v1/runs/:id/cancel", post(handle_cancel))
        .with_state(manager)
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": {"message": message.into()}}))).into_response()
}

async fn handle_create(
    State(manager): State<Arc<Manager>>,
    Json(request): Json<StartRequest>,
) -> Response {
    match manager.create(request).await {
        Ok((created, existing)) => {
            let code = if existing {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            (code, Json(json!({"run": created}))).into_response()
        }
        Err(err) => {
            let message = err.to_string();
            let code = if message.contains("already running") {
                StatusCode::CONFLICT
            } else {
                StatusCode::BAD_REQUEST
            };
            error(code, message)
        }
    }
}

async fn handle_get(State(manager): State<Arc<Manager>>, Path(id): Path<String>) -> Response {
    match manager.get(&id).await {
        Ok(Some(meta)) => Json(json!({"run": meta})).into_response(),
        Ok(None) => error(StatusCode::NOT_FOUND, "run not found"),
        Err(err) => error(StatusCode::BAD_GATEWAY, err.to_string()),
    }
}

async fn handle_cancel(State(manager): State<Arc<Manager>>, Path(id): Path<String>) -> Response {
    match manager.cancel(&id).await {
        Ok(meta) => (StatusCode::ACCEPTED, Json(json!({"run": meta}))).into_response(),
        Err(err) => {
            let message = err.to_string();
            let code = if message.contains("not found") {
                StatusCode::NOT_FOUND
            } else {
                StatusCode::CONFLICT
            };
            error(code, message)
        }
    }
}

async fn handle_events(
    State(manager): State<Arc<Manager>>,
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    let mut after = 0i64;
    if let Some(raw) = query
        .get("after")
        .map(|raw| raw.trim())
        .filter(|raw| !raw.is_empty())
    {
        match raw.parse::<i64>() {
            Ok(parsed) if parsed >= 0 => after = parsed,
            _ => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "after must be a non-negative integer",
                )
            }
        }
    } else if let Some(raw) = headers
        .get("Last-Event-ID")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
    {
        if let Ok(parsed) = raw.parse::<i64>() {
            if parsed > after {
                after = parsed;
            }
        }
    }

    let subscription = match manager.subscribe(&id, after).await {
        Ok(subscription) => subscription,
        Err(err) => return error(StatusCode::BAD_GATEWAY, err.to_string()),
    };
    if subscription.run.id.is_empty() {
        return error(StatusCode::NOT_FOUND, "run not found");
    }

    // `?poll=1` answers with whatever the run has produced since `after` and
    // closes. A caller behind a buffering proxy (Cloudflare Quick Tunnels
    // buffer `text/event-stream` until the origin closes) still sees each turn
    // as it grows by asking again with the last sequence it applied.
    let poll = query
        .get("poll")
        .map(|raw| matches!(raw.trim(), "1" | "true"))
        .unwrap_or(false);

    let status = subscription.run.status;
    let mut backlog = subscription.backlog;
    sort_events(&mut backlog);
    let live = if !poll && subscription.active && subscription.live.is_some() && !status.terminal()
    {
        subscription.live
    } else {
        None
    };
    let guard = subscription.guard;

    let (sender, receiver) = tokio::sync::mpsc::channel::<Result<Bytes, Infallible>>(64);
    tokio::spawn(async move {
        // Hold the guard so the live channel is removed when this stream ends.
        let _guard = guard;
        if sender
            .send(Ok(Bytes::from_static(b"retry: 3000\n\n")))
            .await
            .is_err()
        {
            return;
        }
        for event in backlog {
            if sender
                .send(Ok(Bytes::from(format_event(&event))))
                .await
                .is_err()
            {
                return;
            }
        }
        let Some(mut live) = live else {
            return;
        };
        let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(15));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick is immediate; skip it so the header is not followed by
        // a burst of pings.
        heartbeat.tick().await;
        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    if sender.send(Ok(Bytes::from_static(b": ping\n\n"))).await.is_err() {
                        return;
                    }
                }
                event = live.recv() => {
                    match event {
                        Some(event) => {
                            if sender.send(Ok(Bytes::from(format_event(&event)))).await.is_err() {
                                return;
                            }
                        }
                        None => return,
                    }
                }
            }
        }
    });

    let mut response = Body::from_stream(ReceiverStream::new(receiver)).into_response();
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        "text/event-stream; charset=utf-8".parse().expect("header"),
    );
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        "no-cache, no-transform".parse().expect("header"),
    );
    headers.insert("X-Accel-Buffering", "no".parse().expect("header"));
    headers.insert("X-Run-Status", status.as_str().parse().expect("header"));
    response
}

fn format_event(event: &Event) -> String {
    let payload: Value = serde_json::to_value(event).unwrap_or(Value::Null);
    let kind = if event.kind.is_empty() {
        KIND_STATUS
    } else {
        event.kind.as_str()
    };
    format!("id: {}\nevent: {}\ndata: {}\n\n", event.seq, kind, payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Status;
    use crate::manager::ChatExecutor;
    use crate::sink::SseSink;
    use crate::store::MemoryStore;
    use async_trait::async_trait;
    use axum::http::Request;
    use tokio_util::sync::CancellationToken;
    use tower::ServiceExt;

    struct Immediate;

    #[async_trait]
    impl ChatExecutor for Immediate {
        async fn execute(
            &self,
            _job: crate::event::ChatJob,
            sink: Arc<SseSink>,
            _cancel: CancellationToken,
        ) -> Result<(), String> {
            sink.write(b"data: [DONE]\n\n");
            Ok(())
        }
    }

    /// Emits one tool event, then keeps the run alive so the test can ask what
    /// a poll sees while the turn is still running.
    struct Live;

    #[async_trait]
    impl ChatExecutor for Live {
        async fn execute(
            &self,
            _job: crate::event::ChatJob,
            sink: Arc<SseSink>,
            _cancel: CancellationToken,
        ) -> Result<(), String> {
            sink.write(b"data: {\"tool_event\":{\"stage\":\"tool_result\",\"round\":1}}\n\n");
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_poll_answers_a_running_run_and_closes() {
        let manager = Arc::new(Manager::new(Arc::new(MemoryStore::new()), Arc::new(Live)));
        let app = router(manager.clone());
        let request = Request::builder()
            .method("POST")
            .uri("/v1/runs")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"session_id":"s1","message":"hi"}"#))
            .expect("request");
        let response = app.clone().oneshot(request).await.expect("response");
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let value: Value = serde_json::from_slice(&bytes).expect("json");
        let id = value["run"]["id"].as_str().expect("id").to_string();

        for _ in 0..100 {
            let backlog = manager.subscribe(&id, 0).await.expect("subscribe").backlog;
            if backlog.iter().any(|event| event.kind == "tool") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let request = Request::builder()
            .uri(format!("/v1/runs/{id}/events?after=0&poll=1"))
            .body(Body::empty())
            .expect("request");
        let response =
            tokio::time::timeout(std::time::Duration::from_secs(2), app.oneshot(request))
                .await
                .expect("a poll must not wait for the run to end")
                .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            axum::body::to_bytes(response.into_body(), 64 * 1024),
        )
        .await
        .expect("a poll must close its body")
        .expect("body");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("event: tool"), "got {text}");
    }

    #[tokio::test]
    async fn the_api_creates_and_reads_a_run() {
        let manager = Arc::new(Manager::new(
            Arc::new(MemoryStore::new()),
            Arc::new(Immediate),
        ));
        let app = router(manager.clone());
        let request = Request::builder()
            .method("POST")
            .uri("/v1/runs")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"session_id":"s1","message":"hi"}"#))
            .expect("request");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let value: Value = serde_json::from_slice(&bytes).expect("json");
        let id = value["run"]["id"].as_str().expect("id").to_string();

        // Let the run settle before the replay is read.
        for _ in 0..100 {
            let run = manager.get(&id).await.expect("get").expect("run");
            if run.status.terminal() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let request = Request::builder()
            .uri(format!("/v1/runs/{id}"))
            .body(Body::empty())
            .expect("request");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("body");
        let value: Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(value["run"]["status"], Status::Done.as_str());
    }
}
