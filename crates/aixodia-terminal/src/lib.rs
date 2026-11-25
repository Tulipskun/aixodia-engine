//! The shell of the engine's machine over a WebSocket.
//!
//! Port of `io/terminal`. One master pty owns one shell; subscribers attach and
//! detach without ending it. Output is kept in a 4 MiB ring so a returning
//! client gets the tail and the modes the program left on.

mod holder;
mod modes;
mod pty;

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
/// The grid a shell is opened at before a phone has said what its screen is.
const DEFAULT_COLS: u16 = 80;
const DEFAULT_ROWS: u16 = 24;
const MAX_SIZE: u16 = 1000;
const MIN_WIRE_COLS: i64 = 10;
const MIN_WIRE_ROWS: i64 = 2;
const IDLE_REAP_AFTER: Duration = Duration::from_secs(30 * 60);
const RING_CAP: usize = 4 << 20;
const TICKET_TTL: Duration = Duration::from_secs(90);
const RIS_RESET: &[u8] = b"\x1bc";
/// How many lines of a plain shell's output a reattaching client is handed.
///
/// The screen only needs the last `rows` of them; the rest scroll into the
/// client's own scrollback, which is what a phone asks for when it reconnects
/// and pans history. One screenful was the whole replay, so a client that had
/// just attached had nothing above the viewport to pan and could not look back
/// at all until the shell wrote new lines.
const REPLAY_LINES: usize = 2000;
/// What that history may cost on the wire, whatever the session printed.
///
/// A line count is not a size: one line can carry a progress bar worth a
/// megabyte, and a session that printed no newline has no line to stop at. The
/// frame stays well under the socket's frame cap either way, because the phone
/// parses the replay before it paints anything.
const REPLAY_BYTES: usize = 256 << 10;

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

/// Where a shell actually lives.
///
/// A pty is the simple case: the engine holds the master and the shell is its
/// direct child, which is all a short-lived session needs. A [`Holder`] is the
/// case that matters for a phone: a separate process owns the pty, so the shell
/// keeps running when the engine does not, and the engine reconnects to it
/// instead of starting over.
enum Shell {
    Direct(Arc<Pty>),
    Held(Arc<holder::Holder>),
}

impl Shell {
    fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Shell::Direct(pty) => pty.read(buf),
            // A holder reports nothing when it has nothing to say, which is the
            // same thing `WouldBlock` means to the caller.
            Shell::Held(h) => Ok(0),
        }
    }

    fn write(&self, data: &[u8]) -> std::io::Result<()> {
        match self {
            Shell::Direct(pty) => pty.write(data),
            Shell::Held(h) => h.write_all(data),
        }
    }

    fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        match self {
            Shell::Direct(pty) => pty.resize(cols, rows),
            Shell::Held(h) => h.resize(cols, rows),
        }
    }

    fn kick(&self) -> std::io::Result<()> {
        match self {
            Shell::Direct(pty) => pty.kick(),
            // The holder owns the pty, so the signal has to be asked for over
            // the socket: this is the only way a reattaching phone can make a
            // full-screen program paint the screen it left.
            Shell::Held(h) => h.kick(),
        }
    }

    fn dead(&self) -> bool {
        match self {
            Shell::Direct(pty) => pty.dead(),
            Shell::Held(h) => h.finished(),
        }
    }

    fn exit_code(&self) -> i32 {
        match self {
            Shell::Direct(pty) => pty.exit_code(),
            Shell::Held(h) => h.exit_code(),
        }
    }

    fn close(&self) {
        match self {
            Shell::Direct(pty) => pty.close(),
            // Closing the connection only detaches; the holder ends when its
            // shell does, which is what makes a restart survivable.
            Shell::Held(h) => h.close(),
        }
    }

    fn raw_fd(&self) -> Option<i32> {
        match self {
            Shell::Direct(pty) => Some(pty::raw_fd(&pty.master())),
            Shell::Held(_) => None,
        }
    }
}

