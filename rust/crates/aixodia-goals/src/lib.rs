//! Durable goals and plans the model records with the objective tools.
//!
//! Port of the Go `goals` package. Goals and plans are separate lists: a goal is
//! the outcome the user asked for, a plan is a step toward it. Both live in D1
//! beside chat history, scoped to the chat session that created them.

use std::time::{SystemTime, UNIX_EPOCH};

use aixodia_db::Client;
use async_trait::async_trait;
use rand::RngCore;
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("objective not found: {0:?}")]
    NotFound(String),
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Db(#[from] aixodia_db::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Separates the two lists. They use the same storage and tool shape but never
/// appear in each other's list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Goal,
    Plan,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Goal => "goal",
            Kind::Plan => "plan",
        }
    }
}

pub const STATUS_OPEN: &str = "open";
pub const STATUS_DONE: &str = "done";
pub const STATUS_DROPPED: &str = "dropped";

/// One goal or plan, in any status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub kind: String,
    pub content: String,
    /// `open`, `done` or `dropped`.
    pub status: String,
}

/// What the tools and the reminder builder need.
#[async_trait]
pub trait Store: Send + Sync {
    async fn add(&self, session_id: &str, kind: Kind, content: &str) -> Result<String>;
    async fn edit(&self, session_id: &str, kind: Kind, id: &str, content: &str) -> Result<()>;
    async fn complete(&self, session_id: &str, kind: Kind, id: &str) -> Result<()>;
    async fn drop_item(&self, session_id: &str, kind: Kind, id: &str) -> Result<()>;
    /// The goals and plans of one chat that are still active, oldest first.
    async fn open(&self, session_id: &str) -> Result<Vec<Item>>;
    /// Every goal and plan of one chat in any status, oldest first.
    async fn all(&self, session_id: &str) -> Result<Vec<Item>>;
}

/// The goal and plan store.
pub struct D1 {
    db: Client,
}

impl D1 {
    pub fn new(db: Client) -> Self {
        Self { db }
    }

    /// Creates the objectives table. It runs at startup; an existing table is
    /// left untouched.
    pub async fn ensure(&self) {
        for statement in [
            "CREATE TABLE IF NOT EXISTS objectives (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                content TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'open',
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )",
            "CREATE INDEX IF NOT EXISTS objectives_session ON objectives (session_id, kind, status)",
        ] {
            if let Err(err) = self.db.query(statement, &[]).await {
                tracing::warn!(error = %err, "goals: schema");
            }
        }
    }
}

#[async_trait]
impl Store for D1 {
    async fn add(&self, session_id: &str, kind: Kind, content: &str) -> Result<String> {
        let session_id = normalize_session(session_id)?;
        let content = content.trim();
        if content.is_empty() {
            return Err(Error::Message(format!(
                "{} content must not be empty",
                kind.as_str()
            )));
        }
        let id = new_id(kind);
        let now = now_unix();
        self.db
            .query(
                "INSERT INTO objectives (id, session_id, kind, content, status, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                &[
                    Value::from(id.clone()),
                    Value::from(session_id),
                    Value::from(kind.as_str()),
                    Value::from(content),
                    Value::from(STATUS_OPEN),
                    Value::from(now),
                    Value::from(now),
                ],
            )
            .await?;
        Ok(id)
    }

    async fn edit(&self, session_id: &str, kind: Kind, id: &str, content: &str) -> Result<()> {
        let session_id = normalize_session(session_id)?;
        let id = normalize_id(id, kind)?;
        let content = content.trim();
        if content.is_empty() {
            return Err(Error::Message(format!(
                "{} content must not be empty",
                kind.as_str()
            )));
        }
        let rows = self
            .db
            .query(
                "UPDATE objectives SET content=?, updated_at=?
                 WHERE id=? AND session_id=? AND kind=? AND status=? RETURNING id",
                &[
                    Value::from(content),
                    Value::from(now_unix()),
                    Value::from(id.clone()),
                    Value::from(session_id.clone()),
                    Value::from(kind.as_str()),
                    Value::from(STATUS_OPEN),
                ],
            )
            .await?;
        if rows.is_empty() {
            let status = self.status_of(&session_id, kind, &id).await?;
            return Err(Error::Message(format!(
                "{} {} is {}, not open",
                kind.as_str(),
                id,
                status
            )));
        }
        Ok(())
    }

    async fn complete(&self, session_id: &str, kind: Kind, id: &str) -> Result<()> {
        let session_id = normalize_session(session_id)?;
        let id = normalize_id(id, kind)?;
        let rows = self
            .db
            .query(
                "UPDATE objectives SET status=?, updated_at=?
                 WHERE id=? AND session_id=? AND kind=? AND status!=? RETURNING id",
                &[
                    Value::from(STATUS_DONE),
                    Value::from(now_unix()),
                    Value::from(id.clone()),
                    Value::from(session_id.clone()),
                    Value::from(kind.as_str()),
                    Value::from(STATUS_DROPPED),
                ],
            )
            .await?;
        if rows.is_empty() {
            let status = self.status_of(&session_id, kind, &id).await?;
            if status == STATUS_DROPPED {
                return Err(Error::Message(format!(
                    "{} {} was dropped and cannot be completed",
                    kind.as_str(),
                    id
                )));
            }
        }
        Ok(())
    }

    async fn drop_item(&self, session_id: &str, kind: Kind, id: &str) -> Result<()> {
        let session_id = normalize_session(session_id)?;
        let id = normalize_id(id, kind)?;
        let rows = self
            .db
            .query(
                "UPDATE objectives SET status=?, updated_at=?
                 WHERE id=? AND session_id=? AND kind=? RETURNING id",
                &[
                    Value::from(STATUS_DROPPED),
                    Value::from(now_unix()),
                    Value::from(id.clone()),
                    Value::from(session_id),
                    Value::from(kind.as_str()),
                ],
            )
            .await?;
        if rows.is_empty() {
            return Err(Error::NotFound(id));
        }
        Ok(())
    }

    async fn open(&self, session_id: &str) -> Result<Vec<Item>> {
        self.rows(session_id, Some(STATUS_OPEN)).await
    }

    async fn all(&self, session_id: &str) -> Result<Vec<Item>> {
        self.rows(session_id, None).await
    }
}

