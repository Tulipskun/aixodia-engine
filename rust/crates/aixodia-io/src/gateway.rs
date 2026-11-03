//! The gateway itself: its state, its routes, and the session API.
//!
//! Port of the Go `io/gateway/gateway.go`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aixodia_provider::{Message, Registry};
use aixodia_session::{
    AgentPatch, Error as SessionError, NewSession, Session, Turn, TurnInput, UnfinishedTurn,
};
use aixodia_tools::Tool;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

/// How many model→tool→model rounds one turn may spend.
///
/// Four was not enough for real work: a session asking for a change to a
/// notebook used read, bash, read, bash and was cut off there, so the turn
/// ended with the model still narrating what it was about to look at and
/// nothing done.
pub const MAX_TOOL_ROUNDS: i64 = 100;
pub const KEY_ATTEMPTS: usize = 3;
/// Keyless providers are tried again on a quota answer: the free tiers throttle
/// per IP for a fixed window (Pollinations answers again after ~30 s), so a
/// paced retry succeeds where one try would look like an outage.
pub const KEYLESS_ATTEMPTS: usize = 4;
pub const KEYLESS_BACKOFF: [Duration; 3] = [
    Duration::from_secs(3),
    Duration::from_secs(8),
    Duration::from_secs(20),
];
pub const UPSTREAM_WAIT: Duration = Duration::from_secs(180);
pub const REQUEST_MAX_BODY: usize = 8 << 20;

/// The release tag, baked in at build time. Bump on every release so a live
/// service can be identified via `GET /healthz`.
pub const VERSION: &str = "v1.172";

/// The rule a client-supplied draft id has to follow, as a readable pattern.
pub const SESSION_ID_PATTERN: &str = "^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$";

/// What a client-supplied draft id may contain. The phone puts this id in URLs
/// and on disk, so path separators and whitespace must not get through.
pub fn valid_session_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_alphanumeric() {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    rest.len() <= 127
        && rest
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// The session store the gateway reads and writes.
///
/// It is a trait so the tests can drive the gateway against an in-memory store,
/// exactly like the Go `SessionStore` interface.
#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    async fn create(&self, params: NewSession) -> Result<Session, SessionError>;
    async fn get(&self, id: &str) -> Result<Session, SessionError>;
    async fn list(&self) -> Result<Vec<Session>, SessionError>;
    async fn delete(&self, id: &str) -> Result<(), SessionError>;
    async fn save_config(&self, id: &str, saved: &Session) -> Result<(), SessionError>;
    async fn save_agent(&self, id: &str, patch: &AgentPatch) -> Result<(), SessionError>;
    async fn turns(&self, id: &str) -> Result<Vec<Turn>, SessionError>;
    async fn history(&self, id: &str) -> Result<Vec<Message>, SessionError>;
    async fn append_turns(&self, id: &str, turns: &[TurnInput]) -> Result<Vec<Turn>, SessionError>;

    /// The same append, reporting a partial write.
    ///
    /// Go's `AppendTurns` returns the rows it managed to store *and* the error
    /// that stopped it, and the trail writer needs both: it marks only what was
    /// written, so the next round retries the suffix instead of duplicating it.
    async fn append_turns_partial(
        &self,
        id: &str,
        turns: &[TurnInput],
    ) -> aixodia_session::Appended {
        match self.append_turns(id, turns).await {
            Ok(rows) => aixodia_session::Appended { rows, error: None },
            Err(err) => aixodia_session::Appended {
                rows: Vec::new(),
                error: Some(err),
            },
        }
    }
    async fn unfinished(&self, limit: i64) -> Result<Vec<UnfinishedTurn>, SessionError>;

    /// One page of a transcript. The default slices the full answer the same way
    /// the SQL does, which is what the Go fallback for a store without
    /// `TurnsPage` does.
    async fn turns_page(
        &self,
        id: &str,
        before_seq: i64,
        limit: i64,
    ) -> Result<Vec<Turn>, SessionError> {
        let turns = self.turns(id).await?;
        Ok(turn_page(&turns, before_seq, limit))
    }
}

/// Reads recent engine log entries, newest first.
pub type LogReader = Arc<dyn Fn(i64, &str) -> Vec<Value> + Send + Sync>;

/// The engine's key/value state: the settings blob the phone saves. D1's
/// `system_state` is the production implementation.
#[async_trait::async_trait]
pub trait StateStore: Send + Sync {
    async fn get_state(&self, key: &str) -> anyhow::Result<Option<String>>;
    async fn put_state(&self, key: &str, value: &str) -> anyhow::Result<()>;
}

