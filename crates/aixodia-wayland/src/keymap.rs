//! The keymap a keyboard sends: how a client turns an evdev keycode into a
//! character.
//!
//! Wayland does not send characters. It sends an xkb keymap — a text description
//! of the keyboard — once, and then numbers for each key; the client does the
//! translation, because only the client knows what the user is typing into.
//!
//! The map is five lines because xkb can compose itself from the pieces the
//! machine already has; a self-contained copy would be thousands of lines of
//! tables that say exactly the same thing. `xkb-data` is what those lines resolve
//! against, and the launcher installs it with the rest of the desktop's packages.

use std::io::{self, Seek, SeekFrom, Write};

/// An xkb keymap: the layout a phone keyboard is expected to type in.
const KEYMAP: &str = concat!(
    "xkb_keymap {\n",
    "  xkb_keycodes { include \"evdev+aliases(qwerty)\" };\n",
    "  xkb_types { include \"complete\" };\n",
    "  xkb_compatibility { include \"complete\" };\n",
    "  xkb_symbols { include \"pc+us+inet(evdev)\" };\n",
    "};\n",
);

/// Writes the keymap to a sealed-in-memory file and says how big it is.
///
/// A memfd rather than a temporary file: the client reads it once, immediately,
/// and nothing should be able to find it on disk afterwards.
pub fn write() -> io::Result<(std::fs::File, u32)> {
    let name = c"aixodia-keymap";
    // SAFETY: the name is a NUL-terminated C string literal, and the flags are
    // the documented ones for a private, close-on-exec file.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just created and is owned here from now on.
    let mut file = unsafe { <std::fs::File as std::os::unix::io::FromRawFd>::from_raw_fd(fd) };
    file.write_all(KEYMAP.as_bytes())?;
    file.seek(SeekFrom::Start(0))?;
    Ok((file, KEYMAP.len() as u32))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn the_keymap_is_written_and_rewound_for_the_client() {
        let (mut file, size) = write().unwrap();
        assert_eq!(size as usize, KEYMAP.len());
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert_eq!(text, KEYMAP);
        assert!(text.contains("xkb_symbols"));
    }

    #[test]
    fn the_keymap_names_a_layout_a_qwerty_keyboard_can_type() {
        assert!(KEYMAP.contains("pc+us+inet(evdev)"));
    }
}
