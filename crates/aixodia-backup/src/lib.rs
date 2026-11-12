//! aixodia-backup — the engine's own backup of a filesystem tree onto a
//! Hugging Face storage bucket.
//!
//! The layout is content addressed and holds exactly one backup at a time:
//!
//! ```text
//! <prefix>/objects/<aa>/<sha256>       one file's content, addressed by its hash
//! <prefix>/shards/<aa>/<sha256>        many files' content, packed and compressed
//! <prefix>/manifests/<utc>.json        the manifest of the newest backup run
//! <prefix>/latest.json                 pointer at that manifest
//! ```
//!
//! Each run replaces the manifest before it, so the bucket holds the tree as it
//! was at the last successful backup and nothing older. Objects are never
//! deleted: they are what makes the next run cheap, and Xet dedups their
//! bytes, so unchanged content costs nothing to keep.
//!
//! Two properties make this fast. A file whose size and mtime still match the
//! previous manifest is never read or hashed, so an unchanged tree costs one
//! `stat` per file and no bytes on the wire. And because an object key is the
//! content's sha256, bytes that already exist in the bucket need no upload at
//! all — the new manifest simply points at the key that is already there. What
//! does move goes through Xet, which dedups at chunk level, so a file that only
//! grew uploads the growth.
//!
//! Almost every file goes into a shard rather than into an object of its own,
//! because the round trip is what a restore pays for: see [`pack`]. A manifest
//! that names no shards is read the old way, one object per file, so a backup
//! taken before this exists still restores.
//!
//! Nothing here is `async`, so a caller cannot hold a tokio worker: start
//! [`backup`] and [`restore`] with `std::thread::spawn` and read progress from
//! the [`Reporter`] they share. Within them, files are copied, hashed and
//! transferred concurrently with plain threads.

mod hub;
pub mod ops;
mod pack;
pub mod scan;
mod xet;

pub use hub::{FileItem, Hub, Resolver};
pub use ops::{backup, fetch_manifest, latest_manifest, restore, BackupReport, RestoreReport};
pub use scan::{plan, DEFAULT_EXCLUDES};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The bucket's public hub endpoint. The S3 gateway is not usable with a plain
/// hub token — it needs separate `HFAK` credentials — so all transfers go
/// through the hub API.
pub const DEFAULT_ENDPOINT: &str = "https://huggingface.co";
pub const DEFAULT_PREFIX: &str = "aixodia/root";

/// Concurrent downloads during a restore.
///
/// Measured on this host (two cores) restoring the same 1.75 GB across 32,761
/// entries into an empty root, from the same bucket: twelve workers took
/// 1,431,743 ms; thirty-two took 897,617 ms and one rejected file; sixty-four
/// finished in 497,347 ms and named 3,779 failures — thirty-nine percent of the
/// tree refused at `http 429`. That last number is not a faster restore, it is
/// an incomplete one wearing a faster clock, so thirty-two stands as the
/// measured ceiling rather than a compromise. The hub shares a fraction of a
/// megabyte per second per connection and begins throttling somewhere above
/// thirty-two simultaneous downloads; the work that still pays is cutting a
/// large file into several streams (see `RANGE_MIN_SIZE` in `ops`), which is how
/// the receive rate reached forty-six megabytes a second at eighteen percent
/// CPU. Each worker gets its own connection pool and its own thread to drive it,
/// so one stalled stream cannot hold up the others.
pub const RESTORE_CONCURRENCY: usize = 32;

/// Attempts per file before a restore gives up on it. A half-open connection
/// to the CDN is routine on a mobile uplink; refusing the whole restore for
/// one of them is what left trees half-restored.
pub const RESTORE_ATTEMPTS: usize = 4;

/// How long one download may take in total, including the body. This is the
/// bound that turns a stalled stream into a retry instead of a lost minute: a
/// request that hangs is dropped at 30 seconds and the object is asked for
/// again, rather than being waited on until the caller's own timeout.
pub const DOWNLOAD_TIMEOUT_SECS: u64 = 30;

/// How far [`Config::timeout_secs`] may raise [`DOWNLOAD_TIMEOUT_SECS`]. A
/// configuration that allows a download to run for ten minutes reproduces the
/// failure this is here to prevent, so the ceiling holds.
pub const DOWNLOAD_TIMEOUT_MAX: u64 = 90;

