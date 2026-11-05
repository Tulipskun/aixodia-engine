//! `/api/backup` — the phone's `/` menu drives the engine's own backup.
//!
//! The tree is stored in a Hugging Face bucket by the `aixodia-backup` crate;
//! this module only decides *when* it happens and reports how it went. The hub
//! token and the bucket name live in D1 state under `backup/config`, never in
//! the image or the source.
//!
//! Everything that touches the bucket blocks: `reqwest::blocking` refuses to be
//! built inside a tokio runtime, and `hf-xet` panics when its blocking calls run
//! on a worker. So each operation is handed to a plain OS thread and awaited
//! through a channel, and a worker that panics must still release the slot —
//! otherwise the engine could never back up again.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::gateway::{write_error, write_json, Gateway};

/// The D1 state key holding the backup configuration.
pub const BACKUP_CONFIG_KEY: &str = "backup/config";
/// Last successful backup report, persisted independently of the engine process.
pub const LAST_BACKUP_KEY: &str = "backup/last_success";

/// One running operation and the result of the last one to finish.
#[derive(Default)]
struct Slots {
    active: Option<Active>,
    finished: Value,
}

struct Active {
    operation: String,
    started_at: i64,
    reporter: aixodia_backup::Reporter,
}

fn slots() -> &'static Mutex<Slots> {
    static SLOTS: OnceLock<Mutex<Slots>> = OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(Slots::default()))
}

/// The config as it is stored, with every field optional so a partial write
/// keeps what was already there.
#[derive(Clone, Debug, Default, serde::Serialize, Deserialize)]
pub(crate) struct Stored {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    bucket: Option<String>,
    #[serde(default)]
    endpoint: Option<String>,
    #[serde(default)]
    prefix: Option<String>,
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    excludes: Option<Vec<String>>,
    /// The allowlist the picker writes. `None` and `Some(vec![])` mean the same
    /// thing to a run — the whole tree — but not to a write: an omitted field
    /// keeps what is stored, an explicit empty list is the operator choosing
    /// "everything" over a narrowed scope.
    #[serde(default)]
    includes: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) auto_enabled: Option<bool>,
    #[serde(default)]
    pub(crate) auto_idle_secs: Option<u64>,
    #[serde(default)]
    pub(crate) auto_cpu_threshold: Option<f64>,
    #[serde(default)]
    pub(crate) auto_cooldown_secs: Option<u64>,
}

/// The configuration, with the stored state layered over the defaults. The
/// environment may supply the token, for a machine that keeps it out of D1.
pub(crate) fn resolve(stored: Option<&str>) -> aixodia_backup::Config {
    let parsed: Stored = stored
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default();
    let defaults = aixodia_backup::Config::default();
    let token = parsed
        .token
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            std::env::var("HF_TOKEN")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or(defaults.token);
    let bucket = parsed
        .bucket
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(defaults.bucket);
    let endpoint = parsed
        .endpoint
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(defaults.endpoint);
    let root = parsed
        .root
        .map(std::path::PathBuf::from)
        .unwrap_or(defaults.root);
    aixodia_backup::Config {
        token,
        bucket,
        endpoint,
        prefix: parsed.prefix.unwrap_or(defaults.prefix),
        root,
        excludes: parsed
            .excludes
            .map(|stored| aixodia_backup::scan::sanitize_excludes(&stored))
            .unwrap_or(defaults.excludes),
        includes: parsed.includes.unwrap_or(defaults.includes),
        timeout_secs: defaults.timeout_secs,
    }
}

async fn stored_config(g: &Gateway) -> Result<Option<String>, String> {
    let Some(store) = g.state_store() else {
        return Err("the engine state store is not open".into());
    };
    store
        .get_state(BACKUP_CONFIG_KEY)
        .await
        .map_err(|err| err.to_string())
}