#[async_trait::async_trait]
impl StateStore for aixodia_db::State {
    async fn get_state(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self.0.get_state(key).await?)
    }

    async fn put_state(&self, key: &str, value: &str) -> anyhow::Result<()> {
        self.0.put_state(key, value).await?;
        Ok(())
    }
}

pub struct Gateway {
    pub(crate) sessions: Arc<dyn SessionStore>,
    pub(crate) registry: Arc<Registry>,
    pub(crate) tools: HashMap<String, Tool>,
    key_index: Mutex<HashMap<String, usize>>,
    pub(crate) model_list: Mutex<HashMap<String, CachedModels>>,
    pub(crate) breakers: Mutex<HashMap<String, crate::breaker::Breaker>>,
    log_reader: RwLock<Option<LogReader>>,
    pub(crate) probes: crate::api::ProbeCache,
    started_at: AtomicI64,
    /// Where attachments are stored, so the agent can read them.
    upload_dir: RwLock<String>,
    /// Where the phone's settings live: D1's `system_state` in production.
    pub(crate) state: RwLock<Option<Arc<dyn StateStore>>>,
    /// Where the active goals and plans live.
    pub(crate) objectives: RwLock<Option<Arc<dyn aixodia_goals::Store>>>,
    /// The chats with a handler currently running. A reconnecting phone can
    /// resend a message whose ack was lost while the first turn still runs; the
    /// duplicate must not start a second answer.
    turns: Mutex<HashSet<String>>,
    /// The D1 rows each chat still owes, written behind the turn.
    pub(crate) backups: Backups,
}

/// The D1 writes a process owes, one ordered queue per chat.
///
/// A turn never waits for its own rows. D1 answers one HTTPS request per call,
/// so a write the reader is not waiting on would still put a second or more
/// between the message and the model. Writes queue here instead, serialized per
/// chat, so a question lands before the tool steps of the round it asked for and
/// the answer lands last.
#[derive(Clone)]
pub struct Backups {
    queues: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Writes queued and not finished yet. An update that replaces this process
    /// with `execve` cannot finish them afterwards, so it waits for this to
    /// reach zero the same way it waits for a chat to end.
    owed: Arc<AtomicI64>,
}

impl Default for Backups {
    fn default() -> Self {
        Self {
            queues: Arc::default(),
            owed: Arc::new(AtomicI64::new(0)),
        }
    }
}

