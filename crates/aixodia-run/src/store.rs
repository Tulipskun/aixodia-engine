//! Persistence for runs and their events.
//!
//! Port of the Go `run/store.go`: the D1 implementation is the real one, the
//! memory store is used by tests.

use std::collections::HashMap;
use std::sync::Mutex;

use aixodia_db::Client;
use async_trait::async_trait;
use serde_json::Value;

use crate::event::{Event, Run, Status};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Db(#[from] aixodia_db::Error),
    #[error("{0}")]
    Message(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Everything the manager needs to survive its own process.
#[async_trait]
pub trait Store: Send + Sync {
    async fn ensure(&self) -> Result<()>;
    async fn insert(&self, run: &Run) -> Result<()>;
    async fn update(&self, run: &Run) -> Result<()>;
    async fn get(&self, id: &str) -> Result<Option<Run>>;
    async fn find_by_client(&self, client_id: &str) -> Result<Option<Run>>;
    async fn interrupt_stale(&self) -> Result<()>;
    async fn append_events(&self, events: &[Event]) -> Result<()>;
    async fn events(&self, run_id: &str, after: i64) -> Result<Vec<Event>>;
}

/// Keeps runs and their events in Cloudflare D1, beside the chat history the
/// runs write. Events are inserted in small batches because D1 limits the
/// number of bound parameters in one statement.
pub struct D1Store {
    db: Client,
}

impl D1Store {
    pub fn new(db: Client) -> Self {
        Self { db }
    }
}

#[async_trait]
impl Store for D1Store {
    async fn ensure(&self) -> Result<()> {
        for statement in [
            "CREATE TABLE IF NOT EXISTS runs (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                client_run_id TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL,
                error TEXT NOT NULL DEFAULT '',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL,
                last_seq INTEGER NOT NULL DEFAULT 0
            )",
            "CREATE UNIQUE INDEX IF NOT EXISTS runs_client_run_id
                ON runs(client_run_id) WHERE client_run_id != ''",
            "CREATE INDEX IF NOT EXISTS runs_session_id ON runs(session_id, created_at DESC)",
            "CREATE TABLE IF NOT EXISTS run_events (
                run_id TEXT NOT NULL,
                seq INTEGER NOT NULL,
                kind TEXT NOT NULL,
                data TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (run_id, seq)
            )",
            "CREATE INDEX IF NOT EXISTS run_events_run_seq ON run_events(run_id, seq)",
        ] {
            self.db.query(statement, &[]).await?;
        }
        Ok(())
    }

    async fn insert(&self, run: &Run) -> Result<()> {
        self.db
            .query(
                "INSERT INTO runs (id,session_id,client_run_id,status,error,created_at,updated_at,last_seq)
                 VALUES (?,?,?,?,?,?,?,?)",
                &[
                    Value::from(run.id.clone()),
                    Value::from(run.session_id.clone()),
                    Value::from(run.client_run_id.clone()),
                    Value::from(run.status.as_str()),
                    Value::from(run.error.clone()),
                    Value::from(run.created_at),
                    Value::from(run.updated_at),
                    Value::from(run.last_seq),
                ],
            )
            .await?;
        Ok(())
    }

    async fn update(&self, run: &Run) -> Result<()> {
        self.db
            .query(
                "UPDATE runs SET status=?, error=?, updated_at=?, last_seq=? WHERE id=?",
                &[
                    Value::from(run.status.as_str()),
                    Value::from(run.error.clone()),
                    Value::from(run.updated_at),
                    Value::from(run.last_seq),
                    Value::from(run.id.clone()),
                ],
            )
            .await?;
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<Run>> {
        let rows = self
            .db
            .query(
                "SELECT id,session_id,client_run_id,status,error,created_at,updated_at,last_seq FROM runs WHERE id=? LIMIT 1",
                &[Value::from(id)],
            )
            .await?;
        Ok(rows.first().map(run_from_row))
    }

    async fn find_by_client(&self, client_id: &str) -> Result<Option<Run>> {
        if client_id.trim().is_empty() {
            return Ok(None);
        }
        let rows = self
            .db
            .query(
                "SELECT id,session_id,client_run_id,status,error,created_at,updated_at,last_seq FROM runs WHERE client_run_id=? LIMIT 1",
                &[Value::from(client_id)],
            )
            .await?;
        Ok(rows.first().map(run_from_row))
    }

    /// Marks work this process cannot still be running. A task does not survive
    /// a restart; saying "running" would make the phone wait for events that
    /// will never arrive. `waiting_approval` is a status older builds wrote;
    /// no build writes it any more, but rows already in D1 still carry it and
    /// would otherwise read back as running forever.
    async fn interrupt_stale(&self) -> Result<()> {
        self.db
            .query(
                "UPDATE runs SET status='interrupted', updated_at=unixepoch(),
                     error=CASE WHEN error='' THEN 'engine restarted' ELSE error END
                 WHERE status IN ('running','waiting_approval')",
                &[],
            )
            .await?;
        Ok(())
    }

    async fn append_events(&self, events: &[Event]) -> Result<()> {
        for batch in events.chunks(16) {
            let placeholders = vec!["(?,?,?,?,?)"; batch.len()].join(",");
            let mut values = Vec::with_capacity(batch.len() * 5);
            for event in batch {
                values.extend([
                    Value::from(event.run_id.clone()),
                    Value::from(event.seq),
                    Value::from(event.kind.clone()),
                    Value::from(
                        serde_json::to_string(&event.data).unwrap_or_else(|_| "null".to_string()),
                    ),
                    Value::from(event.at),
                ]);
            }
            self.db
                .query(
                    &format!(
                        "INSERT INTO run_events (run_id,seq,kind,data,created_at) VALUES {placeholders} ON CONFLICT(run_id,seq) DO NOTHING"
                    ),
                    &values,
                )
                .await?;
        }
        Ok(())
    }

    async fn events(&self, run_id: &str, after: i64) -> Result<Vec<Event>> {
        let rows = self
            .db
            .query(
                "SELECT seq,kind,data,created_at FROM run_events WHERE run_id=? AND seq>? ORDER BY seq ASC",
                &[Value::from(run_id), Value::from(after)],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|row| Event {
                run_id: run_id.to_string(),
                seq: int_field(row, "seq"),
                kind: string_field(row, "kind"),
                at: int_field(row, "created_at"),
                data: serde_json::from_str(&string_field(row, "data")).unwrap_or(Value::Null),
            })
            .collect())
    }
}

