//! A record of which schema migrations this machine has already applied.
//!
//! Every engine start used to run the same DDL — three `ALTER TABLE`s and two
//! `CREATE TABLE IF NOT EXISTS`s — against D1, and each statement is a round
//! trip over the tunnel that only ever answers "duplicate column" after the
//! first time. The version is instead recorded in the local state directory,
//! which an update's `execve` does not erase, so a later image skips the DDL
//! its predecessor already ran.
//!
//! The version strings are the caller's responsibility: bump the literal a
//! DDL list is stamped with whenever that list changes.

use std::fs;
use std::path::PathBuf;

/// The state directory an engine shares with the images that replaced it.
fn state_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty())?;
    Some(PathBuf::from(home).join(".aixodia"))
}

fn schema_path() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join("schema.json"))
}

fn claimed() -> std::collections::HashMap<String, String> {
    let Some(path) = schema_path() else {
        return std::collections::HashMap::new();
    };
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Whether the DDL stamped `version` still needs to run. The answer is
/// recorded once it has run; until then every process applies it again, which
/// is what migrations are supposed to be.
pub fn claim(group: &str, version: &str) -> bool {
    claimed().get(group).map(String::as_str) != Some(version)
}

/// Marks `group` as having reached `version`. The file write is best-effort:
/// when it fails the only cost is re-running the DDL on the next start.
pub fn record(group: &str, version: &str) {
    let Some(path) = schema_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        if fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    let mut map = claimed();
    map.insert(group.to_string(), version.to_string());
    let Ok(raw) = serde_json::to_string(&map) else {
        return;
    };
    let temp = path.with_extension("json.new");
    if fs::write(&temp, raw).is_err() || fs::rename(&temp, &path).is_err() {
        let _ = fs::remove_file(&temp);
    }
}
