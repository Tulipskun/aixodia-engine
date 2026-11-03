//! Local rotating SQLite log store.
//!
//! Port of the engine's Go `d1log.go`. Logs live on the engine machine in a
//! local SQLite database (`~/.aixodia/aixodia-logs.db`), not in D1: D1 is only
//! used for daemon discovery (the tunnel URL), which the phone must read
//! remotely.
//!
//! Writes are funnelled through a bounded buffer to one writer thread, exactly
//! as the Go engine funnels them through `logCh` to `logWriter`, so a slow disk
//! never blocks the caller and a full buffer drops the line instead.
//!
//! The store can be used two ways: [`LogStore::open`] builds an isolated handle
//! (tests, one-off tools), and [`start`] installs the process-wide handle the
//! free functions write to.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde_json::{Map, Value};
use tokio::sync::mpsc;
use tokio::sync::mpsc::Sender;

/// Rows kept in one table before the oldest is rotated aside.
pub const MAX_ENGINE_LOG_ROWS: i64 = 5000;

/// Depth of the write buffer. A full buffer drops the line, as in Go.
const LOG_BUFFER: usize = 2048;

/// One line handed to the writer.
struct LogEntry {
    level: String,
    msg: String,
}

/// The local log store. Cheap to clone: every clone shares one connection and
/// one write buffer, like the Go package globals.
#[derive(Clone)]
pub struct LogStore {
    conn: Arc<Mutex<Connection>>,
    tx: Sender<LogEntry>,
}

impl LogStore {
    /// Opens the log database at `path`, creating its directory and table.
    pub fn open<P: AsRef<Path>>(path: P) -> anyhow::Result<LogStore> {
        let path = path.as_ref();
        if let Some(dir) = path.parent() {
            if !dir.as_os_str().is_empty() {
                std::fs::create_dir_all(dir)?;
            }
        }
        let conn = Connection::open(path)?;
        create_table(&conn);
        let conn = Arc::new(Mutex::new(conn));

        let (tx, rx) = mpsc::channel::<LogEntry>(LOG_BUFFER);
        let writer_conn = Arc::clone(&conn);
        std::thread::Builder::new()
            .name("aixodia-logs".to_string())
            .spawn(move || log_writer(writer_conn, rx))?;

        Ok(LogStore { conn, tx })
    }

    /// Queues one line. A full buffer drops it rather than blocking, matching
    /// the Go `select`/`default` send.
    pub fn push(&self, level: &str, msg: String) {
        let _ = self.tx.try_send(LogEntry {
            level: level.to_string(),
            msg,
        });
    }

    /// Reads the newest `limit` lines, optionally restricted to one level.
    /// Returns an empty vector when the query fails, as the Go port does.
    pub fn recent(&self, limit: i64, level: &str) -> Vec<Map<String, Value>> {
        let Ok(conn) = self.conn.lock() else {
            return Vec::new();
        };

        let mut tables = vec!["engine_logs".to_string()];
        for i in 1..=3 {
            let table = format!("engine_logs_{i}");
            if table_exists(&conn, &table) {
                tables.push(table);
            }
        }

        let mut query = String::new();
        for (i, table) in tables.iter().enumerate() {
            if i > 0 {
                query.push_str(" UNION ALL ");
            }
            query.push_str(&format!(
                "SELECT id, level, message, at, '{table}' AS tbl FROM {table}"
            ));
        }
        if !level.is_empty() {
            query = format!("SELECT * FROM ({query}) WHERE level = ?");
        }
        query.push_str(&format!(" ORDER BY at DESC LIMIT {limit}"));

        let mut stmt = match conn.prepare(&query) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let columns: Vec<String> = stmt
            .column_names()
            .iter()
            .map(|c| (*c).to_string())
            .collect();
        let mut rows = match if level.is_empty() {
            stmt.query([])
        } else {
            stmt.query([level])
        } {
            Ok(rows) => rows,
            Err(_) => return Vec::new(),
        };

        let mut out = Vec::new();
        while let Ok(Some(row)) = rows.next() {
            let mut entry = Map::new();
            for (i, column) in columns.iter().enumerate() {
                let value = match row.get_ref(i) {
                    Ok(value) => json_value(value),
                    Err(_) => Value::Null,
                };
                entry.insert(column.clone(), value);
            }
            out.push(entry);
        }
        out
    }
}