impl Backups {
    fn queue_for(&self, session_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut queues = self.queues.lock().unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            queues
                .entry(session_id.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    /// Whether this process still owes a chat a write.
    pub fn owes(&self) -> bool {
        self.owed.load(Ordering::SeqCst) > 0
    }

    /// Runs a write behind everything already queued for the chat. The caller
    /// does not wait for it, and nothing waits for the caller.
    pub fn queue<F>(&self, session_id: &str, job: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if session_id.is_empty() {
            return;
        }
        let queue = self.queue_for(session_id);
        let owed = self.owed.clone();
        owed.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(async move {
            let _turn = queue.lock().await;
            job.await;
            owed.fetch_sub(1, Ordering::SeqCst);
        });
    }

    /// Waits until every write queued for the chat has landed. The next turn of
    /// a chat takes this before reading its history, so a message sent the
    /// moment its answer appeared still reads that answer.
    pub async fn drain(&self, session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        let queue = self.queue_for(session_id);
        let _turn = queue.lock().await;
    }

    /// Waits for every write of every chat. A turn that answered before D1 kept
    /// its rows leaves the catch-up to this queue, and a process about to exit
    /// must not take the catch-up away with it.
    pub async fn drain_all(&self) {
        let queues: Vec<Arc<tokio::sync::Mutex<()>>> = self
            .queues
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(Arc::clone)
            .collect();
        for queue in queues {
            let _turn = queue.lock().await;
        }
    }
}

/// One upstream's model list and when it was fetched.
#[derive(Debug, Clone)]
pub(crate) struct CachedModels {
    pub(crate) fetched: std::time::Instant,
    pub(crate) ids: Vec<String>,
}

impl Gateway {
    pub fn new(
        sessions: Arc<dyn SessionStore>,
        registry: Arc<Registry>,
        tools: HashMap<String, Tool>,
    ) -> Self {
        Self {
            sessions,
            registry,
            tools,
            key_index: Mutex::new(HashMap::new()),
            model_list: Mutex::new(HashMap::new()),
            breakers: Mutex::new(HashMap::new()),
            log_reader: RwLock::new(None),
            probes: crate::api::ProbeCache::new(),
            started_at: AtomicI64::new(0),
            upload_dir: RwLock::new(String::new()),
            state: RwLock::new(None),
            objectives: RwLock::new(None),
            turns: Mutex::new(HashSet::new()),
            backups: Backups::default(),
        }
    }

    /// Runs a D1 write behind the chat's turn instead of inside it.
    pub(crate) fn queue_backup<F>(&self, session_id: &str, job: F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        self.backups.queue(session_id, job);
    }

    /// Waits for the writes an earlier turn of this chat left behind, so the
    /// history read that follows sees them.
    pub(crate) async fn await_backups(&self, session_id: &str) {
        self.backups.drain(session_id).await;
    }

    /// Waits for every D1 write this process owes, which is what an exit does
    /// before it stops.
    pub async fn await_all_backups(&self) {
        self.backups.drain_all().await;
    }

    /// Whether a turn of any chat still owes D1 a write.
    pub fn owes_backups(&self) -> bool {
        self.backups.owes()
    }

    /// Claims a chat for one handling turn. Direct HTTP callers and the mobile
    /// socket share the same gateway, so both are covered.
    pub(crate) fn begin_turn(&self, session_id: &str) -> bool {
        if session_id.is_empty() {
            return true;
        }
        let mut turns = self.turns.lock().unwrap_or_else(|e| e.into_inner());
        if turns.contains(session_id) {
            return false;
        }
        turns.insert(session_id.to_string());
        true
    }

    pub(crate) fn end_turn(&self, session_id: &str) {
        if session_id.is_empty() {
            return;
        }
        self.turns
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    /// Records when this engine process began, so `/healthz` can report how long
    /// the daemon has been up.
    pub fn set_started_at(&self, unix_seconds: i64) {
        self.started_at.store(unix_seconds, Ordering::SeqCst);
    }

    pub fn started_at(&self) -> i64 {
        self.started_at.load(Ordering::SeqCst)
    }

    pub fn set_log_reader(&self, reader: LogReader) {
        *self.log_reader.write().unwrap_or_else(|e| e.into_inner()) = Some(reader);
    }

    pub(crate) fn read_logs(&self, limit: i64, level: &str) -> Vec<Value> {
        match self
            .log_reader
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(reader) => reader(limit, level),
            None => Vec::new(),
        }
    }

    pub fn set_state_store(&self, store: Arc<dyn StateStore>) {
        *self.state.write().unwrap_or_else(|e| e.into_inner()) = Some(store);
    }

    pub fn set_objective_store(&self, store: Arc<dyn aixodia_goals::Store>) {
        *self.objectives.write().unwrap_or_else(|e| e.into_inner()) = Some(store);
    }

    pub(crate) fn objective_store(&self) -> Option<Arc<dyn aixodia_goals::Store>> {
        self.objectives
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The whole HTTP surface.
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/healthz", get(healthz))
            .route("/restart", post(crate::restart::api_restart))
            .route("/v1/providers", get(list_providers))
            .route("/v1/models", get(crate::models::list_models))
            .route("/v1/sessions", post(create_session).get(list_sessions))
            .route(
                "/v1/sessions/:id",
                get(get_session)
                    .delete(delete_session)
                    .patch(update_session),
            )
            .route("/v1/sessions/:id/turns", get(get_session_turns))
            .route("/v1/sessions/:id/objectives", get(get_session_objectives))
            .route("/v1/chat/completions", post(crate::chat::chat))
            .merge(self.register_api())
            // The gateway checks the request size itself, with the same
            // message Go's handler produces; axum's default 2 MiB limit would
            // answer first and with a different error.
            .layer(axum::extract::DefaultBodyLimit::disable())
            .with_state(self.clone())
    }

    fn register_api(self: &Arc<Self>) -> Router<Arc<Self>> {
        Router::new()
            .route(
                "/api/providers",
                get(crate::api::api_list_providers).post(crate::api::api_add_provider),
            )
            .route(
                "/api/providers/refresh",
                post(crate::api::api_refresh_providers),
            )
            .route(
                "/api/providers/:id/refresh",
                post(crate::api::api_refresh_provider),
            )
            .route("/api/providers/:id/keys", post(crate::api::api_change_keys))
            .route(
                "/api/providers/:id",
                delete(crate::api::api_remove_provider),
            )
            .route("/api/models", get(crate::api::api_models))
            .route(
                "/api/settings",
                get(crate::api::api_get_settings).put(crate::api::api_put_settings),
            )
            .route(
                "/v1/attachments",
                post(crate::upload::api_upload_attachment),
            )
            .route("/api/logs", get(crate::api::api_logs))
            .route(
                "/api/backup",
                get(crate::backup::api_backup_status).post(crate::backup::api_backup_start),
            )
            .route(
                "/api/backup/config",
                get(crate::backup::api_backup_status).put(crate::backup::api_backup_config_put),
            )
            .route(
                "/api/backup/restore",
                post(crate::backup::api_backup_restore),
            )
            // The two read-only tools behind the picker: what a folder holds, and
            // what a scope would cover. Neither stores anything or takes the run
            // slot, so they answer while a backup is configured but idle.
            .route("/api/backup/browse", get(crate::backup::api_backup_browse))
            .route(
                "/api/backup/preview",
                post(crate::backup::api_backup_preview),
            )
            .route(
                "/api/sessions/:id",
                get(crate::api::api_get_session_agent).patch(crate::api::api_patch_session_agent),
            )
    }

    pub(crate) fn next_key(&self, name: &str) -> usize {
        let mut index = self.key_index.lock().unwrap_or_else(|e| e.into_inner());
        let value = *index.get(name).unwrap_or(&0);
        index.insert(name.to_string(), value + 1);
        value
    }

    pub(crate) async fn stored_settings(&self) -> anyhow::Result<Option<String>> {
        let store = self.state.read().unwrap_or_else(|e| e.into_inner()).clone();
        let Some(store) = store else {
            anyhow::bail!("the engine state store is not open");
        };
        store.get_state(crate::api::SETTINGS_STATE_KEY).await
    }

    pub fn set_upload_dir(&self, dir: impl Into<String>) {
        *self.upload_dir.write().unwrap_or_else(|e| e.into_inner()) = dir.into();
    }

    pub(crate) fn upload_dir(&self) -> String {
        self.upload_dir
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn sessions(&self) -> Arc<dyn SessionStore> {
        self.sessions.clone()
    }

    pub(crate) fn tool(&self, name: &str) -> Option<Tool> {
        self.tools.get(name).cloned()
    }

    pub(crate) fn registry(&self) -> Arc<Registry> {
        self.registry.clone()
    }
}

async fn healthz(State(g): State<Arc<Gateway>>) -> Response {
    // The launcher reads `pid` before signalling and `update` before deciding
    // that SIGUSR1 is safe: on a build that predates the handshake the signal
    // would kill the process. `idle` is the same check `/v1/runs` uses to avoid
    // a swap mid-turn.
    let idle = g.turns.lock().unwrap_or_else(|e| e.into_inner()).is_empty();
    write_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "version": VERSION,
            "started": g.started_at(),
            "pid": std::process::id(),
            "update": "signal:USR1",
            "idle": idle,
        }),
    )
}