impl D1 {
    async fn rows(&self, session_id: &str, status: Option<&str>) -> Result<Vec<Item>> {
        let session_id = normalize_session(session_id)?;
        let (statement, params): (&str, Vec<Value>) = match status {
            Some(STATUS_OPEN) => (
                "SELECT id, kind, content, status FROM objectives
                 WHERE session_id=? AND status=? ORDER BY created_at ASC, id ASC",
                vec![Value::from(session_id), Value::from(STATUS_OPEN)],
            ),
            _ => (
                "SELECT id, kind, content, status FROM objectives
                 WHERE session_id=? ORDER BY created_at ASC, id ASC",
                vec![Value::from(session_id)],
            ),
        };
        let rows = self.db.query(statement, &params).await?;
        Ok(rows
            .iter()
            .map(|row| Item {
                id: match row.get("id") {
                    Some(Value::String(text)) => text.clone(),
                    _ => String::new(),
                },
                kind: match row.get("kind") {
                    Some(Value::String(text)) => text.clone(),
                    _ => String::new(),
                },
                content: match row.get("content") {
                    Some(Value::String(text)) => text.clone(),
                    _ => String::new(),
                },
                status: match row.get("status") {
                    Some(Value::String(text)) => text.clone(),
                    _ => STATUS_OPEN.to_string(),
                },
            })
            .collect())
    }
}

impl D1 {
    async fn status_of(&self, session_id: &str, kind: Kind, id: &str) -> Result<String> {
        let rows = self
            .db
            .query(
                "SELECT status FROM objectives WHERE id=? AND session_id=? AND kind=? LIMIT 1",
                &[
                    Value::from(id),
                    Value::from(session_id),
                    Value::from(kind.as_str()),
                ],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::NotFound(id.to_string()));
        };
        Ok(match row.get("status") {
            Some(Value::String(text)) => text.clone(),
            _ => String::new(),
        })
    }
}

fn normalize_session(session_id: &str) -> Result<String> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return Err(Error::Message(
            "goal and plan tools need an active chat session".to_string(),
        ));
    }
    Ok(session_id.to_string())
}