fn run_from_row(row: &aixodia_db::Row) -> Run {
    let status = match string_field(row, "status").as_str() {
        "done" => Status::Done,
        "failed" => Status::Failed,
        "cancelled" => Status::Cancelled,
        "interrupted" => Status::Interrupted,
        _ => Status::Running,
    };
    Run {
        id: string_field(row, "id"),
        session_id: string_field(row, "session_id"),
        client_run_id: string_field(row, "client_run_id"),
        status,
        error: string_field(row, "error"),
        created_at: int_field(row, "created_at"),
        updated_at: int_field(row, "updated_at"),
        last_seq: int_field(row, "last_seq"),
    }
}

fn string_field(row: &aixodia_db::Row, key: &str) -> String {
    match row.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn int_field(row: &aixodia_db::Row, key: &str) -> i64 {
    match row.get(key) {
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|float| float as i64))
            .unwrap_or_default(),
        Some(Value::String(text)) => text.parse().unwrap_or_default(),
        Some(Value::Bool(value)) => i64::from(*value),
        _ => 0,
    }
}

/// The test store. It keeps ordering explicit so replay tests can assert exact
/// seq values.
#[derive(Default)]
pub struct MemoryStore {
    state: Mutex<MemoryState>,
}

#[derive(Default)]
struct MemoryState {
    runs: HashMap<String, Run>,
    events: HashMap<String, Vec<Event>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl Store for MemoryStore {
    async fn ensure(&self) -> Result<()> {
        Ok(())
    }

    async fn insert(&self, run: &Run) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        if !run.client_run_id.is_empty()
            && state
                .runs
                .values()
                .any(|existing| existing.client_run_id == run.client_run_id)
        {
            return Err(Error::Message("duplicate client run id".to_string()));
        }
        state.runs.insert(run.id.clone(), run.clone());
        Ok(())
    }

    async fn update(&self, run: &Run) -> Result<()> {
        self.state
            .lock()
            .unwrap()
            .runs
            .insert(run.id.clone(), run.clone());
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<Run>> {
        Ok(self.state.lock().unwrap().runs.get(id).cloned())
    }

    async fn find_by_client(&self, client_id: &str) -> Result<Option<Run>> {
        if client_id.is_empty() {
            return Ok(None);
        }
        Ok(self
            .state
            .lock()
            .unwrap()
            .runs
            .values()
            .find(|run| run.client_run_id == client_id)
            .cloned())
    }

    async fn interrupt_stale(&self) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        for run in state.runs.values_mut() {
            if !run.status.terminal() {
                run.status = Status::Interrupted;
                run.error = "engine restarted".to_string();
                run.updated_at = crate::event::unix_now();
            }
        }
        Ok(())
    }

    async fn append_events(&self, events: &[Event]) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        for event in events {
            let list = state.events.entry(event.run_id.clone()).or_default();
            if let Some(existing) = list.iter_mut().find(|seen| seen.seq == event.seq) {
                *existing = event.clone();
            } else {
                list.push(event.clone());
                list.sort_by_key(|seen| seen.seq);
            }
        }
        Ok(())
    }

    async fn events(&self, run_id: &str, after: i64) -> Result<Vec<Event>> {
        Ok(self
            .state
            .lock()
            .unwrap()
            .events
            .get(run_id)
            .map(|list| {
                list.iter()
                    .filter(|event| event.seq > after)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn the_memory_store_round_trips_and_orders_events() {
        let store = MemoryStore::new();
        let run = Run {
            id: "run_1".to_string(),
            session_id: "s".to_string(),
            status: Status::Running,
            ..Run::default()
        };
        store.insert(&run).await.expect("insert");
        assert!(store.get("run_1").await.expect("get").is_some());

        let mut second = Event {
            run_id: "run_1".to_string(),
            seq: 2,
            kind: "status".to_string(),
            at: 2,
            data: Value::Null,
        };
        let first = Event {
            run_id: "run_1".to_string(),
            seq: 1,
            kind: "user".to_string(),
            at: 1,
            data: Value::Null,
        };
        store
            .append_events(&[second.clone(), first])
            .await
            .expect("append");
        let events = store.events("run_1", 0).await.expect("events");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].seq, 1);
        second.seq = 3;
        store.append_events(&[second]).await.expect("append");
        assert_eq!(store.events("run_1", 2).await.expect("events").len(), 1);
    }

    #[tokio::test]
    async fn stale_runs_are_interrupted() {
        let store = MemoryStore::new();
        store
            .insert(&Run {
                id: "a".to_string(),
                status: Status::Running,
                ..Run::default()
            })
            .await
            .expect("insert");
        store.interrupt_stale().await.expect("interrupt");
        let run = store.get("a").await.expect("get").expect("run");
        assert_eq!(run.status, Status::Interrupted);
        assert_eq!(run.error, "engine restarted");
    }
}
