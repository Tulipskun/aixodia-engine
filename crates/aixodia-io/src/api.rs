//! The `/api` routes the AIxodia app's Settings screen and per-chat agent
//! settings call over the tunnel.
//!
//! Port of the Go `io/gateway/api.go` and `io/gateway/logs.go`. Chat history and
//! settings live in D1; the providers, keys and all, live in Workers KV. The
//! probe results are kept in memory and reset when the daemon restarts.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aixodia_config::Provider as ConfigProvider;
use aixodia_session::{AgentPatch, Error as SessionError, Session};
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::gateway::{write_error, write_json, write_raw, Gateway};

/// The provider id rule, as a readable pattern.
pub const PROVIDER_ID_PATTERN: &str = "^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$";

pub fn valid_provider_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    rest.len() <= 63
        && rest
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[derive(Debug, Clone, Default)]
pub struct ProbeResult {
    pub probed: bool,
    pub reachable: bool,
    pub model_count: usize,
    pub working_model: String,
    pub last_error: String,
    pub models: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ProbeCache {
    results: Mutex<HashMap<String, ProbeResult>>,
}

impl ProbeCache {
    pub fn new() -> Self {
        Self {
            results: Mutex::new(HashMap::new()),
        }
    }

    pub fn get(&self, name: &str) -> ProbeResult {
        self.results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    pub fn set(&self, name: &str, result: ProbeResult) {
        self.results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.to_string(), result);
    }

    pub fn forget(&self, name: &str) {
        self.results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(name);
    }
}

/// The new key list. `replace` wins over everything; then `remove` (by index
/// into the current list), then `add`.
pub fn apply_key_changes(
    current: &[String],
    add: &[String],
    remove: &[i64],
    replace: &[String],
) -> Vec<String> {
    if !replace.is_empty() {
        return clean_keys(replace);
    }
    let mut keys: Vec<String> = current.to_vec();
    let mut indexes: Vec<i64> = remove.to_vec();
    indexes.sort_unstable_by(|a, b| b.cmp(a));
    for index in indexes {
        if index >= 0 && (index as usize) < keys.len() {
            keys.remove(index as usize);
        }
    }
    let mut joined = keys;
    joined.extend(add.iter().cloned());
    clean_keys(&joined)
}

pub fn clean_keys(keys: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for key in keys {
        let key = key.trim();
        if key.is_empty() || out.iter().any(|seen| seen == key) {
            continue;
        }
        out.push(key.to_string());
    }
    out
}

pub fn provider_status_json(cfg: &ConfigProvider, probe: &ProbeResult) -> Value {
    json!({
        "id": cfg.name,
        "adapter": cfg.adapter,
        "endpoint": cfg.api_url,
        "key_count": cfg.keys.len(),
        "model_count": probe.model_count,
        "reachable": probe.reachable,
        "probed": probe.probed,
        "working_model": probe.working_model,
        "last_error": probe.last_error,
        "builtin": aixodia_config::is_builtin(&cfg.name),
    })
}

pub async fn api_list_providers(State(g): State<Arc<Gateway>>) -> Response {
    let list: Vec<Value> = aixodia_config::get_providers()
        .iter()
        .map(|cfg| provider_status_json(cfg, &g.probes.get(&cfg.name)))
        .collect();
    write_json(StatusCode::OK, json!({"providers": list}))
}

#[derive(Debug, Default, Deserialize)]
struct AddProviderRequest {
    #[serde(default)]
    id: String,
    #[serde(default)]
    adapter: String,
    #[serde(default)]
    endpoint: String,
    #[serde(default)]
    keys: Vec<String>,
}

pub async fn api_add_provider(State(g): State<Arc<Gateway>>, body: Bytes) -> Response {
    let body: AddProviderRequest = match crate::gateway::decode_body(body) {
        Ok(body) => body,
        Err(err) => {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &err.to_string(),
            )
        }
    };
    let id = body.id.trim().to_string();
    if !valid_provider_id(&id) {
        return write_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "provider id must be letters, digits, '.', '_' or '-'",
        );
    }
    if aixodia_config::get_provider_by_name(&id).is_some() {
        return write_error(
            StatusCode::CONFLICT,
            "conflict",
            &format!("provider {id:?} already exists"),
        );
    }
    if let Err(err) = g.registry().require(&body.adapter) {
        return write_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &err.to_string(),
        );
    }
    if body.endpoint.trim().is_empty() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "endpoint is required",
        );
    }
    let keys = clean_keys(&body.keys);
    if let Err(err) = aixodia_config::add_provider(&id, &body.endpoint, &body.adapter, keys).await {
        return write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string());
    }
    reload_providers().await;
    let cfg = aixodia_config::get_provider_by_name(&id).unwrap_or_default();
    write_json(
        StatusCode::CREATED,
        provider_status_json(&cfg, &ProbeResult::default()),
    )
}

