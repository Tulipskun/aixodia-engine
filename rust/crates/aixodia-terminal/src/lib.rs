//! The shell of the engine's machine over a WebSocket.
//!
//! Port of `io/terminal`. One master pty owns one shell; subscribers attach and
//! detach without ending it. Output is kept in a 4 MiB ring so a returning
//! client gets the tail and the modes the program left on.

mod modes;
mod pty;
pub mod tmux;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

type Logf = Arc<dyn Fn(String) + Send + Sync>;

use aixodia_wsock::{OP_BINARY, OP_CLOSE, OP_PING, OP_PONG, OP_TEXT};

use modes::{strip_queries, trim_ring, ModeTracker};
use pty::{process_alive, process_start, Pty};

const DEFAULT_LIMIT: usize = 16;
const DEFAULT_PING_EVERY: Duration = Duration::from_secs(30);
const READ_CHUNK: usize = 32 << 10;
const MIN_SIZE: u16 = 1;
const MAX_SIZE: u16 = 1000;
const MIN_WIRE_COLS: i64 = 10;
const MIN_WIRE_ROWS: i64 = 2;
const IDLE_REAP_AFTER: Duration = Duration::from_secs(30 * 60);
const RING_CAP: usize = 4 << 20;
const TICKET_TTL: Duration = Duration::from_secs(90);
const RIS_RESET: &[u8] = b"\x1bc";

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn clamp(value: i64) -> u16 {
    value.clamp(MIN_SIZE as i64, MAX_SIZE as i64) as u16
}

#[derive(Clone)]
enum Frame {
    Binary(Arc<Vec<u8>>),
    Text(String),
    Ping,
}

struct Session {
    id: String,
    pty: Arc<Pty>,
    start: u64,
    shell: String,
    dir: String,
    /// The shell process this session *is*. Under tmux that is the pane's shell,
    /// not the tmux client the pty actually runs, so the number a tab was opened
    /// with survives a detach and an engine restart.
    shell_pid: AtomicI32,
    cols: AtomicU16,
    rows: AtomicU16,
    created_at: i64,
    last_act: AtomicI64,
    dead: AtomicBool,
    exit_code: AtomicI32,
    ring: Mutex<Vec<u8>>,
    modes: Mutex<ModeTracker>,
    subs: Mutex<Vec<(u64, broadcast::Sender<Frame>)>>,
    next_sub: AtomicI64,
    cancel: CancellationToken,
}

impl Session {
    fn pid(&self) -> i32 {
        self.shell_pid.load(Ordering::SeqCst)
    }

    fn send(&self, frame: Frame) {
        let subs = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        for (_, tx) in subs.iter() {
            let _ = tx.send(frame.clone());
        }
    }

    fn detach(&self, id: u64) {
        let mut subs = self.subs.lock().unwrap_or_else(|e| e.into_inner());
        subs.retain(|(sub, _)| *sub != id);
        self.last_act.store(now_unix(), Ordering::SeqCst);
    }

    fn alive(&self) -> bool {
        !self.dead.load(Ordering::SeqCst) && process_alive(self.pid(), self.start)
    }

    fn mark_dead(&self) -> i32 {
        let already = self.dead.swap(true, Ordering::SeqCst);
        let code = self.pty.exit_code();
        self.exit_code.store(code, Ordering::SeqCst);
        if !already {
            self.cancel.cancel();
        }
        code
    }
}

/// One shell as the engine reports it.
#[derive(Clone)]
pub struct Info {
    pub id: String,
    pub pid: i32,
    pub shell: String,
    pub cwd: String,
    pub cols: u16,
    pub rows: u16,
    pub alive: bool,
    pub created_at: i64,
    /// What the program in the shell calls itself (OSC 0/2), when it said so.
    pub title: String,
}

impl Info {
    fn json(&self) -> Value {
        json!({
            "id": self.id,
            "pid": self.pid,
            "shell": self.shell,
            "cwd": self.cwd,
            "cols": self.cols,
            "rows": self.rows,
            "alive": self.alive,
            "created_at": self.created_at,
            "title": self.title,
        })
    }
}

/// The terminal server: tickets, live sessions and their subscriptions.
pub struct Server {
    token: String,
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    tickets: Mutex<HashMap<String, Instant>>,
    limit: usize,
    ping_every: Duration,
    idle_after: Duration,
    shell: String,
    pub tmux: bool,
    logf: Mutex<Option<Logf>>,
}

