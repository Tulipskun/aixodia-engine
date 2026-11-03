//! The D1 chat store.
//!
//! Port of the Go `session/d1.go`: sessions and their turns live in Cloudflare
//! D1, so a fresh engine — and the phone — find the same history. There is no
//! local database in the engine.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use aixodia_db::{Client, Row};
use aixodia_provider::Message;
use serde_json::{Map, Value};
use tokio::sync::Mutex;

use crate::trail::messages_from_turns;
use crate::types::{
    bool_int, new_id, stored_turn, AgentPatch, NewSession, Session, SessionUsage, Turn, TurnInput,
    UnfinishedTurn,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("session not found: {0:?}")]
    NotFound(String),
    #[error(transparent)]
    Db(#[from] aixodia_db::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// What one append wrote, and why it stopped.
///
/// Go's `AppendTurns` returns both: a chunk that fails leaves the rows before it
/// on record, and the gateway's trail writer marks only what was written so the
/// next round retries the suffix instead of duplicating it. A `Result` cannot
/// carry both, so this does.
#[derive(Debug, Default)]
pub struct Appended {
    pub rows: Vec<Turn>,
    pub error: Option<Error>,
}

/// D1 is the chat store.
pub struct D1 {
    db: Client,
    /// Appends serialise within the process: `next_seq` is a read followed by an
    /// insert, and two answers for one chat that overlapped could pick the same
    /// seq.
    append_mu: Mutex<()>,
}

impl D1 {
    pub fn new(db: Client) -> Self {
        Self {
            db,
            append_mu: Mutex::new(()),
        }
    }

    pub fn db(&self) -> &Client {
        &self.db
    }

    pub async fn create(&self, params: NewSession) -> Result<Session> {
        let now = now_unix();
        let id = if params.id.trim().is_empty() {
            new_id()
        } else {
            params.id.trim().to_string()
        };
        let mut session = Session {
            id: id.clone(),
            title: params.title,
            provider: params.provider,
            model: params.model,
            reasoning: params.reasoning,
            temperature: params.temperature,
            top_p: params.top_p,
            max_tokens: params.max_tokens,
            created_at: now,
            updated_at: now,
            usage: Some(SessionUsage::default()),
        };

        // The sub_provider/sub_model/sub_enabled columns still exist in the
        // external D1 schema. Write inert values so old rows and readers keep
        // working.
        self.db
            .query(
                "INSERT INTO chat_sessions (id,title,provider,model,sub_provider,sub_model,sub_enabled,reasoning,temperature,top_p,max_tokens,created_at,updated_at)
                 VALUES (?,?,?,?,'','',0,?,?,?,?,?,?)",
                &[
                    Value::from(session.id.clone()),
                    Value::from(session.title.clone()),
                    Value::from(session.provider.clone()),
                    Value::from(session.model.clone()),
                    Value::from(bool_int(session.reasoning)),
                    json_float(session.temperature),
                    json_float(session.top_p),
                    Value::from(session.max_tokens),
                    Value::from(session.created_at),
                    Value::from(session.updated_at),
                ],
            )
            .await?;
        session.usage = Some(SessionUsage::default());
        Ok(session)
    }

    pub async fn get(&self, id: &str) -> Result<Session> {
        let rows = self
            .db
            .query(
                &format!("{SESSION_SELECT} WHERE s.id=? LIMIT 1"),
                &[Value::from(id)],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Err(Error::NotFound(id.to_string()));
        };
        Ok(session_from_row(row))
    }

    pub async fn list(&self) -> Result<Vec<Session>> {
        // `updated_at` is whole seconds, so many chats can share a timestamp;
        // the id tiebreaker keeps identical reads in the same order.
        let rows = self
            .db
            .query(
                &format!("{SESSION_SELECT} ORDER BY s.updated_at DESC, s.id DESC LIMIT 200"),
                &[],
            )
            .await?;
        Ok(rows.iter().map(session_from_row).collect())
    }

    pub async fn delete(&self, id: &str) -> Result<()> {
        self.db
            .query(
                "DELETE FROM chat_turns WHERE session_id=?",
                &[Value::from(id)],
            )
            .await?;
        self.db
            .query("DELETE FROM chat_sessions WHERE id=?", &[Value::from(id)])
            .await?;
        Ok(())
    }

    pub async fn save_config(&self, id: &str, session: &Session) -> Result<()> {
        let rows = self
            .db
            .query(
                "UPDATE chat_sessions SET provider=?, model=?, title=CASE WHEN ? != '' THEN ? ELSE title END, updated_at=? WHERE id=? RETURNING id",
                &[
                    Value::from(session.provider.clone()),
                    Value::from(session.model.clone()),
                    Value::from(session.title.clone()),
                    Value::from(session.title.clone()),
                    Value::from(now_unix()),
                    Value::from(id),
                ],
            )
            .await?;
        if rows.is_empty() {
            return Err(Error::NotFound(id.to_string()));
        }
        Ok(())
    }

    /// Writes a chat's pinned agent choice and moves `updated_at` with it. The
    /// title and the model the chat itself runs on are left alone.
    pub async fn save_agent(&self, id: &str, patch: &AgentPatch) -> Result<()> {
        let mut columns: Vec<&str> = Vec::new();
        let mut values: Vec<Value> = Vec::new();
        if let Some(provider) = &patch.provider {
            columns.push("provider=?");
            values.push(Value::from(provider.clone()));
        }
        if let Some(model) = &patch.model {
            columns.push("model=?");
            values.push(Value::from(model.clone()));
        }
        if columns.is_empty() {
            return Ok(());
        }
        columns.push("updated_at=?");
        values.push(Value::from(now_unix()));
        values.push(Value::from(id));

        // D1's HTTP API reports no affected-row count, so existence is checked
        // first: a patch to a chat that is gone must read as 404.
        self.get(id).await?;
        self.db
            .query(
                &format!("UPDATE chat_sessions SET {} WHERE id=?", columns.join(", ")),
                &values,
            )
            .await?;
        Ok(())
    }

    pub async fn turns(&self, id: &str) -> Result<Vec<Turn>> {
        let rows = self
            .db
            .query(
                &format!(
                    "SELECT {TURN_COLUMNS} FROM chat_turns WHERE session_id=? ORDER BY seq ASC"
                ),
                &[Value::from(id)],
            )
            .await?;
        Ok(rows.iter().map(turn_from_row).collect())
    }

    /// The newest page ending just before `before_seq`, returned oldest-first
    /// like [`turns`](Self::turns). `before_seq <= 0` means the newest page.
    pub async fn turns_page(&self, id: &str, before_seq: i64, limit: i64) -> Result<Vec<Turn>> {
        if limit <= 0 {
            return self.turns(id).await;
        }
        let rows = if before_seq > 0 {
            self.db
                .query(
                    &format!("SELECT {TURN_COLUMNS} FROM chat_turns WHERE session_id=? AND seq<? ORDER BY seq DESC LIMIT ?"),
                    &[Value::from(id), Value::from(before_seq), Value::from(limit)],
                )
                .await?
        } else {
            self.db
                .query(
                    &format!("SELECT {TURN_COLUMNS} FROM chat_turns WHERE session_id=? ORDER BY seq DESC LIMIT ?"),
                    &[Value::from(id), Value::from(limit)],
                )
                .await?
        };
        // SQL walked backwards so LIMIT took the page nearest `before_seq`;
        // callers read a transcript oldest-first.
        Ok(rows.iter().rev().map(turn_from_row).collect())
    }

    pub async fn history(&self, id: &str) -> Result<Vec<Message>> {
        Ok(messages_from_turns(&self.turns(id).await?))
    }

    /// Writes the turns of one round. It retries only the unwritten suffix:
    /// replaying a successful chunk used to duplicate it when a later insert
    /// failed.
    pub async fn append_turns(&self, id: &str, turns: &[TurnInput]) -> Result<Vec<Turn>> {
        let appended = self.append_turns_partial(id, turns).await;
        match appended.error {
            Some(err) => Err(err),
            None => Ok(appended.rows),
        }
    }

    /// [`D1::append_turns`], reporting a partial write.
    ///
    /// Go's `AppendTurns` returns the rows it managed to store *and* the error
    /// that stopped it, and the gateway's trail writer needs both: it marks only
    /// what was actually written so the next round retries the suffix instead of
    /// duplicating it. A `Result` cannot carry both, so this does.
    pub async fn append_turns_partial(&self, id: &str, turns: &[TurnInput]) -> Appended {
        if turns.is_empty() {
            return Appended::default();
        }
        let _guard = self.append_mu.lock().await;

        let mut saved: Vec<Turn> = Vec::new();
        let mut last_err: Option<aixodia_db::Error> = None;

        // The starting seq is chosen once, not per attempt. D1 can commit an
        // insert and still lose the HTTP answer, and a fresh seq would store the
        // same rows twice.
        let seq = match self.next_seq(id).await {
            Ok(seq) => seq,
            Err(err) => {
                return Appended {
                    rows: Vec::new(),
                    error: Some(err),
                }
            }
        };

        for _attempt in 0..3 {
            if saved.len() >= turns.len() {
                break;
            }
            last_err = None;
            let now = now_unix();
            let mut start = saved.len();
            while start < turns.len() {
                let end = (start + MAX_TURNS_PER_STATEMENT).min(turns.len());
                let chunk = &turns[start..end];

                let mut placeholders = Vec::with_capacity(chunk.len());
                let mut values: Vec<Value> = Vec::with_capacity(chunk.len() * 16);
                for (i, turn) in chunk.iter().enumerate() {
                    placeholders.push("(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)");
                    let position = seq + start as i64 + i as i64;
                    values.extend([
                        Value::from(id),
                        Value::from(position),
                        Value::from(turn.role.clone()),
                        Value::from(turn.text.clone()),
                        Value::from(turn.model.clone()),
                        Value::from(turn.input_tokens),
                        Value::from(turn.output_tokens),
                        Value::from(turn.cache_read_tokens),
                        Value::from(turn.cache_write_tokens),
                        Value::from(turn.reasoning_tokens),
                        Value::from(turn.reasoning_content.clone()),
                        Value::from(bool_int(turn.input_includes_cache)),
                        Value::from(turn.duration_ms),
                        Value::from(turn.generation_ms),
                        Value::from(turn.block.clone()),
                        Value::from(now),
                    ]);
                }

                let statement = format!(
                    "INSERT INTO chat_turns (session_id, seq, role, text, model, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, reasoning_content, input_includes_cache, duration_ms, generation_ms, block, created_at) VALUES {} ON CONFLICT(session_id, seq) DO NOTHING",
                    placeholders.join(", ")
                );
                match self.db.query(&statement, &values).await {
                    Ok(_) => {
                        for (i, turn) in chunk.iter().enumerate() {
                            saved.push(stored_turn(id, seq + start as i64 + i as i64, now, turn));
                        }
                        start = end;
                    }
                    Err(err) => {
                        last_err = Some(err);
                        break;
                    }
                }
            }

            if last_err.is_some() {
                continue;
            }
            // The rows are committed even if updating the timestamp fails.
            if let Err(err) = self.touch(id).await {
                tracing::warn!(error = %err, "session: touch after append");
            }
            // Read the rows back so the caller gets the D1 row ids: the phone
            // keys rows by them and only the database can assign them.
            return match self.turns_between(id, seq, seq + turns.len() as i64).await {
                Ok(stored) => Appended {
                    rows: stored,
                    error: None,
                },
                Err(err) => Appended {
                    rows: saved,
                    error: Some(err),
                },
            };
        }

        // Every attempt reported an error, but D1 can commit an insert and lose
        // every HTTP answer. Look at the seq range once more: if all the rows are
        // there, the write landed.
        if let Ok(stored) = self.turns_between(id, seq, seq + turns.len() as i64).await {
            if stored.len() == turns.len() {
                if let Err(err) = self.touch(id).await {
                    tracing::warn!(error = %err, "session: touch after append");
                }
                return Appended {
                    rows: stored,
                    error: None,
                };
            }
        }
        Appended {
            rows: saved,
            error: last_err.map(Error::Db),
        }
    }

    /// Reads the rows of one append in seq order (upper bound exclusive).
    async fn turns_between(&self, id: &str, from: i64, to: i64) -> Result<Vec<Turn>> {
        let rows = self
            .db
            .query(
                &format!("SELECT {TURN_COLUMNS} FROM chat_turns WHERE session_id=? AND seq>=? AND seq<? ORDER BY seq ASC"),
                &[Value::from(id), Value::from(from), Value::from(to)],
            )
            .await?;
        Ok(rows.iter().map(turn_from_row).collect())
    }

    async fn next_seq(&self, id: &str) -> Result<i64> {
        let rows = self
            .db
            .query(
                "SELECT COALESCE(MAX(seq), 0) AS seq FROM chat_turns WHERE session_id=?",
                &[Value::from(id)],
            )
            .await?;
        let Some(row) = rows.first() else {
            return Ok(1);
        };
        Ok(int_field(row, "seq") + 1)
    }

    async fn touch(&self, id: &str) -> Result<()> {
        self.db
            .query(
                "UPDATE chat_sessions SET updated_at=? WHERE id=?",
                &[Value::from(now_unix()), Value::from(id)],
            )
            .await?;
        Ok(())
    }

    /// The questions with nothing after them, newest first. Only user and
    /// assistant rows are considered: a turn that ended mid tool call has its
    /// question answered in the next round.
    pub async fn unfinished(&self, limit: i64) -> Result<Vec<UnfinishedTurn>> {
        let limit = if limit <= 0 { 20 } else { limit };
        let rows = self
            .db
            .query(
                &format!(
                    "SELECT t.session_id, t.seq, t.text, t.created_at
                       FROM chat_turns t
                       JOIN (SELECT session_id, MAX(seq) AS seq FROM chat_turns
                              WHERE role IN ('user','assistant') GROUP BY session_id) last
                         ON last.session_id = t.session_id AND last.seq = t.seq
                      WHERE t.role = 'user'
                      ORDER BY t.created_at DESC
                      LIMIT {limit}"
                ),
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|row| UnfinishedTurn {
                session_id: string_field(row, "session_id"),
                seq: int_field(row, "seq"),
                text: truncate_text(&string_field(row, "text"), 200),
                created_at: int64_field(row, "created_at"),
            })
            .collect())
    }

    /// Has the columns the current engine writes. The D1 tables predate some of
    /// them, and adding a column is cheap and idempotent.
    pub async fn ensure_turns(&self) {
        for statement in [
            "ALTER TABLE chat_turns ADD COLUMN generation_ms INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE chat_turns ADD COLUMN reasoning_content TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE chat_turns ADD COLUMN block TEXT NOT NULL DEFAULT ''",
        ] {
            if let Err(err) = self.db.query(statement, &[]).await {
                let message = err.to_string();
                if !message.contains("duplicate column") {
                    tracing::warn!(error = %err, "session: schema");
                }
            }
        }
    }
}

