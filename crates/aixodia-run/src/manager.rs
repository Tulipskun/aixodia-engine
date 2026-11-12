//! Owns the runs this process is executing and serves their logs.
//!
//! Port of the Go `run/manager.go`. The chat handler is injected as
//! [`ChatExecutor`] instead of being called over HTTP: the run still shares
//! routing, history and persistence with `POST /v1/chat/completions`, because
//! the gateway's executor is the same code.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::event::{
    unix_now, Attachment, ChatJob, Event, Run, StartRequest, Status, KIND_STATUS, KIND_USER,
};
use crate::sink::SseSink;
use crate::store::{Error, Result, Store};

/// The gateway side of a run. It writes the same SSE stream a direct client
/// receives into the sink and returns when the turn is over.
#[async_trait]
pub trait ChatExecutor: Send + Sync {
    async fn execute(
        &self,
        job: ChatJob,
        sink: Arc<SseSink>,
        cancel: CancellationToken,
    ) -> std::result::Result<(), String>;
}

struct ActiveState {
    meta: Run,
    events: Vec<Event>,
    subs: HashMap<u64, mpsc::Sender<Event>>,
    next_sub: u64,
    persisted: usize,
}

pub struct ActiveRun {
    id: String,
    state: Mutex<ActiveState>,
    flush_mu: tokio::sync::Mutex<()>,
    store: Arc<dyn Store>,
    cancel: CancellationToken,
}

impl std::fmt::Debug for ActiveRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActiveRun").field("id", &self.id).finish()
    }
}

impl ActiveRun {
    fn snapshot(&self) -> Run {
        self.state.lock().unwrap().meta.clone()
    }

    pub(crate) fn append(&self, kind: &str, data: serde_json::Value) {
        let mut state = self.state.lock().unwrap();
        state.meta.last_seq += 1;
        let event = Event {
            run_id: state.meta.id.clone(),
            seq: state.meta.last_seq,
            kind: kind.to_string(),
            at: unix_now(),
            data,
        };
        state.meta.updated_at = event.at;
        state.events.push(event.clone());
        let mut dropped = Vec::new();
        state.subs.retain(|_, channel| {
            if channel.try_send(event.clone()).is_err() {
                dropped.push(());
                false
            } else {
                true
            }
        });
    }

    async fn finish(&self, status: Status, err_text: &str) {
        let meta = {
            let mut state = self.state.lock().unwrap();
            state.meta.status = status;
            state.meta.error = err_text.to_string();
            state.meta.updated_at = unix_now();
            state.meta.last_seq += 1;
            let event = Event {
                run_id: state.meta.id.clone(),
                seq: state.meta.last_seq,
                kind: KIND_STATUS.to_string(),
                at: state.meta.updated_at,
                data: json!({"status": status.as_str(), "error": err_text}),
            };
            state.events.push(event.clone());
            for channel in state.subs.values() {
                let _ = channel.try_send(event.clone());
            }
            state.subs.clear();
            state.meta.clone()
        };
        self.flush().await;
        if let Err(err) = self.store.update(&meta).await {
            tracing::warn!(run = %meta.id, error = %err, "run: save status");
        }
    }

    async fn subscribe(
        self: &Arc<Self>,
        after: i64,
    ) -> (
        Run,
        Vec<Event>,
        Option<mpsc::Receiver<Event>>,
        SubscriptionGuard,
    ) {
        let mut state = self.state.lock().unwrap();
        let meta = state.meta.clone();
        let backlog: Vec<Event> = state
            .events
            .iter()
            .filter(|event| event.seq > after)
            .cloned()
            .collect();
        if meta.status.terminal() {
            return (meta, backlog, None, SubscriptionGuard::empty());
        }
        let (sender, receiver) = mpsc::channel(256);
        state.next_sub += 1;
        let key = state.next_sub;
        state.subs.insert(key, sender);
        drop(state);
        (
            meta,
            backlog,
            Some(receiver),
            SubscriptionGuard::new(self.clone(), key),
        )
    }

    async fn flush(&self) {
        let _guard = self.flush_mu.lock().await;
        let (batch, start) = {
            let state = self.state.lock().unwrap();
            let start = state.persisted;
            if start >= state.events.len() {
                return;
            }
            (state.events[start..].to_vec(), start)
        };
        if let Err(err) = self.store.append_events(&batch).await {
            tracing::warn!(run = %self.id, count = batch.len(), error = %err, "run: save events");
            return;
        }
        let mut state = self.state.lock().unwrap();
        if state.persisted == start {
            state.persisted = start + batch.len();
        }
    }
}

/// Removes a live subscription when the viewer goes away.
pub struct SubscriptionGuard {
    run: Option<Arc<ActiveRun>>,
    key: u64,
}

