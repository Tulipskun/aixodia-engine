//! A shell that outlives the engine.
//!
//! A pty belongs to whichever process holds its master descriptor. When the
//! engine holds it, an engine that dies closes the descriptor and the shell dies
//! with it — the phone's tab then reconnects to nothing, and typing again means
//! starting over.
//!
//! So the descriptor is not held by the engine. The engine starts the same
//! binary again as `term-holder`, hands it the shell, and talks to it over a
//! unix socket. The holder is a child of the engine but not its prisoner: it
//! keeps the pty and keeps reading, and it is still there when a new engine
//! connects. Nothing about an engine restart reaches the shell; the pane simply
//! has a different reader afterwards.
//!
//! The socket carries the same three things a subscriber needs — output as it
//! arrives, input to write, and a resize — so the engine treats a holder exactly
//! like a local pty. Output produced while no engine was connected is not lost
//! either: the holder keeps a ring of it and replays the tail on attach, which is
//! what makes the scrollback of a rebooted engine look like a paused one.

use std::io::{Error as IoError, ErrorKind, Read, Write};
use std::os::unix::io::AsRawFd as _;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::pty::Pty;

/// How much output a holder keeps for a client that has not attached yet.
const RING_CAP: usize = 4 << 20;

/// A frame is a one-byte kind and its payload.
pub const KIND_OUTPUT: u8 = b'o';
const KIND_INPUT: u8 = b'i';
const KIND_RESIZE: u8 = b'r';
/// Ask the shell's program to paint its screen again, at the size it has.
const KIND_KICK: u8 = b'k';
/// End the shell, and with it the holder.
const KIND_END: u8 = b'e';
pub const KIND_EXIT: u8 = b'x';

/// Where holder sockets and pid files live.
///
/// The engine's own state directory, so a terminal's socket sits beside the
/// engine's pid and log rather than in a world-writable `/tmp` shared with
/// anything else that happens to use the same name. `HOME` is the one variable
/// the engine and the holder it starts both already have.
fn holder_dir() -> PathBuf {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".aixodia")
        .join("terminals")
}

/// Where a holder for `id` advertises itself.
pub fn socket_path(id: &str) -> PathBuf {
    let mut name = String::new();
    for c in id.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            name.push(c);
        } else {
            name.push('_');
        }
    }
    holder_dir().join(format!("{name}.sock"))
}

/// The engine connection the shell's output is being handed to.
struct Attached {
    /// Tells this connection from the one that replaces it. The old connection
    /// notices it was closed after the new one is already attached, and must not
    /// take the slot with it.
    id: u64,
    stream: UnixStream,
}

/// The ring a holder keeps while nobody is attached.
struct Ring {
    data: Vec<u8>,
}

impl Ring {
    fn new() -> Self {
        Self {
            data: Vec::with_capacity(64 << 10),
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
        if self.data.len() > RING_CAP {
            // The tail is what a returning client needs, so the oldest bytes go.
            let cut = self.data.len() - RING_CAP;
            self.data.drain(..cut);
        }
    }

    fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.data)
    }
}

