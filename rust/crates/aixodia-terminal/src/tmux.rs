//! Keeping shells in a private tmux server so they survive an engine restart.
//!
//! Port of `io/terminal/tmux.go`.

use std::process::{Command, Stdio};
use std::time::Duration;

use crate::pty::Pty;

pub const TMUX_SOCKET: &str = "aixodia";

pub fn available() -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join("tmux");
                candidate.is_file()
            })
        })
        .unwrap_or(false)
}

pub fn tmux_name(id: &str) -> String {
    let mut name = String::from("aix-");
    for c in id.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            name.push(c);
        } else {
            name.push('_');
        }
    }
    name
}

/// Ends a shell for good. Closing the pty only detaches the client, so the tmux
/// session must be killed or the phone reattaches to a tab it closed.
pub fn kill(id: &str) {
    let _ = Command::new("tmux")
        .args(["-L", TMUX_SOCKET, "kill-session", "-t", &tmux_name(id)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// One tmux session as the terminal list reports it.
pub struct TmuxSession {
    pub id: String,
    pub pid: i32,
    pub cwd: String,
    pub created_at: i64,
    /// The program's own window title, if it set one.
    pub title: String,
}

/// The pid of the shell inside a tmux session.
///
/// The engine's pty runs a tmux *client*, so its pid is a property of one
/// connection: it changes on every attach and dies when the client detaches. The
/// pane's pid is the shell itself, and it is the same number before and after an
/// engine restart, which is what lets a tab keep naming one process.
///
/// The session is created by the server while the client is still starting up, so
/// a query that lands too early says nothing yet. Retries are bounded: the caller
/// falls back rather than waiting on a session that is not there.
pub fn pane_pid(id: &str) -> Option<i32> {
    let name = tmux_name(id);
    for attempt in 0..10 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        let output = Command::new("tmux")
            .args([
                "-L",
                TMUX_SOCKET,
                "display-message",
                "-p",
                "-t",
                &name,
                "#{pane_pid}",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            continue;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        match text.trim().parse::<i32>() {
            Ok(pid) if pid > 0 => return Some(pid),
            _ => continue,
        }
    }
    None
}

/// How far back a reattaching client is told to have seen, and the most bytes
/// that answer is allowed to cost. tmux clamps a start line deeper than the
/// pane's own history, so the number is a ceiling, not a promise.
const HISTORY_LINES: usize = 2000;
const HISTORY_BYTES: usize = 512 << 10;

/// Everything the pane holds: its history, then its screen, with the colors the
/// program last asked for.
///
/// The engine's ring only carries what this process has watched, so a tab that
/// survived a restart reconnects to a pane thick with lines and an empty ring.
/// tmux has them, and `capture-pane` is the only way to hand them over — which
/// is also what makes a drag instant: the phone pans a scrollback it actually
/// holds, and a pan is a repaint rather than a round trip to a server that
/// repaints on its own clock.
///
/// The tail is what matters, so an oversized capture keeps the end of it. The
/// cut lands on a line boundary: an escape sequence split in half would put the
/// client's screen into a state no program asked for.
pub fn capture_history(id: &str) -> Option<Vec<u8>> {
    let start = format!("-{}", HISTORY_LINES);
    let output = Command::new("tmux")
        .args([
            "-L",
            TMUX_SOCKET,
            "capture-pane",
            "-p",
            "-e",
            "-S",
            &start,
            "-t",
            &tmux_name(id),
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut text = output.stdout;
    if text.len() > HISTORY_BYTES {
        let mut from = text.len() - HISTORY_BYTES;
        while from < text.len() && text[from] != b'\n' {
            from += 1;
        }
        if from < text.len() {
            text = text.split_off(from + 1);
        }
    }
    // A trailing newline would scroll the client's own screen up one line at the
    // end of the replay, blanking the row the prompt is on.
    while text.last() == Some(&b'\n') {
        text.pop();
    }
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// What the program in the pane last called itself (OSC 0/2). tmux keeps the
/// title, so it survives a detached client, and `#{pane_title}` may carry the
/// padding tmux adds up to `set-titles-string`'s width.
pub fn pane_title(id: &str) -> Option<String> {
    let output = Command::new("tmux")
        .args([
            "-L",
            TMUX_SOCKET,
            "display-message",
            "-p",
            "-t",
            &tmux_name(id),
            "#{pane_title}",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let title = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

/// Sessions that survived an engine restart.
pub fn list() -> Vec<TmuxSession> {
    let output = Command::new("tmux")
        .args([
            "-L",
            TMUX_SOCKET,
            "list-sessions",
            "-F",
            "#{session_name}\u{1f}#{pane_pid}\u{1f}#{pane_current_path}\u{1f}#{session_created}\u{1f}#{pane_title}",
        ])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut out = Vec::new();
    for line in text.trim().split('\n') {
        let fields: Vec<&str> = line.splitn(5, '\u{1f}').collect();
        if fields.len() < 4 {
            continue;
        }
        let Some(name) = fields[0].strip_prefix("aix-") else {
            continue;
        };
        out.push(TmuxSession {
            id: name.to_string(),
            pid: fields[1].parse().unwrap_or(0),
            cwd: fields[2].to_string(),
            created_at: fields[3].parse().unwrap_or(0),
            title: fields
                .get(4)
                .map(|t| t.trim().to_string())
                .unwrap_or_default(),
        });
    }
    out
}

/// Opens a pty that carries a tmux client, creating the session when it is new.
pub fn start(
    id: &str,
    shell: &str,
    cols: u16,
    rows: u16,
    dir: Option<&str>,
) -> std::io::Result<Pty> {
    if !available() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "tmux is not installed",
        ));
    }
    let name = tmux_name(id);
    let mut args = vec![
        "-L".to_string(),
        TMUX_SOCKET.to_string(),
        "new-session".to_string(),
        "-A".to_string(),
        "-s".to_string(),
        name.clone(),
    ];
    if let Some(dir) = dir {
        args.push("-c".to_string());
        args.push(dir.to_string());
    }
    args.push(shell.to_string());
    args.push("-l".to_string());
    // The status bar spends a phone row on a session name and a clock, so it is
    // turned off in the same client round-trip that creates the session. A
    // separate `set-option` after the spawn loses a race: the server may not
    // exist yet, or may not have registered the name, and the failure is silent.
    args.push(";".to_string());
    args.push("set-option".to_string());
    args.push("-t".to_string());
    args.push(name);
    args.push("status".to_string());
    args.push("off".to_string());
    Pty::start("tmux", &args, cols, rows, dir)
}

/// Keeps tmux out of the phone's drags.
///
/// The mouse mode makes tmux announce mouse reporting to its client, so the
/// phone's emulator sends every drag as wheel notches into copy-mode — and the
/// pane's *state* moves instantly while its *paint* follows on tmux's own
/// redraw clock, measured here at a fifth of a second for a burst and more than
/// a second for a slow drag. The history belongs to the client instead, which
/// `capture_history` hands over at attach so a drag pans a screen it holds.
///
/// This is a global option, and panes outlive an engine restart, so the server
/// this boot merely adopted may still carry it on from a build that turned it on.
pub fn disable_mouse() {
    let _ = Command::new("tmux")
        .args(["-L", TMUX_SOCKET, "set-option", "-g", "mouse", "off"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Turns the status bar off on every session already on the server. Shells
/// outlive an engine restart, so the sessions this boot merely adopts are the
/// ones a previous build may have left with the bar showing.
pub fn hide_status_bars() {
    let Ok(output) = Command::new("tmux")
        .args(["-L", TMUX_SOCKET, "list-sessions", "-F", "#{session_name}"])
        .output()
    else {
        return;
    };
    if !output.status.success() {
        return;
    }
    for name in String::from_utf8_lossy(&output.stdout).lines() {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let _ = Command::new("tmux")
            .args(["-L", TMUX_SOCKET, "set-option", "-t", name, "status", "off"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}
