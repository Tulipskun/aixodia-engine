//! The working directory the `shell` tool falls back to.
//!
//! `set_directory` records one path per chat session, so a conversation can
//! point `shell` at a project once instead of prefixing every call with `cd`.
//! The state lives here rather than in `ToolContext` because the registry in
//! `tools_with` is built once and shared by every session.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

fn state() -> &'static Mutex<HashMap<String, PathBuf>> {
    static STATE: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, PathBuf>> {
    // A poisoned lock only means some other session panicked mid-update; the
    // map itself is still usable, so recover instead of failing every shell.
    state().lock().unwrap_or_else(|err| err.into_inner())
}

/// Records `path` as the default working directory for `session`.
pub fn set(session: &str, path: &Path) {
    lock().insert(session.to_string(), path.to_path_buf());
}

/// The directory recorded for `session`, if one was set.
pub fn get(session: &str) -> Option<PathBuf> {
    lock().get(session).cloned()
}

/// Forgets the directory recorded for `session`.
pub fn clear(session: &str) {
    lock().remove(session);
}