pub async fn api_remove_provider(
    State(g): State<Arc<Gateway>>,
    Path(id): Path<String>,
) -> Response {
    let Some(cfg) = aixodia_config::get_provider_by_name(&id) else {
        return write_error(StatusCode::NOT_FOUND, "not_found", "unknown provider");
    };
    if let Err(err) = aixodia_config::remove_provider(&cfg.name).await {
        return write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string());
    }
    g.probes.forget(&cfg.name);
    reload_providers().await;
    write_json(StatusCode::OK, json!({"ok": true}))
}

#[derive(Debug, Default, Deserialize)]
struct ChangeKeysRequest {
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<i64>,
    #[serde(default)]
    replace: Vec<String>,
}

pub async fn api_change_keys(
    State(g): State<Arc<Gateway>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Some(cfg) = aixodia_config::get_provider_by_name(&id) else {
        return write_error(StatusCode::NOT_FOUND, "not_found", "unknown provider");
    };
    let body: ChangeKeysRequest = match crate::gateway::decode_body(body) {
        Ok(body) => body,
        Err(err) => {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &err.to_string(),
            )
        }
    };
    let keys = apply_key_changes(&cfg.keys, &body.add, &body.remove, &body.replace);
    if let Err(err) = aixodia_config::set_provider_keys(&cfg.name, keys).await {
        return write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string());
    }
    g.probes.forget(&cfg.name);
    reload_providers().await;
    let updated = aixodia_config::get_provider_by_name(&cfg.name).unwrap_or_default();
    write_json(
        StatusCode::OK,
        provider_status_json(&updated, &ProbeResult::default()),
    )
}

/// Caps how many upstreams are asked at once. With a hundred providers a
/// sequential refresh would wait on each timeout in turn.
pub const PROBE_LIMIT: usize = 8;

pub async fn api_refresh_providers(State(g): State<Arc<Gateway>>) -> Response {
    let providers = aixodia_config::get_providers();
    let gateway = g.clone();
    let results = futures_util::future::join_all(providers.iter().cloned().map(|cfg| {
        let gateway = gateway.clone();
        async move { gateway.probe_provider(&cfg).await }
    }))
    .await;
    let list: Vec<Value> = providers
        .iter()
        .zip(results.iter())
        .map(|(cfg, probe)| provider_status_json(cfg, probe))
        .collect();
    write_json(StatusCode::OK, json!({"providers": list}))
}

pub async fn api_refresh_provider(
    State(g): State<Arc<Gateway>>,
    Path(id): Path<String>,
) -> Response {
    let Some(cfg) = aixodia_config::get_provider_by_name(&id) else {
        return write_error(StatusCode::NOT_FOUND, "not_found", "unknown provider");
    };
    let probe = g.probe_provider(&cfg).await;
    write_json(StatusCode::OK, provider_status_json(&cfg, &probe))
}

impl Gateway {
    /// Asks the upstream for its model list, which doubles as a check that the
    /// endpoint and the first key work.
    pub(crate) async fn probe_provider(&self, cfg: &ConfigProvider) -> ProbeResult {
        let mut result = ProbeResult {
            probed: true,
            ..ProbeResult::default()
        };
        match crate::models::fetch_models(cfg).await {
            Err(err) => result.last_error = err.to_string(),
            Ok(models) => {
                result.reachable = true;
                result.model_count = models.len();
                result.working_model = models.first().cloned().unwrap_or_default();
                result.models = models;
            }
        }
        self.probes.set(&cfg.name, result.clone());
        result
    }
}

