//! The screen pane's own display: a Wayland compositor instead of an X server.
//!
//! It produces a [`Frame`] of BGRX pixels and a set of pointer and key events
//! going the other way, so everything above it (the tiling encoder, the socket,
//! the RFB server) speaks in frames and buttons and never in protocol.
//!
//! What the host needs is nothing. There is no X server to install, no `xterm`, no
//! `XTest` to move a pointer nobody can see, and no X libraries linked into the
//! engine: the compositor *is* the display server and the window manager, and
//! clients draw into shared memory we already have.
//!
//! Keys keep their old vocabulary: above this module a key is an X keysym *name*
//! — `Return`, `BackSpace`, `a`, `A` — because that is what the phone and a VNC
//! viewer send. A Wayland client is told a keymap and then an evdev keycode, so
//! this module owns the one table that turns a name into a keycode and says
//! whether shift has to be held to make that name come out.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use aixodia_wayland::Compositor;

use crate::cursor;
use crate::frame::{Button, Frame, ScreenError};

/// Where clients find the compositor: `XDG_RUNTIME_DIR` names the directory and
/// `WAYLAND_DISPLAY` the socket inside it.
pub const SOCKET: &str = "wayland-0";

/// The compositor this process has, made once.
///
/// Two doors lead to one screen — the app's socket and the RFB server — and two
/// compositors would be two desktops wearing one name: a viewer on one of them
/// would click into a picture the other never drew.
pub fn shared(width: u32, height: u32) -> Result<Arc<Pane>, ScreenError> {
    static SHARED: OnceLock<Mutex<Option<Arc<Pane>>>> = OnceLock::new();
    let cell = SHARED.get_or_init(|| Mutex::new(None));
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(pane) = guard.as_ref() {
        if pane.display_size() != (width, height) {
            tracing::debug!(
                "wayland: the pane is already {:?}, not {:?}",
                pane.display_size(),
                (width, height)
            );
        }
        return Ok(Arc::clone(pane));
    }
    let pane = Arc::new(Pane::open(width, height)?);
    *guard = Some(Arc::clone(&pane));
    // The compositor starts empty, and an empty desktop is indistinguishable
    // from a broken one — measured as a navy rectangle with nothing in it, which
    // is exactly what a broken compositor looks like. So a fresh pane is opened
    // with a browser in it, the way the strip's own button would, and
    // `AIXODIA_PANE_APP` (or an empty value) says otherwise.
    let program = std::env::var("AIXODIA_PANE_APP").unwrap_or_default();
    let program = if program.trim().is_empty() {
        "browser"
    } else {
        program.trim()
    };
    if program != "none" {
        crate::launch::launch_in_compositor(program, pane.dir(), SOCKET);
    }
    // Two things keep the desktop from being an empty rectangle: a browser that
    // exits is started again, and the launcher tile drawn on the desktop starts
    // one when there is none.
    let (asked, requests) = std::sync::mpsc::channel();
    {
        let mut compositor = pane.compositor.lock().unwrap_or_else(|e| e.into_inner());
        compositor.set_launcher(asked);
    }
    let watched = Arc::clone(&pane);
    std::thread::Builder::new()
        .name("aixodia-desktop".into())
        .spawn(move || watch_desktop(&watched, requests))
        .map_err(|e| ScreenError::Open(format!("cannot watch the desktop: {e}")))?;
    Ok(pane)
}

/// Keeps the desktop furnished: a launcher click starts a browser, and a browser
/// that is gone with nothing on screen is started again.
///
/// The wait between attempts grows, because a browser that dies on the first
/// keystroke would otherwise be restarted in a tight loop; and a relaunch never
/// happens while a window is showing, so a browser the viewer closed by hand is
/// not brought back from under them.
fn watch_desktop(pane: &Pane, requests: std::sync::mpsc::Receiver<()>) {
    use std::sync::mpsc::RecvTimeoutError;
    let mut wait = std::time::Duration::from_secs(2);
    loop {
        let asked = match requests.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(()) => true,
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if asked {
            // A browser that is already there is the answer to a launcher click:
            // a second one would share the profile and corrupt it.
            if crate::launch::browser_running() || pane.shown_windows() > 0 {
                tracing::info!("desktop: the launcher was clicked; a browser is already open");
                continue;
            }
            tracing::info!("desktop: the launcher was clicked; opening a browser");
            crate::launch::launch_in_compositor("browser", pane.dir(), SOCKET);
            wait = std::time::Duration::from_secs(2);
            continue;
        }
        if crate::launch::browser_running() || pane.shown_windows() > 0 {
            wait = std::time::Duration::from_secs(2);
            continue;
        }
        std::thread::sleep(wait);
        if crate::launch::browser_running() || pane.shown_windows() > 0 {
            continue;
        }
        tracing::info!(
            "desktop: nothing is open and the browser is gone; starting it again in {}s",
            wait.as_secs()
        );
        crate::launch::launch_in_compositor("browser", pane.dir(), SOCKET);
        wait = (wait * 2).min(std::time::Duration::from_secs(30));
    }
}

