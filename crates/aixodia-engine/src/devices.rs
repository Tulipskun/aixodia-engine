//! Device / MCP gateway, migrated from the dashboard Cloudflare Worker.
//!
//! The Worker used a `MCP_GATEWAY` service binding that POSTed JSON-RPC to
//! `https://mcp.aixodia.dpdns.org/<device>` and read the registered device
//! names from the Workers KV `DEVICE_TUNNELS` namespace. With the Worker gone,
//! the engine owns these three routes directly. They sit inside `client_guard`
//! like every other `/v1/*` route, so a browser uses its short-lived token (or
//! a caller uses `CF_TOKEN`); `CF_TOKEN` is never forwarded upstream.

use std::env;
use std::sync::Arc;
use std::time::Duration;

use aixodia_db::{valid_device_name, Client as DbClient};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

const PROTOCOL_VERSION: &str = "2025-11-25";
const DEFAULT_UPSTREAM: &str = "https://mcp.aixodia.dpdns.org";

/// Tools the device exposes. Anything else is rejected so an arbitrary name is
/// never forwarded to the MCP server.
const DEVICE_TOOLS: &[&str] = &[
    "tap",
    "long_press",
    "swipe",
    "gesture",
    "multi_touch",
    "key_event",
    "input_text",
    "click_sequence",
    "screen_size",
    "ui_hierarchy",
    "screenshot",
    "act_and_observe",
];

/// Tools sent straight through as the MCP call; the rest are wrapped in the
/// `act_and_observe` action envelope (matching the old Worker).
const PASSTHROUGH_TOOLS: &[&str] = &[
    "screenshot",
    "screen_size",
    "ui_hierarchy",
    "act_and_observe",
];

pub struct Devices {
    db: DbClient,
    http: reqwest::Client,
    upstream: String,
    static_devices: Vec<String>,
}

struct McpError {
    code: &'static str,
    message: String,
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn device_error(status: StatusCode, code: &str, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "error": message.into(), "code": code })),
    )
        .into_response()
}

impl Devices {
    pub fn new(db: DbClient) -> Arc<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(25))
            .build()
            .expect("reqwest client");
        let upstream = env::var("AIXODIA_MCP_UPSTREAM")
            .unwrap_or_else(|_| DEFAULT_UPSTREAM.to_string())
            .trim()
            .trim_end_matches('/')
            .to_string();
        let static_devices = env::var("AIXODIA_DEVICE_TUNNELS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|name| valid_device_name(name))
            .map(str::to_string)
            .collect();
        Arc::new(Self {
            db,
            http,
            upstream,
            static_devices,
        })
    }

    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/api/devices", get(list_devices))
            .route("/api/devices/:id/tools", get(device_tools))
            .route("/api/devices/:id/action", post(device_action))
            .with_state(self.clone())
    }

    /// Registered device names: Workers KV `DEVICE_TUNNELS` first (via
    /// `DbClient::device_names`), then the `AIXODIA_DEVICE_TUNNELS` env
    /// fallback. An empty KV namespace is not an error — the caller answers
    /// `{devices: []}` with an explanatory note.
    async fn registered(&self) -> (Vec<String>, &'static str) {
        match self.db.device_names().await {
            Ok(names) if !names.is_empty() => return (names, "kv:DEVICE_TUNNELS"),
            Ok(_) => {}
            Err(err) => tracing::warn!("device registry KV: {err}"),
        }
        if !self.static_devices.is_empty() {
            return (self.static_devices.clone(), "env:AIXODIA_DEVICE_TUNNELS");
        }
        (Vec::new(), "none")
    }

    /// One JSON-RPC call to the device's MCP endpoint, with a per-request
    /// timeout and a single retry on transport errors / 5xx / 429.
    async fn mcp(
        &self,
        device: &str,
        payload: Value,
        timeout: Duration,
    ) -> Result<Value, McpError> {
        if !valid_device_name(device) {
            return Err(McpError {
                code: "INVALID_DEVICE",
                message: "Invalid device name".into(),
            });
        }
        let url = format!("{}/{}", self.upstream, device);
        let mut last = "upstream request failed".to_string();
        for attempt in 0..2u32 {
            if attempt > 0 {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            let request = self
                .http
                .post(&url)
                .header("content-type", "application/json")
                .header("MCP-Protocol-Version", PROTOCOL_VERSION)
                .json(&payload)
                .timeout(timeout);
            match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    let value: Value = response.json().await.unwrap_or(Value::Null);
                    if !status.is_success() {
                        let message = value
                            .pointer("/error/message")
                            .and_then(Value::as_str)
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("Device returned HTTP {}", status.as_u16()));
                        let retryable = status.is_server_error() || status.as_u16() == 429;
                        if retryable && attempt == 0 {
                            last = message;
                            continue;
                        }
                        return Err(McpError {
                            code: "DEVICE_ERROR",
                            message,
                        });
                    }
                    if let Some(error) = value.get("error") {
                        let message = error
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("MCP request failed")
                            .to_string();
                        return Err(McpError {
                            code: "DEVICE_ERROR",
                            message,
                        });
                    }
                    return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                }
                Err(err) => last = err.to_string(),
            }
        }
        Err(McpError {
            code: "DEVICE_UNAVAILABLE",
            message: format!("MCP gateway unreachable: {last}"),
        })
    }

    async fn probe(&self, name: &str) -> Value {
        let payload = json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "aixodia-engine", "version": "1.0" },
            },
        });
        let checked_at = now_iso();
        match self.mcp(name, payload, Duration::from_secs(4)).await {
            Ok(result) => {
                let online =
                    result.pointer("/serverInfo/name").and_then(Value::as_str) == Some("Droid-MCP");
                json!({
                    "online": online,
                    "checkedAt": checked_at,
                    "reason": if online { Value::Null } else { json!("Unexpected MCP endpoint") },
                    "version": result.pointer("/serverInfo/version").cloned().unwrap_or(Value::Null),
                    "latencyMs": Value::Null,
                })
            }
            Err(err) => json!({
                "online": false,
                "checkedAt": checked_at,
                "reason": err.message,
                "version": Value::Null,
                "latencyMs": Value::Null,
            }),
        }
    }
}

