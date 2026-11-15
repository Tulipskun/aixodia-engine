//! Keeping shells in a private tmux server so they survive an engine restart.
//!
//! Port of `io/terminal/tmux.go`.

use std::process::{Command, Stdio};
use std::time::Duration;

use crate::pty::Pty;

pub const TMUX_SOCKET: &str = "aixodia";

/// Never give the Cloudflare account token to the persistent tmux server or
/// to tmux client processes. A user shell must not be able to read the Engine's
/// Cloudflare credential from its inherited environment.
fn tmux_command() -> Command {
    let mut command = Command::new("tmux");
    command.env_remove("CF_TOKEN");
    command
}

/// Remove a token inherited by a tmux server from an older Engine process,
/// including per-session copies, before adopting its existing sessions.
fn scrub_master_token() {
    let _ = tmux_command()
        .args(["-L", TMUX_SOCKET, "set-environment", "-gu", "CF_TOKEN"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let Ok(output) = tmux_command()
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
        let _ = tmux_command()
            .args([
                "-L",
                TMUX_SOCKET,
                "set-environment",
                "-u",
                "-t",
                name,
                "CF_TOKEN",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

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
    let _ = tmux_command()
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
        let output = tmux_command()
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
    let output = tmux_command()
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
    let output = tmux_command()
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
    let output = tmux_command()
        .args([
            "-L",
            TMUX_SOCKET,
            "list-sessions",
            "-F",
            // Fields are separated by a real tab. A `\u{1f}` here does not travel:
            // tmux prints control characters in a format string as the four
            // character escape `\037`, so the separator never matched, every
            // line was one field, and the session list came back empty.
            "#{session_name}\t#{pane_pid}\t#{pane_current_path}\t#{session_created}\t#{pane_title}",
        ])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_list(&text)
}

/// Reads the output of `list-sessions -F` into sessions.
///
/// tmux escapes control characters in a format string, so a separator that is
/// not printable never arrives as itself and every line reads as one field. The
/// separator here is a tab, which does travel, and a line without the expected
/// number of fields is skipped rather than guessed at.
fn parse_list(text: &str) -> Vec<TmuxSession> {
    let mut out = Vec::new();
    for line in text.trim().split('\n') {
        let fields: Vec<&str> = line.splitn(5, '\t').collect();
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
    // A pane inherits the environment of the tmux *server*, which started from
    // whatever shell first opened this socket and keeps that environment for as
    // long as it lives. A server booted without a locale gives every pane after
    // it the `C` locale, where text is measured in bytes: `printf '%-10s'` pads
    // Thai by byte length so the columns land wrong, and `wc -L` answers 0. The
    // locale is therefore set on the session itself rather than left to the
    // server, and only on this socket's sessions.
    let locale = crate::pty::utf8_locale();
    args.push("-e".to_string());
    args.push(format!("LANG={locale}"));
    args.push("-e".to_string());
    args.push(format!("LC_CTYPE={locale}"));
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
    let _ = tmux_command()
        .args(["-L", TMUX_SOCKET, "set-option", "-g", "mouse", "off"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Turns the status bar off on every session already on the server. Shells
/// outlive an engine restart, so the sessions this boot merely adopts are the
/// ones a previous build may have left with the bar showing.
pub fn hide_status_bars() {
    scrub_master_token();
    let Ok(output) = tmux_command()
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
        let _ = tmux_command()
            .args(["-L", TMUX_SOCKET, "set-option", "-t", name, "status", "off"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_session_on_a_tab_separated_line_is_read() {
        // The shape tmux actually prints: fields joined by tabs.
        let text = "aix-first\t111\t/root\t1700000000\tpi - root\naix-second\t222\t/home/pi\t1700000001\t\n";
        let sessions = parse_list(text);
        assert_eq!(sessions.len(), 2, "both sessions should be listed");
        assert_eq!(sessions[0].id, "first");
        assert_eq!(sessions[0].pid, 111);
        assert_eq!(sessions[0].cwd, "/root");
        assert_eq!(sessions[0].created_at, 1700000000);
        assert_eq!(sessions[0].title, "pi - root");
        assert_eq!(sessions[1].id, "second");
        assert_eq!(sessions[1].title, "");
    }

    #[test]
    fn an_escaped_separator_is_not_a_separator() {
        // tmux prints a control character in a format string as the four
        // characters `\037`. Treating that as the separator made every line one
        // field, so this test proves the mistake stays visible: such a line has
        // no tab and is skipped instead of producing a bogus session.
        let text = "aix-thing\\037111\\037/root\\0371700000000\\037\n";
        assert!(
            parse_list(text).is_empty(),
            "a line with no real separator must not be read as a session"
        );
    }

    #[test]
    fn a_session_name_without_the_prefix_is_skipped() {
        let text = "someone-elses\t111\t/root\t1700000000\t\naix-mine\t222\t/root\t1700000001\t\n";
        let sessions = parse_list(text);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "mine");
    }
}