/// A display of our own: the compositor, and the frame it has just composited.
pub struct Pane {
    compositor: Arc<Mutex<Compositor>>,
    dir: PathBuf,
}

/// Removes the runtime directories of compositors whose engine is gone.
///
/// Every start of the engine makes a new one — the directory is named after the
/// pid — and nothing removes it when the engine is killed, so `/tmp` filled with
/// nineteen of them in one afternoon of restarts. A directory whose pid has no
/// `/proc` entry is a directory whose compositor cannot come back, so it is safe
/// to remove and nothing else is touched.
fn sweep_stale_dirs() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix("aixodia-wayland-"))
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == std::process::id() {
            continue;
        }
        if std::path::Path::new(&format!("/proc/{pid}")).exists() {
            continue;
        }
        let _ = std::fs::remove_dir_all(entry.path());
    }
}

impl Pane {
    /// Brings up a compositor at `width` x `height`, with its socket in a
    /// directory only its owner can read — the same rule a session's
    /// `XDG_RUNTIME_DIR` follows, and what a client checks before trusting it.
    pub fn open(width: u32, height: u32) -> Result<Self, ScreenError> {
        sweep_stale_dirs();
        let dir = std::env::temp_dir().join(format!("aixodia-wayland-{}", std::process::id()));
        std::fs::create_dir_all(&dir)
            .map_err(|e| ScreenError::Open(format!("cannot make {}: {e}", dir.display())))?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(
                |e| ScreenError::Open(format!("cannot restrict {}: {e}", dir.display())),
            )?;
        }
        let width = width.clamp(320, 4096);
        let height = height.clamp(240, 4096);
        let compositor = Compositor::new(&dir, SOCKET, width, height)
            .map_err(|e| ScreenError::Open(e.to_string()))?;
        let compositor = Arc::new(Mutex::new(compositor));
        // The compositor is a running thing, not a camera: a client draws only
        // while its requests are being answered, and a client that is waiting for
        // a configure nobody sends has not drawn a window yet. Tying that
        // progress to a viewer asking for a picture made a browser take as many
        // seconds to appear as the viewer sent updates — measured: the window
        // arrived on the fourth request, not on the first. So the pane turns the
        // wheel itself and a frame is a copy of whatever it last composited.
        {
            let ticked = Arc::clone(&compositor);
            std::thread::Builder::new()
                .name("aixodia-wayland".into())
                .spawn(move || loop {
                    {
                        let mut compositor = ticked.lock().unwrap_or_else(|e| e.into_inner());
                        if let Err(err) = compositor.poll(TICK_MS) {
                            tracing::warn!("wayland: the compositor stopped: {err}");
                            return;
                        }
                    }
                    // A client with something to say every turn makes `poll`
                    // return at once, and this loop would then take the lock again
                    // before anything else could have it — measured as a viewer
                    // waiting fifteen seconds for a frame while the compositor
                    // spun. A short sleep hands the lock back to whoever is
                    // waiting, which is what the frame behind that viewer is.
                    std::thread::sleep(std::time::Duration::from_millis(2));
                })
                .map_err(|e| ScreenError::Open(format!("cannot start the compositor: {e}")))?;
        }
        Ok(Self { compositor, dir })
    }

    /// The directory a launched program must be given as `XDG_RUNTIME_DIR`.
    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    /// How many windows the compositor is actually showing.
    ///
    /// A window that has been created but never confirmed by its client is not
    /// showing, so this is what is on the picture, not what was asked for.
    pub fn shown_windows(&self) -> usize {
        let compositor = self.compositor.lock().unwrap_or_else(|e| e.into_inner());
        compositor
            .state
            .arrangement()
            .iter()
            .filter(|(_, _, _, _, shown)| *shown)
            .count()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Compositor> {
        // A panicking client must not take the display with it: the frame is
        // still perfectly good, and the alternative is a black screen.
        self.compositor.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn width(&self) -> u32 {
        self.lock().size().0
    }

    pub fn height(&self) -> u32 {
        self.lock().size().1
    }

    pub fn display_size(&self) -> (u32, u32) {
        self.lock().size()
    }

    /// The fast path is the only path here: clients hand over shared memory, so
    /// there is never a copy through a socket to avoid.
    pub fn using_shm(&self) -> bool {
        true
    }

    /// Runs the compositor for one short turn and hands back the picture.
    ///
    /// A frame is only rebuilt when a client committed something, which is what
    /// `poll` reports; when nothing changed the previous frame is returned
    /// unchanged, and the encoder above diffs it to no updates at all.
    pub fn frame(&self) -> Result<Frame, ScreenError> {
        let mut compositor = self.lock();
        compositor.poll(0).map_err(|_| ScreenError::Gone)?;
        let (width, height) = compositor.size();
        let pixels = frame_bytes(compositor.frame());
        Ok(Frame {
            width,
            height,
            pixels,
        })
    }

    pub fn move_pointer(&self, x: i32, y: i32) {
        let _ = self.lock().pointer_motion(x as f64, y as f64);
    }

    pub fn pointer_position(&self) -> Option<(i32, i32)> {
        let (x, y) = self.lock().pointer_position();
        Some((x as i32, y as i32))
    }

    /// The cursor the client under the pointer asked for.
    ///
    /// A Wayland client sets its cursor as a surface with a bitmap and a hot
    /// spot, which is exactly what a viewer wants to draw — and what the app
    /// wants to look at before it decides whether a tap lands on something text
    /// can be typed into. Nothing set means nothing to report: an arrow drawn by
    /// the caller is better than an invented shape.
    pub fn cursor_shape(&self) -> Option<cursor::Now> {
        let compositor = self.lock();
        let (width, height, hot_x, hot_y, serial, argb) = compositor.cursor()?;
        Some(cursor::Now {
            serial,
            width,
            height,
            hot_x,
            hot_y,
            rgba: cursor::straighten(&argb),
        })
    }

    pub fn button(&self, button: Button, pressed: bool) {
        let mut compositor = self.lock();
        let _ = match button {
            Button::Left => compositor.pointer_button(0x110, pressed),
            Button::Middle => compositor.pointer_button(0x112, pressed),
            Button::Right => compositor.pointer_button(0x111, pressed),
            // A wheel is not a button here: it is an axis with a distance, and a
            // notch is one unit of it.
            Button::WheelUp if pressed => compositor.pointer_axis(1.0),
            Button::WheelDown if pressed => compositor.pointer_axis(-1.0),
            Button::WheelLeft if pressed => compositor.pointer_axis_across(-1.0),
            Button::WheelRight if pressed => compositor.pointer_axis_across(1.0),
            _ => Ok(()),
        };
    }

    /// Presses a key by its keysym name, holding shift when the name needs it or
    /// when the caller says it is held.
    pub fn key(&self, name: &str, mods: &[String]) {
        let Some((code, needs_shift)) = keycode(name) else {
            return;
        };
        let held = mods.iter().any(|m| m.contains("Shift"));
        let mut compositor = self.lock();
        let shift = held ^ needs_shift;
        let mut release_shift = false;
        if shift {
            release_shift = true;
            let _ = compositor.key(SHIFT, true);
        }
        let _ = compositor.key(code, true);
        let _ = compositor.key(code, false);
        if release_shift {
            let _ = compositor.key(SHIFT, false);
        }
    }

    /// A key as a VNC viewer sends it: a keysym number rather than a name.
    ///
    /// A printable keysym is the character's own number, and the rest are the
    /// handful a viewer's keyboard has — so the number is read back into the name
    /// the table above understands, and a key with no name types nothing.
    pub fn keysym(&self, keysym: u32, pressed: bool) {
        let Some((code, needs_shift)) = keysym_code(keysym) else {
            return;
        };
        let mut compositor = self.lock();
        let mut release_shift = false;
        if needs_shift {
            release_shift = true;
            let _ = compositor.key(SHIFT, true);
        }
        let _ = compositor.key(code, pressed);
        if release_shift && !pressed {
            let _ = compositor.key(SHIFT, false);
        }
    }

    /// Types text one character at a time, through the keymap the client was
    /// given. A character the table cannot name is dropped rather than guessed
    /// at: a wrong key is worse than a missing one.
    pub fn type_text(&self, text: &str) {
        for character in text.chars() {
            let name = character.to_string();
            if keycode(&name).is_none() {
                continue;
            }
            self.key(&name, &[]);
        }
    }
}

/// The frame's ARGB words as the bytes the encoder reads: blue, green, red, pad.
fn frame_bytes(argb: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(argb.len() * 4);
    for pixel in argb {
        bytes.push(*pixel as u8);
        bytes.push((*pixel >> 8) as u8);
        bytes.push((*pixel >> 16) as u8);
        bytes.push(0xff);
    }
    bytes
}

/// How long one turn of the compositor waits for a client, in milliseconds. Long
/// enough that an idle pane sleeps, short enough that a click is answered at once.
const TICK_MS: i32 = 8;

/// The left shift keycode, so a shifted character can be typed on a keymap that
/// only knows what shift does.
const SHIFT: u32 = 42;

/// The keycode a keysym name needs, and whether shift has to be held for it.
///
/// The keymap clients are sent is `pc+us+inet(evdev)`, so this table is the US
/// layout written down: a letter is its own key, a capital is that key with
/// shift, and the row of symbols above the letters is a digit with shift. Names
/// with no keycode on that layout — a function key we never send, a symbol from
/// another keyboard — return `None`, and the caller sends nothing.
pub fn keycode(name: &str) -> Option<(u32, bool)> {
    let named = match name {
        "Return" | "KP_Enter" => 28,
        "BackSpace" => 14,
        "Tab" => 15,
        "Escape" => 1,
        "Delete" => 111,
        "Insert" => 110,
        "Home" => 102,
        "End" => 107,
        "Left" => 105,
        "Right" => 106,
        "Up" => 103,
        "Down" => 108,
        "Page_Up" | "Prior" => 104,
        "Page_Down" | "Next" => 109,
        "space" | " " => 57,
        "Shift_L" => 42,
        "Shift_R" => 54,
        "Control_L" => 29,
        "Control_R" => 97,
        "Alt_L" => 56,
        "Alt_R" => 100,
        "Super_L" => 125,
        "Caps_Lock" => 58,
        "minus" => 12,
        "equal" => 13,
        "bracketleft" => 26,
        "bracketright" => 27,
        "semicolon" => 39,
        "apostrophe" => 40,
        "grave" => 41,
        "comma" => 51,
        "period" => 52,
        "slash" => 53,
        "backslash" => 43,
        _ => 0,
    };
    if named != 0 {
        return Some((named, false));
    }
    // A single character, as the phone sends it: the character itself, not a
    // keysym number.
    let mut characters = name.chars();
    let (character, rest) = (characters.next()?, characters.next());
    if rest.is_some() {
        return None;
    }
    if let Some(code) = letter(character) {
        return Some((code, false));
    }
    if let Some(code) = letter(character.to_ascii_lowercase()) {
        return Some((code, true));
    }
    if let Some((code, shift)) = symbol(character) {
        return Some((code, shift));
    }
    // A named keysym we do not know, e.g. one from `XStringToKeysym` that has no
    // key on this layout.
    None
}

/// The keycode an X keysym number stands for.
///
/// The X keysyms a keyboard can produce are mostly their own character's number:
/// `0x61` is `a`, `0x20` is space. The rest are the named ones, which have numbers
/// in the `0xff00` page.
pub fn keysym_code(keysym: u32) -> Option<(u32, bool)> {
    let named = match keysym {
        0xff0d | 0xff8d => "Return",
        0xff08 => "BackSpace",
        0xff09 => "Tab",
        0xff1b => "Escape",
        0xffff => "Delete",
        0xff63 => "Insert",
        0xff50 => "Home",
        0xff57 => "End",
        0xff51 => "Left",
        0xff53 => "Right",
        0xff52 => "Up",
        0xff54 => "Down",
        0xff55 => "Page_Up",
        0xff56 => "Page_Down",
        0x0020 => "space",
        0xffe1 => "Shift_L",
        0xffe2 => "Shift_R",
        0xffe3 => "Control_L",
        0xffe4 => "Control_R",
        0xffe9 => "Alt_L",
        0xffea => "Alt_R",
        0xffeb => "Super_L",
        0xffe5 => "Caps_Lock",
        _ => "",
    };
    if !named.is_empty() {
        return keycode(named);
    }
    let character = char::from_u32(keysym)?;
    if character.is_control() {
        return None;
    }
    keycode(&character.to_string())
}

/// The key a letter sits on, which is the same for both cases: the US layout's
/// three rows, left to right.
fn letter(character: char) -> Option<u32> {
    Some(match character {
        'q' => 16,
        'w' => 17,
        'e' => 18,
        'r' => 19,
        't' => 20,
        'y' => 21,
        'u' => 22,
        'i' => 23,
        'o' => 24,
        'p' => 25,
        'a' => 30,
        's' => 31,
        'd' => 32,
        'f' => 33,
        'g' => 34,
        'h' => 35,
        'j' => 36,
        'k' => 37,
        'l' => 38,
        'z' => 44,
        'x' => 45,
        'c' => 46,
        'v' => 47,
        'b' => 48,
        'n' => 49,
        'm' => 50,
        _ => return None,
    })
}

/// The key a symbol sits on, and whether shift is what makes that symbol.
fn symbol(character: char) -> Option<(u32, bool)> {
    Some(match character {
        '1' => (2, false),
        '2' => (3, false),
        '3' => (4, false),
        '4' => (5, false),
        '5' => (6, false),
        '6' => (7, false),
        '7' => (8, false),
        '8' => (9, false),
        '9' => (10, false),
        '0' => (11, false),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        '\\' => (43, false),
        '|' => (43, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_edit_keys_a_phone_sends_have_keycodes() {
        for name in [
            "BackSpace",
            "Delete",
            "Left",
            "Right",
            "Up",
            "Down",
            "Home",
            "End",
            "Return",
        ] {
            let (code, shift) = keycode(name).unwrap_or_else(|| panic!("{name} has no keycode"));
            assert!(code > 0, "{name} is a real keycode");
            assert!(!shift, "{name} needs no shift");
        }
    }

    #[test]
    fn a_capital_is_its_letter_with_shift() {
        assert_eq!(keycode("a"), Some((30, false)));
        assert_eq!(keycode("A"), Some((30, true)));
        assert_eq!(keycode("z"), Some((44, false)));
        assert_eq!(keycode("Z"), Some((44, true)));
    }

    #[test]
    fn a_symbol_from_the_row_above_the_letters_is_a_shifted_digit() {
        assert_eq!(keycode("1"), Some((2, false)));
        assert_eq!(keycode("!"), Some((2, true)));
        assert_eq!(keycode(")"), Some((11, true)));
        assert_eq!(keycode("?"), Some((53, true)));
        assert_eq!(keycode("'"), Some((40, false)));
        assert_eq!(keycode("\""), Some((40, true)));
    }

    #[test]
    fn a_viewer_s_keysym_is_a_key_a_compositor_can_press() {
        // RFC 6143 sends X keysym numbers; a viewer's Return, letters and
        // digits are the ones that matter in practice.
        assert_eq!(keysym_code(0xff0d), keycode("Return"));
        assert_eq!(keysym_code(0x0061), Some((30, false)));
        assert_eq!(keysym_code(0x0041), Some((30, true)));
        assert_eq!(keysym_code(0x0021), Some((2, true)));
        assert_eq!(keysym_code(0x0000), None);
        // A Thai character has no key on a US keymap, and guessing would type
        // something else.
        assert_eq!(keysym_code(0x0e01), None);
    }

    #[test]
    fn a_name_this_layout_has_no_key_for_is_not_guessed_at() {
        // A phone's compose box can hold anything at all, and a keycode chosen
        // by guessing types something else.
        assert_eq!(keycode("ก"), None);
        assert_eq!(keycode("emoji"), None);
        assert_eq!(keycode(""), None);
        assert_eq!(keycode("F13"), None);
    }

    #[test]
    fn the_frame_comes_out_blue_green_red_the_way_the_encoder_reads_it() {
        // 0xAARRGGBB in a u32; the four bytes on a little-endian machine are
        // blue, green, red, alpha, which is what `Frame::pixels` promises.
        let argb = [0xff11_2233u32, 0xffaa_bbccu32];
        let bytes = frame_bytes(&argb);
        // 0xff112233 is red 0x11, green 0x22, blue 0x33, and the encoder reads
        // blue first.
        assert_eq!(bytes, vec![0x33, 0x22, 0x11, 0xff, 0xcc, 0xbb, 0xaa, 0xff]);
    }
}