/// Keeps each INSERT inside D1's per-statement bound-parameter limit (100
/// parameters; 16 columns => 6 rows is 96), so appending N turns costs O(N/6)
/// HTTP round-trips instead of N.
const MAX_TURNS_PER_STATEMENT: usize = 6;

pub const TURN_COLUMNS: &str = "id,session_id,seq,role,agent,job_id,text,created_at,model,input_tokens,output_tokens,duration_ms,cache_read_tokens,cache_write_tokens,reasoning_tokens,input_includes_cache,generation_ms,reasoning_content,block";

/// Returns a chat and its whole cost, summed over the stored turns in one query.
///
/// `input_includes_cache` is a per-provider convention, not a display choice:
/// when true the prompt already contains the cache read, so adding cache on top
/// would double the prompt.
pub const SESSION_SELECT: &str = "
SELECT s.id,s.title,s.provider,s.model,s.reasoning,s.temperature,s.top_p,s.max_tokens,s.created_at,s.updated_at,
       COALESCE(u.prompt_tokens,0) AS usage_prompt_tokens,
       COALESCE(u.cache_read,0) AS usage_cache_read,
       COALESCE(u.fresh_input,0) AS usage_fresh_input,
       COALESCE(u.output_tokens,0) AS usage_output_tokens,
       COALESCE(u.total_ms,0) AS usage_total_ms,
       COALESCE((
           SELECT t.duration_ms FROM chat_turns t
           WHERE t.session_id=s.id AND t.role != 'user' AND t.role NOT LIKE 'tool%'
                 AND t.duration_ms > 0
           ORDER BY t.seq DESC LIMIT 1
       ),0) AS usage_last_turn_ms