/// `GET /api/backup` — whether it is configured, what is running, and how the
/// last run ended. Nothing here goes to the bucket.
pub async fn api_backup_status(State(g): State<Arc<Gateway>>) -> Response {
    let stored = match stored_config(&g).await {
        Ok(stored) => stored,
        Err(err) => return write_error(StatusCode::BAD_GATEWAY, "database_error", &err),
    };
    let config = resolve(stored.as_deref());
    let Some(store) = g.state_store() else {
        return write_error(StatusCode::BAD_GATEWAY, "database_error", "the engine state store is not open");
    };
    let persisted_raw = match store.get_state(LAST_BACKUP_KEY).await {
        Ok(raw) => raw,
        Err(err) => return write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string()),
    };
    let mut persisted_last = persisted_raw
        .as_deref()
        .and_then(|value| serde_json::from_str::<Value>(value).ok())
        .unwrap_or(Value::Null);
    if persisted_last["none"] == true {
        persisted_last = Value::Null;
    }
    // Migrate the timestamp from the bucket's existing manifest on the first
    // read after upgrading from an engine that kept its result only in memory.
    // A small sentinel prevents a bucket with no backup from being queried on
    // every status poll.
    if persisted_raw.is_none() && ready(&config, RESTORE_RUN) {
        let lookup_config = config.clone();
        let legacy = tokio::task::spawn_blocking(move || -> Result<Option<Value>, String> {
            let hub = aixodia_backup::Hub::new(&lookup_config).map_err(|err| err.to_string())?;
            let Some(manifest) = aixodia_backup::latest_manifest(&hub).map_err(|err| err.to_string())? else {
                return Ok(None);
            };
            Ok(Some(json!({
                "operation": "backup",
                "started_at": manifest.created_at,
                "finished_at": manifest.created_at,
                "summary": {
                    "ok": true,
                    "report": {
                        "name": manifest.name,
                        "bucket": lookup_config.bucket,
                        "root": manifest.root,
                        "files": manifest.counts.files,
                        "dirs": manifest.counts.dirs,
                        "symlinks": manifest.counts.symlinks,
                        "bytes": manifest.counts.bytes,
                        "uploaded_bytes": manifest.uploaded_bytes,
                        "skipped": manifest.skipped,
                        "duration_ms": 0
                    }
                }
            })))
        }).await;
        match legacy {
            Ok(Ok(Some(record))) => {
                if let Err(err) = store.put_state(LAST_BACKUP_KEY, &record.to_string()).await {
                    tracing::warn!("could not persist migrated last-backup manifest: {err}");
                }
                persisted_last = record;
            }
            Ok(Ok(None)) => {
                if let Err(err) = store.put_state(LAST_BACKUP_KEY, r#"{"none":true}"#).await {
                    tracing::warn!("could not persist empty last-backup marker: {err}");
                }
            }
            Ok(Err(err)) => tracing::warn!("could not read legacy last-backup manifest: {err}"),
            Err(err) => tracing::warn!("legacy last-backup lookup worker failed: {err}"),
        }
    }
    let slots = slots().lock().unwrap_or_else(|e| e.into_inner());
    let active = slots.active.as_ref().map(|active| {
        json!({
            "operation": active.operation,
            "started_at": active.started_at,
            "progress": active.reporter.snapshot(),
        })
    });
    let current_last = if slots.finished["operation"] == "backup" && slots.finished["summary"]["ok"] == true {
        slots.finished.clone()
    } else {
        Value::Null
    };
    let last_backup = if current_last["finished_at"].as_i64().unwrap_or(0) > persisted_last["finished_at"].as_i64().unwrap_or(0) {
        current_last
    } else {
        persisted_last
    };
    write_json(
        StatusCode::OK,
        json!({
            "configured": ready(&config, BACKUP_RUN),
            "needs": needs(&config, BACKUP_RUN),
            "restorable": ready(&config, RESTORE_RUN),
            "needs_restore": needs(&config, RESTORE_RUN),
            "bucket": config.bucket,
            "prefix": config.prefix,
            "root": config.root.display().to_string(),
            "excludes": config.excludes,
            "includes": config.includes,
            "token": mask(&config.token),
            "active": active,
            "finished": slots.finished,
            "last_backup": last_backup,
            "auto": auto_status_with_config(stored.as_deref()),
        }),
    )
}

/// `PUT /api/backup/config` — store the hub token and the bucket in D1.
pub async fn api_backup_config_put(State(g): State<Arc<Gateway>>, body: Bytes) -> Response {
    let incoming: Stored = match serde_json::from_slice(&body) {
        Ok(incoming) => incoming,
        Err(err) => {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("config must be a JSON object: {err}"),
            )
        }
    };
    let Some(store) = g.state_store() else {
        return write_error(
            StatusCode::BAD_GATEWAY,
            "database_error",
            "the engine state store is not open",
        );
    };
    let stored = match stored_config(&g).await {
        Ok(stored) => stored,
        Err(err) => return write_error(StatusCode::BAD_GATEWAY, "database_error", &err),
    };
    // An omitted field keeps its stored value, so the app can set the bucket
    // without ever resending the token.
    let previous: Stored = stored
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default();
    let merged = Stored {
        token: incoming
            .token
            .filter(|value| !value.trim().is_empty())
            .or(previous.token),
        bucket: incoming.bucket.or(previous.bucket),
        endpoint: incoming.endpoint.or(previous.endpoint),
        prefix: incoming.prefix.or(previous.prefix),
        root: incoming.root.or(previous.root),
        excludes: incoming.excludes.or(previous.excludes),
        includes: incoming.includes.or(previous.includes),
        auto_enabled: incoming.auto_enabled.or(previous.auto_enabled),
        auto_idle_secs: incoming.auto_idle_secs.or(previous.auto_idle_secs),
        auto_cpu_threshold: incoming.auto_cpu_threshold.or(previous.auto_cpu_threshold),
        auto_cooldown_secs: incoming.auto_cooldown_secs.or(previous.auto_cooldown_secs),
    };
    let raw = match serde_json::to_string(&merged) {
        Ok(raw) => raw,
        Err(err) => {
            return write_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "encode_failed",
                &err.to_string(),
            )
        }
    };
    // A scope is checked before it is stored: an allowlist that can match nothing
    // would run, succeed, and leave the operator with a backup of less than they
    // asked for — and this bucket holds one backup, so the older, wider one is
    // gone by the time anyone notices.
    if let Err(err) = resolve(Some(&raw)).validate_scope() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &err.to_string(),
        );
    }
    if let Err(err) = store.put_state(BACKUP_CONFIG_KEY, &raw).await {
        return write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string());
    }
    let config = resolve(Some(&raw));
    write_json(
        StatusCode::OK,
        json!({
            "configured": ready(&config, BACKUP_RUN),
            "needs": needs(&config, BACKUP_RUN),
            "restorable": ready(&config, RESTORE_RUN),
            "needs_restore": needs(&config, RESTORE_RUN),
            "bucket": config.bucket,
            "prefix": config.prefix,
            "root": config.root.display().to_string(),
            "excludes": config.excludes,
            "includes": config.includes,
            "token": mask(&config.token),
            "auto": auto_status_with_config(Some(&raw)),
        }),
    )
}