/// How long a TCP and TLS handshake may take before the request is abandoned.
pub const CONNECT_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("backup: {0}")]
    Config(String),
    #[error("backup: {operation} on {key} failed: http {status}: {body}")]
    Hub {
        operation: String,
        key: String,
        status: u16,
        body: String,
    },
    #[error("backup: {operation} failed: {message}")]
    Api { operation: String, message: String },
    #[error("backup: {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("backup: xet upload failed: {0}")]
    Xet(String),
    #[error("backup: {path}: checksum mismatch after restore (wanted {want}, got {got})")]
    Verify {
        path: String,
        want: String,
        got: String,
    },
    #[error("backup: manifest {0} is malformed: {1}")]
    Manifest(String, String),
    #[error("backup: refusing to write {0}: it escapes the restore root")]
    Escape(String),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// Whether asking the hub again for the same thing could produce a
    /// different answer. A refused or missing object will be refused again, and
    /// spending four attempts and their backoff on it is what makes a restore
    /// slow for no reason; a dead stream, a timeout and a body that hashed
    /// wrong are all worth a second request.
    pub fn is_permanent(&self) -> bool {
        match self {
            Error::Hub { status, .. } => {
                (400..500).contains(status) && !matches!(status, 408 | 409 | 425 | 429)
            }
            Error::Config(_) | Error::Manifest(..) | Error::Escape(_) | Error::Xet(_) => true,
            _ => false,
        }
    }

    /// Whether the hub said "not now" rather than "no". These are the answers
    /// whose own remedy is a wait, and the fast backoff below is shorter than
    /// the window they describe: measured at sixty-four workers, thirty-nine
    /// percent of a tree was lost to `http 429` inside two seconds of retries.
    pub fn is_throttled(&self) -> bool {
        matches!(
            self,
            Error::Hub { status, .. } if matches!(*status, 408 | 425 | 429 | 503)
        )
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// The hub token used for both the bucket API and the Xet write token.
    pub token: String,
    /// `namespace/name`, as the bucket API spells it.
    pub bucket: String,
    pub endpoint: String,
    /// The directory that is backed up. Stored paths are relative to it.
    pub root: PathBuf,
    /// Key prefix the backup owns inside the bucket.
    pub prefix: String,
    /// Exclusion patterns; see [`scan`].
    pub excludes: Vec<String>,
    /// Inclusion patterns; see [`scan::is_included`]. Only explicitly selected
    /// paths are included; an empty list selects no paths.
    pub includes: Vec<String>,
    /// Extra seconds added to every request timeout, for a slow uplink.
    pub timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            token: String::new(),
            bucket: String::new(),
            endpoint: DEFAULT_ENDPOINT.to_string(),
            root: PathBuf::from("/root"),
            prefix: DEFAULT_PREFIX.to_string(),
            excludes: Vec::new(),
            includes: Vec::new(),
            timeout_secs: 120,
        }
    }
}

impl Config {
    /// The shape of a usable configuration. Deliberately says nothing about
    /// whether [`Config::root`] exists: a backup needs the tree to be there and
    /// says so itself, while a restore is how a missing tree comes back.
    pub fn validate(&self) -> Result<()> {
        if self.token.trim().is_empty() {
            return Err(Error::Config("no hub token is set".into()));
        }
        if self.bucket.trim().is_empty() {
            return Err(Error::Config("no bucket is set".into()));
        }
        if !self.bucket.contains('/') {
            return Err(Error::Config(format!(
                "bucket {:?} must be namespace/name",
                self.bucket
            )));
        }
        if self.prefix.starts_with('/') || self.prefix.contains("//") {
            return Err(Error::Config(format!("bad key prefix {:?}", self.prefix)));
        }
        self.validate_scope()
    }

    /// The part of [`Config::validate`] that depends on nothing but the scope, so
    /// the config endpoint can refuse a bad allowlist on the way in. An include
    /// that can never match does not fail loudly — it backs up a smaller tree and
    /// reports success — which is the one mistake this tool cannot take back.
    pub fn validate_scope(&self) -> Result<()> {
        for raw in &self.includes {
            let pattern = raw.trim().strip_prefix('!').unwrap_or(raw.trim());
            if pattern.is_empty() {
                continue;
            }
            // The walker produces paths relative to the root with no `.` and no
            // doubled separators, so these forms could never match one.
            if pattern.starts_with('/') || pattern.contains("//") || pattern.starts_with("./") {
                return Err(Error::Config(format!("bad include pattern {pattern:?}")));
            }
            if pattern.split('/').any(|part| part == "..") {
                return Err(Error::Config(format!(
                    "include pattern {pattern:?} escapes the root"
                )));
            }
        }
        Ok(())
    }

    pub fn object_key(&self, sha256: &str) -> String {
        format!("{}/objects/{}/{}", self.prefix, &sha256[..2], sha256)
    }