/// Runs the holder for one shell and never returns.
///
/// This is the body of the `term-holder` subcommand: it owns the pty, serves
/// whichever engine connects, and outlives any number of them.
pub fn run(id: &str, shell: &str, cols: u16, rows: u16, dir: Option<&str>) -> IoError {
    let path = socket_path(id);
    if let Some(parent) = path.parent() {
        if let Err(err) = std::fs::create_dir_all(parent) {
            return err;
        }
    }
    // A socket file left by a holder that has since died would make bind fail.
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = UnixListener::bind(&path) else {
        return IoError::last_os_error();
    };
    // Only this user may talk to a shell.
    let _ = std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600));

    let Ok(pty) = Pty::start(shell, &["-l".to_string()], cols, rows, dir) else {
        let _ = std::fs::remove_file(&path);
        return IoError::last_os_error();
    };
    let _ = std::fs::write(socket_path(id).with_extension("pid"), pty.pid().to_string());
    let pty = Arc::new(pty);
    let ring = Arc::new(Mutex::new(Ring::new()));
    let dead = Arc::new(AtomicBool::new(false));

    // One reader for the whole life of the shell. Output goes into the ring
    // whether or not anyone is attached, and to the attached client when there
    // is one, so a reconnect loses nothing it could have shown.
    let reader_pty = pty.clone();
    let reader_ring = ring.clone();
    let reader_dead = dead.clone();
    let attached: Arc<Mutex<Option<Attached>>> = Arc::new(Mutex::new(None));
    let writer = attached.clone();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 32 << 10];
        loop {
            if reader_pty.dead() {
                reader_dead.store(true, Ordering::SeqCst);
                if let Some(mut att) = writer.lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = frame(
                        &mut att.stream,
                        KIND_EXIT,
                        &reader_pty.exit_code().to_le_bytes(),
                    );
                }
                return;
            }
            // Wait for the pty to have something before reading it. Sleeping
            // instead capped this loop at one pass per sleep, and every byte the
            // shell printed waited for the next pass: a keystroke's echo cost a
            // full interval. `poll` returns the moment a byte is there.
            if !wait_readable(&reader_pty) {
                continue;
            }
            match reader_pty.read(&mut buf) {
                Ok(0) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Ok(n) => {
                    let bytes = &buf[..n];
                    reader_ring
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(bytes);
                    let mut slot = writer.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(att) = slot.as_mut() {
                        if frame(&mut att.stream, KIND_OUTPUT, bytes).is_err() {
                            *slot = None;
                        }
                    }
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {}
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(_) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        }
    });

    // Accept loop: one engine at a time. A new connection replaces the old one,
    // which is what lets a restarted engine take back a pane the previous one
    // was still holding when it died.
    let mut next_connection = 0u64;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        next_connection += 1;
        let mine = next_connection;
        // A connection whose engine died must not stop the next one from being
        // accepted: reading from a socket nobody is writing to blocks forever,
        // and the accept loop cannot wait for it.
        let pty = pty.clone();
        let ring = ring.clone();
        let attached = attached.clone();
        let dead = dead.clone();
        std::thread::spawn(move || {
            let _ = serve(stream, mine, &pty, &ring, &attached, &dead);
        });
    }
    let _ = std::fs::remove_file(&path);
    IoError::new(ErrorKind::Other, "the holder stopped accepting")
}

/// Serves one connected engine until it goes away.
fn serve(
    stream: UnixStream,
    mine: u64,
    pty: &Arc<Pty>,
    ring: &Arc<Mutex<Ring>>,
    attached: &Arc<Mutex<Option<Attached>>>,
    dead: &Arc<AtomicBool>,
) -> std::io::Result<()> {
    // Whatever happened while nobody was attached is handed over first, so the
    // phone sees the output it missed instead of a screen that forgot it.
    let replay = ring.lock().unwrap_or_else(|e| e.into_inner()).take();
    let mut out = stream.try_clone()?;
    if !replay.is_empty() {
        // A client that connected only to give an order — ending the shell — is
        // already gone when this lands, so the write fails. That says nothing
        // about the order, which is still in the socket to be read below; ending
        // the connection here is what left closed tabs' shells running.
        let _ = frame(&mut out, KIND_OUTPUT, &replay);
    }
    if dead.load(Ordering::SeqCst) {
        let _ = frame(&mut out, KIND_EXIT, &pty.exit_code().to_le_bytes());
        return Ok(());
    }
    *attached.lock().unwrap_or_else(|e| e.into_inner()) = Some(Attached {
        id: mine,
        stream: out,
    });

    let mut input = stream;
    let mut header = [0u8; 5];
    loop {
        if input.read_exact(&mut header).is_err() {
            break;
        }
        let kind = header[0];
        let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        if input.read_exact(&mut payload).is_err() {
            break;
        }
        match kind {
            KIND_INPUT => {
                if pty.write(&payload).is_err() {
                    break;
                }
            }
            KIND_RESIZE => {
                if payload.len() >= 4 {
                    let cols = u16::from_le_bytes([payload[0], payload[1]]);
                    let rows = u16::from_le_bytes([payload[2], payload[3]]);
                    let _ = pty.resize(cols, rows);
                    let _ = pty.kick();
                }
            }
            KIND_KICK => {
                let _ = pty.kick();
            }
            KIND_END => {
                // The shell is what this holder exists for. Ending it ends the
                // holder too: one that stayed would answer the next connect with
                // a pty whose shell is gone, and its pid file would name a pid
                // that no longer means anything.
                pty.close();
                std::process::exit(0);
            }
            _ => {}
        }
    }
    // Only the connection that is still the attached one may clear the slot. A
    // new engine attaches the moment its socket opens and this one only notices
    // its own close when the read fails — often afterwards. Clearing
    // unconditionally left the holder forwarding the shell to nobody, so the
    // pane went quiet until the engine itself was restarted.
    let mut slot = attached.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().map(|att| att.id) == Some(mine) {
        *slot = None;
    }
    Ok(())
}