pub async fn api_models(State(g): State<Arc<Gateway>>) -> Response {
    let providers = aixodia_config::get_providers();
    let (saved_provider, saved_model) = g.saved_main_model().await;
    let gateway = g.clone();
    let fetched = futures_util::future::join_all(providers.iter().cloned().map(|cfg| {
        let gateway = gateway.clone();
        async move {
            let ids = gateway.provider_models(&cfg).await;
            (cfg, ids)
        }
    }))
    .await;

    let mut views: Vec<Value> = Vec::new();
    for (cfg, result) in fetched {
        let mut view = json!({
            "id": cfg.name,
            "name": cfg.name,
            "default_model": "",
            "models": [],
        });
        if let Ok(ids) = result {
            let models: Vec<Value> = ids
                .iter()
                .map(|id| {
                    json!({
                        "id": id,
                        "name": id,
                        "supports_streaming": true,
                        "supports_tools": cfg.adapter == "openai"
                            || cfg.adapter == "opencode"
                            || cfg.adapter == "anthropic",
                    })
                })
                .collect();
            view["models"] = Value::Array(models);
            if let Some(first) = ids.first() {
                view["default_model"] = Value::String(first.clone());
                // Point the default at the model saved in settings when it
                // belongs to this provider, so the picker opens on what the user
                // actually chose instead of the upstream list head.
                if !saved_model.is_empty() && cfg.name == saved_provider {
                    if let Some(found) = ids.iter().find(|id| **id == saved_model) {
                        view["default_model"] = Value::String(found.clone());
                    }
                }
            }
        }
        views.push(view);
    }
    write_json(StatusCode::OK, json!({"providers": views}))
}

pub const SETTINGS_STATE_KEY: &str = "engine/settings";

impl Gateway {
    /// The provider/model saved in settings. When nothing is stored, or the
    /// stored value names no route, this falls back to `default_main_model()` so
    /// a fresh daemon routes to the built-in keyless provider instead of failing
    /// for a model that settings never named.
    pub(crate) async fn saved_main_model(&self) -> (String, String) {
        let stored = match self.stored_settings().await {
            Ok(Some(stored)) => stored,
            _ => return default_main_model(),
        };
        let (provider, model) = match serde_json::from_str::<Saved>(&stored) {
            Ok(saved) => (saved.main.provider, saved.main.model),
            Err(_) => default_main_model(),
        };
        if provider.trim().is_empty() && model.trim().is_empty() {
            return default_main_model();
        }
        (provider, model)
    }
}

/// The route a daemon with no stored settings answers with: the built-in
/// non-login (keyless) provider and the model the official client defaults to.
pub(crate) fn default_main_model() -> (String, String) {
    let Ok(saved) = serde_json::from_str::<Saved>(&default_settings()) else {
        return (String::new(), String::new());
    };
    (saved.main.provider, saved.main.model)
}

#[derive(Deserialize, Default)]
struct Main {
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
}

#[derive(Deserialize, Default)]
struct Saved {
    #[serde(default)]
    main: Main,
}

pub fn default_settings() -> String {
    r#"{"main":{"provider":"Opencode","model":"big-pickle","generation":{}}}"#.to_string()
}

pub async fn api_get_settings(State(g): State<Arc<Gateway>>) -> Response {
    let stored = match g.stored_settings().await {
        Ok(stored) => stored,
        Err(err) => {
            return write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string())
        }
    };
    let usable = stored
        .as_deref()
        .filter(|stored| !stored.is_empty())
        .filter(|stored| serde_json::from_str::<Value>(stored).is_ok());
    match usable {
        Some(stored) => write_raw(StatusCode::OK, stored.to_string()),
        None => write_raw(StatusCode::OK, default_settings()),
    }
}

pub async fn api_put_settings(State(g): State<Arc<Gateway>>, body: Bytes) -> Response {
    let raw = match std::str::from_utf8(&body) {
        Ok(raw) => raw.to_string(),
        Err(_) => {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "request body is not valid UTF-8; send the message as UTF-8 text",
            )
        }
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(_)) => {}
        _ => {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "settings must be a JSON object",
            )
        }
    }
    let store = {
        // stored_settings reports the missing store; writing needs it directly.
        match g.stored_settings().await {
            Ok(_) => {}
            Err(err) => {
                return write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string())
            }
        }
        g.state_store()
    };
    let Some(store) = store else {
        return write_error(
            StatusCode::BAD_GATEWAY,
            "database_error",
            "the engine state store is not open",
        );
    };
    if let Err(err) = store.put_state(SETTINGS_STATE_KEY, &raw).await {
        return write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string());
    }
    write_raw(StatusCode::OK, raw)
}