    /// Where a packed shard lives. The same address as an object's, one directory
    /// over, so a bucket listing says at a glance which of the two it is.
    pub fn shard_key(&self, sha256: &str) -> String {
        format!("{}/shards/{}/{}", self.prefix, &sha256[..2], sha256)
    }

    pub fn manifest_key(&self, name: &str) -> String {
        format!("{}/manifests/{}.json", self.prefix, name)
    }

    pub fn latest_key(&self) -> String {
        format!("{}/latest.json", self.prefix)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    File,
    Dir,
    Symlink,
}

/// One filesystem entry. `path` is relative to the backup root with `/`
/// separators and never starts with `/`, so a manifest cannot name a target
/// outside the root it was taken from.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    #[serde(default)]
    pub size: u64,
    /// Permission bits only; ownership is not portable and is not restored.
    #[serde(default)]
    pub mode: u32,
    #[serde(default)]
    pub mtime: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// Which [`Manifest::shards`] carries this file's bytes, when it is packed
    /// rather than standing as an object of its own.
    ///
    /// `#[serde(default)]` is the compatibility lever here, and it is the reason
    /// the manifest's `version` needs no new machinery: a v1 document simply has
    /// no shards, and a reader of either kind can read both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard: Option<usize>,
}

impl Entry {
    /// The object this entry's bytes live in, or `None` when they live in a shard
    /// or there are none to have. A packed entry deliberately answers nothing here:
    /// the keys a run must guarantee in the bucket are the shard's, and a manifest
    /// that also claimed a per-file object for it would be audited for an upload
    /// that was never meant to happen.
    pub fn object_key(&self, prefix: &str) -> Option<String> {
        if self.shard.is_some() {
            return None;
        }
        let sha = self.sha256.as_deref()?;
        Some(format!("{prefix}/objects/{}/{}", &sha[..2], sha))
    }

    /// Whether this file arrives as one member of a packed shard.
    pub fn is_packed(&self) -> bool {
        self.shard.is_some()
    }
}

/// One packed shard: the object its members' bytes live in, and what says whether
/// that object can be reused as it stands.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ShardRef {
    /// sha256 of the container, which is also the key it is stored under.
    pub sha256: String,
    /// The identity of the member list the container was built from; see
    /// [`pack::fingerprint`]. Equal fingerprints mean equal bytes, which is what
    /// lets a run reuse a shard without reading a single one of its files.
    pub fingerprint: String,
    /// Bytes on the wire: the container's own length.
    #[serde(default)]
    pub size: u64,
    /// The files inside it.
    #[serde(default)]
    pub members: usize,
    /// The raw bytes those files hold, so a run can say what a shard is worth
    /// without adding up its members.
    #[serde(default)]
    pub raw: u64,
}

