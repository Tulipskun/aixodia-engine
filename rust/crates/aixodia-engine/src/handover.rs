//! The handover note a session leaves for the one that replaces it.
//!
//! Port of the engine's Go `handover.go`. A session is killed at its fixed limit
//! whether or not it is in the middle of something, so the interesting question
//! on boot is not "did I start" but "was the session before me cut off
//! mid-turn, and what was it answering".

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use aixodia_session::UnfinishedTurn;

use crate::tunnel::{now_unix, started_at, version_string};

/// Where a session leaves a note for the one that replaces it. The key already
/// existed; nothing wrote it, so it is defined here.
pub const HANDOVER_STATE_KEY: &str = "daemon/ready";

/// What a new session records about itself and about the session it replaced.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handover {
    /// Identifies this process, so a note can be told apart from the one the
    /// previous session left.
    pub instance: String,
    pub tunnel: String,
    pub version: String,
    /// When this session began, in unix seconds. A session ends when its engine
    /// ends, so the engine's own start is the session's start.
    pub kernel_started: i64,
    pub engine_started: i64,
    pub at: i64,
    /// The questions the previous session stored and never answered, newest
    /// first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unfinished: Vec<UnfinishedTurn>,
}

/// The one call the note makes: the questions the previous session left
/// unanswered. `gateway.SessionStore` in production, a fake in tests.
#[async_trait::async_trait]
pub trait UnfinishedStore: Send + Sync {
    async fn unfinished(&self, limit: i64) -> anyhow::Result<Vec<UnfinishedTurn>>;
}

static SESSION_STORE: RwLock<Option<Arc<dyn UnfinishedStore>>> = RwLock::new(None);

/// Installs the session store the note reads. `None` means the store is not
/// open, which [`publish_handover`] reports and gives up on.
pub fn set_session_store(store: Option<Arc<dyn UnfinishedStore>>) {
    *SESSION_STORE.write().unwrap() = store;
}

fn session_store() -> Option<Arc<dyn UnfinishedStore>> {
    SESSION_STORE.read().unwrap().clone()
}

static PUT_HANDOVER_STATE: RwLock<Option<Arc<dyn aixodia_db::StateStore>>> = RwLock::new(None);

/// Installs where the note is written: D1's `system_state` in production.
pub fn set_put_handover_state(sink: Option<Arc<dyn aixodia_db::StateStore>>) {
    *PUT_HANDOVER_STATE.write().unwrap() = sink;
}

fn put_handover_state() -> Option<Arc<dyn aixodia_db::StateStore>> {
    PUT_HANDOVER_STATE.read().unwrap().clone()
}

/// Writes the note. It runs after the tunnel is published, so the record names
/// a URL that is already reachable, and it never blocks startup: a note is
/// useful, not load-bearing.
pub async fn publish_handover(tunnel_url: &str) {
    let mut note = Handover {
        instance: format!("daemon-{}", started_at()),
        tunnel: tunnel_url.to_string(),
        version: version_string(),
        kernel_started: started_at(),
        engine_started: started_at(),
        at: now_unix(),
        unfinished: Vec::new(),
    };

    let Some(store) = session_store() else {
        tracing::warn!("handover: the session store is not open");
        return;
    };
    match store.unfinished(20).await {
        Ok(unfinished) => note.unfinished = unfinished,
        Err(err) => tracing::warn!("handover: cannot list unfinished turns: {err}"),
    }

    let payload = match serde_json::to_string(&note) {
        Ok(payload) => payload,
        Err(err) => {
            tracing::warn!("handover: {err}");
            return;
        }
    };

    // The note is written to D1's system_state, beside the daemon keys, so a
    // fresh engine on another host can read what was in flight.
    let Some(sink) = put_handover_state() else {
        tracing::warn!("handover: no state writer is configured");
        return;
    };
    if let Err(err) = sink.put_state(HANDOVER_STATE_KEY, &payload).await {
        tracing::warn!("handover: cannot write the note: {err}");
        return;
    }

    let count = note.unfinished.len();
    if count > 0 {
        tracing::info!("handover: the previous session left {count} question(s) unanswered");
        for turn in &note.unfinished {
            tracing::info!(
                "handover:   {} seq {}: {}",
                turn.session_id,
                turn.seq,
                turn.text
            );
        }
    }
    tracing::info!("handover: note written (uptime {})", uptime());
}