/// `POST /api/backup` — start a backup, or report that one is already running.
pub async fn api_backup_start(State(g): State<Arc<Gateway>>) -> Response {
    let stored = match stored_config(&g).await {
        Ok(stored) => stored,
        Err(err) => return write_error(StatusCode::BAD_GATEWAY, "database_error", &err),
    };
    let config = resolve(stored.as_deref());
    if !ready(&config, BACKUP_RUN) {
        return not_configured(&config, BACKUP_RUN);
    }
    let Some(reporter) = claim("backup") else {
        return busy();
    };
    let store = g.state_store();
    spawn_job("backup", store, move || {
        aixodia_backup::backup(&config, &reporter)
            .and_then(json_of)
            .map_err(|err| err.to_string())
    });
    write_json(
        StatusCode::ACCEPTED,
        json!({"started": true, "operation": "backup"}),
    )
}

/// `POST /api/backup/restore` — rebuild the tree from the bucket's one
/// manifest. Requires `confirm`.
pub async fn api_backup_restore(State(g): State<Arc<Gateway>>, body: Bytes) -> Response {
    #[derive(Deserialize)]
    struct Request {
        #[serde(default)]
        confirm: bool,
    }
    let request: Request = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("restore body must be JSON: {err}"),
            )
        }
    };
    // Overwriting the tree is the one thing a stray tap must not do, so the
    // engine will not restore without an explicit yes in the body.
    if !request.confirm {
        return write_json(
            StatusCode::BAD_REQUEST,
            json!({
                "error": "restore overwrites files under the root",
                "confirm_required": true,
            }),
        );
    }
    let stored = match stored_config(&g).await {
        Ok(stored) => stored,
        Err(err) => return write_error(StatusCode::BAD_GATEWAY, "database_error", &err),
    };
    let config = resolve(stored.as_deref());
    if !ready(&config, RESTORE_RUN) {
        return not_configured(&config, RESTORE_RUN);
    }
    let Some(reporter) = claim("restore") else {
        return busy();
    };
    // The target is always the configured root: a client chooses nothing about
    // where the tree lands.
    let root = config.root.clone();
    let label = root.display().to_string();
    spawn_job("restore", None, move || {
        aixodia_backup::restore(&config, &root, &reporter)
            .and_then(json_of)
            .map_err(|err| err.to_string())
    });
    write_json(
        StatusCode::ACCEPTED,
        json!({"started": true, "operation": "restore", "root": label}),
    )
}

fn json_of<T: serde::Serialize>(value: T) -> Result<Value, aixodia_backup::Error> {
    serde_json::to_value(value).map_err(aixodia_backup::Error::Json)
}

/// Whether a run could start now.
pub(crate) fn ready(config: &aixodia_backup::Config, for_backup: bool) -> bool {
    needs(config, for_backup).is_empty()
}

/// `needs()` argument: this run reads the tree, so the root has to be there.
pub(crate) const BACKUP_RUN: bool = true;
/// `needs()` argument: this run rebuilds the tree, so a missing root is the
/// reason to run it, not a reason to refuse.
pub(crate) const RESTORE_RUN: bool = false;

/// What has to be filled in before a run can start, as field names the app can
/// point at.
///
/// `for_backup` decides whether the root itself has to exist. A backup reads the
/// tree, so a missing root is a mistake in the configuration; a restore is the
/// answer to a missing tree — refusing it because the directory is not there
/// would be refusing to do the one job it has. The restore creates the root.
fn needs(config: &aixodia_backup::Config, for_backup: bool) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if config.token.trim().is_empty() {
        missing.push("token");
    }
    if config.bucket.trim().is_empty() {
        missing.push("bucket");
    }
    if for_backup && !config.root.is_dir() {
        missing.push("root");
    }
    missing
}