FROM chat_sessions s
LEFT JOIN (
    SELECT session_id,
           SUM(CASE WHEN input_includes_cache = 1 THEN input_tokens
                    ELSE input_tokens + cache_read_tokens + cache_write_tokens END) AS prompt_tokens,
           SUM(cache_read_tokens) AS cache_read,
           SUM(CASE WHEN input_includes_cache = 1
                    THEN MAX(input_tokens - cache_read_tokens, 0)
                    ELSE input_tokens + cache_write_tokens END) AS fresh_input,
           SUM(output_tokens) AS output_tokens,
           SUM(duration_ms) AS total_ms
    FROM chat_turns
    WHERE role != 'user' AND role NOT LIKE 'tool%'
    GROUP BY session_id
) u ON u.session_id = s.id";

pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn json_float(value: Option<f64>) -> Value {
    match value {
        Some(number) => Value::from(number),
        None => Value::Null,
    }
}

/// Shortens a question for a handover record, which only has to be recognisable.
pub fn truncate_text(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{}…", head.trim())
}

pub fn session_from_row(row: &Row) -> Session {
    Session {
        id: string_field(row, "id"),
        title: string_field(row, "title"),
        provider: string_field(row, "provider"),
        model: string_field(row, "model"),
        reasoning: bool_field(row, "reasoning"),
        temperature: float_field(row, "temperature"),
        top_p: float_field(row, "top_p"),
        max_tokens: int_field(row, "max_tokens"),
        created_at: int64_field(row, "created_at"),
        updated_at: int64_field(row, "updated_at"),
        usage: Some(SessionUsage {
            prompt_tokens: int_field(row, "usage_prompt_tokens"),
            cache_read: int_field(row, "usage_cache_read"),
            fresh_input: int_field(row, "usage_fresh_input"),
            output_tokens: int_field(row, "usage_output_tokens"),
            total_ms: int64_field(row, "usage_total_ms"),
            last_turn_ms: int64_field(row, "usage_last_turn_ms"),
        }),
    }
}

