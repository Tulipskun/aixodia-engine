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

use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
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
}

/// Why the display could not be brought up, in the words the app shows.
pub struct DisplayError(pub String);

impl std::fmt::Display for DisplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
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
    let mut guard = OWNED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut owned) = guard.take() {
        let _ = owned.child.kill();
        let _ = owned.child.wait();
    }
}

/// Whether Xvfb is on PATH. Reported before a start is attempted so the app can
/// say "no display server" instead of "the socket never appeared".
pub fn available() -> bool {
    which("Xvfb").is_some()
}

fn name(number: u32) -> String {
    format!(":{number}")
}

/// An X server that already answers, if there is one.
fn working_display() -> Option<String> {
    let current = std::env::var("DISPLAY")
        .ok()
        .filter(|d| !d.trim().is_empty())?;
    if socket_exists(&current) {
        Some(current)
    } else {
        None
    }
}

/// Whether an X server has published its socket for this display name.
///
/// The check is the socket rather than a connection attempt because capture
/// opens the display itself; this only decides whether a start is needed.
fn socket_exists(display: &str) -> bool {
    let Some(number) = display.trim_start_matches(':').split('.').next() else {
        return false;
    };
    if number.is_empty() {
        return false;
    }
    std::path::Path::new(&format!("/tmp/.X11-unix/X{number}")).exists()
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
            continue;
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
                return Ok(Owned { number, child });
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
        assert!(socket_exists(":0") == std::path::Path::new("/tmp/.X11-unix/X0").exists());
        assert!(!socket_exists(""));
        assert!(!socket_exists(":"));
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
}