/// Writes to stdout and queues every non-empty line for the log store, which is
/// how the Go engine tees its `log` output into the database.
pub struct TeeWriter {
    store: LogStore,
}

impl TeeWriter {
    /// Wraps a store so that everything written to the returned writer also
    /// lands in it.
    pub fn new(store: LogStore) -> Self {
        Self { store }
    }
}

impl std::io::Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = std::io::stdout().write(buf)?;
        let msg = String::from_utf8_lossy(buf).trim().to_string();
        if !msg.is_empty() {
            self.store.push("info", msg);
        }
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().flush()
    }
}

/// Drains the write buffer, rotating the table when it is full.
fn log_writer(conn: Arc<Mutex<Connection>>, mut rx: mpsc::Receiver<LogEntry>) {
    while let Some(entry) = rx.blocking_recv() {
        let Ok(conn) = conn.lock() else {
            return;
        };
        if let Ok(count) = count_rows(&conn, "engine_logs") {
            if count >= MAX_ENGINE_LOG_ROWS {
                rotate(&conn);
            }
        }
        let _ = conn.execute(
            "INSERT INTO engine_logs (level, message, at) VALUES (?, ?, unixepoch())",
            rusqlite::params![entry.level, entry.msg],
        );
    }
}

/// Counts the rows of one of the fixed log tables. The names come from a fixed
/// list, never from a caller.
fn count_rows(conn: &Connection, table: &str) -> rusqlite::Result<i64> {
    let mut stmt = conn.prepare(&format!("SELECT COUNT(*) AS c FROM {table}"))?;
    stmt.query_row([], |row| row.get(0))
}

/// Whether a table exists.
fn table_exists(conn: &Connection, name: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?",
        [name],
        |_| Ok(()),
    )
    .is_ok()
}

/// Creates the table and its index when they are missing.
fn create_table(conn: &Connection) {
    let _ = conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS engine_logs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            level TEXT NOT NULL DEFAULT 'info',
            message TEXT NOT NULL DEFAULT '',
            at INTEGER NOT NULL DEFAULT (unixepoch())
        );
        CREATE INDEX IF NOT EXISTS idx_engine_logs_at ON engine_logs(at);",
    );
}

/// Shifts the tables up one slot and starts a fresh one, keeping three
/// generations of history.
fn rotate(conn: &Connection) {
    if table_exists(conn, "engine_logs_3") {
        let _ = conn.execute("DROP TABLE engine_logs_3", []);
    }
    if table_exists(conn, "engine_logs_2") {
        let _ = conn.execute("ALTER TABLE engine_logs_2 RENAME TO engine_logs_3", []);
    }
    if table_exists(conn, "engine_logs_1") {
        let _ = conn.execute("ALTER TABLE engine_logs_1 RENAME TO engine_logs_2", []);
    }
    if table_exists(conn, "engine_logs") {
        let _ = conn.execute("ALTER TABLE engine_logs RENAME TO engine_logs_1", []);
    }
    create_table(conn);
}

/// Converts one SQLite value into the dynamic JSON value D1 rows use.
fn json_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::from(i),
        ValueRef::Real(f) => Value::from(f),
        ValueRef::Text(t) => Value::from(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::from(String::from_utf8_lossy(b).into_owned()),
    }
}

// --- process-wide store --------------------------------------------------

static GLOBAL: OnceLock<LogStore> = OnceLock::new();

/// The default database path: `~/.aixodia/aixodia-logs.db`, or the working
/// directory when there is no home.
pub fn default_path() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => {
            PathBuf::from(home).join(".aixodia").join("aixodia-logs.db")
        }
        _ => PathBuf::from("./aixodia-logs.db"),
    }
}

/// Starts the process-wide store once, as the Go `sync.Once` does.
pub fn start() -> anyhow::Result<&'static LogStore> {
    if let Some(store) = GLOBAL.get() {
        return Ok(store);
    }
    let store = LogStore::open(default_path())?;
    // Two callers may race here; `set` keeps whichever store lands first and
    // drops the loser, which only closes a file handle nobody reads from.
    let _ = GLOBAL.set(store);
    Ok(GLOBAL.get().expect("the store was just installed"))
}