pub fn turn_from_row(row: &Row) -> Turn {
    Turn {
        session_id: string_field(row, "session_id"),
        id: int64_field(row, "id"),
        seq: int_field(row, "seq"),
        role: string_field(row, "role"),
        agent: string_field(row, "agent"),
        job_id: string_field(row, "job_id"),
        text: string_field(row, "text"),
        model: string_field(row, "model"),
        input_tokens: int_field(row, "input_tokens"),
        output_tokens: int_field(row, "output_tokens"),
        cache_read_tokens: int_field(row, "cache_read_tokens"),
        cache_write_tokens: int_field(row, "cache_write_tokens"),
        reasoning_tokens: int_field(row, "reasoning_tokens"),
        reasoning_content: string_field(row, "reasoning_content"),
        input_includes_cache: int_field(row, "input_includes_cache") != 0,
        duration_ms: int64_field(row, "duration_ms"),
        generation_ms: int64_field(row, "generation_ms"),
        // A database that predates the block column has no such key, which
        // reads as blank.
        block: string_field(row, "block"),
        created_at: int64_field(row, "created_at"),
    }
}

pub fn string_field(row: &Row, key: &str) -> String {
    match row.get(key) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}

pub fn int_field(row: &Row, key: &str) -> i64 {
    int64_field(row, key)
}