struct Session {
    id: String,
    pty: Arc<Shell>,
    start: u64,
    shell: String,
    dir: String,
    /// The shell process this session *is*. The holder runs it, so the number a
    /// tab was opened with survives a detach and an engine restart.
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
            logf: Mutex::new(None),
        }
    }

    pub fn set_logf(&self, f: Logf) {
        *self.logf.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
    }

    /// Takes back the shells a previous engine left running.
    ///
    /// A holder owns its pty, so an engine restart does not reach the shell — but
    /// this engine's own map is empty after one, and a shell nobody lists is a
    /// shell the phone cannot reattach to: it prunes the tab, starts a new shell,
    /// and the old one runs on with nobody able to reach it. Adopting the
    /// survivors at startup is what makes a restart invisible to a tab.
    ///
    /// Each is adopted at the default size, because the holder is not asked: the
    /// phone's first attach carries its own grid and a resize makes a full-screen
    /// program repaint by itself, exactly as it does on a cold start.
    pub fn resume(self: &Arc<Self>) {
        let dir = Self::dir();
        let shell = self.shell().to_string();
        for id in holder::survivors() {
            if self
                .sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(&id)
            {
                continue;
            }
            let held =
                match holder::connect_or_start(&id, &shell, DEFAULT_COLS, DEFAULT_ROWS, Some(&dir))
                {
                    Ok(held) => held,
                    Err(err) => {
                        self.logf(format!("terminal {id}: leftover holder not adopted: {err}"));
                        continue;
                    }
                };
            let p = Arc::new(Shell::Held(Arc::new(held)));
            match self.adopt(
                &id,
                shell.clone(),
                dir.clone(),
                DEFAULT_COLS,
                DEFAULT_ROWS,
                p,
            ) {
                Ok(_) => self.logf(format!("terminal {id}: adopted from a previous engine")),
                Err(err) => self.logf(format!("terminal {id}: adopt failed: {err}")),
            }
        }
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

    /// Every live session, oldest first.
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
        // Ending a tab has to reach the shell, not only this map. A holder
        // outlives the engine on purpose, so shutting the socket down is a
        // detach: the shell would keep running with no engine listing it, which
        // is how closed tabs left their shells behind for good.
        match &*session.pty {
            Shell::Held(_) => holder::close(id),
            Shell::Direct(_) => session.pty.close(),
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

        let cols = if cols == 0 { DEFAULT_COLS } else { cols };
        let rows = if rows == 0 { DEFAULT_ROWS } else { rows };
        let dir = Self::dir();
        let shell = self.shell().to_string();
        // A shell is started in a holder, which keeps it across an engine
        // restart: the pty belongs to that process, not to this one, so the
        // phone's tab still has something to reattach to when a new engine
        // comes up.
        let held = holder::connect_or_start(id, &shell, cols, rows, Some(&dir))
            .map_err(|err| std::io::Error::new(err.kind(), format!("holder: {err}")))?;
        let p = Arc::new(Shell::Held(Arc::new(held)));
        self.adopt(id, shell, dir, cols, rows, p)
            .map(|s| (s, false))
    }

    /// Registers a started shell as a session and starts its readers.
    fn adopt(
        self: &Arc<Self>,
        id: &str,
        shell: String,
        dir: String,
        cols: u16,
        rows: u16,
        p: Arc<Shell>,
    ) -> std::io::Result<Arc<Session>> {
        // The pid the app is told about is the shell the holder runs, which is
        // the same number before and after an engine restart.
        let pid = holder::shell_pid(id).unwrap_or_default();
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
        Ok(session)
    }

    fn spawn_reader(self: &Arc<Self>, session: Arc<Session>) {
        if let Shell::Held(held) = &*session.pty {
            self.spawn_holder_reader(session.clone(), held.clone());
            return;
        }
        let Some(fd) = session.pty.raw_fd() else {
            return;
        };
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

    /// Reads the frames a holder sends: the shell's output, and its exit.
    ///
    /// A holder replays what its ring kept before it serves the first frame, so
    /// the output produced while no engine was connected arrives here as if it
    /// had just been written.
    fn spawn_holder_reader(self: &Arc<Self>, session: Arc<Session>, held: Arc<holder::Holder>) {
        std::thread::spawn(move || {
            loop {
                if session.cancel.is_cancelled() {
                    break;
                }
                // Sleep on the socket rather than on the clock: a keystroke's
                // echo must not wait for the next poll tick.
                match held.wait_readable(250).and_then(|_| held.read_frame()) {
                    Ok(Some((holder::KIND_OUTPUT, chunk))) => {
                        if !chunk.is_empty() {
                            let mut tracker =
                                session.modes.lock().unwrap_or_else(|e| e.into_inner());
                            let mut ring = session.ring.lock().unwrap_or_else(|e| e.into_inner());
                            tracker.scan(&chunk);
                            ring.extend_from_slice(&chunk);
                            trim_ring(&mut ring, RING_CAP);
                            drop(ring);
                            drop(tracker);
                            session.last_act.store(now_unix(), Ordering::SeqCst);
                            session.send(Frame::Binary(Arc::new(chunk)));
                        }
                    }
                    Ok(Some((holder::KIND_EXIT, payload))) => {
                        let code = payload
                            .get(..4)
                            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                            .unwrap_or(-1);
                        held.set_exit_code(code);
                        break;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => std::thread::sleep(Duration::from_millis(5)),
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
        let (ring, modes, alt_screen) = {
            let mut subs = session.subs.lock().unwrap_or_else(|e| e.into_inner());
            subs.push((sub_id, tx));
            let ring = session
                .ring
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let tracker = session.modes.lock().unwrap_or_else(|e| e.into_inner());
            (ring, tracker.replay(), tracker.alt_screen())
        };
        // The ring here is what this engine has watched since it started, which
        // under a holder includes the replay the holder handed over: the output
        // produced while no engine was connected. Replaying it back is right —
        // a reattaching phone has seen none of it. What must not happen is the
        // reset being applied to a client that is *already* at that screen. So
        // the reset opens the frame and the ring follows it once, and the ring
        // is emptied as it is sent; otherwise the same history is painted again
        // on every attach and the screen grows a blank gap between an old
        // prompt and the one the shell is actually on.
        //
        // A full-screen program is the exception. It draws by moving a cursor
        // rather than by writing lines, so the tail of the ring is not the screen
        // it is showing and no tail can be cut: this session's ring holds no
        // newlines at all, so every attach shipped the whole four megabytes of
        // cursor moves for the phone to parse before it could paint. The screen
        // that matters is the one the program can draw on demand, so the client
        // is cleared and the program is asked for it — that ask is the kick below.
        let replay = if alt_screen {
            replay_reset(&modes)
        } else {
            replay_frame(None, &modes, &ring, eff_rows)
        };

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
                    // A slow link drops frames rather than growing without
                    // bound, and `continue` left the client painting the rest of
                    // its life from a screen that had lost a middle: the phone
                    // showed a stale screen while the shell kept printing, and
                    // only a resize (which asks for a repaint) put it right. So a
                    // gap is repaired, not skipped: the client is cleared and
                    // given the tail of the ring — the same frame an attach
                    // sends — or, for a full-screen program, cleared and asked to
                    // draw itself again.
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        let (resync, alt) = {
                            let ring = writer_session
                                .ring
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .clone();
                            let tracker = writer_session
                                .modes
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            resync_frame(&tracker, &ring, eff_rows)
                        };
                        tracing::warn!(
                            "terminal {}: the client fell {missed} frame(s) behind; resyncing its screen",
                            writer_session.id
                        );
                        if writer.write_message(OP_BINARY, &resync).await.is_err() {
                            return;
                        }
                        if alt {
                            // The screen the program draws is the only one that
                            // matters, so it is asked for rather than guessed.
                            let _ = writer_session.pty.kick();
                        }
                        continue;
                    }
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
                        if let Err(err) = session.pty.write(&payload) {
                            self.logf(format!("terminal {id}: {err} bytes dropped"));
                        }
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
            .route("/api/terminals/ticket", post(ticket))
            .route("/api/terminals", get(list))
            .route("/api/terminals/:id", delete(close))
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

/// Runs a shell for the engine that started it, and for the ones that replace it.
///
/// This is the body of the `term-holder` subcommand. It never returns.
pub fn run_holder(id: &str, shell: &str, cols: u16, rows: u16, dir: Option<&str>) -> ! {
    let err = holder::run(id, shell, cols, rows, dir);
    eprintln!("term-holder: {err}");
    std::process::exit(1);
}

/// The bytes that put a client's screen where the shell already is.
///
/// The ring is the record, and it is replayed with the shell's own queries
/// stripped — an unanswered `DA`/`DSC` tail would have the client waiting on a
/// device that is not there.
fn replay_frame(history: Option<Vec<u8>>, modes: &[u8], ring: &[u8], rows: u16) -> Vec<u8> {
    let body = match history {
        Some(pane) => pane,
        None => history_tail(&strip_queries(ring), rows),
    };
    let mut out = replay_reset(modes);
    out.extend_from_slice(&body);
    out
}

/// What a client that fell behind is sent, and whether it has to be asked to
/// draw the screen it is on.
///
/// A subscriber's frames are dropped rather than queued without bound, so a slow
/// link sees a gap in the middle of the stream. Continuing from there left the
/// phone painting from a screen that no longer matched the shell, which read as
/// a terminal that had stopped: no new lines, and the cursor in the wrong place.
/// Only a resize — which asks for a repaint — put it right. The repair is the
/// frame an attach sends: clear, then the modes and the tail of the ring, or for
/// a full-screen program a clear and a kick.
fn resync_frame(modes: &ModeTracker, ring: &[u8], rows: u16) -> (Vec<u8>, bool) {
    let replay = modes.replay();
    if modes.alt_screen() {
        (replay_reset(&replay), true)
    } else {
        (replay_frame(None, &replay, ring, rows), false)
    }
}

/// The bytes that clear a client's screen and put it in the modes the program
/// left on, with no output of its own.
fn replay_reset(modes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(RIS_RESET.len() + modes.len());
    out.extend_from_slice(RIS_RESET);
    out.extend_from_slice(modes);
    out
}

/// The history of a shell's output a client is given, cut on a line boundary.
///
/// A replay is painted by scrolling a fresh screen, not by restoring one: every
/// line fed in above the screen's height pushes the earlier ones up and away,
/// which is what a terminal is supposed to do. That is why the tail is what
/// goes on the wire — the last `rows` lines end up on the screen, and the ones
/// before them end up in the client's scrollback, where a reader can pan to
/// them. Feeding the whole ring instead is what made the phone parse the entire
/// history of the session before it painted anything.
///
/// So the window is bounded twice: by lines, so the frame stays a history and
/// not a transcript, and by bytes, so a session that printed one enormous line
/// cannot spend the whole frame on it. Both cuts land on a newline, so an escape
/// sequence is never split in half. The one case with no boundary to cut on — no
/// newline in the last [`REPLAY_BYTES`] — keeps the raw tail: the cap is there to
/// keep the frame small, and that matters more than a mid-sequence cut the client
/// recovers from at the next reset.
fn history_tail(ring: &[u8], rows: u16) -> Vec<u8> {
    let want = (rows as usize).max(REPLAY_LINES);
    let mut seen = 0usize;
    // Nothing is dropped unless the ring really holds more lines than the window
    // wants; a short ring is the whole history already.
    let mut start = 0usize;
    for i in (0..ring.len()).rev() {
        if ring[i] == b'\n' {
            seen += 1;
            if seen > want {
                start = i + 1;
                break;
            }
        }
    }
    if ring.len() - start > REPLAY_BYTES {
        // Applied second, and only forward, so the line cut still holds.
        let cut = ring.len() - REPLAY_BYTES;
        start = match ring[cut..].iter().position(|b| *b == b'\n') {
            Some(offset) => cut + offset + 1,
            None => cut,
        };
    }
    ring[start..].to_vec()
}

/// Whether a reattach must ask the client to paint the screen back.
///
/// A size change is itself the signal: TIOCSWINSZ already reached the program
/// through the pty, so asking again would only make it repaint twice. What is
/// left is a same-size attach, where the replay above normally carries the whole
/// screen already. A full-screen program still needs the kick, because its grid
/// belongs to the program and only a SIGWINCH makes it paint after a reconnect.
fn reattach_kick(resized: bool, alt_screen: bool) -> bool {
    !resized && alt_screen
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
    } else {
        // Nothing in memory is not nothing on the machine: a shell outlives an
        // engine restart in its holder, so a close that only looked in the map
        // would report success and leave the shell running.
        holder::close(&id);
    }
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::{reattach_kick, replay_frame};
    use crate::modes::ModeTracker;

    #[test]
    fn a_resize_is_its_own_repaint() {
        // TIOCSWINSZ already signalled the client, so asking again is noise.
        assert!(!reattach_kick(true, false));
        assert!(!reattach_kick(true, true));
    }

    #[test]
    fn a_plain_shell_reattaches_from_the_ring() {
        assert!(!reattach_kick(false, false));
    }

    #[test]
    fn a_full_screen_program_is_asked_to_paint() {
        // A program in the alternate screen repaints only when signalled, and a
        // phone that just reconnected otherwise sees the blank it left behind.
        assert!(reattach_kick(false, true));
    }

    #[test]
    fn the_ring_is_replayed_when_there_is_no_history() {
        // A holder keeps its own ring across an engine restart, so the engine's
        // replay is what the phone paints on reconnect.
        let ring_only = replay_frame(None, b"\x1b[?1h", b"ring tail\n", 24);
        assert!(ring_only.ends_with(b"ring tail\n"));
        assert!(
            ring_only.starts_with(b"\x1bc"),
            "the client must clear first"
        );
    }

    #[test]
    fn history_wins_over_the_ring_when_both_exist() {
        let replay = replay_frame(
            Some(b"old history".to_vec()),
            b"\x1b[?1h",
            b"ring tail\n",
            24,
        );
        assert_eq!(replay, b"\x1bc\x1b[?1hold history".to_vec());
    }

    #[test]
    fn a_replay_ends_on_the_last_screenful_and_keeps_history_above_it() {
        // Painting by scrolling means the tail of the replay is the screen and
        // everything before it is the client's scrollback, which is exactly what
        // a reader who just reattached wants to pan. A replay that stopped at one
        // screenful left that scrollback empty.
        let mut ring = Vec::new();
        for n in 0..100 {
            ring.extend_from_slice(format!("line {n}\n").as_bytes());
        }
        let replay = replay_frame(None, b"", &ring, 5);
        let body = &replay[super::RIS_RESET.len()..];
        assert_eq!(body, ring.as_slice(), "the ring is the history");
        let painted: Vec<&[u8]> = body.split(|b| *b == b'\n').collect();
        assert!(
            painted[painted.len() - 6..].starts_with(&[&b"line 95"[..]]),
            "the last screenful is what the screen ends on"
        );
    }

    #[test]
    fn a_long_history_is_bounded_by_lines_and_not_by_the_ring() {
        let mut ring = Vec::new();
        for n in 0..(super::REPLAY_LINES * 3) {
            ring.extend_from_slice(format!("{n}\n").as_bytes());
        }
        let replay = replay_frame(None, b"", &ring, 10);
        let body = &replay[super::RIS_RESET.len()..];
        assert_eq!(
            body.iter().filter(|b| **b == b'\n').count(),
            super::REPLAY_LINES,
            "the window is a history, not the transcript"
        );
        assert!(body.ends_with(format!("{}\n", super::REPLAY_LINES * 3 - 1).as_bytes()));
    }

    #[test]
    fn a_line_too_wide_for_the_frame_is_cut_by_bytes() {
        // One line can be worth more than the whole frame; the cap has to bite
        // even though there is no line boundary to cut on below it.
        let mut ring = b"first\n".to_vec();
        ring.extend(std::iter::repeat_n(b'x', super::REPLAY_BYTES * 2));
        let replay = replay_frame(None, b"", &ring, 10);
        let body = &replay[super::RIS_RESET.len()..];
        assert!(
            body.len() <= super::REPLAY_BYTES,
            "the frame was {} bytes",
            body.len()
        );
    }

    #[test]
    fn a_cut_never_lands_inside_an_escape_sequence() {
        // Splitting an escape leaves the client in a state no program asked for.
        let mut ring = Vec::new();
        for n in 0..10 {
            let colour = 30 + n;
            let line = format!("{}[{}mtext {}\n", '\u{1b}', colour, n);
            ring.extend_from_slice(line.as_bytes());
        }
        let replay = replay_frame(None, b"", &ring, 3);
        let body = &replay[super::RIS_RESET.len()..];
        assert!(
            body.starts_with(b"\x1b[3"),
            "the cut must fall on a line boundary, not mid-sequence"
        );
        // The byte cap cuts on a line boundary too.
        let mut wide = Vec::new();
        for n in 0..(super::REPLAY_BYTES / 8) {
            let colour = n % 8;
            wide.extend_from_slice(format!("\u{1b}[3{colour}mtext\n").as_bytes());
        }
        let replay = replay_frame(None, b"", &wide, 3);
        let body = &replay[super::RIS_RESET.len()..];
        assert!(body.starts_with(b"\x1b[3"), "a byte cut began mid-sequence");
    }

    /// A client that fell 256 frames behind lost a piece of the middle of the
    /// stream. Skipping the gap left it painting a screen that no longer matched
    /// the shell; the repair is the frame an attach sends, so the resize that
    /// used to be the only cure is no longer needed.
    #[test]
    fn a_client_that_fell_behind_is_given_the_screen_again() {
        let modes = ModeTracker::new();
        let (frame, alt) = super::resync_frame(&modes, b"last line\n", 24);
        assert!(!alt, "a plain shell is repainted from its ring");
        assert!(frame.starts_with(b"\x1bc"), "the client must clear first");
        assert!(frame.ends_with(b"last line\n"), "and then be caught up");

        let mut full_screen = ModeTracker::new();
        full_screen.scan(b"\x1b[?1049h");
        let (frame, alt) = super::resync_frame(&full_screen, b"cursor moves only", 24);
        assert!(alt, "a full-screen program draws itself when asked");
        assert_eq!(
            frame,
            b"\x1bc\x1b[?1049h".to_vec(),
            "and its ring is not a screen to replay"
        );
    }

    #[test]
    fn a_full_screen_session_replays_the_reset_and_not_the_ring() {
        // A program in the alternate screen draws with cursor moves, so its ring
        // is not a screen and has no line to cut on: replaying it shipped four
        // megabytes to the phone on every attach. The repaint is the kick's job.
        assert_eq!(
            super::replay_reset(b"\x1b[?1049h"),
            b"\x1bc\x1b[?1049h".to_vec()
        );
    }
}