async fn list_devices(State(devices): State<Arc<Devices>>) -> Response {
    let (names, source) = devices.registered().await;
    if names.is_empty() {
        return (
            StatusCode::OK,
            Json(json!({
                "devices": [],
                "items": [],
                "checkedAt": now_iso(),
                "ephemeral": true,
                "source": source,
                "note": "No devices registered: Workers KV DEVICE_TUNNELS (and AIXODIA_DEVICE_TUNNELS) are empty, so the device list is intentionally empty rather than a 500.",
            })),
        )
            .into_response();
    }
    let probes = names.iter().map(|name| {
        let devices = devices.clone();
        let name = name.clone();
        async move { devices.probe(&name).await }
    });
    let statuses = futures_util::future::join_all(probes).await;
    let items: Vec<Value> = names
        .iter()
        .zip(statuses)
        .map(|(name, status)| {
            json!({
                "id": name,
                "name": name,
                "host": format!("{}/{}", devices.upstream, name),
                "status": status,
            })
        })
        .collect();
    (
        StatusCode::OK,
        Json(json!({
            "devices": items,
            "items": items,
            "checkedAt": now_iso(),
            "ephemeral": true,
            "source": source,
        })),
    )
        .into_response()
}

async fn device_tools(State(devices): State<Arc<Devices>>, Path(id): Path<String>) -> Response {
    if !valid_device_name(&id) {
        return device_error(
            StatusCode::BAD_REQUEST,
            "INVALID_DEVICE",
            "Invalid device name",
        );
    }
    let payload = json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} });
    match devices.mcp(&id, payload, Duration::from_secs(5)).await {
        Ok(result) => {
            let tools = result.get("tools").cloned().unwrap_or_else(|| json!([]));
            (StatusCode::OK, Json(json!({ "tools": tools }))).into_response()
        }
        Err(err) => device_error(StatusCode::BAD_GATEWAY, err.code, err.message),
    }
}

async fn device_action(
    State(devices): State<Arc<Devices>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    if !valid_device_name(&id) {
        return device_error(
            StatusCode::BAD_REQUEST,
            "INVALID_DEVICE",
            "Invalid device name",
        );
    }
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => {
            return device_error(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "Body must be JSON with a `tool` string",
            )
        }
    };
    let tool = match parsed.get("tool").and_then(Value::as_str) {
        Some(tool) if !tool.is_empty() => tool.to_string(),
        _ => {
            return device_error(
                StatusCode::BAD_REQUEST,
                "INVALID_REQUEST",
                "`tool` is required",
            )
        }
    };
    if !DEVICE_TOOLS.contains(&tool.as_str()) {
        return device_error(
            StatusCode::BAD_REQUEST,
            "INVALID_TOOL",
            "Tool is not allowed",
        );
    }
    let arguments = parsed
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let arguments_obj = match &arguments {
        Value::Object(map) => map.clone(),
        Value::Null => serde_json::Map::new(),
        _ => {
            return device_error(
                StatusCode::BAD_REQUEST,
                "INVALID_ARGUMENTS",
                "Arguments must be an object",
            )
        }
    };
    let (mcp_tool, mcp_args) = if PASSTHROUGH_TOOLS.contains(&tool.as_str()) {
        (tool.clone(), Value::Object(arguments_obj))
    } else {
        let mut action = serde_json::Map::new();
        action.insert("name".into(), json!(tool));
        for (key, value) in arguments_obj {
            action.insert(key, value);
        }
        (
            "act_and_observe".to_string(),
            json!({ "action": Value::Object(action), "settle_ms": 500, "include_image": true }),
        )
    };
    let payload = json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": mcp_tool, "arguments": mcp_args },
    });
    match devices.mcp(&id, payload, Duration::from_secs(20)).await {
        Ok(result) => {
            let content = result
                .get("content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let image = content.iter().find(|item| {
                item.get("type").and_then(Value::as_str) == Some("image")
                    && item.get("data").and_then(Value::as_str).is_some()
            });
            let screenshot = image.map(|item| {
                json!({
                    "mimeType": item.get("mimeType").and_then(Value::as_str).unwrap_or("image/png"),
                    "data": item.get("data").and_then(Value::as_str).unwrap_or(""),
                })
            });
            let text = content
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n");
            let is_error = result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let status = if is_error {
                StatusCode::BAD_GATEWAY
            } else {
                StatusCode::OK
            };
            (
                status,
                Json(json!({
                    "ok": !is_error,
                    "device": id,
                    "tool": tool,
                    "text": text,
                    "screenshot": screenshot,
                    "at": now_iso(),
                })),
            )
                .into_response()
        }
        Err(err) => device_error(StatusCode::BAD_GATEWAY, err.code, err.message),
    }
}