fn normalize_id(id: &str, kind: Kind) -> Result<String> {
    let id = id.trim();
    if id.is_empty() {
        return Err(Error::Message(format!(
            "{}_id must not be empty",
            kind.as_str()
        )));
    }
    Ok(id.to_string())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Short enough to quote in a conversation and wide enough (48 random bits) that
/// two items cannot reasonably collide. The prefix makes a goal id and a plan
/// id distinguishable at a glance.
fn new_id(kind: Kind) -> String {
    let prefix = match kind {
        Kind::Goal => "g-",
        Kind::Plan => "p-",
    };
    let mut bytes = [0u8; 6];
    if rand::thread_rng().try_fill_bytes(&mut bytes).is_err() {
        return format!("{prefix}{}", now_unix());
    }
    format!("{prefix}{}", hex::encode(bytes))
}

/// One stored row: id, session, kind, content, status.
type Row = (String, String, String, String, String);

/// An in-memory store, for tests.
#[derive(Default)]
pub struct MemoryStore {
    items: std::sync::Mutex<Vec<Row>>,
    next: std::sync::atomic::AtomicU64,
}

#[async_trait]
impl Store for MemoryStore {
    async fn add(&self, session_id: &str, kind: Kind, content: &str) -> Result<String> {
        let session_id = normalize_session(session_id)?;
        let content = content.trim();
        if content.is_empty() {
            return Err(Error::Message(format!(
                "{} content must not be empty",
                kind.as_str()
            )));
        }
        let number = self.next.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        let id = format!("{}-{number}", &kind.as_str()[..1]);
        self.items.lock().unwrap().push((
            id.clone(),
            session_id.to_string(),
            kind.as_str().to_string(),
            content.to_string(),
            STATUS_OPEN.to_string(),
        ));
        Ok(id)
    }

    async fn edit(&self, session_id: &str, kind: Kind, id: &str, content: &str) -> Result<()> {
        let content = content.trim();
        if content.is_empty() {
            return Err(Error::Message(format!(
                "{} content must not be empty",
                kind.as_str()
            )));
        }
        let mut items = self.items.lock().unwrap();
        for item in items.iter_mut() {
            if item.0 == id && item.1 == session_id && item.2 == kind.as_str() {
                item.3 = content.to_string();
                return Ok(());
            }
        }
        Err(Error::NotFound(id.to_string()))
    }

    async fn complete(&self, session_id: &str, kind: Kind, id: &str) -> Result<()> {
        let mut items = self.items.lock().unwrap();
        for item in items.iter_mut() {
            if item.0 == id && item.1 == session_id && item.2 == kind.as_str() {
                item.4 = STATUS_DONE.to_string();
                return Ok(());
            }
        }
        Err(Error::NotFound(id.to_string()))
    }

    async fn drop_item(&self, session_id: &str, kind: Kind, id: &str) -> Result<()> {
        let mut items = self.items.lock().unwrap();
        for item in items.iter_mut() {
            if item.0 == id && item.1 == session_id && item.2 == kind.as_str() {
                item.4 = STATUS_DROPPED.to_string();
                return Ok(());
            }
        }
        Err(Error::NotFound(id.to_string()))
    }

    async fn open(&self, session_id: &str) -> Result<Vec<Item>> {
        self.list(session_id, Some(STATUS_OPEN))
    }

    async fn all(&self, session_id: &str) -> Result<Vec<Item>> {
        self.list(session_id, None)
    }
}

impl MemoryStore {
    fn list(&self, session_id: &str, status: Option<&str>) -> Result<Vec<Item>> {
        Ok(self
            .items
            .lock()
            .unwrap()
            .iter()
            .filter(|item| {
                item.1 == session_id && status.map(|want| item.4 == want).unwrap_or(true)
            })
            .map(|item| Item {
                id: item.0.clone(),
                kind: item.2.clone(),
                content: item.3.clone(),
                status: item.4.clone(),
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ids_are_prefixed_by_kind() {
        assert!(new_id(Kind::Goal).starts_with("g-"));
        assert!(new_id(Kind::Plan).starts_with("p-"));
    }

    #[tokio::test]
    async fn an_empty_session_is_refused() {
        let store = MemoryStore::default();
        let err = store.add("  ", Kind::Goal, "x").await.unwrap_err();
        assert!(err.to_string().contains("active chat session"));
    }

    #[tokio::test]
    async fn the_memory_store_round_trips() {
        let store = MemoryStore::default();
        let id = store.add("s1", Kind::Plan, "step one").await.expect("add");
        store
            .edit("s1", Kind::Plan, &id, "step one!")
            .await
            .expect("edit");
        let items = store.open("s1").await.expect("open");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].content, "step one!");
        store.drop_item("s1", Kind::Plan, &id).await.expect("drop");
        assert!(store.open("s1").await.expect("open").is_empty());
    }
}
