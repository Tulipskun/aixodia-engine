//! This engine's public name, claimed in the shared `engines` table.
//!
//! Every engine is reachable as `https://<engine host>/<public name>/…`, the same
//! shape droidime uses for its devices (`mcp…/<device name>/…`). The name is not
//! derived from the machine here: it is whatever the registry has room for, so
//! two machines that were set up with the same `AIXODIA_INSTANCE` get `work` and
//! `work-1` instead of overwriting each other.
//!
//! The row is found again by `origin_host` — the tunnel hostname this engine
//! answers on — and never by the name, so renaming the machine keeps the row it
//! already owns.

use aixodia_db::{Client as DbClient, Row};
use serde_json::Value;

/// How many numbered names to try before giving up.
const ATTEMPTS: u32 = 64;

/// The public name of this engine, claiming a row the first time it starts.
pub async fn claim(db: &DbClient, origin_host: &str, desired: &str) -> anyhow::Result<String> {
    if let Some(name) = owner(db, origin_host).await? {
        return Ok(name);
    }

    for attempt in 0..ATTEMPTS {
        let candidate = if attempt == 0 {
            desired.to_string()
        } else {
            format!("{desired}-{attempt}")
        };
        if !free(db, &candidate).await? {
            continue;
        }
        let url = format!("https://{origin_host}/{candidate}");
        db.query(
            "INSERT INTO engines (id, name, url, origin_host) VALUES (?1, ?2, ?3, ?4)",
            &[
                Value::from(candidate.as_str()),
                Value::from(candidate.as_str()),
                Value::from(url.as_str()),
                Value::from(origin_host),
            ],
        )
        .await?;
        return Ok(candidate);
    }

    anyhow::bail!("no free engine name within {ATTEMPTS} tries for {desired}")
}

/// The name this machine already has, if any.
async fn owner(db: &DbClient, origin_host: &str) -> anyhow::Result<Option<String>> {
    let rows = db
        .query(
            "SELECT id FROM engines WHERE origin_host = ?1 LIMIT 1",
            &[Value::from(origin_host)],
        )
        .await?;
    Ok(rows.first().and_then(name_of))
}

async fn free(db: &DbClient, candidate: &str) -> anyhow::Result<bool> {
    let rows = db
        .query(
            "SELECT 1 FROM engines WHERE id = ?1 LIMIT 1",
            &[Value::from(candidate)],
        )
        .await?;
    Ok(rows.is_empty())
}

fn name_of(row: &Row) -> Option<String> {
    row.get("id")?.as_str().map(|value| value.to_string())
}

/// The path a request carries once this engine's own name is taken off it.
///
/// `/work/api/chat` is `/api/chat`; `/worked/api` and `/work` are untouched and
/// `/work` (the bare name) is the root.
pub fn strip_name<'a>(prefix: &str, path: &'a str) -> Option<&'a str> {
    let rest = path.strip_prefix(prefix)?;
    if rest.is_empty() {
        Some("/")
    } else if rest.starts_with('/') {
        Some(rest)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_off_only_a_whole_segment() {
        assert_eq!(strip_name("/work", "/work/api/chat"), Some("/api/chat"));
        assert_eq!(strip_name("/work", "/work"), Some("/"));
        assert_eq!(strip_name("/work", "/worked/api"), None);
        assert_eq!(strip_name("/work", "/api/chat"), None);
        assert_eq!(strip_name("/work", "/work/"), Some("/"));
    }
}