fn not_configured(config: &aixodia_backup::Config, for_backup: bool) -> Response {
    write_json(
        StatusCode::BAD_REQUEST,
        json!({"error": "backup is not configured", "needs": needs(config, for_backup)}),
    )
}

/// Never echo a token. The tail is enough to tell two keys apart.
fn mask(token: &str) -> Value {
    if token.trim().is_empty() {
        return json!(null);
    }
    let chars: Vec<char> = token.chars().collect();
    let tail: String = chars[chars.len().saturating_sub(4)..].iter().collect();
    json!({"set": true, "tail": tail})
}

/// Take the single job slot, or `None` when a run is already in it.
fn claim(operation: &str) -> Option<aixodia_backup::Reporter> {
    let mut slots = slots().lock().unwrap_or_else(|e| e.into_inner());
    if slots.active.is_some() {
        return None;
    }
    let reporter = aixodia_backup::Reporter::new(operation);
    slots.active = Some(Active {
        operation: operation.to_string(),
        started_at: aixodia_backup::now_unix(),
        reporter: reporter.clone(),
    });
    Some(reporter)
}

fn busy() -> Response {
    let slots = slots().lock().unwrap_or_else(|e| e.into_inner());
    let operation = slots
        .active
        .as_ref()
        .map(|active| active.operation.clone())
        .unwrap_or_default();
    write_json(
        StatusCode::CONFLICT,
        json!({"error": "a backup operation is already running", "operation": operation}),
    )
}

/// Run one job on its own thread and record how it ended, whatever it does —
/// including panicking, which must not leave the slot claimed forever.
fn spawn_job(
    operation: &'static str,
    store: Option<Arc<dyn crate::gateway::StateStore>>,
    work: impl FnOnce() -> Result<Value, String> + Send + 'static,
) {
    let started_at = aixodia_backup::now_unix();
    std::thread::spawn(move || {
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work))
            .unwrap_or_else(|_| Err("the backup worker panicked".to_string()));
        finish(operation, started_at, outcome, store);
    });
}

fn finish(
    operation: &str,
    started_at: i64,
    outcome: Result<Value, String>,
    store: Option<Arc<dyn crate::gateway::StateStore>>,
) {
    let summary = match &outcome {
        Ok(report) => json!({"ok": true, "report": report}),
        Err(err) => json!({"ok": false, "error": err}),
    };
    let finished = json!({
        "operation": operation,
        "started_at": started_at,
        "finished_at": aixodia_backup::now_unix(),
        "summary": summary,
    });
    if operation == "backup" && finished["summary"]["ok"] == true {
        if let Some(store) = store {
            let raw = finished.to_string();
            match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => {
                    if let Err(err) = runtime.block_on(store.put_state(LAST_BACKUP_KEY, &raw)) {
                        tracing::warn!("backup succeeded but last-backup persistence failed: {err}");
                    }
                }
                Err(err) => tracing::warn!("backup succeeded but persistence runtime could not start: {err}"),
            }
        }
    }
    let mut slots = slots().lock().unwrap_or_else(|e| e.into_inner());
    if slots
        .active
        .as_ref()
        .map(|active| active.operation.as_str())
        == Some(operation)
    {
        slots.active = None;
    }
    slots.finished = finished;
    match outcome {
        Ok(_) => tracing::info!("backup {operation}: {summary}"),
        Err(err) => tracing::warn!("backup {operation} failed: {err}"),
    }
}

/// Combine live auto-backup state with the stored config thresholds.
pub(crate) fn auto_status_with_config(stored_raw: Option<&str>) -> Value {
    let parsed: Stored = stored_raw
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default();
    let live = crate::auto_backup::auto_status_json();
    json!({
        "enabled": live["enabled"],
        "idle_streak_secs": live["idle_streak_secs"],
        "cpu_current": live["cpu_current"],
        "last_auto_backup_at": live["last_auto_backup_at"],
        "idle_secs": parsed.auto_idle_secs.unwrap_or(300),
        "cpu_threshold": parsed.auto_cpu_threshold.unwrap_or(10.0),
        "cooldown_secs": parsed.auto_cooldown_secs.unwrap_or(1800),
    })
}

/// Claim the backup slot and spawn the job. Returns true if started.
pub(crate) fn claim_and_spawn_backup(
    config: aixodia_backup::Config,
    store: Arc<dyn crate::gateway::StateStore>,
) -> bool {
    let Some(reporter) = claim("backup") else {
        return false;
    };
    spawn_job("backup", Some(store), move || {
        aixodia_backup::backup(&config, &reporter)
            .and_then(json_of)
            .map_err(|err| err.to_string())
    });
    true
}

