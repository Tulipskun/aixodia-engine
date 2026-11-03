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
    args.push(name.clone());
    args.push("status".to_string());
    args.push("off".to_string());
    // The phone's drag is a wheel, and only the mouse mode lets a pane's own
    // history be read with it: the program inside repaints in place, so the
    // lines above it are in tmux and nowhere else. Without this the wheel bytes
    // reach the program, it has no use for them, and the reader cannot scroll.
    args.push(";".to_string());
    args.push("set-option".to_string());
    args.push("-g".to_string());
    args.push("mouse".to_string());
    args.push("on".to_string());
    Pty::start("tmux", &args, cols, rows, dir)
}

/// Lets the wheel page a pane's history. Sessions outlive an engine restart, so
/// a server this boot merely adopted may still have the mode off.
pub fn enable_mouse() {
    let _ = Command::new("tmux")
        .args(["-L", TMUX_SOCKET, "set-option", "-g", "mouse", "on"])
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
