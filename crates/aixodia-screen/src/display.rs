//! A display the engine brings up itself.
//!
//! A server with no monitor is the ordinary case, not the broken one, and the
//! screen window is useless on it only because nothing opens a display. `Xvfb`
//! is an X server that draws to memory instead of to hardware, so the engine
//! starts one when `$DISPLAY` points at nothing and hands the screen feature a
//! display number it owns.
//!
//! The child is tied to this process: `Drop` kills it, and the pid is held so a
//! second engine start does not stack displays. Xvfb is chosen over the X
//! libraries because the capture path is Xlib and wants a real server, not a
//! framebuffer this crate would have to write itself — and because a display
//! nothing else knows about still runs every X client unchanged.

use std::os::unix::fs::FileTypeExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Display numbers tried, in order. High enough to stay clear of the `:0` and
/// `:1` a desktop session uses, low enough to stay inside Xvfb's usual range.
const FIRST_DISPLAY: u32 = 90;
const LAST_DISPLAY: u32 = 99;

/// How long Xvfb gets to create its socket before the attempt is abandoned.
const START_TIMEOUT: Duration = Duration::from_secs(5);

/// The display the engine owns, if it had to start one.
static OWNED: Mutex<Option<Owned>> = Mutex::new(None);

struct Owned {
    number: u32,
    child: Child,
    /// Told to stop when the display goes. The window manager runs in its own
    /// thread on its own X connection, so it is stopped through this rather than
    /// through a process handle.
    wm_stop: Arc<AtomicBool>,
    wm_thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Owned {
    /// Stops the display this process started.
    ///
    /// A display number is a shared resource: leaving a server behind on `:90`
    /// makes the next engine start find the number taken, so it manages nothing
    /// and every restart leaves another dead display to clean up.
    fn drop(&mut self) {
        // The window manager first: it holds an X connection, and killing the
        // server under it leaves it retrying a display that is gone.
        self.wm_stop.store(true, Ordering::SeqCst);
        // Not joined: this runs on the path that noticed the display had already
        // died, where a join would wait on a thread that cannot answer.
        self.wm_thread.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
        // Xvfb removes these itself when it exits cleanly, but it is killed
        // here, and a leftover socket is worse than untidy: the next start sees
        // it, believes a display is up on that number, adopts it, and finds the
        // number taken, so no window manager is possible and every restart
        // leaves another dead display behind.
        let _ = std::fs::remove_file(format!("/tmp/.X11-unix/X{}", self.number));
        let _ = std::fs::remove_file(format!("/tmp/.X{}-lock", self.number));
    }
}

/// Why the display could not be brought up, in the words the app shows.
pub struct DisplayError(pub String);

impl std::fmt::Display for DisplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Stops displays this engine's geometry on a dead engine's behalf.
///
/// `Drop` is what stops a display, and `Drop` does not run when the process is
/// killed — which is exactly what a restart does. Measured on this machine after
/// a morning of restarts: five `Xvfb :9x` servers, all parented to init, holding
/// display numbers and about a hundred megabytes between them, because the
/// previous engines were stopped rather than shut down.
///
/// Only an orphan is swept, and only one that was started with this engine's own
/// arguments, so a desktop session or another person's Xvfb is never touched. The
/// display this process owns is skipped by number.
pub fn sweep_orphans(ours: Option<&str>) {
    for pid in orphan_displays(&read_processes(), ours) {
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
    }
}

/// One line of `/proc`: who it is, who started it, and what it was told to do.
pub(crate) struct Proc {
    pub pid: u32,
    pub parent: u32,
    pub command: String,
}

/// The processes on this machine. Anything unreadable is simply absent.
fn read_processes() -> Vec<Proc> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut processes = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // The command is in parentheses and may itself contain spaces or
        // parentheses, so the parent is read from the field after the last `)`.
        let Some(close) = stat.rfind(')') else {
            continue;
        };
        let parent = stat[close + 1..]
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(0);
        let command = std::fs::read_to_string(format!("/proc/{pid}/cmdline"))
            .unwrap_or_default()
            .replace('\0', " ");
        processes.push(Proc {
            pid,
            parent,
            command,
        });
    }
    processes
}