/// The shell a holder is running, read from the process table.
///
/// The holder records it beside its socket so the engine can name the same pid
/// the tab was opened with, which is what makes "the shell is still the one you
/// left" checkable rather than assumed.
pub fn shell_pid(id: &str) -> Option<i32> {
    let path = socket_path(id).with_extension("pid");
    let text = std::fs::read_to_string(path).ok()?;
    text.trim().parse().ok()
}

/// Blocks until the pty has bytes, or reports that it is time to look at the
/// shell's liveness again.
fn wait_readable(pty: &Pty) -> bool {
    let mut pollfd = libc::pollfd {
        fd: pty.master().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ready = unsafe { libc::poll(&mut pollfd, 1, 250) };
    // A timeout is not an error: it is the tick that notices a dead shell.
    ready >= 0
}

fn frame(mut stream: &UnixStream, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    stream.write_all(&header)?;
    stream.write_all(payload)?;
    stream.flush()
}

/// Starts a holder for `id` and connects to it.
///
/// The holder is detached from the engine's own lifetime: it is started in its
/// own session, and its output streams are the engine's own close-on-exec files
/// so nothing of the engine survives inside it.
pub fn connect_or_start(
    id: &str,
    shell: &str,
    cols: u16,
    rows: u16,
    dir: Option<&str>,
) -> IoErrorResult<Holder> {
    let path = socket_path(id);
    if let Ok(stream) = UnixStream::connect(&path) {
        return Ok(Holder::new(stream, path));
    }
    let exe = std::env::current_exe()?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("term-holder")
        .arg(id)
        .arg(shell)
        .arg(cols.to_string())
        .arg(rows.to_string())
        .env_remove("CF_TOKEN")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    // The holder must not die with the engine, so it leads its own session and
    // never receives the engine's terminal signals.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(IoError::last_os_error());
            }
            // A holder owns a shell, not the engine's port. Descriptors the
            // engine marked inheritable for its own update would otherwise pass
            // through to the holder and then to the shell a user types into.
            close_inheritable_sockets();
            Ok(())
        });
    }
    let mut child = command.spawn()?;

    // The socket appears once the holder has bound it, which it does before
    // starting the shell, so a short wait is enough and a long one is a bug.
    for _ in 0..200 {
        if let Ok(stream) = UnixStream::connect(&path) {
            // Reaping here would tie the holder's exit to this process; it is
            // left to the holder's own session instead.
            drop(child.id());
            return Ok(Holder::new(stream, path));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    Err(IoError::new(
        ErrorKind::TimedOut,
        "the terminal holder did not come up",
    ))
}

type IoErrorResult<T> = std::io::Result<T>;

/// Marks every listening socket this process holds close-on-exec.
///
/// The engine clears that flag on its own listener while an update hands it
/// over, and the flag stays cleared, so a child started afterwards would hold
/// the port. This runs between `fork` and `exec`, where allocating is not safe:
/// every step is a fixed-size ioctl or fcntl on a bounded range of numbers, and
/// a descriptor that is not a listening socket is left alone.
fn close_inheritable_sockets() {
    // A ceiling well above any descriptor this process can realistically hold.
    const MAX_FD: i32 = 256;
    let mut fd: i32 = 0;
    while fd < MAX_FD {
        let mut accepting: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ACCEPTCONN,
                &mut accepting as *mut libc::c_int as *mut libc::c_void,
                &mut len,
            )
        };
        if rc == 0 && accepting != 0 {
            unsafe {
                let flags = libc::fcntl(fd, libc::F_GETFD);
                if flags >= 0 {
                    libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
                }
            }
        }
        fd += 1;
    }
}

