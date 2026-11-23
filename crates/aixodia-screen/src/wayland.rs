//! The screen pane's own display: a Wayland compositor instead of an X server.
//!
//! This is the same picture the X path produces — a [`Frame`] of BGRX pixels and
//! a set of pointer and key events going the other way — so everything above it
//! (the tiling encoder, the socket, the RFB server) does not know which display
//! made it.
//!
//! What changes is what the host needs. The X path needs `Xvfb`, `xterm`, `xwd`'s
//! extension, `XTest` to move a pointer nobody can see, and six shared libraries
//! linked into the engine. Here the compositor *is* the display server and the
//! window manager, clients draw into shared memory we already have, and nothing
//! on the host has to be installed for it.
//!
//! Keys are the one place where the two disagree about vocabulary. Above this
//! module a key is an X keysym *name* — `Return`, `BackSpace`, `a`, `A` — because
//! that is what the phone sends and what the X path resolves. A Wayland client is
//! told a keymap and then an evdev keycode, so this module owns the one table
//! that turns a name into a keycode and says whether shift has to be held to make
//! that name come out.

use std::path::PathBuf;
use std::sync::Mutex;

use aixodia_wayland::Compositor;

use crate::capture::{Button, Frame, ScreenError};

/// Where clients find the compositor: `XDG_RUNTIME_DIR` names the directory and
/// `WAYLAND_DISPLAY` the socket inside it.
pub const SOCKET: &str = "wayland-0";

/// A display of our own: the compositor, and the frame it has just composited.
pub struct Pane {
    compositor: Mutex<Compositor>,
    dir: PathBuf,
}

impl Pane {
    /// Brings up a compositor at `width` x `height`, with its socket in a
    /// directory only its owner can read — the same rule a session's
    /// `XDG_RUNTIME_DIR` follows, and what a client checks before trusting it.
    pub fn open(width: u32, height: u32) -> Result<Self, ScreenError> {
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
        Ok(Self {
            compositor: Mutex::new(compositor),
            dir,
        })
    }

    /// The directory a launched program must be given as `XDG_RUNTIME_DIR`.
    pub fn dir(&self) -> &PathBuf {
        &self.dir
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
        compositor.poll(0).map_err(|e| ScreenError::Gone)?;
        let (width, height) = compositor.size();
        let pixels = compositor
            .frame()
            .iter()
            .flat_map(|pixel| {
                // The frame is ARGB in a u32; the encoder wants blue, green, red
                // and a pad byte, which on a little-endian machine is exactly
                // these four bytes in this order.
                [
                    (*pixel >> 16) as u8,
                    (*pixel >> 8) as u8,
                    *pixel as u8,
                    0xff,
                ]
            })
            .collect();
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
        let bytes: Vec<u8> = argb
            .iter()
            .flat_map(|pixel| {
                [
                    (*pixel >> 16) as u8,
                    (*pixel >> 8) as u8,
                    *pixel as u8,
                    0xff,
                ]
            })
            .collect();
        assert_eq!(bytes, vec![0x11, 0x22, 0x33, 0xff, 0xaa, 0xbb, 0xcc, 0xff]);
    }
}