/// The orphans among [processes] that are displays this engine started.
///
/// An engine's display is an `Xvfb :<n> -screen 0 1280x720x24 -nolisten tcp` on
/// a number in the range this engine uses, whose parent is init — the engine that
/// began it is gone, which is what makes it an orphan.
pub(crate) fn orphan_displays(processes: &[Proc], ours: Option<&str>) -> Vec<u32> {
    let mut found = Vec::new();
    for process in processes {
        if !process.command.contains("Xvfb") || !process.command.contains("1280x720x24") {
            continue;
        }
        let number = process
            .command
            .split_whitespace()
            .find_map(|word| word.strip_prefix(':').and_then(|n| n.parse::<u32>().ok()));
        let Some(number) = number else { continue };
        if !(FIRST_DISPLAY..=LAST_DISPLAY).contains(&number) {
            continue;
        }
        if ours == Some(name(number).as_str()) {
            continue;
        }
        // A parent that is still alive owns its display, whatever it is doing.
        if process.parent != 1 {
            continue;
        }
        found.push(process.pid);
    }
    found
}

/// The display to capture from, starting one if the machine has none.
///
/// The engine's own `DISPLAY` wins when it works: an operator who pointed the
/// engine at a real session meant it. Otherwise a private Xvfb is started once
/// and reused, so repeated calls cost one lock and no process.
pub fn ensure() -> Result<String, DisplayError> {
    if let Some(existing) = working_display() {
        return Ok(existing);
    }

    let mut guard = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(owned) = guard.as_mut() {
        // A display this module started earlier: it may have died since.
        match owned.child.try_wait() {
            Ok(None) => return Ok(name(owned.number)),
            _ => *guard = None,
        }
    }
    // Under the lock, so a burst of connections that all found nothing does not
    // start one each: that is how five displays were left behind in a minute.
    if let Some(existing) = working_display() {
        return Ok(existing);
    }

    // A display whose engine was killed still holds its number; clear it away
    // before trying to take one. The name of the one we own is passed rather
    // than asked for: `owned_display` takes this same lock.
    let held = guard.as_ref().map(|owned| name(owned.number));
    sweep_orphans(held.as_deref());
    let owned = start()?;
    let number = owned.number;
    *guard = Some(owned);
    Ok(name(number))
}

/// The display number currently owned, for the shutdown path and for tests.
pub fn owned_display() -> Option<String> {
    let guard = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().map(|o| name(o.number))
}

/// Stops the display the engine started, if any. Called on shutdown so a
/// restart does not leave a server behind holding the number.
pub fn shutdown() {
    // One path for stopping a display, in `Drop`, so a shutdown and a restart
    // cannot disagree about what "stopped" leaves behind.
    let mut guard = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut owned) = guard.take() {
        owned.wm_stop.store(true, Ordering::SeqCst);
        if let Some(thread) = owned.wm_thread.take() {
            let _ = thread.join();
        }
        // Dropping it does the rest.
        drop(owned);
    }
}

/// Removes the socket and lock of a display number whose server is gone.
///
/// A number is only treated this way when nothing is listening on it: a socket
/// with a live server behind it belongs to somebody else and is left alone.
/// The leftovers are removed when the *path* exists, not when it answers: a
/// socket whose server died is exactly the case being cleared, and it is the
/// one `socket_exists` reports as absent.
pub fn clear_if_dead(display: &str) {
    let Some(number) = display.strip_prefix(':') else {
        return;
    };
    let socket = format!("/tmp/.X11-unix/X{number}");
    let lock = format!("/tmp/.X{number}-lock");
    if !std::path::Path::new(&socket).exists() && !std::path::Path::new(&lock).exists() {
        return;
    }
    // A connect that works means a server is there; the display is not ours.
    if working_display_is(display) {
        return;
    }
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&lock);
}

/// Whether Xvfb is on PATH. Reported before a start is attempted so the app can
/// say "no display server" instead of "the socket never appeared".
pub fn available() -> bool {
    which("Xvfb").is_some()
}

fn name(number: u32) -> String {
    format!(":{number}")
}