impl SubscriptionGuard {
    fn empty() -> Self {
        Self { run: None, key: 0 }
    }

    fn new(run: Arc<ActiveRun>, key: u64) -> Self {
        Self {
            run: Some(run),
            key,
        }
    }
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        if let Some(run) = &self.run {
            run.state.lock().unwrap().subs.remove(&self.key);
        }
    }
}

pub struct Subscription {
    pub run: Run,
    pub backlog: Vec<Event>,
    pub live: Option<mpsc::Receiver<Event>>,
    pub active: bool,
    pub guard: SubscriptionGuard,
}

pub struct Manager {
    store: Arc<dyn Store>,
    executor: Arc<dyn ChatExecutor>,
    runs: Mutex<HashMap<String, Arc<ActiveRun>>>,
    shutdown: CancellationToken,
}

impl Manager {
    /// Builds a manager over the gateway's chat executor.
    pub fn new(store: Arc<dyn Store>, executor: Arc<dyn ChatExecutor>) -> Self {
        Self {
            store,
            executor,
            runs: Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
        }
    }

    /// Prepares the schema, marks runs left behind by an earlier process as
    /// interrupted, and starts the background persistence loop.
    pub async fn start(self: &Arc<Self>) -> Result<()> {
        self.store.ensure().await?;
        if let Err(err) = self.store.interrupt_stale().await {
            tracing::warn!(error = %err, "run: mark stale runs");
        }
        let weak: Weak<Manager> = Arc::downgrade(self);
        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_millis(700));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    _ = ticker.tick() => {}
                }
                let Some(manager) = weak.upgrade() else {
                    return;
                };
                let runs: Vec<Arc<ActiveRun>> =
                    { manager.runs.lock().unwrap().values().cloned().collect() };
                for run in runs {
                    run.flush().await;
                }
                // A run's events keep growing while it is live; make sure the
                // metadata the phone polls does not lag behind them for long.
            }
        });
        Ok(())
    }

    /// Stops accepting work and cancels the turns this process owns.
    pub fn close(&self) {
        self.shutdown.cancel();
        let runs: Vec<Arc<ActiveRun>> = self.runs.lock().unwrap().values().cloned().collect();
        for run in runs {
            run.cancel.cancel();
        }
    }

    /// The number of runs that have not reached a terminal status.
    pub fn active(&self) -> usize {
        self.runs
            .lock()
            .unwrap()
            .values()
            .filter(|run| !run.snapshot().status.terminal())
            .count()
    }

    fn lookup(&self, id: &str) -> Option<Arc<ActiveRun>> {
        self.runs.lock().unwrap().get(id).cloned()
    }

    fn forget(&self, id: &str) {
        self.runs.lock().unwrap().remove(id);
    }

    /// Starts a run, or returns the existing one when `client_run_id` was
    /// already used. A retry that reached a lost response must not run the turn
    /// twice.
    pub async fn create(self: &Arc<Self>, mut request: StartRequest) -> Result<(Run, bool)> {
        request.session_id = request.session_id.trim().to_string();
        request.client_run_id = request.client_run_id.trim().to_string();
        request.message = request.message.trim().to_string();
        if request.session_id.is_empty() {
            return Err(Error::Message("session_id is required".to_string()));
        }
        if request.message.is_empty() && !request.retry {
            return Err(Error::Message("message must not be empty".to_string()));
        }
        if !request.client_run_id.is_empty() {
            if let Some(existing) = self.store.find_by_client(&request.client_run_id).await? {
                return Ok((existing, true));
            }
        }

        {
            let runs = self.runs.lock().unwrap();
            for run in runs.values() {
                let meta = run.snapshot();
                if meta.session_id == request.session_id && !meta.status.terminal() {
                    return Err(Error::Message(
                        "a turn is already running for this chat".to_string(),
                    ));
                }
            }
        }

        let now = unix_now();
        let meta = Run {
            id: new_run_id(),
            session_id: request.session_id.clone(),
            client_run_id: request.client_run_id.clone(),
            status: Status::Running,
            created_at: now,
            updated_at: now,
            ..Run::default()
        };
        let cancel = self.shutdown.child_token();
        let run = Arc::new(ActiveRun {
            id: meta.id.clone(),
            state: Mutex::new(ActiveState {
                meta: meta.clone(),
                events: Vec::new(),
                subs: HashMap::new(),
                next_sub: 0,
                persisted: 0,
            }),
            flush_mu: tokio::sync::Mutex::new(()),
            store: self.store.clone(),
            cancel: cancel.clone(),
        });
        self.runs
            .lock()
            .unwrap()
            .insert(meta.id.clone(), run.clone());

        // The run row is a backup of a turn this process already owns: the store
        // is one HTTPS call per write, and making the phone wait for it before
        // its message is even accepted is the slowest possible place to learn
        // that. The turn runs from the memory copy either way.
        let store = self.store.clone();
        let stored_meta = meta.clone();
        tokio::spawn(async move {
            if let Err(err) = store.insert(&stored_meta).await {
                tracing::warn!(run = %stored_meta.id, error = %err, "run: save start");
            }
        });

        if request.retry {
            run.append(KIND_USER, json!({"retry": true}));
        } else {
            run.append(KIND_USER, json!({"text": request.message}));
        }
        run.append(KIND_STATUS, json!({"status": Status::Running.as_str()}));
        let snapshot = run.snapshot();

        let manager = self.clone();
        let request_for_job = request.clone();
        let job = ChatJob {
            run_id: meta.id.clone(),
            session_id: request.session_id.clone(),
            message: request.message.clone(),
            retry: request.retry,
            attachments: request_for_job.attachments.clone(),
        };
        tokio::spawn(async move {
            manager.execute(run, job, cancel).await;
        });
        Ok((snapshot, false))
    }

    async fn execute(&self, run: Arc<ActiveRun>, job: ChatJob, cancel: CancellationToken) {
        // Once the log is flushed, D1 is the source of truth. Keeping every
        // finished run in memory would make a long-lived engine grow without
        // bound.
        let sink = SseSink::new(run.clone());
        let result = self
            .executor
            .execute(job, sink.clone(), cancel.clone())
            .await;
        sink.close();

        let status = if cancel.is_cancelled() {
            Status::Cancelled
        } else {
            let error = sink.error_text();
            if !error.is_empty() || sink.status() != 0 && sink.status() != 200 || result.is_err() {
                Status::Failed
            } else {
                Status::Done
            }
        };
        let detail = if status == Status::Failed {
            let text = sink.error_text();
            if !text.is_empty() {
                text
            } else if let Err(message) = &result {
                message.clone()
            } else {
                let raw = sink.raw().trim().to_string();
                if raw.is_empty() {
                    format!("chat returned HTTP {}", sink.status())
                } else {
                    truncate_chars(&raw, 800)
                }
            }
        } else {
            String::new()
        };
        run.finish(status, &detail).await;
        self.forget(&run.id);
    }

    /// Stops a live run. The worker records the terminal status once the chat
    /// handler returns, so callers can poll `GET /v1/runs/{id}`.
    pub async fn cancel(&self, id: &str) -> Result<Run> {
        if let Some(run) = self.lookup(id) {
            run.cancel.cancel();
            return Ok(run.snapshot());
        }
        match self.store.get(id).await? {
            Some(meta) => Err(Error::Message(format!(
                "run is not active ({})",
                meta.status
            ))),
            None => Err(Error::Message("run not found".to_string())),
        }
    }

    /// Returns run metadata from memory, or from D1 after a restart.
    pub async fn get(&self, id: &str) -> Result<Option<Run>> {
        if let Some(run) = self.lookup(id) {
            return Ok(Some(run.snapshot()));
        }
        self.store.get(id).await
    }

    /// Returns the backlog after `after` and, while the run is live, a channel
    /// of new events. A closed channel with newer events in the store means
    /// "reconnect with the last seq you saw".
    pub async fn subscribe(self: &Arc<Self>, id: &str, after: i64) -> Result<Subscription> {
        if let Some(run) = self.lookup(id) {
            let (meta, backlog, live, guard) = run.subscribe(after).await;
            return Ok(Subscription {
                active: true,
                run: meta,
                backlog,
                live,
                guard,
            });
        }
        let Some(meta) = self.store.get(id).await? else {
            return Ok(Subscription {
                run: Run::default(),
                backlog: Vec::new(),
                live: None,
                active: false,
                guard: SubscriptionGuard::empty(),
            });
        };
        let events = self.store.events(id, after).await?;
        Ok(Subscription {
            active: false,
            run: meta,
            backlog: events,
            live: None,
            guard: SubscriptionGuard::empty(),
        })
    }
}