impl Gateway {
    pub(crate) fn state_store(&self) -> Option<Arc<dyn crate::gateway::StateStore>> {
        self.state.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

async fn session_agent_response(g: &Gateway, id: &str) -> Response {
    let saved: Session = match g.sessions.get(id).await {
        Ok(saved) => saved,
        Err(SessionError::NotFound(_)) => {
            return write_error(StatusCode::NOT_FOUND, "not_found", "session not found")
        }
        Err(err) => {
            return write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string())
        }
    };
    write_json(
        StatusCode::OK,
        json!({
            "provider": saved.provider,
            "model": saved.model,
            "pinned": !saved.model.is_empty(),
        }),
    )
}

pub async fn api_get_session_agent(
    State(g): State<Arc<Gateway>>,
    Path(id): Path<String>,
) -> Response {
    session_agent_response(&g, &id).await
}

#[derive(Debug, Default, Deserialize)]
pub struct PatchSessionAgentRequest {
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub clear_model: bool,
}

/// Turns a PATCH body into the fields to write. The clear flags exist because an
/// absent field means "unchanged": without them the phone could not say "this
/// chat has no pinned model any more".
pub fn apply_session_agent_patch(body: &PatchSessionAgentRequest) -> AgentPatch {
    let mut patch = AgentPatch::default();
    if body.clear_model {
        let empty = String::new();
        patch.provider = Some(empty.clone());
        patch.model = Some(empty);
    }
    if let Some(provider) = &body.provider {
        patch.provider = Some(provider.trim().to_string());
    }
    if let Some(model) = &body.model {
        patch.model = Some(model.trim().to_string());
    }
    patch
}

pub async fn api_patch_session_agent(
    State(g): State<Arc<Gateway>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let body: PatchSessionAgentRequest = match crate::gateway::decode_body(body) {
        Ok(body) => body,
        Err(err) => {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &err.to_string(),
            )
        }
    };
    let patch = apply_session_agent_patch(&body);
    if !patch.empty() {
        match g.sessions.save_agent(&id, &patch).await {
            Ok(()) => {}
            Err(SessionError::NotFound(_)) => {
                return write_error(StatusCode::NOT_FOUND, "not_found", "session not found")
            }
            Err(err) => {
                return write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string())
            }
        }
    }
    session_agent_response(&g, &id).await
}

#[derive(Debug, Default, Deserialize)]
pub struct LogsQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub level: Option<String>,
}

/// `GET /api/logs` returns recent engine log entries (newest first). Logs live
/// on the engine machine (local SQLite), not in D1.
pub async fn api_logs(State(g): State<Arc<Gateway>>, Query(query): Query<LogsQuery>) -> Response {
    let mut limit = 200;
    if let Some(value) = query.limit {
        if value > 0 && value <= 2000 {
            limit = value;
        }
    }
    let level = query.level.unwrap_or_default();
    let logs = g.read_logs(limit, &level);
    let count = logs.len();
    write_json(StatusCode::OK, json!({"logs": logs, "count": count}))
}

pub async fn reload_providers() {
    if let Err(err) = aixodia_config::load_config().await {
        tracing::warn!("gateway: reload providers: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_wins_over_add_and_remove() {
        let current = vec!["a".to_string(), "b".to_string()];
        let got = apply_key_changes(
            &current,
            &["c".to_string()],
            &[0],
            &["z".to_string(), "z".to_string()],
        );
        assert_eq!(got, vec!["z".to_string()]);
    }

    #[test]
    fn removes_are_applied_from_the_highest_index() {
        let current = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let got = apply_key_changes(&current, &["d".to_string()], &[0, 2], &[]);
        assert_eq!(got, vec!["b".to_string(), "d".to_string()]);
    }

    #[test]
    fn clean_keys_trims_and_dedupes() {
        let got = clean_keys(&[" a ".to_string(), "a".to_string(), String::new()]);
        assert_eq!(got, vec!["a".to_string()]);
    }

    #[test]
    fn a_provider_id_keeps_one_label() {
        assert!(valid_provider_id("NousResearch"));
        assert!(valid_provider_id("a.b-c_d"));
        assert!(!valid_provider_id("-leading"));
        assert!(!valid_provider_id("with space"));
        assert!(!valid_provider_id("with/slash"));
    }

    #[test]
    fn a_patch_clears_the_model_when_asked() {
        let patch = apply_session_agent_patch(&PatchSessionAgentRequest {
            clear_model: true,
            ..PatchSessionAgentRequest::default()
        });
        assert_eq!(patch.provider.as_deref(), Some(""));
        assert_eq!(patch.model.as_deref(), Some(""));
        // An empty string is a request to clear, not an absent field, so the
        // patch is not empty: it has to reach the store.
        assert!(!patch.empty());
    }

    #[test]
    fn a_patch_trims_what_it_writes() {
        let patch = apply_session_agent_patch(&PatchSessionAgentRequest {
            provider: Some("  P  ".to_string()),
            model: Some(" m ".to_string()),
            clear_model: false,
        });
        assert_eq!(patch.provider.as_deref(), Some("P"));
        assert_eq!(patch.model.as_deref(), Some("m"));
        assert!(!patch.empty());
    }
}