pub fn int64_field(row: &Row, key: &str) -> i64 {
    match row.get(key) {
        Some(Value::Number(number)) => number.as_i64().unwrap_or_else(|| {
            number
                .as_f64()
                .map(|float| float as i64)
                .unwrap_or_default()
        }),
        Some(Value::String(text)) => text.parse().unwrap_or_default(),
        Some(Value::Bool(value)) => i64::from(*value),
        _ => 0,
    }
}

pub fn float_field(row: &Row, key: &str) -> Option<f64> {
    match row.get(key) {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.parse().ok(),
        _ => None,
    }
}

pub fn bool_field(row: &Row, key: &str) -> bool {
    match row.get(key) {
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::Bool(value)) => *value,
        Some(Value::String(text)) => text == "1" || text == "true",
        _ => false,
    }
}

/// Builds a `Row` from key/value pairs, for tests.
pub fn row(pairs: &[(&str, Value)]) -> Row {
    let mut map = Map::new();
    for (key, value) in pairs {
        map.insert((*key).to_string(), value.clone());
    }
    map
}

/// An in-memory D1 double is not provided: `db.Verify` must reach Cloudflare
/// first in Go too, so the store is exercised live. The pure helpers
/// (`turn_from_row`, `anchor`, `compact`, `messages_from_turns`) carry the
/// offline tests.
pub fn empty_rows() -> Vec<Row> {
    let _ = HashMap::<String, String>::new();
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_readers_match_d1_number_conventions() {
        let row = row(&[
            ("seq", Value::from(3)),
            ("session_id", Value::from("abc")),
            ("input_includes_cache", Value::from(1)),
            ("temperature", Value::from(0.7)),
            ("max_tokens", Value::Null),
            ("block", Value::Null),
        ]);
        let turn = turn_from_row(&row);
        assert_eq!(turn.seq, 3);
        assert_eq!(turn.session_id, "abc");
        assert!(turn.input_includes_cache);
        assert_eq!(turn.block, "");
        assert_eq!(float_field(&row, "temperature"), Some(0.7));
        assert_eq!(int_field(&row, "max_tokens"), 0);
        assert_eq!(float_field(&row, "missing"), None);
    }

    #[test]
    fn session_rows_sum_usage() {
        let row = row(&[
            ("id", Value::from("s1")),
            ("title", Value::from("hi")),
            ("reasoning", Value::from(1)),
            ("usage_prompt_tokens", Value::from(10)),
            ("usage_last_turn_ms", Value::from(250)),
        ]);
        let session = session_from_row(&row);
        assert!(session.reasoning);
        let usage = session.usage.expect("usage");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.last_turn_ms, 250);
    }

    #[test]
    fn a_long_question_is_shortened_for_the_handover() {
        let text = "a".repeat(250);
        let short = truncate_text(&text, 200);
        assert!(short.ends_with('…'));
        assert_eq!(truncate_text("short", 200), "short");
    }
}