/// How long this process has been up, truncated to the minute as the Go note
/// logs it.
fn uptime() -> String {
    let secs = (now_unix() - started_at()).max(0) as u64;
    let secs = (secs / 60) * 60;
    let (hours, minutes, seconds) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if hours > 0 {
        format!("{hours}h{minutes}m{seconds}s")
    } else if minutes > 0 {
        format!("{minutes}m{seconds}s")
    } else {
        format!("{seconds}s")
    }
}

/// The production store: the same D1-backed session store the gateway uses.
#[async_trait::async_trait]
impl UnfinishedStore for aixodia_session::D1 {
    async fn unfinished(&self, limit: i64) -> anyhow::Result<Vec<UnfinishedTurn>> {
        Ok(aixodia_session::D1::unfinished(self, limit)
            .await
            .map_err(|err| anyhow::anyhow!(err.to_string()))?)
    }
}

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    use aixodia_db::StateStore;

    /// The note's two globals are process-wide, so the tests that replace them
    /// must not run at the same time and clear each other's store.
    fn serial() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Answers only the one call the note makes: the questions the previous
    /// session left unanswered.
    struct HandoverStore {
        turns: Vec<UnfinishedTurn>,
    }

    #[async_trait::async_trait]
    impl UnfinishedStore for HandoverStore {
        async fn unfinished(&self, _limit: i64) -> anyhow::Result<Vec<UnfinishedTurn>> {
            Ok(self.turns.clone())
        }
    }

    /// Records the one write the note makes.
    struct StoredSink {
        stored: Arc<Mutex<String>>,
    }

    #[async_trait::async_trait]
    impl StateStore for StoredSink {
        async fn get_state(&self, _key: &str) -> aixodia_db::Result<Option<String>> {
            Ok(None)
        }

        async fn put_state(&self, key: &str, value: &str) -> aixodia_db::Result<()> {
            assert_eq!(key, HANDOVER_STATE_KEY, "wrote the wrong system_state key");
            *self.stored.lock().unwrap() = value.to_string();
            Ok(())
        }
    }

    /// Puts the globals back the way the test found them, even on panic.
    struct Restore {
        started: i64,
    }

    impl Drop for Restore {
        fn drop(&mut self) {
            crate::tunnel::set_started_at(self.started);
            set_session_store(None);
            set_put_handover_state(None);
        }
    }

    // The note is how a new session learns whether the one before it was cut
    // off mid-turn. It is a key in D1's system_state beside the daemon keys,
    // and it lists the unfinished turns the session store reports.
    #[tokio::test]
    async fn publish_handover_writes_the_note() {
        let _serial = serial();
        let _restore = Restore {
            started: crate::tunnel::started_at(),
        };
        crate::tunnel::set_started_at(1700000000);

        set_session_store(Some(Arc::new(HandoverStore {
            turns: vec![UnfinishedTurn {
                session_id: "s1".to_string(),
                seq: 3,
                text: "ยังไม่ได้ตอบ".to_string(),
                created_at: 0,
            }],
        })));

        let stored = Arc::new(Mutex::new(String::new()));
        let sink = StoredSink {
            stored: Arc::clone(&stored),
        };
        set_put_handover_state(Some(Arc::new(sink)));

        publish_handover("https://handover.test").await;

        let written = stored.lock().unwrap().clone();
        assert!(!written.is_empty(), "the handover note was not written");

        let note: Handover = serde_json::from_str(&written).expect("note is not JSON");
        assert_eq!(note.tunnel, "https://handover.test");
        assert_eq!(note.kernel_started, 1700000000);
        assert_eq!(note.unfinished.len(), 1);
        assert_eq!(note.unfinished[0].text, "ยังไม่ได้ตอบ");
        assert!(note.instance.starts_with("daemon-"));
    }

    #[tokio::test]
    async fn publish_handover_survives_an_unopened_store() {
        let _serial = serial();
        let _restore = Restore {
            started: crate::tunnel::started_at(),
        };
        set_session_store(None);

        // A missing store is reported, not fatal: a note is useful, not
        // load-bearing.
        publish_handover("https://handover.test").await;
    }
}
