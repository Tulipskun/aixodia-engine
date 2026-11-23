//! Starting a program in the pane's compositor.
//!
//! The pane is a compositor, not a desktop somebody is sitting at: the programs
//! it shows are launched by the engine, with the compositor's socket and runtime
//! directory in their environment and no X display at all. What used to be here
//! was a strip along the bottom of an X screen; with the strip gone, these are
//! the parts of it that were never about X.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// Whether the process that wrote a Chrome profile lock is gone.
///
/// Chrome's lock is a symlink whose target is `<hostname>-<pid>`. A lock left by
/// a machine that no longer exists, or by a pid that is not running, is stale:
/// nothing is holding the profile and Chrome would still refuse to start.
fn lock_is_stale(target: &str, host: &str, alive: impl Fn(u32) -> bool) -> bool {
    let Some((holder, pid)) = target.rsplit_once('-') else {
        // A lock that is not a host and a pid is not one this can judge.
        return false;
    };
    if holder != host {
        return true;
    }
    match pid.parse::<u32>() {
        Ok(pid) => !alive(pid),
        Err(_) => false,
    }
}

/// Removes the profile locks a dead Chrome left behind.
///
/// The three links Chrome writes are removed together: a leftover socket or
/// cookie with no lock makes Chrome fail in a different way.
fn clear_stale_lock() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let profile = Path::new(&home).join(".config/google-chrome");
    let lock = profile.join("SingletonLock");
    let Ok(target) = std::fs::read_link(&lock) else {
        return;
    };
    let host = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|name| name.trim().to_string())
        .unwrap_or_default();
    let stale = lock_is_stale(&target.to_string_lossy(), &host, |pid| {
        Path::new(&format!("/proc/{pid}")).exists()
    });
    if !stale {
        return;
    }
    for name in ["SingletonLock", "SingletonSocket", "SingletonCookie"] {
        let _ = std::fs::remove_file(profile.join(name));
    }
    tracing::info!("browser: cleared a lock left by a Chrome that is gone ({target:?})");
}

/// A session bus for the programs this pane starts, once per engine.
///
/// chromium asks the session bus about the desktop portal, the secret service and
/// power management many times a second and, with no bus at all, it retries every
/// one of them: measured in the browser's own log, hundreds of
/// "Failed to connect to the bus" lines in a few seconds of idle. A private bus
/// costs one process and turns that noise into one answer — "nothing is there".
fn session_bus() -> Option<String> {
    static BUS: OnceLock<Option<String>> = OnceLock::new();
    BUS.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("aixodia-bus-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        let socket = dir.join("bus");
        let address = format!("unix:path={}", socket.display());
        // Not `--fork`: a daemon that detaches leaves this engine unable to say
        // whether it is alive, and `--print-address` through a pipe from a
        // forked child was measured as an empty string with no socket behind it.
        Command::new("dbus-daemon")
            .args(["--session", "--address"])
            .arg(&address)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        // The socket is the proof: the address is worth nothing until it exists.
        for _ in 0..50 {
            if socket.exists() {
                return Some(address);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        tracing::warn!("the pane's session bus did not come up; programs will retry one");
        None
    })
    .clone()
}

pub fn launch_in_compositor(program: &str, runtime_dir: &std::path::Path, socket: &str) {
    let (binary, args): (&str, Vec<&str>) = match program {
        "browser" => (
            "chromium",
            vec![
                "--ozone-platform=wayland",
                // There is no GPU here and the browser's own log says its GPU
                // channel fails to open; asking it not to try is honest, and it
                // keeps the software path the only path.
                "--disable-gpu",
                "--no-sandbox",
                // The first run's "Welcome" dialog and its "restore pages?"
                // bubble are windows on a desktop nobody is sitting at.
                "--no-first-run",
                "--hide-crash-restore-bubble",
                "--window-size=1200,640",
                // Without a page to open, chromium starts with no window at all:
                // the pane then shows the desktop and nothing else, which looks
                // exactly like a compositor that does not work.
                "about:blank",
            ],
        ),
        other => {
            tracing::warn!("{other}: a terminal needs an X server, and this pane has none");
            return;
        }
    };
    if binary == "chromium" {
        clear_stale_lock();
    }
    // What a client says when it dies belongs in a file. Measured: chromium was
    // killed by its own SIGSEGV while typing into an address bar, and a log of
    // nothing at all left nothing to read — the same silence as an xterm that
    // exits with no window and no error.
    let (out, err) = match std::fs::File::create(runtime_dir.join(format!("{program}.log"))) {
        Ok(log) => match log.try_clone() {
            Ok(clone) => (Stdio::from(clone), Stdio::from(log)),
            Err(_) => (Stdio::from(log), Stdio::null()),
        },
        Err(_) => (Stdio::null(), Stdio::null()),
    };
    let mut command = Command::new(binary);
    command
        .args(args)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("WAYLAND_DISPLAY", socket)
        .env_remove("DISPLAY");
    if let Some(bus) = session_bus() {
        command.env("DBUS_SESSION_BUS_ADDRESS", bus);
    }
    let started = command.stdin(Stdio::null()).stdout(out).stderr(err).spawn();
    match started {
        Ok(mut child) => {
            std::thread::spawn(move || {
                if let Ok(status) = child.wait() {
                    if !status.success() {
                        tracing::warn!("{binary} exited with {status}");
                    }
                }
            });
        }
        Err(err) => tracing::warn!("could not start {program} ({binary}) in the compositor: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lock_from_a_machine_that_is_gone_is_stale() {
        // Measured after a container was recreated: the old hostname, a pid that
        // cannot exist here, and Chrome refusing to start.
        assert!(lock_is_stale("7d167a919da1-361937", "0f5fe2a9b582", |_| {
            true
        }));
    }

    #[test]
    fn a_lock_from_a_dead_pid_on_this_machine_is_stale() {
        assert!(lock_is_stale("box-4242", "box", |pid| pid != 4242));
    }

    #[test]
    fn a_live_chrome_keeps_its_lock() {
        // Taking the lock from a running Chrome would let two of them corrupt the
        // profile, which is worse than not starting.
        assert!(!lock_is_stale("box-4242", "box", |pid| pid == 4242));
    }

    #[test]
    fn a_lock_that_is_not_a_host_and_a_pid_is_left_alone() {
        assert!(!lock_is_stale("nonsense", "box", |_| false));
    }
}