async fn list_providers() -> Response {
    let providers = aixodia_config::get_providers();
    let list: Vec<Value> = providers
        .iter()
        .map(|provider| {
            json!({
                "name": provider.name,
                "adapter": provider.adapter,
                "endpoint": provider.api_url,
                "keys": provider.keys.len(),
            })
        })
        .collect();
    write_json(StatusCode::OK, json!({"providers": list}))
}

#[derive(Debug, Default, Deserialize)]
pub struct CreateSessionRequest {
    /// Lets the phone promote a locally-created draft without changing the id
    /// the screen and its pending messages already use. A second create with the
    /// same id is idempotent, so a retried HTTP request cannot make twins.
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(default)]
    pub top_p: Option<f64>,
    #[serde(default)]
    pub max_tokens: i64,
}

async fn create_session(State(g): State<Arc<Gateway>>, body: Bytes) -> Response {
    let body: CreateSessionRequest = match decode_body(body) {
        Ok(body) => body,
        Err(err) => {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &err.to_string(),
            )
        }
    };
    if !body.provider.is_empty() && aixodia_config::get_provider_by_name(&body.provider).is_none() {
        return write_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &format!("unknown provider {:?}", body.provider),
        );
    }
    let requested = body.id.trim().to_string();
    if !requested.is_empty() {
        if !valid_session_id(&requested) {
            return write_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "session id must be letters, digits, '.', '_' or '-'",
            );
        }
        // Promoting a draft may be retried after a lost response. Returning the
        // row that already exists makes that retry harmless.
        match g.sessions.get(&requested).await {
            Ok(found) => return write_json(StatusCode::OK, found_json(&found)),
            Err(SessionError::NotFound(_)) => {}
            Err(err) => {
                return write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string())
            }
        }
    }
    let created = g
        .sessions
        .create(NewSession {
            id: requested.clone(),
            title: body.title,
            provider: body.provider,
            model: body.model,
            reasoning: body.reasoning,
            temperature: body.temperature,
            top_p: body.top_p,
            max_tokens: body.max_tokens,
        })
        .await;
    match created {
        Ok(session) => write_json(StatusCode::CREATED, found_json(&session)),
        Err(err) => {
            // Two requests racing with the same draft id: the loser still gets
            // the one row the winner made.
            if !requested.is_empty() {
                if let Ok(found) = g.sessions.get(&requested).await {
                    return write_json(StatusCode::OK, found_json(&found));
                }
            }
            write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string())
        }
    }
}