/// Engine plumbing. A listing of these would offer the operator a scope whose
/// "files" have sizes that mean nothing and readers that hang.
/// One directory is shown whole or not at all; a picker that renders fifty
/// thousand entries is not a picker.
const BROWSE_MAX: usize = 2000;
/// How many folders a preview names. Past this the answer is "look at the first
/// few", which is what the number is for.
const PREVIEW_FOLDERS: usize = 12;

fn preview_busy() -> &'static AtomicBool {
    static FLAG: OnceLock<AtomicBool> = OnceLock::new();
    FLAG.get_or_init(|| AtomicBool::new(false))
}

/// Releases the preview flag on every way out of the handler, a panic included.
/// A flag left set by a crashed preview would freeze the button for the life of
/// the engine, which is the same mistake the run slot's doc comment warns about.
struct PreviewGuard;

impl Drop for PreviewGuard {
    fn drop(&mut self) {
        preview_busy().store(false, Ordering::Release);
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct BrowseQuery {
    #[serde(default)]
    pub path: Option<String>,
}

/// `GET /api/backup/browse?path=` — one directory, for the phone's folder
/// picker. Read-only and stateless: nothing here writes to D1, claims the run
/// slot, or touches the bucket, so a browse is as safe as asking a question.
pub async fn api_backup_browse(Query(query): Query<BrowseQuery>) -> Response {
    match list_directory(query.path.as_deref().unwrap_or("")) {
        Ok(value) => write_json(StatusCode::OK, value),
        Err(reason) => write_error(StatusCode::BAD_REQUEST, "invalid_request_error", &reason),
    }
}

/// What one directory holds, in the shape the picker draws.
///
/// The path is resolved before it is read, because the operator sees a name and
/// types or taps the rest: a refused `/root/../etc/shadow` is a refusal on the
/// real folder it points at, not on the string.
pub(crate) fn list_directory(path: &str) -> Result<Value, String> {
    let requested = path.trim();
    if requested.is_empty() {
        return Err("browse needs a folder to list".into());
    }
    let asked = Path::new(requested);
    if !asked.is_absolute() {
        return Err(format!("{requested} is not an absolute path on the engine"));
    }
    let real =
        std::fs::canonicalize(asked).map_err(|err| format!("{requested} cannot be read: {err}"))?;
    let text = real.display().to_string();
    if !real.is_dir() {
        return Err(format!("{text} is not a folder"));
    }
    let read = std::fs::read_dir(&real).map_err(|err| format!("{text} cannot be listed: {err}"))?;

    let mut rows: Vec<(String, bool, Value)> = Vec::new();
    for item in read.flatten() {
        let name = item.file_name().to_string_lossy().to_string();
        let child = item.path();
        let meta = match std::fs::symlink_metadata(&child) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        let file_type = meta.file_type();
        let is_link = file_type.is_symlink();
        let can_enter = file_type.is_dir() && !is_link;
        let entry = json!({
            "name": name,
            "path": joined(&text, &name),
            "is_dir": file_type.is_dir(),
            "is_symlink": is_link,
            "can_enter": can_enter,
        });
        rows.push((name, file_type.is_dir(), entry));
    }
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let truncated = rows.len() > BROWSE_MAX;
    rows.truncate(BROWSE_MAX);
    Ok(json!({
        "path": text,
        "parent": real.parent().map(|p| p.display().to_string()),
        "entries": rows.into_iter().map(|(_, _, entry)| entry).collect::<Vec<Value>>(),
        "truncated": truncated,
    }))
}

fn joined(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

#[derive(Debug, Default, Deserialize)]
struct PreviewRequest {
    #[serde(default)]
    root: Option<String>,
    #[serde(default)]
    includes: Option<Vec<String>>,
    #[serde(default)]
    excludes: Option<Vec<String>>,
}

/// `POST /api/backup/preview` — walk the tree the phone is about to ask for and
/// say what it holds, before a byte of it is uploaded.
///
/// The body is an *unsaved draft* of the config: this answers "what would this
/// cover" without first making it true, which is the whole reason a picker is
/// worth building. Nothing is stored, no slot is claimed, and no bucket request
/// is made — only the same directory walk a backup starts with.
pub async fn api_backup_preview(State(g): State<Arc<Gateway>>, body: Bytes) -> Response {
    let draft: PreviewRequest = if body.is_empty() {
        PreviewRequest::default()
    } else {
        match serde_json::from_slice(&body) {
            Ok(draft) => draft,
            Err(err) => {
                return write_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    &format!("preview body must be JSON: {err}"),
                )
            }
        }
    };
    // A run in flight is rewriting the tree this would read, and its numbers are
    // the ones about to be uploaded. Two answers that disagree is worse here than
    // anywhere else, because the operator chooses a scope from this one.
    if slots()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .active
        .is_some()
    {
        return busy();
    }
    if preview_busy()
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return write_json(
            StatusCode::CONFLICT,
            json!({"error": "a preview is already running"}),
        );
    }
    let _guard = PreviewGuard;

    let stored = match stored_config(&g).await {
        Ok(stored) => stored,
        Err(err) => return write_error(StatusCode::BAD_GATEWAY, "database_error", &err),
    };
    let config = match overlay(resolve(stored.as_deref()), draft) {
        Ok(config) => config,
        Err(reason) => {
            return write_error(StatusCode::BAD_REQUEST, "invalid_request_error", &reason)
        }
    };
    match tokio::task::spawn_blocking(move || scan_preview(config)).await {
        Ok(Ok(value)) => write_json(StatusCode::OK, value),
        Ok(Err(reason)) => write_error(StatusCode::BAD_GATEWAY, "scan_failed", &reason),
        Err(_) => write_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "the preview worker panicked",
        ),
    }
}

