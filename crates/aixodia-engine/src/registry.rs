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

/// Takes the public name off a request that carries it.
///
/// This is done before the router is asked, not as a layer on it: axum decides
/// which route a request belongs to from the path it arrives with, so a layer
/// that rewrites the path after that decision changes nothing. The engine is
/// served through one fallback that holds the real router, and that is where
/// the rewrite belongs.
pub fn take_name(name: &str, request: &mut axum::extract::Request) {
    let prefix = format!("/{name}");
    let Some(rest) = strip_name(&prefix, request.uri().path()) else {
        return;
    };
    let query = request
        .uri()
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    let mut parts = request.uri().clone().into_parts();
    let Ok(path_and_query) = format!("{rest}{query}").parse::<axum::http::uri::PathAndQuery>()
    else {
        return;
    };
    parts.path_and_query = Some(path_and_query);
    if let Ok(uri) = axum::http::Uri::from_parts(parts) {
        *request.uri_mut() = uri;
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

    fn request(uri: &str) -> axum::extract::Request {
        axum::extract::Request::builder()
            .uri(uri)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[test]
    fn the_name_is_gone_before_the_router_asks_which_route_it_is() {
        let mut named = request("/work/api/chat?stream=1");
        take_name("work", &mut named);
        assert_eq!(named.uri().path(), "/api/chat");
        assert_eq!(named.uri().query(), Some("stream=1"));
    }

    #[test]
    fn a_router_without_the_name_is_left_alone() {
        let mut plain = request("/api/chat");
        take_name("work", &mut plain);
        assert_eq!(plain.uri().path(), "/api/chat");

        let mut similar = request("/worked/api");
        take_name("work", &mut similar);
        assert_eq!(similar.uri().path(), "/worked/api");
    }
}