async fn list_sessions(State(g): State<Arc<Gateway>>) -> Response {
    match g.sessions.list().await {
        Ok(sessions) => write_json(
            StatusCode::OK,
            json!({"sessions": sessions.iter().map(found_json).collect::<Vec<_>>()}),
        ),
        Err(err) => write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string()),
    }
}

async fn get_session(State(g): State<Arc<Gateway>>, Path(id): Path<String>) -> Response {
    match g.sessions.get(&id).await {
        Ok(found) => write_json(StatusCode::OK, found_json(&found)),
        Err(err) => write_error(StatusCode::NOT_FOUND, "not_found", &err.to_string()),
    }
}

async fn delete_session(State(g): State<Arc<Gateway>>, Path(id): Path<String>) -> Response {
    match g.sessions.delete(&id).await {
        Ok(()) => write_json(StatusCode::OK, json!({"deleted": true})),
        Err(err) => write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string()),
    }
}

#[derive(Debug, Default, Deserialize)]
struct UpdateSessionRequest {
    #[serde(default)]
    title: String,
}

async fn update_session(
    State(g): State<Arc<Gateway>>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let body: UpdateSessionRequest = decode_body(body).unwrap_or_default();
    let mut found = match g.sessions.get(&id).await {
        Ok(found) => found,
        Err(err) => return write_error(StatusCode::NOT_FOUND, "not_found", &err.to_string()),
    };
    if !body.title.is_empty() {
        found.title = body.title;
    }
    if let Err(err) = g.sessions.save_config(&id, &found).await {
        return write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string());
    }
    write_json(StatusCode::OK, found_json(&found))
}

#[derive(Debug, Default, Deserialize)]
struct TurnsQuery {
    #[serde(default)]
    before_seq: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn get_session_turns(
    State(g): State<Arc<Gateway>>,
    Path(id): Path<String>,
    Query(query): Query<TurnsQuery>,
) -> Response {
    // A phone opens a long chat on its newest page and asks for one older page
    // per scroll. Without these two parameters every open sent the whole
    // transcript over the tunnel.
    let before_seq = query.before_seq.unwrap_or(0);
    let mut limit = query.limit.unwrap_or(0);
    if limit > 500 {
        limit = 500;
    }
    let turns = if limit > 0 {
        g.sessions.turns_page(&id, before_seq, limit).await
    } else {
        g.sessions.turns(&id).await
    };
    match turns {
        Ok(turns) => write_json(
            StatusCode::OK,
            json!({"turns": turns.iter().map(turn_json).collect::<Vec<_>>()}),
        ),
        Err(err) => write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string()),
    }
}

/// Every goal and plan of one chat, with its status, for the app's goals card.
async fn get_session_objectives(State(g): State<Arc<Gateway>>, Path(id): Path<String>) -> Response {
    let Some(store) = g.objective_store() else {
        return write_json(StatusCode::OK, json!({"goals": [], "plans": []}));
    };
    match store.all(&id).await {
        Ok(items) => {
            let row = |item: &aixodia_goals::Item| {
                json!({
                    "id": item.id,
                    "content": item.content,
                    "status": item.status,
                })
            };
            let goals: Vec<_> = items
                .iter()
                .filter(|item| item.kind == aixodia_goals::Kind::Goal.as_str())
                .map(row)
                .collect();
            let plans: Vec<_> = items
                .iter()
                .filter(|item| item.kind == aixodia_goals::Kind::Plan.as_str())
                .map(row)
                .collect();
            write_json(StatusCode::OK, json!({"goals": goals, "plans": plans}))
        }
        Err(err) => write_error(StatusCode::BAD_GATEWAY, "database_error", &err.to_string()),
    }
}