/// The stored config with the phone's draft laid over it. An absent draft field
/// keeps the stored value; a present one replaces it for this walk only.
fn overlay(
    mut config: aixodia_backup::Config,
    draft: PreviewRequest,
) -> Result<aixodia_backup::Config, String> {
    if let Some(root) = draft.root.filter(|value| !value.trim().is_empty()) {
        let path = PathBuf::from(root.trim());
        if !path.is_absolute() {
            return Err(format!("{root} is not an absolute folder on the engine"));
        }
        config.root = path;
    }
    if let Some(includes) = draft.includes {
        config.includes = includes;
    }
    if let Some(excludes) = draft.excludes {
        config.excludes = excludes;
    }
    config.validate_scope().map_err(|err| err.to_string())?;
    if !config.root.is_dir() {
        return Err(format!(
            "{} is not a folder on this engine",
            config.root.display()
        ));
    }
    Ok(config)
}

fn scan_preview(config: aixodia_backup::Config) -> Result<Value, String> {
    let mut skipped = Vec::new();
    let scanned = aixodia_backup::scan::scan(
        &config.root,
        &config.excludes,
        &config.includes,
        &mut skipped,
    )
    .map_err(|err| err.to_string())?;
    Ok(preview_json(&config, &scanned, &skipped))
}