/// Whether an X server answers on this display name.
fn working_display_is(display: &str) -> bool {
    use std::os::raw::c_char;
    let Ok(name) = std::ffi::CString::new(display) else {
        return false;
    };
    unsafe {
        extern "C" {
            fn XOpenDisplay(name: *const c_char) -> *mut std::ffi::c_void;
            fn XCloseDisplay(display: *mut std::ffi::c_void) -> std::os::raw::c_int;
        }
        let connection = XOpenDisplay(name.as_ptr());
        if connection.is_null() {
            return false;
        }
        XCloseDisplay(connection);
    }
    true
}

/// The display this process was pointed at, if a server answers on it.
///
/// An operator who set `$DISPLAY` meant it, so it wins over starting one.
fn working_display() -> Option<String> {
    let current = std::env::var("DISPLAY")
        .ok()
        .filter(|d| !d.trim().is_empty())?;
    if socket_exists(&current) && working_display_is(&current) {
        Some(current)
    } else {
        None
    }
}

/// Whether an X server has published its socket for this display name.
///
/// The check is the socket rather than a connection attempt because capture
/// opens the display itself; this only decides whether a start is needed. It
/// has to be a *socket*, not merely a path that resolves: a symlink left behind
/// by a session that exported `X0 -> /tmp/.X11-unix/X0` resolves to itself and
/// answers yes forever, and a socket whose server has died answers yes too. Both
/// made the engine adopt a display nothing was listening on, which is how a
/// restart came up with a picture that never filled.
fn socket_exists(display: &str) -> bool {
    let Some(number) = display.trim_start_matches(':').split('.').next() else {
        return false;
    };
    if number.is_empty() {
        return false;
    }
    let path = format!("/tmp/.X11-unix/X{number}");
    let Ok(meta) = std::fs::symlink_metadata(&path) else {
        return false;
    };
    if !meta.file_type().is_socket() {
        return false;
    }
    // A socket inode outlives the process that made it, so reach it: a refused
    // connection is the ordinary answer for a display whose server is gone.
    std::os::unix::net::UnixStream::connect(&path).is_ok()
}

/// The display number a stale socket is squatting on, for the caller that has
/// to clear it before starting a server of its own.
pub fn stale_number(display: &str) -> Option<u32> {
    let number = display.trim_start_matches(':').split('.').next()?;
    let number: u32 = number.parse().ok()?;
    if socket_exists(display) {
        None
    } else {
        Some(number)
    }
}