/// Picks the newest `limit` turns before `before_seq` from an oldest-first
/// transcript. It is the fallback for a store without a paged read, and the
/// same rule the SQL uses.
pub fn turn_page(turns: &[Turn], before_seq: i64, limit: i64) -> Vec<Turn> {
    if limit <= 0 {
        return turns.to_vec();
    }
    let mut end = turns.len();
    if before_seq > 0 {
        end = 0;
        while end < turns.len() && turns[end].seq < before_seq {
            end += 1;
        }
    }
    let start = end.saturating_sub(limit as usize);
    turns[start..end].to_vec()
}

pub fn new_completion_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("chatcmpl-{nanos}")
}

/// Reads and validates a JSON request body. `encoding/json` silently replaces
/// invalid bytes with U+FFFD, so a Thai message sent in the wrong encoding would
/// be saved as a row of replacement characters; this refuses it instead.
pub fn decode_body<T: DeserializeOwned>(body: Bytes) -> anyhow::Result<T> {
    if body.len() > REQUEST_MAX_BODY {
        anyhow::bail!("request body exceeds the {REQUEST_MAX_BODY}-byte limit");
    }
    let raw = std::str::from_utf8(&body).map_err(|_| {
        anyhow::anyhow!("request body is not valid UTF-8; send the message as UTF-8 text")
    })?;
    serde_json::from_str(raw).map_err(|err| anyhow::anyhow!("cannot decode JSON body: {err}"))
}

pub fn write_json(status: StatusCode, payload: Value) -> Response {
    (status, Json(payload)).into_response()
}

pub fn write_error(status: StatusCode, err_type: &str, message: &str) -> Response {
    write_json(
        status,
        json!({"error": {"message": message, "type": err_type}}),
    )
}

pub fn write_raw(status: StatusCode, body: String) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

/// The wire shape of a session. The Rust type already carries the field names
/// the Go struct marshals, so this is the struct itself.
pub fn found_json(session: &Session) -> Value {
    serde_json::to_value(session).unwrap_or_else(|_| json!({}))
}

/// The wire shape of a turn.
pub fn turn_json(turn: &Turn) -> Value {
    serde_json::to_value(turn).unwrap_or_else(|_| json!({}))
}

/// Truncates to a byte budget the way Go's `text[:limit]` does, replacing a
/// split character with U+FFFD rather than refusing to cut.
pub fn truncate(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let cut = String::from_utf8_lossy(&text.as_bytes()[..limit]);
    format!("{}...", cut.trim())
}

pub(crate) fn first_non_zero(value: i64, fallback: i64) -> i64 {
    if value != 0 {
        value
    } else {
        fallback
    }
}

pub(crate) fn first_non_zero_ptr(value: Option<f64>, fallback: Option<f64>) -> Option<f64> {
    value.or(fallback)
}

/// The bearer-token guard. `/healthz` stays open so the tunnel can be checked.
pub async fn require_token(
    State(token): State<String>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    // The terminal and room sockets authenticate themselves: a terminal may
    // carry a single-use ticket instead of the long-lived bearer, and the room
    // checks the same bearer before its upgrade.
    if matches!(request.uri().path(), "/healthz" | "/terminal" | "/room") {
        return next.run(request).await;
    }
    if !bearer_ok(request.headers(), &token) {
        return write_error(StatusCode::UNAUTHORIZED, "unauthorized", "token rejected");
    }
    next.run(request).await
}

/// The bearer token on a handshake, as `RequireToken` reads it. The scheme is
/// optional here because `/ws` never required it: the header is compared after
/// trimming `Bearer `, so a bare token matched before and must keep matching.
pub fn bearer_ok(headers: &HeaderMap, token: &str) -> bool {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim()
        .trim_start_matches("Bearer ")
        .trim();
    !token.is_empty() && constant_time_eq(auth.as_bytes(), token.as_bytes())
}

/// A comparison whose running time does not depend on where the first difference
/// is, like Go's `subtle.ConstantTimeCompare`.
pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}