impl Server {
    pub fn new(token: impl Into<String>) -> Self {
        Self {
            token: token.into(),
            sessions: Mutex::new(HashMap::new()),
            tickets: Mutex::new(HashMap::new()),
            limit: DEFAULT_LIMIT,
            ping_every: DEFAULT_PING_EVERY,
            idle_after: IDLE_REAP_AFTER,
            shell: default_shell(),
            tmux: false,
            logf: Mutex::new(None),
        }
    }

    pub fn set_logf(&self, f: Logf) {
        *self.logf.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    fn logf(&self, message: String) {
        let f = self.logf.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(f) = f {
            f(message);
        } else {
            tracing::info!("{message}");
        }
    }

    fn limit(&self) -> usize {
        self.limit
    }

    pub fn issue_ticket(&self) -> (String, i64) {
        let ticket = uuid::Uuid::new_v4().simple().to_string();
        let expires = Instant::now() + TICKET_TTL;
        let mut tickets = self.tickets.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        tickets.retain(|_, at| *at > now);
        tickets.insert(ticket.clone(), expires);
        (ticket, now_unix() + TICKET_TTL.as_secs() as i64)
    }

    fn consume_ticket(&self, value: &str) -> bool {
        let value = value.trim();
        if value.is_empty() {
            return false;
        }
        let mut tickets = self.tickets.lock().unwrap_or_else(|e| e.into_inner());
        match tickets.remove(value) {
            Some(expires) => Instant::now() < expires,
            None => false,
        }
    }

    fn ticket_ok(&self, request: &Request) -> bool {
        if aixodia_wsock::bearer_ok(request.headers(), &self.token) {
            return true;
        }
        let ticket = request
            .uri()
            .query()
            .unwrap_or("")
            .split('&')
            .find_map(|pair| pair.strip_prefix("ticket="))
            .map(percent_decode)
            .unwrap_or_default();
        self.consume_ticket(&ticket)
    }

    /// Every live session, oldest first, including tmux sessions that survived a
    /// restart.
    pub fn list(&self) -> Vec<Info> {
        self.reap(false);
        let sessions: Vec<Arc<Session>> = {
            let map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            map.values().cloned().collect()
        };
        let mut out: Vec<Info> = sessions
            .iter()
            .map(|session| Info {
                id: session.id.clone(),
                pid: session.pid(),
                shell: session.shell.clone(),
                cwd: session.dir.clone(),
                cols: session.cols.load(Ordering::SeqCst),
                rows: session.rows.load(Ordering::SeqCst),
                alive: session.alive(),
                created_at: session.created_at,
                title: String::new(),
            })
            .collect();
        if self.tmux {
            let seen: std::collections::HashSet<String> =
                out.iter().map(|info| info.id.clone()).collect();
            for session in tmux::list() {
                if seen.contains(&session.id) {
                    continue;
                }
                out.push(Info {
                    id: session.id,
                    pid: session.pid,
                    shell: "tmux".to_string(),
                    cwd: session.cwd,
                    cols: 0,
                    rows: 0,
                    alive: true,
                    created_at: session.created_at,
                    title: session.title,
                });
            }
        }
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        out
    }

    /// Drops finished shells, and — when making room — shells no client has
    /// touched for the idle window.
    fn reap(&self, make_room: bool) {
        let now = now_unix();
        let mut drop = Vec::new();
        {
            let map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            for (id, session) in map.iter() {
                let idle = make_room
                    && session
                        .subs
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_empty()
                    && now - session.last_act.load(Ordering::SeqCst)
                        > self.idle_after.as_secs() as i64;
                if !session.alive() || idle {
                    drop.push((id.clone(), session.clone()));
                }
            }
        }
        for (id, session) in drop {
            self.drop_session(&id, &session);
        }
    }

    fn drop_session(&self, id: &str, session: &Arc<Session>) {
        {
            let mut map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(current) = map.get(id) {
                if Arc::ptr_eq(current, session) {
                    map.remove(id);
                }
            }
        }
        session.mark_dead();
        session.pty.close();
        if self.tmux {
            tmux::kill(id);
        }
    }

    fn shell(&self) -> &str {
        &self.shell
    }

    fn dir() -> String {
        if let Some(home) = std::env::var_os("HOME") {
            if !home.is_empty() {
                return home.to_string_lossy().into_owned();
            }
        }
        std::env::current_dir()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default()
    }

    /// Reuses a live session or starts one.
    fn acquire(
        self: &Arc<Self>,
        id: &str,
        cols: u16,
        rows: u16,
    ) -> std::io::Result<(Arc<Session>, bool)> {
        {
            let map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(session) = map.get(id) {
                if session.alive() {
                    session.last_act.store(now_unix(), Ordering::SeqCst);
                    let old_cols = session.cols.load(Ordering::SeqCst);
                    let old_rows = session.rows.load(Ordering::SeqCst);
                    let resized = cols > 0 && rows > 0 && (cols != old_cols || rows != old_rows);
                    if resized {
                        session.cols.store(cols, Ordering::SeqCst);
                        session.rows.store(rows, Ordering::SeqCst);
                        let _ = session.pty.resize(cols, rows);
                    }
                    let kick = reattach_kick(
                        self.tmux,
                        resized,
                        session
                            .modes
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .alt_screen(),
                    );
                    return Ok((session.clone(), kick));
                }
            }
        }
        self.reap(false);
        if self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
            >= self.limit()
        {
            self.reap(true);
        }
        {
            let map = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
            if map.len() >= self.limit() {
                return Err(std::io::Error::other("too many terminals"));
            }
        }

        let cols = if cols == 0 { 80 } else { cols };
        let rows = if rows == 0 { 24 } else { rows };
        let dir = Self::dir();
        let shell = self.shell().to_string();
        let p = if self.tmux {
            tmux::start(id, &shell, cols, rows, Some(&dir))
                .map_err(|err| std::io::Error::new(err.kind(), format!("tmux: {err}")))?
        } else {
            Pty::start(&shell, &["-l".to_string()], cols, rows, Some(&dir))?
        };
        let p = Arc::new(p);
        // The pid the app is told about is the shell's, not the connection's:
        // under tmux the pty holds a client that is recreated on every attach,
        // while the pane's shell keeps the pid the tab was opened with.
        let pid = if self.tmux {
            tmux::pane_pid(id).unwrap_or_else(|| p.pid())
        } else {
            p.pid()
        };
        let start = process_start(pid).unwrap_or(0);
        let session = Arc::new(Session {
            id: id.to_string(),
            pty: p,
            start,
            shell,
            dir,
            shell_pid: AtomicI32::new(pid),
            cols: AtomicU16::new(cols),
            rows: AtomicU16::new(rows),
            created_at: now_unix(),
            last_act: AtomicI64::new(now_unix()),
            dead: AtomicBool::new(false),
            exit_code: AtomicI32::new(0),
            ring: Mutex::new(Vec::new()),
            modes: Mutex::new(ModeTracker::new()),
            subs: Mutex::new(Vec::new()),
            next_sub: AtomicI64::new(1),
            cancel: CancellationToken::new(),
        });
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), session.clone());
        self.spawn_reader(session.clone());
        self.spawn_pings(session.clone());
        Ok((session, false))
    }

    fn spawn_reader(self: &Arc<Self>, session: Arc<Session>) {
        let fd = pty::raw_fd(&session.pty.master());
        std::thread::spawn(move || {
            let mut buf = vec![0u8; READ_CHUNK];
            loop {
                if session.cancel.is_cancelled() {
                    break;
                }
                let mut pollfd = libc::pollfd {
                    fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut pollfd, 1, 250) };
                if ready < 0 {
                    let err = std::io::Error::last_os_error();
                    if err.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if ready == 0 {
                    continue;
                }
                match session.pty.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let chunk = buf[..n].to_vec();
                        {
                            let mut tracker =
                                session.modes.lock().unwrap_or_else(|e| e.into_inner());
                            tracker.scan(&chunk);
                            let mut ring = session.ring.lock().unwrap_or_else(|e| e.into_inner());
                            ring.extend_from_slice(&chunk);
                            trim_ring(&mut ring, RING_CAP);
                        }
                        session.last_act.store(now_unix(), Ordering::SeqCst);
                        session.send(Frame::Binary(Arc::new(chunk)));
                    }
                    Err(err)
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            || err.kind() == std::io::ErrorKind::Interrupted =>
                    {
                        continue;
                    }
                    Err(_) => break,
                }
            }
            let code = session.mark_dead();
            session.send(Frame::Text(
                json!({"type": "exit", "code": code}).to_string(),
            ));
        });
    }

    fn spawn_pings(self: &Arc<Self>, session: Arc<Session>) {
        let every = self.ping_every;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(every);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = session.cancel.cancelled() => return,
                    _ = ticker.tick() => session.send(Frame::Ping),
                }
            }
        });
    }

    /// Serves one already-upgraded connection as session `id`, creating the
    /// shell when the id is new.
    pub async fn attach<S>(
        self: &Arc<Self>,
        stream: S,
        id: String,
        cols: i64,
        rows: i64,
    ) -> Result<(), aixodia_wsock::Error>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let id = if id.trim().is_empty() {
            new_id()
        } else {
            id.trim().to_string()
        };
        let c = if cols >= MIN_WIRE_COLS {
            clamp(cols)
        } else {
            0
        };
        let r = if rows >= MIN_WIRE_ROWS {
            clamp(rows)
        } else {
            0
        };
        let (session, kick) = match self.acquire(&id, c, r) {
            Ok(pair) => pair,
            Err(err) => {
                let mut conn = aixodia_wsock::ServerConn::new(stream);
                let _ = conn
                    .write_message(
                        OP_TEXT,
                        json!({"type": "error", "error": err.to_string()})
                            .to_string()
                            .as_bytes(),
                    )
                    .await;
                return Ok(());
            }
        };
        let eff_cols = session.cols.load(Ordering::SeqCst);
        let eff_rows = session.rows.load(Ordering::SeqCst);
        self.logf(format!("terminal {id}: attach {eff_cols}x{eff_rows}"));

        let (tx, mut rx) = broadcast::channel::<Frame>(256);
        let sub_id = session.next_sub.fetch_add(1, Ordering::SeqCst) as u64;
        let (ring, modes) = {
            let mut subs = session.subs.lock().unwrap_or_else(|e| e.into_inner());
            subs.push((sub_id, tx));
            let ring = session
                .ring
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let modes = session
                .modes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .replay();
            (ring, modes)
        };
        let replay = replay_frame(self.tmux, &modes, &ring);

        let (mut reader, writer) = aixodia_wsock::ServerConn::new(stream).split();
        let ready = json!({
            "type": "ready",
            "shell": session.shell,
            "cwd": session.dir,
            "cols": eff_cols,
            "rows": eff_rows,
            "id": id,
            "pid": session.pid(),
        })
        .to_string();
        if writer
            .write_message(OP_TEXT, ready.as_bytes())
            .await
            .is_err()
        {
            session.detach(sub_id);
            return Ok(());
        }
        if !replay.is_empty() && writer.write_message(OP_BINARY, &replay).await.is_err() {
            session.detach(sub_id);
            return Ok(());
        }
        // After the reset is on the wire, so the repaint it triggers lands on a
        // screen that has already been cleared, and after the subscriber is
        // registered, so the bytes are not broadcast into nothing.
        if kick {
            let _ = session.pty.kick();
        }

        let writer_session = session.clone();
        let writer_for_task = writer.clone();
        let writer_task = tokio::spawn(async move {
            let writer = writer_for_task;
            loop {
                match rx.recv().await {
                    Ok(Frame::Binary(chunk)) => {
                        if writer.write_message(OP_BINARY, &chunk).await.is_err() {
                            return;
                        }
                    }
                    Ok(Frame::Text(text)) => {
                        if writer
                            .write_message(OP_TEXT, text.as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Ok(Frame::Ping) => {
                        if writer.write_message(OP_PING, &[]).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                }
                if writer_session.cancel.is_cancelled() {
                    return;
                }
            }
        });

        let mut close_after = false;
        loop {
            match reader.read_message().await {
                Ok((OP_BINARY, payload)) => {
                    if !session.dead.load(Ordering::SeqCst) {
                        let _ = session.pty.write(&payload);
                    }
                }
                Ok((OP_TEXT, payload)) => {
                    let Ok(input) = serde_json::from_slice::<Value>(&payload) else {
                        continue;
                    };
                    match input.get("type").and_then(Value::as_str).unwrap_or("") {
                        "resize" => {
                            let cols = input.get("cols").and_then(Value::as_i64);
                            let rows = input.get("rows").and_then(Value::as_i64);
                            let (Some(cols), Some(rows)) = (cols, rows) else {
                                continue;
                            };
                            if cols < MIN_WIRE_COLS || rows < MIN_WIRE_ROWS {
                                self.logf(format!("terminal {id}: resize {cols}x{rows} ignored"));
                                continue;
                            }
                            let c = clamp(cols);
                            let r = clamp(rows);
                            session.cols.store(c, Ordering::SeqCst);
                            session.rows.store(r, Ordering::SeqCst);
                            if !session.dead.load(Ordering::SeqCst) {
                                let _ = session.pty.resize(c, r);
                            }
                        }
                        "close" => {
                            close_after = true;
                            break;
                        }
                        _ => {}
                    }
                }
                Ok((OP_PING, payload)) => {
                    let _ = writer.write_message(OP_PONG, &payload).await;
                }
                Ok((OP_CLOSE, _)) => break,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        session.detach(sub_id);
        writer_task.abort();
        if close_after {
            self.drop_session(&id, &session);
        }
        Ok(())
    }

    /// The HTTP surface: `/terminal` plus the ticket, list and close calls.
    pub fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/terminal", get(websocket))
            .route("/v1/terminals/ticket", post(ticket))
            .route("/v1/terminals", get(list))
            .route("/v1/terminals/:id", delete(close))
            .with_state(self.clone())
    }

    /// Serves an already-upgraded connection from the relay's transport.
    pub async fn attach_stream<S>(
        self: &Arc<Self>,
        stream: S,
        id: String,
        cols: i64,
        rows: i64,
    ) -> Result<(), aixodia_wsock::Error>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        self.attach(stream, id, cols, rows).await
    }
}

/// Whether tmux is on PATH. The engine uses it to keep shells across restarts.
pub fn tmux_available() -> bool {
    tmux::available()
}

fn replay_frame(tmux: bool, modes: &[u8], ring: &[u8]) -> Vec<u8> {
    let ring = if tmux {
        Vec::new()
    } else {
        strip_queries(ring)
    };
    let mut out = Vec::with_capacity(RIS_RESET.len() + modes.len() + ring.len());
    out.extend_from_slice(RIS_RESET);
    out.extend_from_slice(modes);
    out.extend_from_slice(&ring);
    out
}

/// Whether a reattach must ask the client to paint the screen back.
///
/// A size change is itself the signal — TIOCSWINSZ reaches the client as
/// SIGWINCH and it redraws — and without tmux the ring replays the shell's tail
/// instead. What is left is a same-size attach: under tmux the ring is dropped
/// and the screen lives in the tmux server, which paints only when a SIGWINCH
/// reaches its client. Without the kick a reconnected phone sat on a cleared
/// grid until the keyboard changed the size. A plain shell needs it only on the
/// alternate screen, where the program owns the whole grid.
fn reattach_kick(tmux: bool, resized: bool, alt_screen: bool) -> bool {
    !resized && (tmux || alt_screen)
}

fn default_shell() -> String {
    if std::path::Path::new("/bin/bash").exists() {
        "/bin/bash".to_string()
    } else {
        "/bin/sh".to_string()
    }
}

fn new_id() -> String {
    format!("t-{}", uuid::Uuid::new_v4().simple())
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[i + 1..i + 3], 16) {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn wanted_size(query: &str) -> (i64, i64) {
    let mut cols = 80i64;
    let mut rows = 24i64;
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        let value = value.parse::<i64>().unwrap_or(0);
        match key {
            "cols" => cols = value,
            "rows" => rows = value,
            _ => {}
        }
    }
    if cols < MIN_WIRE_COLS {
        cols = 80;
    }
    if rows < MIN_WIRE_ROWS {
        rows = 24;
    }
    (clamp(cols) as i64, clamp(rows) as i64)
}

async fn websocket(State(server): State<Arc<Server>>, request: Request) -> Response {
    if !server.ticket_ok(&request) {
        return (StatusCode::UNAUTHORIZED, "token rejected").into_response();
    }
    if !aixodia_wsock::is_upgrade(request.headers()) {
        return (StatusCode::BAD_REQUEST, "websocket upgrade required").into_response();
    }
    let (cols, rows) = wanted_size(request.uri().query().unwrap_or(""));
    let id = request
        .uri()
        .query()
        .unwrap_or("")
        .split('&')
        .find_map(|pair| pair.strip_prefix("id="))
        .map(percent_decode)
        .unwrap_or_default();
    let server2 = server.clone();
    aixodia_wsock::accept(request, move |conn| async move {
        let _ = server2.attach(conn.into_inner(), id, cols, rows).await;
    })
}

async fn ticket(State(server): State<Arc<Server>>, headers: axum::http::HeaderMap) -> Response {
    if !aixodia_wsock::bearer_ok(&headers, &server.token) {
        return (StatusCode::UNAUTHORIZED, "token rejected").into_response();
    }
    let (ticket, expires_at) = server.issue_ticket();
    Json(json!({"ticket": ticket, "expires_at": expires_at})).into_response()
}

async fn list(State(server): State<Arc<Server>>, headers: axum::http::HeaderMap) -> Response {
    if !aixodia_wsock::bearer_ok(&headers, &server.token) {
        return (StatusCode::UNAUTHORIZED, "token rejected").into_response();
    }
    let terminals: Vec<Value> = server.list().iter().map(Info::json).collect();
    Json(json!({"terminals": terminals})).into_response()
}

async fn close(
    State(server): State<Arc<Server>>,
    headers: axum::http::HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if !aixodia_wsock::bearer_ok(&headers, &server.token) {
        return (StatusCode::UNAUTHORIZED, "token rejected").into_response();
    }
    let id = id.trim_matches('/').to_string();
    if id.is_empty() {
        return (StatusCode::BAD_REQUEST, "id required").into_response();
    }
    let session = server
        .sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .cloned();
    if let Some(session) = session {
        server.drop_session(&id, &session);
    } else if server.tmux {
        // Nothing in memory is not nothing on the machine. An engine restart
        // empties the map while the tmux server keeps the shell, so a close that
        // only looked in the map reported success and left the shell alive —
        // and the next list handed the phone back a tab it had closed.
        tmux::kill(&id);
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::{reattach_kick, tmux};
    use std::time::{Duration, Instant};

    #[test]
    fn a_same_size_reattach_under_tmux_asks_for_a_repaint() {
        // The pane lives in the tmux server and nothing but a SIGWINCH makes
        // the client paint it; this is the reconnect the phone sees as blank.
        assert!(reattach_kick(true, false, false));
        assert!(reattach_kick(true, false, true));
    }

    #[test]
    fn a_resize_is_its_own_repaint() {
        // TIOCSWINSZ already signalled the client, tmux or not.
        assert!(!reattach_kick(true, true, false));
        assert!(!reattach_kick(false, true, false));
    }

    #[test]
    fn a_plain_shell_reattaches_from_the_ring() {
        assert!(!reattach_kick(false, false, false));
    }

    #[test]
    fn a_full_screen_program_without_tmux_is_asked_to_paint() {
        assert!(reattach_kick(false, false, true));
    }

    /// The pty the engine holds keeps its tmux client across a detach, and the
    /// client paints nothing until a SIGWINCH reaches it — the repaint a
    /// reconnected phone was missing. This drives the engine's own pty and kick.
    /// Skipped where tmux is not installed.
    #[test]
    fn a_kick_repaints_the_tmux_pane() {
        if !tmux::available() {
            return;
        }
        let id = format!("kicktest-{}", std::process::id());
        let Ok(pty) = tmux::start(&id, "/bin/bash", 40, 120, Some("/tmp")) else {
            return;
        };
        let drain = |ms: u64| {
            let mut out = Vec::new();
            let mut buf = [0u8; 8192];
            let deadline = Instant::now() + Duration::from_millis(ms);
            while Instant::now() < deadline {
                match pty.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => out.extend_from_slice(&buf[..n]),
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(_) => break,
                }
            }
            out
        };
        let first = drain(1_200);
        // Let the startup paint finish, so nothing still in flight can be
        // mistaken for the repaint the kick asks for.
        let _ = drain(300);
        let kicked = pty.kick().is_ok();
        let repaint = drain(1_200);
        tmux::kill(&id);
        assert!(!first.is_empty(), "the tmux client never painted");
        assert!(kicked, "the pty has no foreground group to signal");
        assert!(!repaint.is_empty(), "a SIGWINCH repainted nothing");
    }
}