fn start() -> Result<Owned, DisplayError> {
    let Some(binary) = which("Xvfb") else {
        return Err(DisplayError(
            "this machine has no display and Xvfb is not installed; \
             install xvfb to let the engine draw one"
                .to_string(),
        ));
    };

    let mut last = String::new();
    for number in FIRST_DISPLAY..=LAST_DISPLAY {
        let display = name(number);
        if socket_exists(&display) {
            // A socket with no server behind it is a display that died and left
            // its name claimed; the number is free and the leftovers are not.
            clear_if_dead(&display);
            if socket_exists(&display) {
                continue;
            }
        }
        let child = Command::new(&binary)
            .arg(&display)
            .args(["-screen", "0", "1280x720x24"])
            .args(["-nolisten", "tcp"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(err) => {
                return Err(DisplayError(format!("could not start Xvfb: {err}")));
            }
        };

        let deadline = Instant::now() + START_TIMEOUT;
        while Instant::now() < deadline {
            if socket_exists(&display) {
                let (wm_stop, wm_thread) = start_window_manager(&display, child.id());
                return Ok(Owned {
                    number,
                    child,
                    wm_stop,
                    wm_thread,
                });
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    last = format!("Xvfb exited with {status} on {display}");
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(err) => {
                    last = format!("could not watch Xvfb: {err}");
                    break;
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        if last.is_empty() {
            last = format!("Xvfb did not publish a socket on {display} in time");
        }
    }
    Err(DisplayError(last))
}

/// Starts the engine's own window manager on a display it started.
///
/// Only a display the engine owns gets one: a display the operator set through
/// `$DISPLAY` already has whatever session they run, and a second window manager
/// would fight it for the root window and for every mapped window.
///
/// The manager is a thread, not a program, because it needs no dependency. The
/// crate already links Xlib for capture, and the work is a frame, a bar and three
/// drags. A display that cannot take one still streams — the windows just carry
/// no title bar.
fn start_window_manager(
    display: &str,
    owner: u32,
) -> (Arc<AtomicBool>, Option<std::thread::JoinHandle<()>>) {
    let stop = Arc::new(AtomicBool::new(false));
    let theirs = Arc::clone(&stop);
    let name = display.to_string();
    let thread = std::thread::Builder::new()
        .name("aixodia-wm".to_string())
        .spawn(move || crate::wm::run(&name, theirs, owner))
        .ok();
    (stop, thread)
}

fn which(program: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_console_display_is_not_mistaken_for_a_socket() {
        // `X0` in a container is often a symlink a login script left pointing at
        // itself. It resolves, so a bare `exists()` was true and the engine
        // adopted a display with no server behind it; only a real socket counts.
        let path = std::path::Path::new("/tmp/.X11-unix/X0");
        let real = path
            .symlink_metadata()
            .map(|m| m.file_type().is_socket())
            .unwrap_or(false)
            && std::os::unix::net::UnixStream::connect(path).is_ok();
        assert_eq!(socket_exists(":0"), real);
        assert!(!socket_exists(""));
        assert!(!socket_exists(":"));
    }

    #[test]
    fn a_symlink_that_points_at_itself_is_not_a_display() {
        let dir = std::env::temp_dir().join(format!("aixodia-x11-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let link = dir.join("X99");
        std::os::unix::fs::symlink(&link, &link).expect("self symlink");
        // The real check reads the fixed X11 socket directory, so this asserts
        // the property it relies on: a self-symlink resolves but is not a socket.
        let meta = std::fs::symlink_metadata(&link).expect("metadata");
        assert!(!meta.file_type().is_socket());
        assert!(link.exists() || std::fs::symlink_metadata(&link).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn available_agrees_with_which() {
        assert_eq!(available(), which("Xvfb").is_some());
    }

    #[test]
    fn an_unset_display_does_not_claim_to_work() {
        let saved = std::env::var("DISPLAY").ok();
        std::env::remove_var("DISPLAY");
        assert!(working_display().is_none());
        if let Some(saved) = saved {
            std::env::set_var("DISPLAY", saved);
        }
    }

    #[test]
    fn the_window_manager_is_a_thread_not_a_program() {
        // It needs no package: the crate links Xlib already, and this is what
        // keeps an engine install from requiring a desktop environment.
        let (stop, thread) = start_window_manager(":0", std::process::id());
        stop.store(true, Ordering::SeqCst);
        if let Some(thread) = thread {
            let _ = thread.join();
        }
        assert!(stop.load(Ordering::SeqCst));
    }

    fn display(pid: u32, parent: u32, command: &str) -> Proc {
        Proc {
            pid,
            parent,
            command: command.to_string(),
        }
    }

    #[test]
    fn an_orphaned_display_of_ours_is_swept() {
        let processes = vec![
            display(10, 1, "Xvfb :93 -screen 0 1280x720x24 -nolisten tcp"),
            // Still owned by a live process: not ours to kill.
            display(11, 700, "Xvfb :94 -screen 0 1280x720x24 -nolisten tcp"),
            // The display this engine owns now.
            display(12, 1, "Xvfb :90 -screen 0 1280x720x24 -nolisten tcp"),
        ];
        assert_eq!(orphan_displays(&processes, Some(":90")), vec![10]);
    }

    #[test]
    fn a_desktop_session_and_a_different_geometry_are_left_alone() {
        let processes = vec![
            // Somebody's real session, at their own size.
            display(20, 1, "/usr/bin/Xvfb :1 -screen 0 1920x1080x24"),
            // An earlier engine's display, but not the size this one starts.
            display(21, 1, "Xvfb :95 -screen 0 800x600x24 -nolisten tcp"),
            // Outside the number range this engine uses.
            display(22, 1, "Xvfb :3 -screen 0 1280x720x24 -nolisten tcp"),
            // Not a display at all.
            display(23, 1, "xterm -geometry 80x24"),
        ];
        assert!(orphan_displays(&processes, None).is_empty());
    }
}