/// Ends the shell held for `id`, whether or not this engine started it.
///
/// A shell outlives an engine restart, so a closed tab must reach the holder
/// rather than only the engine's map: otherwise the shell keeps running and the
/// next list hands back a tab the phone had already closed.
pub fn close(id: &str) {
    let path = socket_path(id);
    if let Ok(stream) = UnixStream::connect(&path) {
        let _ = frame(&stream, KIND_END, &[]);
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(path.with_extension("pid"));
}

/// The engine's end of a holder connection.
pub struct Holder {
    stream: UnixStream,
    path: PathBuf,
    /// Set by the holder's exit frame, so a shell that ended while the engine
    /// was away is reported as ended rather than as a connection that is quiet.
    finished: AtomicBool,
    exit_code: std::sync::atomic::AtomicI32,
}

/// The exit code of a shell whose holder has not said otherwise.
const NO_EXIT_CODE: i32 = -1;

impl Holder {
    fn new(stream: UnixStream, path: PathBuf) -> Self {
        let _ = stream.set_nonblocking(true);
        Self {
            stream,
            path,
            finished: AtomicBool::new(false),
            exit_code: std::sync::atomic::AtomicI32::new(NO_EXIT_CODE),
        }
    }

    /// Records the exit code the holder reported.
    pub fn set_exit_code(&self, code: i32) {
        self.exit_code.store(code, Ordering::SeqCst);
        self.finished.store(true, Ordering::SeqCst);
    }

    /// Whether the shell inside the holder has ended.
    pub fn finished(&self) -> bool {
        self.finished.load(Ordering::SeqCst)
    }

    pub fn exit_code(&self) -> i32 {
        self.exit_code.load(Ordering::SeqCst)
    }

    /// Detaches without ending the shell: the holder outlives this connection.
    pub fn close(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write_all(&self, data: &[u8]) -> std::io::Result<()> {
        let _ = self.stream.set_nonblocking(false);
        let result = frame(&self.stream, KIND_INPUT, data);
        let _ = self.stream.set_nonblocking(true);
        result
    }

    pub fn resize(&self, cols: u16, rows: u16) -> std::io::Result<()> {
        let mut payload = [0u8; 4];
        payload[0..2].copy_from_slice(&cols.to_le_bytes());
        payload[2..4].copy_from_slice(&rows.to_le_bytes());
        let _ = self.stream.set_nonblocking(false);
        let result = frame(&self.stream, KIND_RESIZE, &payload);
        let _ = self.stream.set_nonblocking(true);
        result
    }

    /// Asks the foreground program to paint its screen again, without moving it.
    ///
    /// A full-screen program owns its grid: a client that reattaches can only
    /// get that grid back by having the program draw it.
    pub fn kick(&self) -> std::io::Result<()> {
        let _ = self.stream.set_nonblocking(false);
        let result = frame(&self.stream, KIND_KICK, &[]);
        let _ = self.stream.set_nonblocking(true);
        result
    }

    /// Waits for the socket to have a frame, so a caller is woken by the byte
    /// arriving instead of polling for it.
    pub fn wait_readable(&self, timeout_ms: i32) -> std::io::Result<bool> {
        let mut pollfd = libc::pollfd {
            fd: self.stream.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if ready < 0 {
            let err = IoError::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(err);
        }
        Ok(ready > 0)
    }

    /// Reads one frame, or reports nothing yet.
    pub fn read_frame(&self) -> std::io::Result<Option<(u8, Vec<u8>)>> {
        let mut header = [0u8; 5];
        let mut stream = &self.stream;
        match stream.read_exact(&mut header) {
            Ok(()) => {}
            Err(err) if err.kind() == ErrorKind::WouldBlock => return Ok(None),
            Err(err) => return Err(err),
        }
        let kind = header[0];
        let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        let _ = self.stream.set_nonblocking(false);
        let read = stream.read_exact(&mut payload);
        let _ = self.stream.set_nonblocking(true);
        read?;
        Ok(Some((kind, payload)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_socket_name_has_no_separator() {
        let path = socket_path("../../etc/passwd");
        let name = path.file_name().unwrap().to_str().unwrap();
        assert!(!name.contains('/'), "{name} must not escape the holder dir");
    }
}
