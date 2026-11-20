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
//!
//! One host must have exactly one live owner. Two machines cloned from one image
//! share a machine id, hence the same KV keys and the same tunnel, and would both
//! find that one row and start serving it. Each engine therefore carries a random
//! uid, kept in its state directory and written onto the row it owns; a second
//! live process with a different uid is refused rather than silently sharing the
//! row and making the gateway's route flap between two pids.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aixodia_db::{Client as DbClient, Row};
use rand::RngCore;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// How many numbered names to try before giving up.
const ATTEMPTS: u32 = 64;

/// The gateway's front door. Clients must be handed this host with the engine's
/// public name as a path segment, never the tunnel's own hostname: the origin
/// sits behind a Cloudflare Access application that stops a phone's request.
/// The console and the phone app use the same address as their default, so a
/// plain install needs no extra environment variable. An operator running a
/// different deployment can override it without changing the binary.
const DEFAULT_GATEWAY_BASE: &str = "https://engine.aixodia.dpdns.org";

/// The public base a client should call, from `AIXODIA_GATEWAY_URL` when set.
///
/// Like `CF_TOKEN`, this is not required: the default matches every other
/// component of this deployment. The trailing slash is removed so the name can
/// always be appended as one path segment.
pub fn gateway_base() -> String {
    let configured = std::env::var("AIXODIA_GATEWAY_URL")
        .unwrap_or_default()
        .trim()
        .trim_end_matches('/')
        .to_string();
    if configured.starts_with("https://") || configured.starts_with("http://") {
        configured
    } else {
        DEFAULT_GATEWAY_BASE.to_string()
    }
}

/// The address a client is given for one engine: the gateway front door plus
/// the engine's public name. Kept pure so the gateway form can be tested
/// without a D1 or a tunnel.
pub fn public_url(gateway_base: &str, name: &str) -> String {
    format!("{}/{name}", gateway_base.trim_end_matches('/'))
}

/// How often this engine says it is alive. The gateway's lease is three beats,
/// so two lost writes in a row are still not a death.
const BEAT: Duration = Duration::from_secs(20);

/// A heartbeat older than this means the previous owner is gone and its row may
/// be taken over. Matches the gateway's three-beat lease.
const LEASE_SECS: i64 = 60;

/// The file, inside this engine's state directory, holding its persistent uid.
const UID_FILE: &str = "instance.uid";

/// This engine's stable identity, generated once and reused across restarts.
///
/// Two cloned machines share a machine id and KV keys but own separate state
/// directories, so a random uid created on first start tells them apart. An
/// `execve` update and a normal restart keep the same file and therefore the
/// same uid, so neither loses its own row.
pub fn local_uid() -> anyhow::Result<String> {
    let path = aixodia_state::path(UID_FILE);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let uid = text.trim();
        if !uid.is_empty() {
            return Ok(uid.to_string());
        }
    }

    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let uid: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();

    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(&path, uid.as_bytes())?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(uid)
}

/// Whether a live row with `held_uid` may be claimed by `my_uid`.
///
/// The decision is pure so the split-brain rule can be tested without a D1.
/// `age` is seconds since the row's last heartbeat; a fresh heartbeat from a
/// different uid is another live engine and must never be shared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Takeover {
    /// The row is mine (same uid), or its owner is gone: reclaim it.
    Allow,
    /// A different engine is keeping this row alive: refuse to start.
    Refuse,
}

