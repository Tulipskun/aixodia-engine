//! Where one engine keeps the state it must not share with another.
//!
//! A host can run several engines side by side — one per phone, one per
//! project — and each of them owns a pid, a log database, a self-update state,
//! terminal sockets and the cache of what Workers KV told it. All of that lives
//! under one directory, and pointing each engine at its own directory is the
//! whole of the isolation: the binary behaves the same either way.
//!
//! `AIXODIA_STATE_DIR` names that directory when the launcher starts an
//! instance, and `$HOME/.aixodia` remains the answer when it does not, so a
//! single-engine host (and every update that replaces a running image) keeps
//! the paths it already had.

use std::path::{Path, PathBuf};

/// The state directory of this process.
///
/// Read from the environment on every call rather than cached: the value is set
/// once by the launcher before `execve`, and a test drives both answers in one
/// process.
pub fn state_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("AIXODIA_STATE_DIR").filter(|dir| !dir.is_empty()) {
        return PathBuf::from(dir);
    }
    match std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        Some(home) => PathBuf::from(home).join(".aixodia"),
        None => PathBuf::from(".aixodia"),
    }
}

/// `name` inside this engine's state directory.
pub fn path(name: &str) -> PathBuf {
    state_dir().join(name)
}

/// `name` inside this engine's state directory, creating the directory.
pub fn directory(name: &str) -> std::io::Result<PathBuf> {
    let dir = path(name);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// The name of an instance, as it appears in a KV key and in a public URL.
///
/// The same shape as a device name: lower-case letters, digits and single
/// hyphens, at most 32 characters. It names a path segment, so anything that
/// could change what the segment means is rejected instead of escaped.
pub fn valid_instance_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 32 {
        return false;
    }
    let bytes = name.as_bytes();
    if !bytes[0].is_ascii_lowercase() && !bytes[0].is_ascii_digit() {
        return false;
    }
    if !bytes[bytes.len() - 1].is_ascii_lowercase() && !bytes[bytes.len() - 1].is_ascii_digit() {
        return false;
    }
    let mut last_hyphen = false;
    for byte in bytes {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' => last_hyphen = false,
            b'-' if !last_hyphen => last_hyphen = true,
            _ => return false,
        }
    }
    true
}

/// Whether `dir` belongs to the instance named `name`.
pub fn is_instance_dir(dir: &Path, name: &str) -> bool {
    dir.file_name()
        .and_then(|file| file.to_str())
        .map(|file| file == name)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_dir_follows_the_launcher() {
        let saved = std::env::var_os("AIXODIA_STATE_DIR");
        std::env::set_var("AIXODIA_STATE_DIR", "/tmp/aixodia-instance-b");
        assert_eq!(state_dir(), PathBuf::from("/tmp/aixodia-instance-b"));
        assert_eq!(
            path("schema.json"),
            PathBuf::from("/tmp/aixodia-instance-b/schema.json")
        );
        std::env::set_var("AIXODIA_STATE_DIR", "");
        assert!(state_dir().ends_with(".aixodia"));
        match saved {
            Some(value) => std::env::set_var("AIXODIA_STATE_DIR", value),
            None => std::env::remove_var("AIXODIA_STATE_DIR"),
        }
    }

    #[test]
    fn a_name_that_could_escape_its_path_segment_is_rejected() {
        for good in ["main", "phone-2", "a", "x9"] {
            assert!(valid_instance_name(good), "{good} must be accepted");
        }
        for bad in [
            "",
            "Main",
            "-main",
            "main-",
            "main--two",
            "a/b",
            "..",
            "phone 2",
            "a.b",
        ] {
            assert!(!valid_instance_name(bad), "{bad} must be rejected");
        }
        assert!(!valid_instance_name(&"a".repeat(33)));
    }

    #[test]
    fn a_directory_is_recognised_by_its_instance() {
        assert!(is_instance_dir(
            Path::new("/root/.aixodia/instances/phone"),
            "phone"
        ));
        assert!(!is_instance_dir(
            Path::new("/root/.aixodia/instances/phone"),
            "other"
        ));
    }
}