/// Queues one line on the process-wide store; a no-op before [`start`].
pub fn log(level: &str, msg: String) {
    if let Some(store) = GLOBAL.get() {
        store.push(level, msg);
    }
}

/// Queues an informational line on the process-wide store.
pub fn info(msg: impl Into<String>) {
    log("info", msg.into());
}

/// Queues a warning line on the process-wide store.
pub fn warn(msg: impl Into<String>) {
    log("warn", msg.into());
}

/// Queues an error line on the process-wide store.
pub fn error(msg: impl Into<String>) {
    log("error", msg.into());
}

/// Reads the newest `limit` lines from the process-wide store.
pub fn recent(limit: i64, level: &str) -> Vec<Map<String, Value>> {
    match GLOBAL.get() {
        Some(store) => store.recent(limit, level),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Writes straight to the database, bypassing the writer thread, so a test
    /// can set up a table state without waiting for the buffer.
    impl LogStore {
        fn insert_at(&self, level: &str, msg: String, at: i64) {
            let conn = self.conn.lock().unwrap();
            let _ = conn.execute(
                "INSERT INTO engine_logs (level, message, at) VALUES (?, ?, ?)",
                rusqlite::params![level, msg, at],
            );
        }

        fn count(&self, table: &str) -> i64 {
            let conn = self.conn.lock().unwrap();
            count_rows(&conn, table).unwrap_or(0)
        }

        fn table_exists(&self, name: &str) -> bool {
            let conn = self.conn.lock().unwrap();
            table_exists(&conn, name)
        }
    }

    fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("condition was not met within {timeout:?}");
    }

    fn open() -> (tempfile::TempDir, LogStore) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = LogStore::open(dir.path().join("aixodia-logs.db")).expect("open");
        (dir, store)
    }

    #[test]
    fn recent_logs_reads_back_what_was_written() {
        let (_dir, store) = open();
        store.insert_at("info", "old".to_string(), 100);
        store.insert_at("error", "new".to_string(), 200);

        let rows = store.recent(10, "");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["message"], Value::from("new"));
        assert_eq!(rows[0]["level"], Value::from("error"));
        assert_eq!(rows[1]["message"], Value::from("old"));
    }

    #[test]
    fn recent_logs_filters_by_level() {
        let (_dir, store) = open();
        store.insert_at("info", "quiet".to_string(), 100);
        store.insert_at("error", "loud".to_string(), 200);

        let rows = store.recent(10, "error");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["message"], Value::from("loud"));
    }

    #[test]
    fn recent_logs_honours_the_limit() {
        let (_dir, store) = open();
        for at in 1..=5 {
            store.insert_at("info", format!("row {at}"), at);
        }

        let rows = store.recent(2, "");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["message"], Value::from("row 5"));
    }

    #[test]
    fn the_writer_thread_persists_queued_lines() {
        let (_dir, store) = open();
        store.push("warn", "queued".to_string());
        wait_until(|| store.recent(10, "").len() == 1, Duration::from_secs(5));

        let rows = store.recent(10, "warn");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["message"], Value::from("queued"));
        assert_eq!(rows[0]["level"], Value::from("warn"));
    }

    #[test]
    fn rotation_moves_a_full_table_aside() {
        let (_dir, store) = open();
        for i in 0..MAX_ENGINE_LOG_ROWS {
            store.insert_at("info", format!("row {i}"), i + 1);
        }
        store.push("info", "overflow".to_string());

        // `recent` unions every table, so what says the rotation happened is the
        // live table's own row count.
        wait_until(|| store.count("engine_logs") == 1, Duration::from_secs(5));
        assert_eq!(store.count("engine_logs"), 1);
        assert_eq!(store.count("engine_logs_1"), MAX_ENGINE_LOG_ROWS);
        assert!(store.table_exists("engine_logs_1"));
        assert!(!store.table_exists("engine_logs_2"));
    }

    #[test]
    fn the_default_path_lives_under_home() {
        let path = default_path();
        assert!(path.ends_with(".aixodia/aixodia-logs.db") || path.ends_with("aixodia-logs.db"));
    }
}