fn new_run_id() -> String {
    let mut buffer = [0u8; 8];
    if rand::RngCore::try_fill_bytes(&mut rand::thread_rng(), &mut buffer).is_err() {
        return format!("run_{}", unix_now());
    }
    format!("run_{}", hex::encode(buffer))
}

fn truncate_chars(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    format!("{}…", &text[..floor_boundary(text, limit)])
}

fn floor_boundary(text: &str, mut index: usize) -> usize {
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Sorts a static replay so a store that returned rows out of order cannot make
/// the phone apply them twice.
pub fn sort_events(events: &mut [Event]) {
    events.sort_by_key(|event| event.seq);
}

/// Builds the attachment payload the gateway expects.
pub fn attachment_payload(attachments: &[Attachment]) -> Vec<serde_json::Value> {
    attachments
        .iter()
        .map(|attachment| {
            json!({
                "path": attachment.path,
                "name": attachment.name,
                "mime": attachment.mime,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::KIND_ASSISTANT;
    use crate::store::MemoryStore;
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    struct FakeExecutor {
        chunks: StdMutex<Vec<Vec<u8>>>,
        delay: Duration,
    }

    #[async_trait]
    impl ChatExecutor for FakeExecutor {
        async fn execute(
            &self,
            _job: ChatJob,
            sink: Arc<SseSink>,
            _cancel: CancellationToken,
        ) -> std::result::Result<(), String> {
            let chunks = self.chunks.lock().unwrap().clone();
            for chunk in chunks {
                sink.write(&chunk);
                tokio::time::sleep(self.delay).await;
            }
            Ok(())
        }
    }

    fn manager(chunks: Vec<&str>) -> Arc<Manager> {
        let executor = Arc::new(FakeExecutor {
            chunks: StdMutex::new(chunks.into_iter().map(|s| s.as_bytes().to_vec()).collect()),
            delay: Duration::from_millis(1),
        });
        Arc::new(Manager::new(Arc::new(MemoryStore::new()), executor))
    }

    fn request() -> StartRequest {
        StartRequest {
            session_id: "chat-1".to_string(),
            message: "hello".to_string(),
            ..StartRequest::default()
        }
    }

    async fn wait_terminal(manager: &Arc<Manager>, id: &str) -> Run {
        for _ in 0..100 {
            let run = manager.get(id).await.expect("get").expect("run");
            if run.status.terminal() {
                return run;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("run did not finish");
    }

    #[tokio::test]
    async fn a_run_records_text_and_finishes_done() {
        let manager = manager(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: [DONE]\n\n",
        ]);
        let (created, existing) = manager.create(request()).await.expect("create");
        assert!(!existing);
        assert_eq!(created.status, Status::Running);
        let finished = wait_terminal(&manager, &created.id).await;
        assert_eq!(finished.status, Status::Done);
        assert!(finished.last_seq >= 4);

        let events = manager
            .subscribe(&created.id, 0)
            .await
            .expect("subscribe")
            .backlog;
        let kinds: Vec<&str> = events.iter().map(|event| event.kind.as_str()).collect();
        assert_eq!(kinds[0], KIND_USER);
        assert!(kinds.contains(&KIND_ASSISTANT));
        assert_eq!(kinds.last(), Some(&KIND_STATUS));
    }

    #[tokio::test]
    async fn a_client_run_id_is_idempotent() {
        let manager = manager(vec!["data: [DONE]\n\n"]);
        let mut req = request();
        req.client_run_id = "client-1".to_string();
        let (first, _) = manager.create(req.clone()).await.expect("create");
        wait_terminal(&manager, &first.id).await;
        let (again, existing) = manager.create(req).await.expect("create");
        assert!(existing);
        assert_eq!(again.id, first.id);
    }

    #[tokio::test]
    async fn a_second_turn_for_the_same_chat_is_refused() {
        let executor = Arc::new(FakeExecutor {
            chunks: StdMutex::new(vec![Vec::new()]),
            delay: Duration::from_secs(30),
        });
        let manager = Arc::new(Manager::new(Arc::new(MemoryStore::new()), executor));
        // An empty chunk list returns immediately, so hold a run by making the
        // executor wait before it is called.
        let (created, _) = manager.create(request()).await.expect("create");
        let err = manager.create(request()).await;
        assert!(err.is_err(), "{:?}", err.map(|(run, _)| run.id));
        manager.cancel(&created.id).await.expect("cancel");
    }

    #[tokio::test]
    async fn an_error_event_fails_the_run() {
        let manager = manager(vec![
            "data: {\"error\":{\"message\":\"upstream down\"}}\n\n",
        ]);
        let (created, _) = manager.create(request()).await.expect("create");
        let finished = wait_terminal(&manager, &created.id).await;
        assert_eq!(finished.status, Status::Failed);
        assert_eq!(finished.error, "upstream down");
    }

    #[tokio::test]
    async fn sse_chunks_split_across_writes_are_reassembled() {
        let manager = manager(vec![
            "data: {\"choices\":[{\"delta\":{\"content\":\"a\"}}]}\n",
            "\ndata: [DONE]\n\n",
        ]);
        let (created, _) = manager.create(request()).await.expect("create");
        let finished = wait_terminal(&manager, &created.id).await;
        assert_eq!(finished.status, Status::Done);
        let events = manager
            .subscribe(&created.id, 0)
            .await
            .expect("subscribe")
            .backlog;
        assert!(events
            .iter()
            .any(|event| event.kind == KIND_ASSISTANT && event.data["text"] == "a"));
    }
}
