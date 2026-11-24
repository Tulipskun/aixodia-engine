//! Starting a program in the pane's compositor.
//!
//! The pane is a compositor, not a desktop somebody is sitting at: the programs
//! it shows are launched by the engine, with the compositor's socket and runtime
//! directory in their environment and no X display at all. What used to be here
//! was a strip along the bottom of an X screen; with the strip gone, these are
//! the parts of it that were never about X.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};

/// The browser this pane started, if it is still running.
///
/// The pane has to be able to say whether its browser is alive: a browser that
/// exited leaves an empty desktop behind, and an empty desktop is what a broken
/// compositor looks like too.
static BROWSER: Mutex<Option<Child>> = Mutex::new(None);

/// Whether the browser is still running, having written down its death if not.
pub fn browser_running() -> bool {
    let mut held = BROWSER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(child) = held.as_mut() else {
        return false;
    };
    match child.try_wait() {
        Ok(None) => true,
        Ok(Some(status)) => {
            if !status.success() {
                tracing::warn!("chromium exited with {status}");
            }
            *held = None;
            false
        }
        Err(err) => {
            tracing::warn!("chromium could not be checked: {err}");
            *held = None;
            false
        }
    }
}

/// The page the pane's browser opens with.
///
/// Typing into chromium's own address bar kills chromium on this compositor —
/// measured twice, with the browser's own protocol log showing its autocomplete
/// popup being built one millisecond before the fault — while typing into a
/// page's input works. So the desktop's browser opens a page of ours with its
/// own address field, focused, and a keystroke never has to reach the omnibox.
const START_PAGE: &str = r#"<!doctype html>
<meta charset="utf-8">
<title>aixodia</title>
<style>
  html, body { margin: 0; height: 100%; background: #101a2b; color: #c9d3e8;
               font: 16px/1.5 system-ui, sans-serif; }
  main { max-width: 760px; margin: 0 auto; padding: 18vh 24px 0; }
  h1 { font-size: 22px; font-weight: 600; letter-spacing: .04em; color: #8fa4c9;
       margin: 0 0 28px; }
  input { width: 100%; box-sizing: border-box; padding: 18px 20px; font-size: 20px;
          color: #eef2fa; background: #1a2740; border: 1px solid #33425f;
          border-radius: 12px; outline: none; }
  input:focus { border-color: #4c6da8; }
  p { color: #6f829f; font-size: 13px; margin: 14px 0 0; }
  ul { list-style: none; padding: 0; margin: 40px 0 0; display: flex; gap: 10px;
       flex-wrap: wrap; }
  a { color: #a8bde0; text-decoration: none; background: #1a2740;
      border: 1px solid #2c3a56; border-radius: 10px; padding: 10px 14px; }
</style>
<main>
  <h1>AXODIA</h1>
  <input id="q" autofocus autocomplete="off" spellcheck="false"
         placeholder="ที่อยู่เว็บ หรือคำค้นหา">
  <p>กด Enter เพื่อไป &mdash; พิมพ์ในช่องนี้ ไม่ต้องแตะแถบที่อยู่ของเบราว์เซอร์</p>
  <ul>
    <li><a href="https://duckduckgo.com/">ค้นหา</a></li>
    <li><a href="https://news.ycombinator.com/">ข่าว</a></li>
    <li><a href="https://github.com/">github</a></li>
    <li><a href="https://www.youtube.com/">youtube</a></li>
  </ul>
</main>
<script>
  const q = document.getElementById('q');
  q.addEventListener('keydown', (event) => {
    if (event.key !== 'Enter') return;
    const text = q.value.trim();
    if (!text) return;
    const looks_like_a_site = /^[a-z0-9-]+(\.[a-z0-9-]+)+(\/.*)?$/i.test(text)
      || /^https?:\/\//i.test(text);
    location.href = looks_like_a_site
      ? (text.startsWith('http') ? text : 'https://' + text)
      : 'https://duckduckgo.com/?q=' + encodeURIComponent(text);
  });
</script>
"#;

/// Writes the page and hands back the address the browser should open.
///
/// A page on disk rather than a server: it has to be there before the browser
/// starts, and a pane that cannot write its own runtime directory has bigger
/// problems than a start page.
fn write_start_page(dir: &Path) -> Option<String> {
    let path = dir.join("start.html");
    std::fs::write(&path, START_PAGE).ok()?;
    Some(format!("file://{}", path.display()))
}

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

/// Whether any browser is running on this machine, ours or not.
///
/// The profile belongs to the machine, not to this process: a second chromium
/// sharing it is a corrupted profile — measured as a "Profile error occurred"
/// dialog and two windows arguing over one lock. So a lock is only cleared when
/// no browser at all is alive, and a launcher click is answered by the browser
/// that is already there.
fn a_browser_is_running() -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) else {
            continue;
        };
        let comm = comm.trim();
        if comm == "chrome" || comm == "chromium" || comm == "chrome_crashpad" {
            return true;
        }
    }
    false
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
    // A lock whose pid is gone can still belong to a browser that is alive under
    // another pid: chromium's lock is written once and its pid moves on with its
    // zygote. Taking it away then hands the profile to a second browser.
    if a_browser_is_running() {
        tracing::debug!("browser: a lock looks stale but a browser is running; left alone");
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
    let start = if program == "browser" {
        write_start_page(runtime_dir)
    } else {
        None
    };
    let start = start.as_deref().unwrap_or("about:blank");
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
                start,
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
        Ok(child) => {
            if program == "browser" {
                // Kept, not waited on: whether the browser is alive is a question
                // the pane asks while it runs, and a thread that only ever waits
                // can answer it once, at the end.
                let mut held = BROWSER.lock().unwrap_or_else(|e| e.into_inner());
                *held = Some(child);
            } else {
                let mut child = child;
                std::thread::spawn(move || {
                    if let Ok(status) = child.wait() {
                        if !status.success() {
                            tracing::warn!("{binary} exited with {status}");
                        }
                    }
                });
            }
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