impl ShardRef {
    pub fn object_key(&self, prefix: &str) -> String {
        format!("{prefix}/shards/{}/{}", &self.sha256[..2], self.sha256)
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Counts {
    pub files: usize,
    pub dirs: usize,
    pub symlinks: usize,
    pub bytes: u64,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    /// `1` named one object per file; `2` adds [`Manifest::shards`]. Nothing
    /// branches on this: every field that `2` added defaults, so the shape of the
    /// document says which layout a backup is in.
    pub version: u32,
    /// `20261101T053000Z`; also the manifest's name in the bucket.
    pub name: String,
    pub created_at: i64,
    pub host: String,
    pub root: String,
    #[serde(default)]
    pub counts: Counts,
    /// Bytes actually put on the wire by this run; everything else was reused.
    #[serde(default)]
    pub uploaded_bytes: u64,
    /// Paths the scan could not read, kept so the next run reports them again.
    #[serde(default)]
    pub skipped: Vec<String>,
    pub entries: Vec<Entry>,
    /// The packed containers, in the order their members were grouped. Empty for a
    /// backup that stored every file as an object of its own.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shards: Vec<ShardRef>,
}

impl Manifest {
    /// The object keys this manifest needs, for garbage collection and for the
    /// audit that says the bucket really holds them. A packed file contributes its
    /// shard and not itself: an object no entry points at is not something this
    /// backup needs.
    pub fn object_keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .entries
            .iter()
            .filter(|e| !e.is_packed())
            .filter_map(|e| e.sha256.clone())
            .map(|s| format!("objects/{}/{}", &s[..2], s))
            .collect();
        keys.extend(
            self.shards
                .iter()
                .map(|s| format!("shards/{}/{}", &s.sha256[..2], s.sha256)),
        );
        keys.sort();
        keys.dedup();
        keys
    }

    pub fn absolute(&self, key_prefix: &str) -> Vec<String> {
        self.object_keys()
            .into_iter()
            .map(|k| format!("{key_prefix}/{k}"))
            .collect()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    #[default]
    Idle,
    Scanning,
    Uploading,
    Writing,
    Verifying,
    Pruning,
    Done,
    Failed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Scanning => "scanning",
            Phase::Uploading => "uploading",
            Phase::Writing => "writing",
            Phase::Verifying => "verifying",
            Phase::Pruning => "pruning",
            Phase::Done => "done",
            Phase::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Progress {
    pub phase: Phase,
    pub operation: String,
    pub message: String,
    pub done: usize,
    pub total: usize,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub started_at: i64,
    pub updated_at: i64,
}

/// Progress a worker publishes and the HTTP layer reads. Cloning shares one
/// slot, so the handler can watch while the worker runs.
#[derive(Clone, Default)]
pub struct Reporter(Arc<Mutex<Progress>>);

impl Reporter {
    pub fn new(operation: &str) -> Self {
        let now = now_unix();
        Self(Arc::new(Mutex::new(Progress {
            phase: Phase::Scanning,
            operation: operation.to_string(),
            started_at: now,
            updated_at: now,
            ..Default::default()
        })))
    }

    pub fn snapshot(&self) -> Progress {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set(&self, phase: Phase, message: impl Into<String>) {
        let mut p = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let message = message.into();
        tracing::info!("backup {}: {} — {}", p.operation, phase.as_str(), message);
        p.phase = phase;
        p.message = message;
        p.updated_at = now_unix();
    }

    pub fn set_totals(&self, total: usize, bytes_total: u64) {
        let mut p = self.0.lock().unwrap_or_else(|e| e.into_inner());
        p.total = total;
        p.bytes_total = bytes_total;
        p.updated_at = now_unix();
    }

    pub fn advance(&self, files: usize, bytes: u64) {
        let mut p = self.0.lock().unwrap_or_else(|e| e.into_inner());
        p.done += files;
        p.bytes_done += bytes;
        p.updated_at = now_unix();
    }

    pub fn tick(&self) {
        self.advance(1, 0);
    }
}

pub fn now_unix() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `20261101T053000Z` — sortable, and safe as a key segment.
pub fn stamp_utc() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

pub fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Percent-encode a bucket key for a URL path, keeping `/` literal.
pub(crate) fn encode_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    for byte in key.as_bytes() {
        let c = *byte as char;
        if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '~' | '-' | '/') {
            out.push(c);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Path safety for restore: a manifest entry must stay under the root it is
/// written to.
pub(crate) fn join_under(root: &Path, rel: &str) -> Result<PathBuf> {
    if rel.starts_with('/') || rel.contains("//") || rel.contains('\\') || rel.contains('\0') {
        return Err(Error::Escape(rel.to_string()));
    }
    for part in rel.split('/') {
        if part == ".." || part.is_empty() {
            return Err(Error::Escape(rel.to_string()));
        }
    }
    Ok(root.join(rel))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_under_refuses_escapes() {
        let root = Path::new("/tmp/x");
        assert!(join_under(root, "a/b").unwrap().starts_with(root));
        for bad in ["/abs", "a//b", "../out", "a/../../out", "a\\b", "a/./b/.."] {
            assert!(join_under(root, bad).is_err(), "{bad} should be refused");
        }
    }

    #[test]
    fn keys_are_content_addressed() {
        let cfg = Config {
            token: "t".into(),
            bucket: "u/b".into(),
            ..Default::default()
        };
        let sha = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        assert_eq!(
            cfg.object_key(sha),
            format!("aixodia/root/objects/aa/{sha}")
        );
        assert_eq!(
            cfg.manifest_key("20261101T053000Z"),
            "aixodia/root/manifests/20261101T053000Z.json"
        );
    }

    #[test]
    fn config_needs_token_and_bucket() {
        let cfg = Config::default();
        assert!(cfg.validate().is_err());
        let cfg = Config {
            token: "hf_x".into(),
            bucket: "no-slash".into(),
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    /// A restore is how a wiped tree comes back, so a root that does not exist
    /// is a valid configuration — it is not the machine that is broken yet.
    #[test]
    fn a_config_is_valid_when_its_root_is_gone() {
        let cfg = Config {
            token: "hf_x".into(),
            bucket: "me/bucket".into(),
            root: std::path::PathBuf::from("/definitely/not/here"),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    /// The scope alone can be checked on a partial write, which is what the config
    /// endpoint does — it has no business demanding a token it was not given.
    #[test]
    fn validate_scope_accepts_relative_picks_and_refuses_the_rest() {
        let scoped = |patterns: &[&str]| Config {
            includes: patterns.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        assert!(scoped(&[]).validate_scope().is_ok());
        assert!(
            scoped(&[".codex", "projects/aixodia/app", "*.log", "!root/.cache"])
                .validate_scope()
                .is_ok()
        );
        for bad in [
            "/abs/from/root",
            "a//b",
            "./relative",
            "up/../out",
            "!/abs/from/root",
            "!up/../out",
        ] {
            assert!(
                scoped(&[bad]).validate_scope().is_err(),
                "{bad} must be refused"
            );
        }
    }

    #[test]
    fn object_keys_dedup_within_a_manifest() {
        let entry = |sha: &str| Entry {
            path: "f".into(),
            kind: Kind::File,
            size: 1,
            mode: 0o644,
            mtime: 0,
            target: None,
            sha256: Some(sha.into()),
            shard: None,
        };
        let manifest = Manifest {
            version: 1,
            name: "n".into(),
            created_at: 0,
            host: "h".into(),
            root: "/root".into(),
            counts: Counts::default(),
            uploaded_bytes: 0,
            skipped: vec![],
            entries: vec![
                entry("1111111111111111111111111111111111111111111111111111111111111111"),
                entry("1111111111111111111111111111111111111111111111111111111111111111"),
                entry("2222222222222222222222222222222222222222222222222222222222222222"),
            ],
            shards: vec![],
        };
        assert_eq!(manifest.object_keys().len(), 2);
    }

    /// A manifest that names no shards is what is in the bucket right now, and a
    /// reader that understands packing must still ask for exactly its objects.
    #[test]
    fn a_packed_manifest_needs_its_shards_and_nothing_else() {
        let packed = Entry {
            path: "in-a-shard.txt".into(),
            kind: Kind::File,
            size: 10,
            mode: 0o644,
            mtime: 0,
            target: None,
            sha256: Some("3".repeat(64)),
            shard: Some(0),
        };
        assert!(packed.is_packed());
        assert_eq!(
            packed.object_key("aixodia/root"),
            None,
            "a packed file points at no object of its own, so nothing audits an \
             upload that was never meant to happen"
        );
        let shard = ShardRef {
            sha256: "4".repeat(64),
            fingerprint: "f".into(),
            size: 100,
            members: 1,
            raw: 10,
        };
        let manifest = Manifest {
            version: 2,
            name: "n".into(),
            created_at: 0,
            host: "h".into(),
            root: "/root".into(),
            counts: Counts::default(),
            uploaded_bytes: 0,
            skipped: vec![],
            entries: vec![
                packed,
                Entry {
                    path: "alone.txt".into(),
                    sha256: Some("5".repeat(64)),
                    ..Default::default()
                },
            ],
            shards: vec![shard.clone()],
        };
        assert_eq!(
            manifest.object_keys(),
            vec![
                format!("objects/55/{}", "5".repeat(64)),
                format!("shards/44/{}", "4".repeat(64)),
            ],
            "the packed file's own hash is not a key this backup needs"
        );
        assert_eq!(
            shard.object_key("aixodia/root"),
            format!("aixodia/root/shards/44/{}", "4".repeat(64))
        );
    }

    /// Everything `version` 2 added defaults, which is the whole reason an old
    /// document still reads and no migration is needed for one.
    #[test]
    fn a_version_one_manifest_reads_as_an_unpacked_one() {
        let v1 = r#"{"version":1,"name":"n","created_at":0,"host":"h","root":"/root",
            "counts":{"files":1,"dirs":0,"symlinks":0,"bytes":3},
            "uploaded_bytes":3,"skipped":[],
            "entries":[{"path":"a.txt","kind":"file","size":3,"mode":420,"mtime":0,
                        "sha256":"6666666666666666666666666666666666666666666666666666666666666666"}]}"#;
        let manifest: Manifest = serde_json::from_str(v1).expect("the old shape still reads");
        assert!(manifest.shards.is_empty());
        assert_eq!(manifest.entries[0].shard, None);
        assert!(!manifest.entries[0].is_packed());
        let written = serde_json::to_string(&manifest).expect("and writes back");
        assert!(
            !written.contains("\"shards\""),
            "an unpacked manifest says nothing about shards: {written}"
        );
        assert!(
            !written.contains("\"shard\""),
            "and leaves the field out of every entry: {written}"
        );
    }
}