fn decide(held_uid: &str, my_uid: &str, age_secs: i64) -> Takeover {
    if held_uid.is_empty() || held_uid == my_uid {
        return Takeover::Allow;
    }
    // No heartbeat yet, or one older than the lease: the writer is gone.
    if !(0..=LEASE_SECS).contains(&age_secs) {
        return Takeover::Allow;
    }
    Takeover::Refuse
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn uid_of(row: &Row) -> &str {
    row.get("instance_uid")
        .and_then(|value| value.as_str())
        .unwrap_or("")
}

fn beat_of(row: &Row) -> i64 {
    row.get("heartbeat_at")
        .and_then(|value| value.as_i64())
        .unwrap_or(0)
}

/// What a client should call, and the name it should call it under.
///
/// `url` is the address the registry already publishes for this row. It is not
/// derived from `origin_host`: an engine behind the gateway's front door answers
/// on a hostname only the Worker may use, and a client handed that one would be
/// stopped by the Access application in front of it.
pub struct Claim {
    pub name: String,
    pub url: String,
}

/// The public name of this engine, claiming a row the first time it starts.
pub async fn claim(
    db: &DbClient,
    origin_host: &str,
    gateway_base: &str,
    desired: &str,
    my_uid: &str,
) -> anyhow::Result<Claim> {
    if let Some(claim) = owner(db, origin_host, my_uid).await? {
        return Ok(claim);
    }

    // Claim the row atomically: a plain INSERT would let two engines that start
    // together each see no owner and both write a row. `INSERT ... ON CONFLICT
    // DO NOTHING RETURNING id` yields a row only to the writer that wins.
    for attempt in 0..ATTEMPTS {
        let candidate = if attempt == 0 {
            desired.to_string()
        } else {
            format!("{desired}-{attempt}")
        };
        if !free(db, &candidate).await? {
            continue;
        }
        // The client's address is the gateway front door plus the public name,
        // not the tunnel hostname this process dials. A phone handed the origin
        // would be stopped by the Access app in front of it; the Worker is the
        // only caller allowed to reach the origin directly.
        let url = public_url(gateway_base, &candidate);
        let inserted = db
            .query(
                "INSERT INTO engines (id, name, url, origin_host, instance_uid) \
                 VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(id) DO NOTHING RETURNING id",
                &[
                    Value::from(candidate.as_str()),
                    Value::from(candidate.as_str()),
                    Value::from(url.as_str()),
                    Value::from(origin_host),
                    Value::from(my_uid),
                ],
            )
            .await?;
        if inserted.is_empty() {
            // Another engine took this id between `free` and `INSERT`.
            continue;
        }
        return Ok(Claim {
            name: candidate.clone(),
            url,
        });
    }

    anyhow::bail!("no free engine name within {ATTEMPTS} tries for {desired}")
}

/// Keeps this engine's lease alive in the `engines` table.
///
/// Liveness is the engine's own statement, written where every gateway and
/// client reads it — the standard TTL-lease shape. The alternative, a prober
/// asking `/healthz` on the way to an answer, measures the prober's reach rather
/// than this process, needs a timer to stay fresh when nobody asks, and puts a
/// round trip through the tunnel in front of every list.
///
/// The first beat is immediate, so an engine that has just claimed its name is
/// online before the task loops.
pub async fn heartbeat(
    db: DbClient,
    id: String,
    uid: String,
    version: String,
    cancel: CancellationToken,
) {
    loop {
        match db
            .query(
                "UPDATE engines SET heartbeat_at = unixepoch(), version = ?1 \
                 WHERE id = ?2 AND instance_uid = ?3",
                &[
                    Value::from(version.as_str()),
                    Value::from(id.as_str()),
                    Value::from(uid.as_str()),
                ],
            )
            .await
        {
            Ok(_) => tracing::debug!("heartbeat: {id} at {version}"),
            // A missed beat is not fatal: the lease is three beats long and the
            // next one comes in twenty seconds.
            Err(err) => tracing::warn!("heartbeat: {err}"),
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(BEAT) => {}
        }
    }
}

/// The row this tunnel host already owns, if any.
///
/// A row held by a different uid with a fresh heartbeat is another live engine —
/// the split-brain a cloned pair produces — so starting here would make the
/// gateway flip between two processes. Refuse in that case instead of silently
/// sharing the row. An empty uid is a row written before this rule existed and
/// is adopted once; a stale heartbeat means its owner is gone and may be
/// replaced.
async fn owner(db: &DbClient, origin_host: &str, my_uid: &str) -> anyhow::Result<Option<Claim>> {
    let rows = db
        .query(
            "SELECT id, url, instance_uid, heartbeat_at FROM engines \
             WHERE origin_host = ?1 LIMIT 1",
            &[Value::from(origin_host)],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };

    let held = uid_of(row);
    let age = now_secs() - beat_of(row);
    if decide(held, my_uid, age) == Takeover::Refuse {
        anyhow::bail!(
            "tunnel host {origin_host} is already served by another live engine \
             (instance {}, heartbeat {age}s ago). Two machines cloned from one image \
             share this tunnel and would make the gateway route flip between them. \
             Stop the other engine, or give this machine its own tunnel and instance \
             name, before starting.",
            if held.is_empty() { "unknown" } else { held }
        );
    }

    // Ours or abandoned: stamp our uid onto the row so an old owner's lingering
    // heartbeat cannot refresh it. The compare on the old uid keeps a concurrent
    // takeover from both of us believing we won.
    if held != my_uid {
        let id = name_of(row).ok_or_else(|| anyhow::anyhow!("engine row has no id"))?;
        db.query(
            "UPDATE engines SET instance_uid = ?1, heartbeat_at = unixepoch() \
             WHERE id = ?2 AND instance_uid = ?3",
            &[
                Value::from(my_uid),
                Value::from(id.as_str()),
                Value::from(held),
            ],
        )
        .await?;
    }

    Ok(Some(claim_from(row)?))
}

fn claim_from(row: &Row) -> anyhow::Result<Claim> {
    let name = name_of(row).ok_or_else(|| anyhow::anyhow!("engine row has no id"))?;
    let url = row
        .get("url")
        .and_then(|value| value.as_str())
        .ok_or_else(|| anyhow::anyhow!("engine row has no url"))?
        .to_string();
    Ok(Claim { name, url })
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
mod decision_tests {
    use super::*;

    #[test]
    fn an_empty_uid_row_can_be_adopted() {
        // Rows written before instance_uid existed carry no owner; the first
        // upgraded engine adopts them, however fresh their heartbeat looks.
        assert_eq!(decide("", "me", 1), Takeover::Allow);
    }

    #[test]
    fn our_own_row_is_always_reclaimed() {
        // A restart or an execve update keeps the same uid and must never be
        // refused, even with a heartbeat from a second ago.
        assert_eq!(decide("me", "me", 1), Takeover::Allow);
        assert_eq!(decide("me", "me", 600), Takeover::Allow);
    }

    #[test]
    fn a_live_foreign_owner_is_refused() {
        // This is the cloned-machine case: another process keeps the row alive
        // and we must not share it.
        assert_eq!(decide("other", "me", 0), Takeover::Refuse);
        assert_eq!(decide("other", "me", 30), Takeover::Refuse);
        assert_eq!(decide("other", "me", LEASE_SECS), Takeover::Refuse);
    }

    #[test]
    fn a_dead_foreign_owner_can_be_replaced() {
        // Past the three-beat lease the old process is gone; take its row over.
        assert_eq!(decide("other", "me", LEASE_SECS + 1), Takeover::Allow);
        assert_eq!(decide("other", "me", 10_000), Takeover::Allow);
        // A clock skew putting the heartbeat in the future is not a live owner.
        assert_eq!(decide("other", "me", -1), Takeover::Allow);
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

    #[test]
    fn a_client_is_given_the_gateway_front_door_not_the_origin() {
        // The origin host is only for the Worker. A client handed it would be
        // rejected by the Access app; the name belongs on the gateway host.
        assert_eq!(
            public_url("https://engine.aixodia.dpdns.org", "vm-26-251-tencentos"),
            "https://engine.aixodia.dpdns.org/vm-26-251-tencentos"
        );
        // A trailing slash in an override must not produce a double slash.
        assert_eq!(
            public_url("https://other.example.test/", "work"),
            "https://other.example.test/work"
        );
    }
}