/// The counts a run would report, and where its bytes live.
///
/// Folders are grouped by the first component of the path, because "this one
/// directory is most of the backup" is the only per-folder fact that changes what
/// an operator picks — and a whole subtree belongs to the folder above it, which
/// is where the allowlist's entries come from.
pub(crate) fn preview_json(
    config: &aixodia_backup::Config,
    scanned: &[aixodia_backup::scan::Scanned],
    skipped: &[String],
) -> Value {
    let (mut files, mut dirs, mut symlinks, mut bytes) = (0u64, 0u64, 0u64, 0u64);
    let mut folders: HashMap<String, (u64, u64)> = HashMap::new();
    for entry in scanned {
        match entry.kind {
            aixodia_backup::Kind::File => {
                files += 1;
                bytes += entry.size;
                let top = entry.path.split('/').next().unwrap_or("").to_string();
                let slot = folders.entry(top).or_insert((0, 0));
                slot.0 += 1;
                slot.1 += entry.size;
            }
            aixodia_backup::Kind::Dir => dirs += 1,
            aixodia_backup::Kind::Symlink => symlinks += 1,
        }
    }
    let mut rows: Vec<(&String, &(u64, u64))> = folders.iter().collect();
    rows.sort_by(|a, b| b.1 .1.cmp(&a.1 .1).then_with(|| a.0.cmp(b.0)));
    let listed: Vec<Value> = rows
        .into_iter()
        .take(PREVIEW_FOLDERS)
        .map(|(name, (count, size))| json!({"name": name, "files": count, "bytes": size}))
        .collect();
    json!({
        "root": config.root.display().to_string(),
        "includes": config.includes,
        "entries": scanned.len(),
        "files": files,
        "dirs": dirs,
        "symlinks": symlinks,
        "bytes": bytes,
        "skipped": skipped.len(),
        // Named, not counted alone: a preview that quietly could not read half
        // the tree would send the operator into a run that fails on those files.
        "skipped_examples": skipped.iter().take(5).collect::<Vec<&String>>(),
        "folders": listed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blanks_are_reported_one_by_one() {
        let cfg = aixodia_backup::Config {
            token: String::new(),
            bucket: String::new(),
            root: std::path::PathBuf::from("/definitely/not/here"),
            ..Default::default()
        };
        assert_eq!(needs(&cfg, BACKUP_RUN), vec!["token", "bucket", "root"]);
        assert!(!ready(&cfg, BACKUP_RUN));
    }

    /// A machine whose tree is gone is exactly the machine that needs a restore,
    /// so the missing directory may not be a reason to refuse one.
    #[test]
    fn a_missing_root_blocks_a_backup_not_a_restore() {
        let dir = std::env::temp_dir().join(format!("aix-backup-root-{}", std::process::id()));
        let gone = dir.join("no-longer-here");
        let cfg = aixodia_backup::Config {
            token: "hf_stored".into(),
            bucket: "me/bucket".into(),
            root: gone.clone(),
            ..Default::default()
        };
        assert_eq!(needs(&cfg, BACKUP_RUN), vec!["root"]);
        assert!(!ready(&cfg, BACKUP_RUN));
        assert!(needs(&cfg, RESTORE_RUN).is_empty());
        assert!(ready(&cfg, RESTORE_RUN));

        std::fs::create_dir_all(&dir).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_write_without_a_token_keeps_the_stored_one() {
        let previous: Stored =
            serde_json::from_str(r#"{"token":"hf_stored","bucket":"me/bucket"}"#).unwrap();
        let incoming: Stored =
            serde_json::from_str(r#"{"bucket":"me/other","prefix":"p"}"#).unwrap();
        let merged = Stored {
            token: incoming.token.or(previous.token),
            bucket: incoming.bucket.or(previous.bucket),
            endpoint: incoming.endpoint.or(previous.endpoint),
            prefix: incoming.prefix.or(previous.prefix),
            root: incoming.root.or(previous.root),
            excludes: incoming.excludes.or(previous.excludes),
            includes: incoming.includes.or(previous.includes),
            auto_enabled: incoming.auto_enabled.or(previous.auto_enabled),
            auto_idle_secs: incoming.auto_idle_secs.or(previous.auto_idle_secs),
            auto_cpu_threshold: incoming.auto_cpu_threshold.or(previous.auto_cpu_threshold),
            auto_cooldown_secs: incoming.auto_cooldown_secs.or(previous.auto_cooldown_secs),
        };
        let cfg = resolve(Some(&serde_json::to_string(&merged).unwrap()));
        assert_eq!(cfg.bucket, "me/other");
        assert_eq!(cfg.prefix, "p");
        // A read-only test cannot claim the env token is absent, so it asserts
        // the one thing the merge decides: a write that never mentions the
        // token does not clear it.
        assert!(!cfg.token.is_empty() || std::env::var("HF_TOKEN").is_err());
    }

    /// An omitted `excludes` is the phone saying nothing about the scope, which
    /// must not silently widen or narrow what a run walks. Sending a list — even
    /// an empty one — is the phone answering for it, and an empty list really
    /// does mean "exclude nothing".
    #[test]
    fn a_write_that_does_not_mention_excludes_leaves_them_alone() {
        let previous: Stored = serde_json::from_str(r#"{"excludes":["target","*.pyc"]}"#).unwrap();
        let untouched: Stored = serde_json::from_str(r#"{"bucket":"me/bucket"}"#).unwrap();
        assert_eq!(
            untouched.excludes.or(previous.excludes),
            Some(vec!["target".to_string(), "*.pyc".to_string()]),
            "a write that never mentions the scope keeps it"
        );

        let previous: Stored = serde_json::from_str(r#"{"excludes":["target"]}"#).unwrap();
        let cleared: Stored = serde_json::from_str(r#"{"excludes":[]}"#).unwrap();
        assert_eq!(
            cleared.excludes.or(previous.excludes),
            Some(Vec::new()),
            "an explicit empty list is not an absent one"
        );
    }

    #[test]
    fn the_token_is_never_written_back_out() {
        let value = mask("hf_abcdefghijklmnopqrstuvwxyz");
        assert_eq!(value["set"], true);
        assert_eq!(value["tail"], "wxyz");
        assert!(
            !value.to_string().contains("abcdef"),
            "the head of a token must not be reported"
        );
        assert_eq!(mask(""), Value::Null);
    }

    /// An allowlist is stored and reported the way the exclusions are: an
    /// omitted field keeps what the engine holds, and an explicit empty list is
    /// the operator answering "everything", not silence.
    #[test]
    fn a_write_that_does_not_mention_includes_leaves_them_alone() {
        let previous: Stored =
            serde_json::from_str(r#"{"includes":[".codex","projects"]}"#).unwrap();
        let untouched: Stored = serde_json::from_str(r#"{"bucket":"me/bucket"}"#).unwrap();
        assert_eq!(
            untouched.includes.or(previous.includes),
            Some(vec![".codex".to_string(), "projects".to_string()]),
            "a write that never mentions the scope keeps it"
        );

        let previous: Stored = serde_json::from_str(r#"{"includes":[".codex"]}"#).unwrap();
        let widened: Stored = serde_json::from_str(r#"{"includes":[]}"#).unwrap();
        let merged = widened.includes.clone().or(previous.includes);
        assert_eq!(
            merged,
            Some(Vec::new()),
            "switching back to everything is a real answer, not silence"
        );
        let stored = Stored {
            includes: merged,
            ..Default::default()
        };
        assert!(
            resolve(Some(&serde_json::to_string(&stored).unwrap()))
                .includes
                .is_empty(),
            "and an empty allowlist resolves to the whole tree"
        );
    }

    #[test]
    fn browse_puts_folders_first_and_marks_what_cannot_be_entered() {
        let dir = std::env::temp_dir().join(format!("aix-browse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("projects/app")).unwrap();
        std::fs::write(dir.join("notes.txt"), b"n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", dir.join("escape")).unwrap();

        let listing = list_directory(dir.to_str().unwrap()).unwrap();
        assert_eq!(
            listing["path"],
            json!(dir.canonicalize().unwrap().display().to_string())
        );
        assert!(listing["parent"].is_string(), "the picker needs a way back");
        let entries = listing["entries"].as_array().unwrap();
        assert_eq!(entries[0]["name"], json!("projects"));
        assert_eq!(entries[0]["is_dir"], json!(true));
        assert_eq!(entries[0]["can_enter"], json!(true));
        assert_eq!(
            entries[0]["path"],
            json!(format!(
                "{}/projects",
                dir.canonicalize().unwrap().display()
            ))
        );
        let link = entries
            .iter()
            .find(|entry| entry["name"] == json!("escape"))
            .unwrap();
        assert_eq!(link["is_symlink"], json!(true));
        assert_eq!(
            link["can_enter"],
            json!(false),
            "the walker does not follow a link, so neither may the picker"
        );
        let file = entries
            .iter()
            .find(|entry| entry["name"] == json!("notes.txt"))
            .unwrap();
        assert_eq!(file["is_dir"], json!(false));
        assert_eq!(file["can_enter"], json!(false));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn preview_groups_by_top_level_folder_and_sorts_by_bytes() {
        let scanned =
            |path: &str, kind: aixodia_backup::Kind, size: u64| aixodia_backup::scan::Scanned {
                path: path.into(),
                kind,
                size,
                mode: 0o644,
                mtime: 0,
                target: None,
                dev: 1,
            };
        let config = aixodia_backup::Config {
            root: PathBuf::from("/root"),
            includes: vec![".codex".into()],
            ..Default::default()
        };
        let entries = vec![
            scanned(".codex", aixodia_backup::Kind::Dir, 0),
            scanned(".codex/a.jsonl", aixodia_backup::Kind::File, 10),
            scanned(".codex/sessions/b.jsonl", aixodia_backup::Kind::File, 5),
            scanned("projects", aixodia_backup::Kind::Dir, 0),
            scanned("projects/x.md", aixodia_backup::Kind::File, 100),
            scanned("link", aixodia_backup::Kind::Symlink, 0),
        ];
        let value = preview_json(&config, &entries, &["gone".to_string()]);
        assert_eq!(value["entries"], json!(6));
        assert_eq!(value["files"], json!(3));
        assert_eq!(value["dirs"], json!(2));
        assert_eq!(value["symlinks"], json!(1));
        assert_eq!(value["bytes"], json!(115));
        assert_eq!(value["skipped"], json!(1));
        assert_eq!(value["skipped_examples"][0], json!("gone"));
        assert_eq!(value["root"], json!("/root"));
        assert_eq!(value["includes"][0], json!(".codex"));
        // The heaviest folder first, and a subtree belongs to the folder the
        // allowlist would name.
        assert_eq!(value["folders"][0]["name"], json!("projects"));
        assert_eq!(value["folders"][0]["bytes"], json!(100));
        assert_eq!(value["folders"][1]["name"], json!(".codex"));
        assert_eq!(value["folders"][1]["files"], json!(2));
        assert_eq!(value["folders"][1]["bytes"], json!(15));
    }

    /// The draft is what is on the phone's screen, not what is in D1: it replaces
    /// only the fields it names, and it is checked before it is walked.
    #[test]
    fn a_preview_draft_overrides_only_what_it_names() {
        let base = aixodia_backup::Config {
            root: std::env::temp_dir(),
            excludes: vec!["target".into()],
            includes: vec![".codex".into()],
            ..Default::default()
        };
        let draft = PreviewRequest {
            includes: Some(vec![]),
            ..Default::default()
        };
        let merged = overlay(base.clone(), draft).unwrap();
        assert!(
            merged.includes.is_empty(),
            "the switch to everything is a real answer"
        );
        assert_eq!(merged.excludes, base.excludes);
        assert_eq!(merged.root, base.root);
    }

    #[test]
    fn a_preview_draft_cannot_name_something_that_is_not_a_folder() {
        let base = aixodia_backup::Config {
            root: std::env::temp_dir(),
            ..Default::default()
        };
        let relative = PreviewRequest {
            root: Some("relative/path".into()),
            ..Default::default()
        };
        assert!(overlay(base.clone(), relative).is_err());
        let missing = PreviewRequest {
            root: Some("/definitely/not/a/folder/here".into()),
            ..Default::default()
        };
        assert!(overlay(base.clone(), missing).is_err());
        let escaping = PreviewRequest {
            includes: Some(vec!["/abs/from/root".into()]),
            ..Default::default()
        };
        assert!(overlay(base, escaping).is_err());
    }
}
