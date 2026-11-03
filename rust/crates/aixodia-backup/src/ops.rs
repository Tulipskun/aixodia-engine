//! The two operations.
//!
//! [`backup`] walks the tree, reuses whatever the bucket already holds, and
//! replaces the bucket's one manifest with the result. [`restore`] reads that
//! manifest and rebuilds the tree under a root, in parallel, verifying each
//! file's sha256 as it lands. A file that never arrives is named in the report
//! and the rest of the tree is still written: one bad object must not abandon
//! the four hundred good ones behind it.
//!
//! Both block, on purpose: nothing here is `async`, so a caller cannot hold a
//! tokio worker. Start them with `std::thread::spawn` and read progress from a
//! [`Reporter`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, UNIX_EPOCH};

use crate::pack;
use crate::scan::{self, Scanned};
use crate::xet::{self, Job, Source};
use crate::{
    Config, Counts, Entry, Error, Hub, Kind, Manifest, Phase, Reporter, Resolver, Result,
    RESTORE_ATTEMPTS, RESTORE_CONCURRENCY,
};
/// A file at or above this size is fetched as several range requests.
///
/// The bucket serves roughly a fifth of a megabyte per second to one connection
/// and is generous with many, so the split point is where a single stream stops
/// being the fastest answer: a ninety-megabyte binary left whole is a
/// seven-minute tail on a restore that has otherwise finished.
const RANGE_MIN_SIZE: u64 = 8 * 1024 * 1024;
/// The bytes each range request asks for.
const RANGE_TARGET: u64 = 4 * 1024 * 1024;
/// The most a single file is cut into, so one huge object cannot eat the run.
const RANGE_MAX_PIECES: usize = 12;
/// A file slower than this gets a line of its own, because it is the only way a
/// restore that took minutes can say afterwards where they went.
const SLOW_FILE_MS: u64 = 2_000;

/// What a backup run did. Every byte count is post-exclusion.
#[derive(Clone, Debug, serde::Serialize)]
pub struct BackupReport {
    pub name: String,
    pub manifest_key: String,
    pub bucket: String,
    pub root: String,
    pub files: usize,
    pub dirs: usize,
    pub symlinks: usize,
    pub bytes: u64,
    /// Bytes this run put on the wire. Everything else was either reused
    /// unread from the previous manifest or already in the bucket.
    pub uploaded_bytes: u64,
    pub files_reused: usize,
    pub objects_reused: usize,
    /// How many packed shards the backup is made of.
    pub shards: usize,
    /// How many of them the bucket already held, byte for byte, so that nothing
    /// about their files was read again.
    pub shards_reused: usize,
    /// Files that travelled inside a shard rather than as an object of their own.
    pub packed: usize,
    /// The raw bytes those files hold, which is what a restore writes and the wire
    /// never sees: `uploaded_bytes` is what actually went across.
    pub packed_bytes: u64,
    pub skipped: Vec<String>,
    /// The manifest this run replaced. There is never more than one stored.
    pub deleted_manifests: usize,
    pub duration_ms: u64,
}

/// What a restore run wrote. A file whose content already matched the manifest
/// was not downloaded and counts in `unchanged`.
#[derive(Clone, Debug, serde::Serialize)]
pub struct RestoreReport {
    pub name: String,
    pub root: String,
    pub files: usize,
    pub dirs: usize,
    pub symlinks: usize,
    pub bytes: u64,
    pub unchanged: usize,
    /// What the run could not write, each one named with its reason: a path it
    /// refused, a directory it could not create, a file whose bytes never
    /// arrived. Everything else in the manifest was still written, so this is
    /// the whole distance the tree is short, not a note that the run stopped.
    pub failed: Vec<String>,
    pub duration_ms: u64,
}

/// A content object that still has to be ingested.
#[derive(Clone)]
struct Pending {
    key: String,
    path: PathBuf,
    sha256: Option<String>,
    size: u64,
}

/// One shard of the backup: what the manifest will say about it, and where its
/// bytes are if they still have to go up.
///
/// A reused shard has no `temp`, because there is nothing to send: the bucket
/// already holds the container its fingerprint describes.
#[derive(Clone)]
struct ShardBuild {
    key: String,
    sha256: String,
    fingerprint: String,
    /// Bytes on the wire.
    size: u64,
    /// Bytes the members hold when they are unpacked.
    raw: u64,
    members: usize,
    temp: Option<PathBuf>,
    /// What to pack, kept so a shard the bucket turns out not to hold can be built
    /// again without a second scan of the tree.
    group: Vec<usize>,
}

impl ShardBuild {
    fn reference(&self) -> crate::ShardRef {
        crate::ShardRef {
            sha256: self.sha256.clone(),
            fingerprint: self.fingerprint.clone(),
            size: self.size,
            members: self.members,
            raw: self.raw,
        }
    }
}

pub fn backup(cfg: &Config, reporter: &Reporter) -> Result<BackupReport> {
    let started = std::time::Instant::now();
    cfg.validate()?;
    // A backup reads the tree. Walking a root that is not there would write an
    // empty manifest and then pruning would treat the real tree's objects as
    // unreferenced, so it is refused here rather than at the HTTP layer.
    if !cfg.root.is_dir() {
        return Err(Error::Config(format!(
            "nothing to back up: {} is not a directory",
            cfg.root.display()
        )));
    }
    let hub = Hub::new(cfg)?;

    reporter.set(Phase::Scanning, "reading the previous manifest");
    let previous = latest_manifest(&hub)?;
    let mut known: HashSet<String> = HashSet::new();
    match previous.as_ref() {
        Some(manifest) => known.extend(manifest.absolute(&cfg.prefix)),
        None => reporter.set(Phase::Scanning, "no previous backup: this one is full"),
    }

    let mut skipped = Vec::new();
    let scanned: Vec<Scanned> = scan::scan(&cfg.root, &cfg.excludes, &cfg.includes, &mut skipped)?;
    // Name the scope in the progress line. An operator who narrowed the allowlist
    // and sees the whole tree's count would assume their pick did nothing; the
    // number alone cannot tell a narrow scope from a broken one.
    let scope = match cfg.includes.len() {
        0 => String::new(),
        1 => " (allowlist: 1 folder)".to_string(),
        n => format!(" (allowlist: {n} entries)"),
    };
    reporter.set(
        Phase::Scanning,
        format!(
            "{} entries under {}{scope}",
            scanned.len(),
            cfg.root.display()
        ),
    );

    let mut unreadable = Vec::new();
    let mut plan = scan::plan(
        &cfg.root,
        scanned,
        previous.as_ref(),
        &known,
        &cfg.prefix,
        &mut unreadable,
    )?;
    skipped.extend(unreadable);
    reporter.set(
        Phase::Scanning,
        format!(
            "{} of {} file(s) must be read, {} reused unread",
            plan.uploads.len(),
            plan.entries.iter().filter(|e| e.kind == Kind::File).count(),
            plan.reused
        ),
    );

    // Everything that can be packed is, and packed here: a restore pays for its
    // round trips, and nineteen thousand of them is the eight minutes the operator
    // is not willing to wait out. This runs before the object list is read, since
    // a packed entry names no object of its own.
    let scratch = scratch_dir()?;
    let mut lost: Vec<String> = Vec::new();
    let mut shards = pack_shards(
        cfg,
        &mut plan.entries,
        previous.as_ref(),
        &known,
        &scratch,
        reporter,
        &mut lost,
    )?;
    if !lost.is_empty() {
        // Files that had gone by the time their container was built, which on a live
        // tree is a matter of when, not whether. They leave the manifest exactly as
        // an unreadable file leaves it when the scan finds one, and every shard's
        // member positions move with them — a shard whose list pointed at a removed
        // entry would be rebuilt around the wrong file.
        tracing::warn!(
            "backup: {} file(s) went before they could be packed",
            lost.len()
        );
        let gone: HashSet<&str> = lost.iter().map(String::as_str).collect();
        let mut remap: Vec<usize> = Vec::with_capacity(plan.entries.len());
        let mut kept: Vec<Entry> = Vec::with_capacity(plan.entries.len());
        for entry in std::mem::take(&mut plan.entries) {
            if gone.contains(entry.path.as_str()) {
                continue;
            }
            remap.push(kept.len());
            kept.push(entry);
        }
        plan.entries = kept;
        for shard in shards.iter_mut() {
            shard.group = shard
                .group
                .iter()
                .filter_map(|&at| remap.get(at).copied())
                .collect();
        }
        skipped.extend(
            lost.into_iter()
                .map(|path| format!("{path} (gone before it could be packed)")),
        );
    }

    // Which bytes belong to which object key, so a file that turns out to be
    // missing from the bucket can be sent again by name. A packed entry names no
    // key of its own, so what is left here is the short list of files that still
    // travel as single objects.
    let mut owners: HashMap<String, Pending> = HashMap::new();
    for entry in &plan.entries {
        let Some(key) = entry.object_key(&cfg.prefix) else {
            continue;
        };
        if owners.contains_key(&key) {
            // Two files, one content: the second reuses the first's object.
            continue;
        }
        owners.insert(
            key.clone(),
            Pending {
                key: key.clone(),
                path: crate::join_under(&cfg.root, &entry.path)?,
                sha256: entry.sha256.clone(),
                size: entry.size,
            },
        );
    }
    let mut pending: Vec<Pending> = Vec::new();
    let mut objects_reused = 0usize;
    let mut object_keys: Vec<&String> = owners.keys().collect();
    object_keys.sort();
    for key in object_keys {
        if known.contains(key) {
            // The previous manifest named it, and objects are never deleted.
            objects_reused += 1;
            continue;
        }
        pending.push(owners[key].clone());
    }

    // A newly built shard is asked about in the same breath as an object, because
    // a run stopped after uploading and before publishing would otherwise send the
    // same container again.
    let mut to_ask: Vec<String> = Vec::new();
    for shard in shards.iter().filter(|s| s.temp.is_some()) {
        to_ask.push(shard.key.clone());
    }
    for object in &pending {
        to_ask.push(object.key.clone());
    }
    if !to_ask.is_empty() {
        reporter.set(
            Phase::Uploading,
            format!("asking the bucket about {} object(s)", to_ask.len()),
        );
        let existing: HashSet<String> = hub.existing_keys(&to_ask)?.into_iter().collect();
        let before = pending.len();
        pending.retain(|p| !existing.contains(&p.key));
        objects_reused += before - pending.len();
        for shard in shards.iter_mut() {
            if existing.contains(&shard.key) {
                // The bucket holds these bytes already, so the container built for
                // them was work done for nothing, and it goes with the rest of the
                // scratch when the run ends.
                shard.temp = None;
            }
        }
    }

    let mut uploaded_bytes = 0u64;
    let going: Vec<&ShardBuild> = shards.iter().filter(|s| s.temp.is_some()).collect();
    if pending.is_empty() && going.is_empty() {
        reporter.set(Phase::Uploading, "nothing new to upload");
    } else {
        reporter.set(
            Phase::Uploading,
            format!(
                "{} shard(s) and {} object(s), {} byte(s)",
                going.len(),
                pending.len(),
                going.iter().map(|s| s.size).sum::<u64>()
                    + pending.iter().map(|p| p.size).sum::<u64>()
            ),
        );
        let mut jobs: Vec<Job> = Vec::new();
        for shard in going {
            // `going` is exactly the shards with a container on disk.
            if let Some(temp) = &shard.temp {
                jobs.push(Job {
                    key: shard.key.clone(),
                    source: Source::File(temp.clone()),
                    sha256: Some(shard.sha256.clone()),
                    size: shard.size,
                });
            }
        }
        jobs.extend(pending.into_iter().map(|p| Job {
            key: p.key,
            source: Source::File(p.path),
            sha256: p.sha256,
            size: p.size,
        }));
        let done = xet::ingest(cfg, jobs, reporter.clone())?;
        let now = epoch_ms();
        let items: Vec<(String, String, i64, Option<&str>)> = done
            .iter()
            .map(|d| (d.key.clone(), d.xet_hash.clone(), now, None))
            .collect();
        hub.add_files(&items)?;
        uploaded_bytes = done.iter().map(|d| d.size).sum();
    }

    let counts = count(&plan.entries);
    let name = crate::stamp_utc();
    let mut manifest = Manifest {
        version: 2,
        name: name.clone(),
        created_at: crate::now_unix(),
        host: crate::hostname(),
        root: cfg.root.display().to_string(),
        counts,
        uploaded_bytes,
        skipped: skipped.clone(),
        entries: plan.entries,
        shards: shards.iter().map(ShardBuild::reference).collect(),
    };

    // A manifest naming an object the bucket does not hold is how a restore comes
    // back with holes in the tree, so the bucket is asked whether it holds every
    // key — shards included — before the manifest is published. This is now a
    // request about a few dozen keys rather than nineteen thousand, which is the
    // other thing packing bought.
    let mut absent: Vec<String> = manifest.absolute(&cfg.prefix);
    {
        let present: HashSet<String> = hub.existing_keys(&absent)?.into_iter().collect();
        absent.retain(|key| !present.contains(key));
    }
    if !absent.is_empty() {
        // Trusting the previous manifest's key list was what let a hole through: an
        // object the last run believed was there is re-sent here, once, and then
        // believed only if the bucket says so.
        tracing::warn!(
            "backup: {} object(s) the bucket did not hold: {}",
            absent.len(),
            absent[..absent.len().min(5)].join(", ")
        );
        reporter.set(
            Phase::Uploading,
            format!(
                "{} object(s) were missing from the bucket; sending them",
                absent.len()
            ),
        );
        let missing_shards: Vec<String> = absent
            .iter()
            .filter(|key| shards.iter().any(|s| key == &&s.key))
            .cloned()
            .collect();
        let missing_objects: Vec<String> = absent
            .iter()
            .filter(|key| owners.contains_key(*key))
            .cloned()
            .collect();
        uploaded_bytes += rebuild_shards(
            &hub,
            cfg,
            reporter,
            &shards,
            &missing_shards,
            &scratch,
            &manifest.entries,
        )?;
        uploaded_bytes += upload_objects(&hub, cfg, reporter, &missing_objects, &owners)?;
        let now: HashSet<String> = hub.existing_keys(&absent)?.into_iter().collect();
        absent.retain(|key| !now.contains(key));
        if !absent.is_empty() {
            // A committed object can be slow to show up, so one more look is
            // cheaper than failing a run over a lag.
            std::thread::sleep(Duration::from_secs(1));
            let late: HashSet<String> = hub.existing_keys(&absent)?.into_iter().collect();
            absent.retain(|key| !late.contains(key));
        }
        if !absent.is_empty() {
            // Refuse to publish. `latest.json` still names the manifest that was
            // good before this run, so the next restore gets a tree that is whole;
            // a half-true manifest is the thing that broke the machine.
            return Err(Error::Api {
                operation: "backup".into(),
                message: format!(
                    "{} object(s) are still not in the bucket after upload, so this manifest was not published: {}",
                    absent.len(),
                    absent[..absent.len().min(5)].join(", ")
                ),
            });
        }
    }

    manifest.uploaded_bytes = uploaded_bytes;
    let manifest_key = hub.manifest_key(&name);
    let pointer = serde_json::json!({
        "name": manifest.name,
        "created_at": manifest.created_at,
        "manifest": manifest_key,
    });

    reporter.set(Phase::Writing, format!("writing manifest {name}"));
    // One batch, manifest first: until `latest.json` names it, readers still
    // see the manifest that was good before this run.
    write_json(
        &hub,
        cfg,
        &[
            (
                manifest_key.as_str(),
                serde_json::to_vec_pretty(&manifest)?,
                Some("application/json"),
            ),
            (
                hub.latest_key().as_str(),
                serde_json::to_vec_pretty(&pointer)?,
                Some("application/json"),
            ),
        ],
    )?;

    // The bucket holds one manifest: this run's. Older ones are replaced,
    // while objects stay so the next run can reuse them unread.
    let deleted_manifests = {
        reporter.set(Phase::Pruning, "replacing the previous manifest");
        let stale: Vec<String> = hub
            .list_files(&hub.manifests_prefix())?
            .into_iter()
            .map(|item| item.path)
            .filter(|path| path != &manifest_key)
            .collect();
        if !stale.is_empty() {
            hub.delete_keys(&stale)?;
        }
        stale.len()
    };

    // The containers were only ever a way to get bytes onto the wire. The scratch
    // holds them until the manifest that names them is published, and nothing after.
    let _ = fs::remove_dir_all(&scratch);

    let shards_reused = shards.iter().filter(|s| s.temp.is_none()).count();
    let packed = shards.iter().map(|s| s.members).sum::<usize>();
    reporter.set(
        Phase::Done,
        format!(
            "{} file(s) in {} shard(s) ({} reused as they stood), {} byte(s) uploaded, {} file(s) re-hashed for nothing",
            manifest.counts.files,
            shards.len(),
            shards_reused,
            uploaded_bytes,
            plan.reused
        ),
    );
    Ok(BackupReport {
        name,
        manifest_key,
        bucket: hub.bucket().to_string(),
        root: manifest.root.clone(),
        files: manifest.counts.files,
        dirs: manifest.counts.dirs,
        symlinks: manifest.counts.symlinks,
        bytes: manifest.counts.bytes,
        uploaded_bytes,
        files_reused: plan.reused,
        objects_reused,
        shards: shards.len(),
        shards_reused,
        packed,
        packed_bytes: shards.iter().map(|s| s.raw).sum(),
        skipped,
        deleted_manifests,
        duration_ms: elapsed_ms(started),
    })
}

/// Paths that write themselves. A log line or a state save changes its file's
/// bytes on its own account, so those files are grouped into shards of their own
/// at the end of the run: a shard that mixed them with the toolchain would be
/// rebuilt, and its thirty-two megabytes re-sent, every time a log turns over.
const VOLATILE: &[&str] = &[".aixodia", "logs", "*.log", "*.pid", "*.state"];

/// Where the containers are built. Not under the root, because a backup of `/root`
/// must not find its own scratch in the tree it is scanning.
fn scratch_dir() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("aixodia-shards-{}", std::process::id()));
    fs::create_dir_all(&dir).map_err(|source| Error::Io {
        path: dir.display().to_string(),
        source,
    })?;
    Ok(dir)
}

/// Put every file the manifest names into a shard, and say which.
///
/// Reuse is decided without reading a file: a group whose member list is identical
/// to a shard of the previous manifest describes identical bytes, so its stored key
/// can be written straight into the new manifest and neither the files nor the
/// container are touched again. Only groups that are new or changed are built.
///
/// `entries` is updated in place — each packed file gains its shard's position in
/// the returned list, which is the numbering the published manifest uses.
///
/// `lost_names` collects the files that had gone by the time their container was
/// built. They belong in the manifest's `skipped` list and out of its entries: the
/// scan treats an unreadable file the same way, and a manifest that named a file no
/// container holds would be the old bug with better paperwork.
fn pack_shards(
    cfg: &Config,
    entries: &mut [Entry],
    previous: Option<&Manifest>,
    known: &HashSet<String>,
    scratch: &Path,
    reporter: &Reporter,
    lost_names: &mut Vec<String>,
) -> Result<Vec<ShardBuild>> {
    let volatile: Vec<String> = VOLATILE.iter().map(|pattern| pattern.to_string()).collect();
    let mut steady: Vec<usize> = Vec::new();
    let mut shifting: Vec<usize> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if entry.kind != Kind::File || entry.sha256.is_none() {
            // A directory and a link carry nothing a frame could hold, and a file
            // whose content was never read has no identity to pack.
            continue;
        }
        if scan::is_excluded(&entry.path, &volatile) {
            shifting.push(index);
        } else {
            steady.push(index);
        }
    }
    reporter.set(
        Phase::Uploading,
        format!(
            "packing {} file(s) into shards",
            steady.len() + shifting.len()
        ),
    );

    let previous_by_fingerprint: HashMap<&str, &crate::ShardRef> = previous
        .map(|manifest| {
            manifest
                .shards
                .iter()
                .map(|shard| (shard.fingerprint.as_str(), shard))
                .collect()
        })
        .unwrap_or_default();

    let mut shards: Vec<ShardBuild> = Vec::new();
    let whole = steady.len() + shifting.len();
    let mut read = 0usize;
    for set in [steady, shifting] {
        let sizes: Vec<u64> = set.iter().map(|&index| entries[index].size).collect();
        for group in pack::group(&sizes) {
            // Positions in `entries`, so a shard can be rebuilt by name alone.
            let group: Vec<usize> = group.into_iter().map(|at| set[at]).collect();
            let members: Vec<pack::Member> = group
                .iter()
                .map(|&index| pack::Member {
                    path: entries[index].path.clone(),
                    size: entries[index].size,
                    sha256: entries[index].sha256.clone().unwrap_or_default(),
                })
                .collect();
            let fingerprint = pack::fingerprint(&members);
            // Two halves to a reuse claim: the member list must match, and the
            // object its bytes live in must actually be in the bucket.
            let standing = previous_by_fingerprint
                .get(fingerprint.as_str())
                .filter(|shard| known.contains(&cfg.shard_key(&shard.sha256)))
                .map(|shard| (shard.sha256.clone(), shard.size));
            // The files this container actually holds, at the sizes and digests it
            // actually wrote. The group as scanned is a plan, not a promise: a live
            // tree loses files and grows logs while the container is being built, and
            // the manifest that publishes has to describe the bytes inside it.
            let (sha256, size, fingerprint, temp, held) = match standing {
                Some((sha256, size)) => (sha256, size, fingerprint, None, group),
                None => {
                    let temp = scratch.join(format!("{fingerprint}.shard"));
                    let built = pack::build(&members, &cfg.root, &temp)?;
                    if built.sha256.is_empty() {
                        // Every file in the group had gone. There is no container to
                        // name, and the files it would have carried are reported.
                        lost_names.extend(built.lost);
                        continue;
                    }
                    let mut held: Vec<usize> = Vec::with_capacity(group.len());
                    let mut as_packed: Vec<pack::Member> = Vec::with_capacity(group.len());
                    for (index, member) in group.into_iter().zip(members) {
                        if built.lost.contains(&member.path) {
                            continue;
                        }
                        let member = match built.drifted.iter().find(|d| d.path == member.path) {
                            Some(d) => {
                                // The container holds the new bytes, so this is the
                                // entry that names them; a restore that asked for the
                                // scan's version would refuse a file it could write.
                                entries[index].size = d.size;
                                entries[index].sha256 = Some(d.sha256.clone());
                                pack::Member {
                                    path: d.path.clone(),
                                    size: d.size,
                                    sha256: d.sha256.clone(),
                                }
                            }
                            None => member,
                        };
                        as_packed.push(member);
                        held.push(index);
                    }
                    if !built.drifted.is_empty() {
                        tracing::info!(
                            "backup: {} file(s) moved while they were packed; the manifest names \
                             what was read: {}",
                            built.drifted.len(),
                            built.drifted[0].path
                        );
                    }
                    lost_names.extend(built.lost);
                    // Fewer files, or different bytes, than the list it was asked for:
                    // the identity a later reuse is decided by describes the container
                    // that exists rather than the one that was planned.
                    (
                        built.sha256,
                        built.size,
                        pack::fingerprint(&as_packed),
                        Some(temp),
                        held,
                    )
                }
            };
            if temp.is_some() {
                // A minute of reading twenty-five thousand files is a long time to
                // show nobody's progress, and a reused container reads nothing.
                read += held.len();
                reporter.set(
                    Phase::Uploading,
                    format!(
                        "packing {whole} file(s): {read} read, {} container(s) built",
                        shards.len()
                    ),
                );
            }
            let position = shards.len();
            let shard = ShardBuild {
                key: cfg.shard_key(&sha256),
                size,
                raw: held.iter().map(|&index| entries[index].size).sum(),
                members: held.len(),
                temp,
                group: held,
                sha256,
                fingerprint,
            };
            for index in &shard.group {
                entries[*index].shard = Some(position);
            }
            shards.push(shard);
        }
    }
    Ok(shards)
}

/// Build and send the containers the bucket turned out not to hold.
///
/// A reused shard is a claim about a previous run, and a claim the bucket denies is
/// repaired here rather than published: this is the half of the pre-publish audit
/// that only packing has, and a manifest naming a container that is not there is
/// exactly the hole that cost the operator a tree.
fn rebuild_shards(
    hub: &Hub,
    cfg: &Config,
    reporter: &Reporter,
    shards: &[ShardBuild],
    keys: &[String],
    scratch: &Path,
    entries: &[Entry],
) -> Result<u64> {
    let mut jobs = Vec::new();
    for key in keys {
        let shard = shards
            .iter()
            .find(|shard| &shard.key == key)
            .ok_or_else(|| {
                Error::Manifest(key.clone(), "no shard of this manifest names it".into())
            })?;
        let members: Vec<pack::Member> = shard
            .group
            .iter()
            .map(|&index| pack::Member {
                path: entries[index].path.clone(),
                size: entries[index].size,
                sha256: entries[index].sha256.clone().unwrap_or_default(),
            })
            .collect();
        let temp = scratch.join(format!("repair-{}.shard", &shard.fingerprint[..16]));
        let built = pack::build(&members, &cfg.root, &temp)?;
        let (sha256, size, lost) = (built.sha256, built.size, built.lost);
        if !lost.is_empty() {
            // A repair has to reproduce the bytes the manifest already names, and a
            // member that is gone cannot be reproduced. A normal backup drops such a
            // file from its own shard and carries on; a repair has no such choice, and
            // saying which file went is better than publishing a short container under
            // a key that describes a full one.
            let _ = fs::remove_file(&temp);
            return Err(Error::Api {
                operation: "backup".into(),
                message: format!(
                    "shard {} cannot be rebuilt: {} of its members went, starting with {}",
                    shard.fingerprint,
                    lost.len(),
                    lost[0]
                ),
            });
        }
        if sha256 != shard.sha256 {
            // The manifest already names a key built from this member list, and the
            // bytes say otherwise. Nothing the manifest claims can be trusted after
            // that, and publishing a container under a hash that is not its own is
            // how every later restore of it fails verification.
            let _ = fs::remove_file(&temp);
            return Err(Error::Api {
                operation: "backup".into(),
                message: format!(
                    "shard {} rebuilt as {sha256} and not {}; its member list does not describe these bytes",
                    shard.fingerprint, shard.sha256
                ),
            });
        }
        jobs.push(Job {
            key: key.clone(),
            source: Source::File(temp),
            sha256: Some(sha256),
            size,
        });
    }
    if jobs.is_empty() {
        return Ok(0);
    }
    let done = xet::ingest(cfg, jobs, reporter.clone())?;
    let now = epoch_ms();
    let items: Vec<(String, String, i64, Option<&str>)> = done
        .iter()
        .map(|d| (d.key.clone(), d.xet_hash.clone(), now, None))
        .collect();
    hub.add_files(&items)?;
    Ok(done.iter().map(|d| d.size).sum())
}

/// The manifest `latest.json` points at, or `None` when there is no backup yet.
pub fn latest_manifest(hub: &Hub) -> Result<Option<Manifest>> {
    let Some(bytes) = hub.get_bytes(&hub.latest_key())? else {
        return Ok(None);
    };
    let pointer: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| Error::Manifest("latest.json".into(), e.to_string()))?;
    let name = pointer["name"]
        .as_str()
        .ok_or_else(|| Error::Manifest("latest.json".into(), "no name field".into()))?;
    fetch_manifest(hub, name).map(Some)
}

/// One manifest by name (`20261101T053000Z`).
pub fn fetch_manifest(hub: &Hub, name: &str) -> Result<Manifest> {
    let key = hub.manifest_key(name);
    let bytes = hub.get_bytes(&key)?.ok_or_else(|| {
        Error::Manifest(
            name.to_string(),
            "the bucket has no manifest with that name".into(),
        )
    })?;
    serde_json::from_slice(&bytes).map_err(|e| Error::Manifest(name.to_string(), e.to_string()))
}

/// Rebuild the bucket's manifest under `root`.
///
/// Nothing is deleted: files the manifest does not name are left exactly as
/// they are, so a restore onto a live tree only ever adds and overwrites. The
/// run always goes through the whole manifest and reports what it could not
/// write, so a partial tree is never the surprise ending of a failed call.
pub fn restore(cfg: &Config, root: &Path, reporter: &Reporter) -> Result<RestoreReport> {
    let started = std::time::Instant::now();
    cfg.validate()?;
    let hub = Hub::new(cfg)?;
    let reading = std::time::Instant::now();
    let manifest = latest_manifest(&hub)?
        .ok_or_else(|| Error::Config("the bucket holds no backup yet".into()))?;
    tracing::info!(
        "restore: manifest {} holds {} entries, read in {}ms",
        manifest.name,
        manifest.entries.len(),
        elapsed_ms(reading),
    );
    fs::create_dir_all(root).map_err(|source| Error::Io {
        path: root.display().to_string(),
        source,
    })?;

    reporter.set(
        Phase::Writing,
        format!("restoring {} into {}", manifest.name, root.display()),
    );
    reporter.set_totals(
        manifest.entries.len(),
        manifest
            .entries
            .iter()
            .filter(|e| e.kind == Kind::File)
            .map(|e| e.size)
            .sum(),
    );

    let mut problems: Vec<String> = Vec::new();
    let mut dirs: Vec<&Entry> = Vec::new();
    let mut files: Vec<(Entry, PathBuf)> = Vec::new();
    let mut links: Vec<(Entry, PathBuf)> = Vec::new();
    for entry in &manifest.entries {
        // An entry whose path cannot be honoured is that entry's own problem.
        // The rest of the manifest is still worth restoring.
        let path = match crate::join_under(root, &entry.path) {
            Ok(path) => path,
            Err(err) => {
                problems.push(format!("{}: {err}", entry.path));
                continue;
            }
        };
        match entry.kind {
            Kind::Dir => dirs.push(entry),
            Kind::File => files.push((entry.clone(), path)),
            Kind::Symlink => links.push((entry.clone(), path)),
        }
    }
    // Shallowest first, so a parent directory always exists before its
    // children are written into it.
    dirs.sort_by_key(|e| depth(&e.path));
    let mut dirs_out = 0usize;
    for entry in dirs.clone() {
        let Ok(path) = crate::join_under(root, &entry.path) else {
            continue;
        };
        match fs::create_dir_all(&path) {
            Ok(()) => {
                let _ =
                    fs::set_permissions(&path, fs::Permissions::from_mode(entry.mode.max(0o700)));
                dirs_out += 1;
            }
            Err(source) => problems.push(format!("{}: create directory: {source}", entry.path)),
        }
        reporter.tick();
    }

    links.sort_by_key(|(_, path)| path.components().count());
    let mut links_out = 0usize;
    for (entry, path) in &links {
        let want = entry.target.as_deref().unwrap_or_default();
        if fs::symlink_metadata(path).is_ok() {
            if read_link_text(path).as_deref() == Some(want) {
                links_out += 1;
                reporter.tick();
                continue;
            }
            if let Err(source) = fs::remove_file(path) {
                problems.push(format!("{}: replace what is there: {source}", entry.path));
                reporter.tick();
                continue;
            }
        }
        if let Some(parent) = path.parent() {
            if let Err(source) = fs::create_dir_all(parent) {
                problems.push(format!("{}: {}: {source}", entry.path, parent.display()));
                reporter.tick();
                continue;
            }
        }
        match symlink(want, path) {
            Ok(()) => links_out += 1,
            Err(source) => problems.push(format!("{}: link to {want}: {source}", entry.path)),
        }
        reporter.tick();
    }

    let wanted_files = files.len();
    let counters = Arc::new(Mutex::new((0usize, 0u64, 0usize)));
    let errors: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(problems));
    let slowest: Arc<Mutex<(u64, String)>> = Arc::new(Mutex::new((0, String::new())));
    // Where a container is held while its members are taken out of it. Not in the
    // tree: what appears there is what the manifest names, and a shard is nobody's
    // file. A v1 restore never writes in it.
    let scratch = scratch_dir()?;

    // The unit of work is a range request, not a file. The bucket answers a
    // download at a fraction of a megabyte per second per connection, so one
    // ninety-megabyte binary is a seven-minute tail on a restore that is
    // otherwise finished — and a single request cannot use more than one
    // connection. A large file is therefore cut into pieces that land in the
    // same temporary sibling at their own offsets, and any worker may take any
    // piece of any file.
    let mut slots: Vec<Slot> = Vec::new();
    let mut queue: VecDeque<Piece> = VecDeque::new();
    // The files still owed by a container, by shard position. A file whose bytes are
    // already in the tree never appears here, which is why a warm restore of a packed
    // backup asks the bucket for nothing at all.
    let mut members: HashMap<usize, Vec<(String, MemberTarget)>> = HashMap::new();
    for (entry, path) in files {
        let label = entry.path.clone();
        let want = entry.sha256.clone().unwrap_or_default();
        if let Ok(found) = fs::symlink_metadata(&path) {
            if found.is_file() && found.len() == entry.size {
                // A restore is the only writer of a restored file's stamp, so an
                // mtime that matches is enough to know the bytes are current.
                let fresh = entry.mtime > 0 && found.mtime() == entry.mtime;
                let same = !want.is_empty() && scan::hash_file(&path).is_ok_and(|got| got == want);
                if fresh || same {
                    if let Err(source) = fs::set_permissions(
                        &path,
                        fs::Permissions::from_mode(entry.mode.max(0o600)),
                    ) {
                        note(&errors, format!("{label}: set mode: {source}"));
                    } else {
                        set_mtime(&path, entry.mtime);
                        counters.lock().unwrap_or_else(|e| e.into_inner()).2 += 1;
                    }
                    reporter.advance(1, entry.size);
                    continue;
                }
            } else if !found.is_file() {
                // Something else occupies the path: a symlink, or a directory
                // where a file belongs. Refusing to replace a directory is how a
                // restore dies at the moment the machine needs it most.
                if found.is_dir() {
                    tracing::warn!(
                        "restore: {label} is a directory in a file's place; replacing it"
                    );
                }
                let removing = if found.is_dir() {
                    fs::remove_dir_all(&path)
                } else {
                    fs::remove_file(&path)
                };
                if let Err(source) = removing {
                    note(&errors, format!("{label}: replace what is there: {source}"));
                    reporter.tick();
                    continue;
                }
            }
        }
        if let Some(parent) = path.parent() {
            if let Err(source) = fs::create_dir_all(parent) {
                note(&errors, format!("{label}: {}: {source}", parent.display()));
                reporter.tick();
                continue;
            }
        }
        if entry.size == 0 {
            // Nothing to fetch: an empty file is entirely its own content.
            if let Err(source) = fs::File::create(&path) {
                note(&errors, format!("{label}: create: {source}"));
                reporter.tick();
                continue;
            }
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(entry.mode.max(0o600)));
            set_mtime(&path, entry.mtime);
            counters.lock().unwrap_or_else(|e| e.into_inner()).0 += 1;
            reporter.tick();
            continue;
        }
        // Packed or standing alone. A file the manifest put inside a container waits
        // for that container; one it stored as its own object is fetched here, which
        // is the whole of a v1 manifest's story.
        if let Some(at) = entry.shard {
            if manifest.shards.get(at).is_none() {
                // A number past the end of the shard list is this file's problem, and
                // no reason to abandon the rest of the tree.
                note(
                    &errors,
                    format!("{label}: shard {at} is not in this manifest"),
                );
                reporter.tick();
                continue;
            }
            members.entry(at).or_default().push((
                label,
                MemberTarget {
                    temp: crate::hub::temp_path(&path),
                    // Its sibling is created when it is decoded, not now: a pre-sized
                    // one would promise a length this file has not earned yet.
                    path,
                    want,
                    size: entry.size,
                    mode: entry.mode,
                    mtime: entry.mtime,
                },
            ));
            continue;
        }
        let Some(key) = entry.object_key(&cfg.prefix) else {
            note(&errors, format!("{label}: no object key"));
            reporter.tick();
            continue;
        };
        let temp = crate::hub::temp_path(&path);
        if let Err(source) = sized_temp(&temp, entry.size) {
            note(&errors, format!("{label}: {}: {source}", temp.display()));
            reporter.tick();
            continue;
        }
        let ranges = ranges_for(entry.size);
        let index = slots.len();
        slots.push(Slot {
            index,
            label,
            key,
            path,
            temp,
            want,
            size: entry.size,
            mode: entry.mode,
            mtime: entry.mtime,
            seal: Seal::File,
            shard: None,
            outstanding: AtomicUsize::new(ranges.len()),
            failed: Mutex::new(None),
            verifies: AtomicUsize::new(0),
            started: std::time::Instant::now(),
        });
        queue.extend(ranges.into_iter().map(|(first, last)| Piece {
            slot: index,
            first,
            last,
            attempts: 0,
        }));
    }

    // One slot per container that still owes a file. This is where packing earns its
    // keep: thirty-odd requests, each range-split like a large file and carried by
    // whichever worker is free, instead of nineteen thousand of them.
    let mut groups: Vec<(usize, Vec<(String, MemberTarget)>)> = members.into_iter().collect();
    groups.sort_by_key(|(at, _)| *at);
    for (at, group) in groups {
        let Some(shard) = manifest.shards.get(at) else {
            continue;
        };
        let seal = Arc::new(Mutex::new(ShardSeal {
            targets: group.into_iter().collect(),
        }));
        if shard.size == 0 {
            // There is nothing to fetch from an empty object, and a file nobody could
            // write is named rather than left silent.
            let lost = seal.lock().unwrap_or_else(|e| e.into_inner()).owed();
            for label in lost {
                note(&errors, format!("{label}: its shard is empty"));
                reporter.tick();
            }
            continue;
        }
        let temp = scratch.join(format!("restore-{at}.shard"));
        let ranges = ranges_for(shard.size);
        let index = slots.len();
        slots.push(Slot {
            index,
            label: format!("shard {at}/{}", manifest.shards.len()),
            key: shard.object_key(&cfg.prefix),
            // A container has no name in the tree: its members are what appear there.
            path: temp.clone(),
            temp,
            want: shard.sha256.clone(),
            size: shard.size,
            mode: 0,
            mtime: 0,
            seal: Seal::Shard,
            shard: Some(seal),
            outstanding: AtomicUsize::new(ranges.len()),
            failed: Mutex::new(None),
            verifies: AtomicUsize::new(0),
            started: std::time::Instant::now(),
        });
        queue.extend(ranges.into_iter().map(|(first, last)| Piece {
            slot: index,
            first,
            last,
            attempts: 0,
        }));
    }
    let slots = Arc::new(slots);
    let queue = Arc::new(Mutex::new(queue));
    // One window on the bucket per worker. A `reqwest` blocking client drives
    // every request it is handed from a single internal thread, so workers that
    // share one client queue behind each other — and behind whatever one of them
    // happens to be stuck on.
    let downloaders = (0..RESTORE_CONCURRENCY)
        .map(|_| hub.downloader())
        .collect::<Result<Vec<_>>>()?;
    let transferring = std::time::Instant::now();
    std::thread::scope(|scope| {
        for downloader in &downloaders {
            let (slots, queue, counters, errors, slowest, reporter) = (
                &slots,
                &queue,
                &counters,
                &errors,
                &slowest,
                reporter.clone(),
            );
            scope.spawn(move || loop {
                let next = queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
                let Some(piece) = next else { break };
                let slot = &slots[piece.slot];
                if let Some(again) = run_piece(downloader, slot, piece) {
                    queue
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push_back(again);
                    continue;
                }
                // Only the worker that takes the last piece of a file finalises
                // it, so a file is hashed and renamed exactly once however many
                // hands carried its bytes.
                if slot.outstanding.fetch_sub(1, Ordering::AcqRel) == 1 {
                    finish_slot(slot, queue, counters, errors, slowest, &reporter);
                }
            });
        }
    });
    let problems = errors.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let (files_out, bytes, unchanged) = *counters.lock().unwrap_or_else(|e| e.into_inner());
    let (slow_ms, slow_path) = slowest.lock().unwrap_or_else(|e| e.into_inner()).clone();
    tracing::info!(
        "restore: {} of {wanted_files} file(s) written in {}ms, {unchanged} already current, {} failed",
        files_out,
        elapsed_ms(transferring),
        problems.len(),
    );
    if !slow_path.is_empty() {
        tracing::warn!("restore: slowest file {slow_path} took {slow_ms}ms");
    }

    if !problems.is_empty() {
        // Whatever the run could write, it wrote, and then it says which files
        // it could not. Refusing the whole run over one of them is what left a
        // tree half-restored with no record of which half.
        reporter.set(
            Phase::Failed,
            format!(
                "{files_out} file(s) written, {} could not be",
                problems.len()
            ),
        );
    } else {
        reporter.set(
            Phase::Done,
            format!("{files_out} file(s) restored under {}", root.display()),
        );
    }
    // The containers were only ever a way to get many files down at once. What the
    // tree holds afterwards is what the manifest names.
    let _ = fs::remove_dir_all(&scratch);
    Ok(RestoreReport {
        name: manifest.name.clone(),
        root: root.display().to_string(),
        files: files_out,
        dirs: dirs_out,
        symlinks: links_out,
        bytes,
        unchanged,
        failed: problems,
        duration_ms: elapsed_ms(started),
    })
}

/// What a settled slot's bytes turn into: a file that stands as an object of its
/// own, or a container holding other files' bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Seal {
    File,
    Shard,
}

/// One packed file, waiting for the container that carries its bytes.
#[derive(Clone, Debug)]
struct MemberTarget {
    /// Where it goes once its content has been verified.
    path: PathBuf,
    /// The sibling it is decoded into. Making the final name appear only at the
    /// end is what keeps a half-restored tree indistinguishable from a tree that
    /// was not started, exactly as it is for a file of its own.
    temp: PathBuf,
    want: String,
    size: u64,
    mode: u32,
    mtime: i64,
}

/// The files one container carries.
///
/// A shard is a request that either succeeds or does not, but what a restore has to
/// report is files, so this is the list a shard's failure has to be spread over. A
/// member leaves it only when it has either landed or been named as failed, so what
/// is still in here at the end is exactly the tree's remaining holes.
struct ShardSeal {
    /// By manifest path, which is what the container names each member by.
    targets: HashMap<String, MemberTarget>,
}

impl ShardSeal {
    /// The paths still owed, in a fixed order, so two runs over the same failure
    /// read the same way.
    fn owed(&self) -> Vec<String> {
        let mut names: Vec<String> = self.targets.keys().cloned().collect();
        names.sort();
        names
    }
}

/// A file waiting for its bytes, and how much of them is still on the wire.
///
/// Pieces of one file are written into `temp`, the sibling the manifest's own
/// name is renamed over at the end, so no partial file is ever the one a user
/// sees at the path. For a container, `temp` is the shard and the renaming happens
/// one member at a time inside [`Self::shard`].
struct Slot {
    index: usize,
    /// The manifest path, or the shard's position, for the report.
    label: String,
    key: String,
    path: PathBuf,
    temp: PathBuf,
    want: String,
    size: u64,
    mode: u32,
    mtime: i64,
    seal: Seal,
    /// The members a container slot still owes. `None` for a file of its own.
    shard: Option<Arc<Mutex<ShardSeal>>>,
    /// Pieces that have not settled yet. Reaching zero is what finalises the
    /// file, and only the worker that takes it to zero runs that.
    outstanding: AtomicUsize,
    /// The first error any piece of this file met. Later ones are the same
    /// broken object seen again, and one line names it.
    failed: Mutex<Option<String>>,
    /// Whole-file refetches already spent on a body that did not hash right.
    verifies: AtomicUsize,
    started: std::time::Instant,
}

impl Slot {
    fn fail(&self, reason: &str) {
        let mut failed = self.failed.lock().unwrap_or_else(|e| e.into_inner());
        if failed.is_none() {
            *failed = Some(reason.to_string());
        }
    }
}

/// One range request for one slot: the inclusive byte span, and how many times
/// it has been asked for.
struct Piece {
    slot: usize,
    first: u64,
    last: u64,
    attempts: usize,
}

/// The byte ranges a file of `size` is fetched in. Below the split point the
/// answer is one span covering the object, which is asked for with no `Range`
/// header at all — a plain GET is what every server answers correctly and what
/// the bucket serves without the range bookkeeping.
fn ranges_for(size: u64) -> Vec<(u64, u64)> {
    if size < RANGE_MIN_SIZE {
        return vec![(0, size - 1)];
    }
    let pieces = (size.div_ceil(RANGE_TARGET) as usize).min(RANGE_MAX_PIECES);
    let step = size.div_ceil(pieces as u64);
    let mut out = Vec::with_capacity(pieces);
    let mut first = 0u64;
    while first < size {
        let last = (first + step - 1).min(size - 1);
        out.push((first, last));
        first = last + 1;
    }
    out
}

/// The sibling every piece of one file lands in, already the length the
/// manifest names so a piece can seek and write without anyone owning the end
/// of the file.
fn sized_temp(temp: &Path, size: u64) -> std::io::Result<()> {
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(temp)?;
    file.set_len(size)?;
    Ok(())
}

fn note(errors: &Mutex<Vec<String>>, message: String) {
    errors
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(message);
}

fn report_speed(slowest: &Mutex<(u64, String)>, label: &str, took_ms: u64) {
    if took_ms < SLOW_FILE_MS {
        return;
    }
    let mut top = slowest.lock().unwrap_or_else(|e| e.into_inner());
    if took_ms > top.0 {
        *top = (took_ms, label.to_string());
    }
}

/// Ask a bucket for one piece. Returns the piece to queue again when this
/// attempt should be repeated, and nothing once the piece has settled —
/// successfully or into the slot's recorded failure.
///
/// Verification is not here: it needs the whole file, so it happens in
/// [`finish_slot`], and a file that comes back wrong is fetched again from
/// nothing. A CDN can hand a stale or truncated body for a good key, which is
/// why an answer that will not change (a missing or refused object) is never
/// asked for again, and why four attempts on one file cost under two seconds.
fn run_piece(downloader: &Resolver, slot: &Slot, piece: Piece) -> Option<Piece> {
    let whole = piece.first == 0 && piece.last + 1 == slot.size;
    let range = (!whole).then_some((piece.first, piece.last));
    let expected = piece.last - piece.first + 1;
    match downloader.download_piece(&slot.key, range, expected, &slot.temp) {
        Ok(written) if written >= expected => None,
        Ok(written) => {
            // A body shorter than the range asked for leaves a hole in the middle of
            // the file that nothing would notice until the hash at the end, by which
            // time the reason is a mystery. A longer one is the bucket answering a
            // ranged request with the whole object, which is a different problem and
            // handled below as a retry of the file. So only a shortfall is spoken of
            // here, and asked for again.
            if piece.attempts + 1 >= RESTORE_ATTEMPTS {
                slot.fail(&format!("the bucket gave {written} of {expected} bytes"));
                return None;
            }
            tracing::warn!(
                "restore: {} bytes {}-{} came back {written} of {expected}; retrying",
                slot.label,
                piece.first,
                piece.last,
            );
            std::thread::sleep(retry_delay(piece.attempts + 1, false));
            Some(Piece {
                attempts: piece.attempts + 1,
                ..piece
            })
        }
        Err(err) => {
            if err.is_permanent() || piece.attempts + 1 >= RESTORE_ATTEMPTS {
                slot.fail(&err.to_string());
                return None;
            }
            let redo = if matches!(err, Error::Api { .. }) && !whole {
                // The bucket answered a range request with the whole object,
                // which cannot be written at an offset. One stream can be.
                Piece {
                    slot: piece.slot,
                    first: 0,
                    last: slot.size - 1,
                    attempts: piece.attempts + 1,
                }
            } else {
                Piece {
                    attempts: piece.attempts + 1,
                    ..piece
                }
            };
            tracing::warn!(
                "restore: {} bytes {}-{} attempt {} failed ({err}); retrying",
                slot.label,
                piece.first,
                piece.last,
                piece.attempts + 1,
            );
            std::thread::sleep(retry_delay(piece.attempts + 1, err.is_throttled()));
            Some(redo)
        }
    }
}

/// The last piece of one file has landed: check the bytes, then put the file in
/// place with its mode and its stamp. Anything that goes wrong names the file
/// and removes the half of it, because a restore that leaves a wrong file at the
/// path is worse than one that leaves the path empty.
fn finish_slot(
    slot: &Slot,
    queue: &Mutex<VecDeque<Piece>>,
    counters: &Mutex<(usize, u64, usize)>,
    errors: &Mutex<Vec<String>>,
    slowest: &Mutex<(u64, String)>,
    reporter: &Reporter,
) {
    let took = elapsed_ms(slot.started);
    if let Some(reason) = slot.failed.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = fs::remove_file(&slot.temp);
        miss(slot, errors, &reason, reporter);
        report_speed(slowest, &slot.label, took);
        return;
    }
    let wrong = if slot.want.is_empty() {
        None
    } else {
        match scan::hash_file(&slot.temp) {
            Ok(got) if got == slot.want => None,
            Ok(got) => Some(format!(
                "content is not what the manifest names (want {}, got {got})",
                slot.want
            )),
            Err(err) => Some(err.to_string()),
        }
    };
    if let Some(problem) = wrong {
        if slot.verifies.fetch_add(1, Ordering::AcqRel) < RESTORE_ATTEMPTS - 1 {
            // Start the file over from nothing, so a second stale body cannot be
            // read as part of the first one.
            tracing::warn!(
                "restore: {} came back wrong ({problem}); fetching it again",
                slot.label
            );
            let _ = fs::remove_file(&slot.temp);
            if sized_temp(&slot.temp, slot.size).is_ok() {
                slot.outstanding.store(1, Ordering::Release);
                queue
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push_back(Piece {
                        slot: slot.index,
                        first: 0,
                        last: slot.size - 1,
                        attempts: 0,
                    });
                return;
            }
        }
        let _ = fs::remove_file(&slot.temp);
        miss(slot, errors, &problem, reporter);
        report_speed(slowest, &slot.label, took);
        return;
    }
    if slot.seal == Seal::Shard {
        unpack(slot, counters, errors, slowest, reporter, took);
        return;
    }
    if let Err(source) =
        fs::set_permissions(&slot.temp, fs::Permissions::from_mode(slot.mode.max(0o600)))
    {
        let _ = fs::remove_file(&slot.temp);
        note(errors, format!("{}: set mode: {source}", slot.label));
        report_speed(slowest, &slot.label, took);
        reporter.tick();
        return;
    }
    set_mtime(&slot.temp, slot.mtime);
    if let Err(source) = fs::rename(&slot.temp, &slot.path) {
        let _ = fs::remove_file(&slot.temp);
        note(errors, format!("{}: put in place: {source}", slot.label));
        report_speed(slowest, &slot.label, took);
        reporter.tick();
        return;
    }
    count_file(counters, slot.size);
    report_speed(slowest, &slot.label, took);
    // The byte count moves only here: a file is restored when its last piece
    // landed and it was verified, so a progress bar never shows a partial as a
    // whole file.
    reporter.advance(1, slot.size);
}

/// Name what a slot that could not be completed leaves the tree short of: itself for
/// a file, and every member for a container. A shard is not something the operator
/// can go and look for; the files it was carrying are.
fn miss(slot: &Slot, errors: &Mutex<Vec<String>>, reason: &str, reporter: &Reporter) {
    match slot
        .shard
        .as_ref()
        .map(|seal| seal.lock().unwrap_or_else(|e| e.into_inner()).owed())
    {
        None => {
            note(errors, format!("{}: {reason}", slot.label));
            reporter.tick();
        }
        Some(lost) => {
            for path in lost {
                note(
                    errors,
                    format!("{path}: carried by {}: {reason}", slot.label),
                );
                reporter.tick();
            }
        }
    }
}

/// The container is whole and it is the one the manifest named: hand its files to the
/// tree.
///
/// Each member is verified, stamped, put in place and counted on its own, so the
/// progress bar still moves by files rather than by containers and a member that
/// cannot be written costs exactly itself. Nothing is left in the tree of the
/// container: it was only a way to carry many files at once.
///
/// The ledger is the point. A member leaves it when it has been spoken for — landed,
/// or refused with a reason — and whatever is still in it when the walk ends is named
/// as failed. That covers both ways a container can be a lie: one that stops part way,
/// and one that simply does not hold a file the manifest put inside it. The second is
/// the hole that cost the operator a tree, and it is silent without this sweep.
fn unpack(
    slot: &Slot,
    counters: &Mutex<(usize, u64, usize)>,
    errors: &Mutex<Vec<String>>,
    slowest: &Mutex<(u64, String)>,
    reporter: &Reporter,
    took: u64,
) {
    let Some(this) = slot.shard.clone() else {
        // A slot that says it carries a container and names none is a bug in this
        // file, not a state a restore can be asked to survive.
        miss(slot, errors, "its shard was never asked for", reporter);
        return;
    };
    let walked = pack::extract(
        &slot.temp,
        &mut |path| {
            let found = this.lock().unwrap_or_else(|e| e.into_inner());
            found.targets.get(path).map(|target| pack::Destination {
                size: target.size,
                sha256: (!target.want.is_empty()).then(|| target.want.clone()),
                temp: target.temp.clone(),
            })
        },
        &mut |path, outcome| {
            let target = this
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .targets
                .remove(path);
            match (outcome, target) {
                (Ok(_), Some(target)) => settle_member(path, target, counters, errors, reporter),
                (Err(why), _) => {
                    // Whatever was decoded was refused and removed; the name at `path`
                    // is still the one the manifest meant.
                    note(errors, format!("{path}: {why}"));
                    reporter.tick();
                }
                (Ok(_), None) => {}
            }
        },
    );
    let reason = match &walked {
        Ok(()) => "its container does not hold it".to_string(),
        Err(err) => format!("its shard ended at {err}"),
    };
    let left = this.lock().unwrap_or_else(|e| e.into_inner()).owed();
    for path in left {
        note(errors, format!("{path}: {reason}"));
        reporter.tick();
    }
    let _ = fs::remove_file(&slot.temp);
    report_speed(slowest, &slot.label, took);
}

/// One member's bytes are on disk, verified by the container's own pass: give it its
/// mode, its stamp, and its name.
fn settle_member(
    label: &str,
    target: MemberTarget,
    counters: &Mutex<(usize, u64, usize)>,
    errors: &Mutex<Vec<String>>,
    reporter: &Reporter,
) {
    if let Err(source) = fs::set_permissions(
        &target.temp,
        fs::Permissions::from_mode(target.mode.max(0o600)),
    ) {
        let _ = fs::remove_file(&target.temp);
        note(errors, format!("{label}: set mode: {source}"));
        reporter.tick();
        return;
    }
    set_mtime(&target.temp, target.mtime);
    if let Err(source) = fs::rename(&target.temp, &target.path) {
        let _ = fs::remove_file(&target.temp);
        note(errors, format!("{label}: put in place: {source}"));
        reporter.tick();
        return;
    }
    count_file(counters, target.size);
    // By member, at the member's own size: a progress bar that counted containers
    // would show a restore of nineteen thousand files finishing in thirty steps.
    reporter.advance(1, target.size);
}

fn count_file(counters: &Mutex<(usize, u64, usize)>, size: u64) {
    let mut c = counters.lock().unwrap_or_else(|e| e.into_inner());
    c.0 += 1;
    c.1 += size;
}

/// 200ms, 400ms, 800ms: long enough for a CDN hop to come back, short enough
/// that four attempts on one file cost under two seconds rather than minutes.
/// A throttle is the exception, because its own remedy is a wait longer than
/// that budget: the bucket's `429` window outlasts a fast retry, and a restore
/// that retries inside it loses the file to its own eagerness. So a refused
/// `429`/`503` comes back after 2s, 4s, 8s.
fn retry_delay(attempt: usize, throttled: bool) -> Duration {
    let base = if throttled { 2_000 } else { 200 };
    Duration::from_millis(base * 2u64.pow(attempt.saturating_sub(1) as u32).min(8))
}

fn read_link_text(path: &Path) -> Option<String> {
    fs::read_link(path)
        .ok()
        .map(|t| t.to_string_lossy().to_string())
}

/// Send the content `keys` name and register it under them. This is the repair
/// path for an object the bucket was believed to hold and did not, and it reads
/// each file out of the tree again — a file that has to be re-sent is a file this
/// run never hashed, since reuse is what made believing possible.
fn upload_objects(
    hub: &Hub,
    cfg: &Config,
    reporter: &Reporter,
    keys: &[String],
    owners: &HashMap<String, Pending>,
) -> Result<u64> {
    let mut jobs = Vec::with_capacity(keys.len());
    for key in keys {
        let want = owners.get(key.as_str()).ok_or_else(|| {
            Error::Manifest(key.clone(), "no entry of this manifest names it".into())
        })?;
        jobs.push(Job {
            key: key.clone(),
            source: Source::File(want.path.clone()),
            sha256: want.sha256.clone(),
            size: want.size,
        });
    }
    let done = xet::ingest(cfg, jobs, reporter.clone())?;
    let now = epoch_ms();
    let items: Vec<(String, String, i64, Option<&str>)> = done
        .iter()
        .map(|d| (d.key.clone(), d.xet_hash.clone(), now, None))
        .collect();
    hub.add_files(&items)?;
    Ok(done.iter().map(|d| d.size).sum())
}

/// Ingest then register small JSON objects, in the order given.
fn write_json(hub: &Hub, cfg: &Config, items: &[(&str, Vec<u8>, Option<&str>)]) -> Result<()> {
    let mut jobs = Vec::new();
    let mut order: Vec<(&str, Option<&str>)> = Vec::new();
    for (key, bytes, content_type) in items {
        let size = bytes.len() as u64;
        jobs.push(Job {
            key: key.to_string(),
            source: Source::Bytes(bytes.clone()),
            sha256: None,
            size,
        });
        order.push((*key, *content_type));
    }
    let done = xet::ingest(cfg, jobs, Reporter::default())?;
    let hashes: HashMap<String, String> = done.into_iter().map(|d| (d.key, d.xet_hash)).collect();
    let now = epoch_ms();
    let mut registered = Vec::new();
    for (key, content_type) in &order {
        let hash = hashes
            .get(*key)
            .ok_or_else(|| Error::Xet(format!("{key}: the bucket did not acknowledge it")))?;
        registered.push((key.to_string(), hash.clone(), now, *content_type));
    }
    hub.add_files(&registered)
}

fn count(entries: &[Entry]) -> Counts {
    let mut counts = Counts::default();
    for entry in entries {
        match entry.kind {
            Kind::File => {
                counts.files += 1;
                counts.bytes += entry.size;
            }
            Kind::Dir => counts.dirs += 1,
            Kind::Symlink => counts.symlinks += 1,
        }
    }
    counts
}

fn depth(path: &str) -> usize {
    path.matches('/').count()
}

fn set_mtime(path: &Path, mtime: i64) {
    if mtime <= 0 {
        return;
    }
    let when = UNIX_EPOCH + Duration::from_secs(mtime as u64);
    if let Ok(file) = fs::OpenOptions::new().write(true).open(path) {
        let _ = file.set_times(fs::FileTimes::new().set_modified(when));
    }
}

fn epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

fn elapsed_ms(started: std::time::Instant) -> u64 {
    started.elapsed().as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ShardRef;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn sha(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(bytes))
    }

    enum Route {
        Bytes(Vec<u8>),
        /// An error the first time, the bytes after: a CDN hop that dropped.
        Flaky(Vec<u8>),
        Gone,
        /// Bytes that answer a `Range` header the way a CDN does: the slice, and
        /// a 206 saying so.
        Ranged(Vec<u8>),
        /// Bytes that answer a `Range` header with the whole object, because the
        /// server never implemented it.
        Rangeless(Vec<u8>),
        /// Fewer bytes than were asked for, and a `Content-Length` that agrees with
        /// the body rather than the range: a CDN handing back a truncated object.
        ShortBody(Vec<u8>, usize),
    }

    /// A bucket that answers from a fixed table on a loopback port, counting
    /// how many times each path was asked for. The count is the point: a
    /// restore's cost is the number of requests it makes per file.
    fn fake_bucket(routes: HashMap<String, Route>) -> (String, Arc<Mutex<HashMap<String, usize>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a free port");
        let addr = listener.local_addr().expect("the address it took");
        let routes = Arc::new(Mutex::new(routes));
        let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::new(Mutex::new(HashMap::new()));
        let (server_routes, server_hits) = (routes.clone(), hits.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let (routes, hits) = (server_routes.clone(), server_hits.clone());
                std::thread::spawn(move || {
                    let mut buffer = [0u8; 4096];
                    let read = match stream.read(&mut buffer) {
                        Ok(0) | Err(_) => return,
                        Ok(read) => read,
                    };
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let Some(path) = request.split_whitespace().nth(1) else {
                        return;
                    };
                    let path = path.to_string();
                    let range: Option<(usize, usize)> = request.lines().find_map(|line| {
                        let spec = line.trim().strip_prefix("Range: bytes=")?;
                        let (first, last) = spec.split_once('-')?;
                        Some((first.parse().ok()?, last.parse().ok()?))
                    });
                    let found = {
                        let routes = routes.lock().unwrap_or_else(|e| e.into_inner());
                        routes.get(&path).map(|route| match route {
                            Route::Bytes(b) => (200, "OK", b.clone()),
                            Route::Flaky(b) => (500, "Internal Server Error", b.clone()),
                            Route::Gone => (404, "Not Found", Vec::new()),
                            Route::Ranged(b) => match range {
                                Some((first, last)) => (
                                    206,
                                    "Partial Content",
                                    b[first..last.min(b.len() - 1) + 1].to_vec(),
                                ),
                                None => (200, "OK", b.clone()),
                            },
                            Route::Rangeless(b) => (200, "OK", b.clone()),
                            Route::ShortBody(b, take) => {
                                (200, "OK", b[..(*take).min(b.len())].to_vec())
                            }
                        })
                    };
                    let Some((status, reason, body)) = found else {
                        let _ = stream.write_all(
                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        );
                        return;
                    };
                    let seen = {
                        let mut hits = hits.lock().unwrap_or_else(|e| e.into_inner());
                        *hits.entry(path.clone()).or_insert(0) += 1;
                        hits[&path]
                    };
                    let (status, reason, body) = if status == 500 && seen > 1 {
                        // A `Flaky` route drops the first attempt and answers the
                        // second, which is how a retry is proved without a network
                        // to break.
                        let routes = routes.lock().unwrap_or_else(|e| e.into_inner());
                        match routes.get(&path) {
                            Some(Route::Flaky(b)) => (200, "OK", b.clone()),
                            _ => (status, reason, body),
                        }
                    } else {
                        (status, reason, body)
                    };
                    let head = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(&body);
                });
            }
        });
        (format!("http://{addr}"), hits)
    }

    struct Tree {
        cfg: Config,
        dir: std::path::PathBuf,
        urls: HashMap<String, String>,
        hits: Arc<Mutex<HashMap<String, usize>>>,
    }

    /// A manifest of four files, of which the fake bucket serves three: one
    /// only after a retry, and one that is not there at all.
    fn tree(name: &str) -> Tree {
        tree_with(name, false)
    }

    /// `escape` adds an entry naming a path outside the root, which the run
    /// must refuse on its own and not for the whole manifest.
    fn tree_with(name: &str, escape: bool) -> Tree {
        let dir = std::env::temp_dir().join(format!("aix-restore-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a restore root");
        let prefix = "aixodia/root";
        let files = [
            ("a.txt", &b"first"[..]),
            ("sub/b.txt", &b"second"[..]),
            ("flaky.txt", &b"retried"[..]),
            ("gone.txt", &b"never served"[..]),
        ];
        let mut entries = Vec::new();
        let mut routes: HashMap<String, Route> = HashMap::new();
        let mut urls = HashMap::new();
        for (path, bytes) in files {
            let sha = sha(bytes);
            entries.push(Entry {
                path: path.to_string(),
                kind: Kind::File,
                size: bytes.len() as u64,
                mode: 0o644,
                mtime: 0,
                target: None,
                sha256: Some(sha.clone()),
                ..Default::default()
            });
            let url = format!(
                "/buckets/u/b/resolve/{prefix}/objects/{}/{}",
                &sha[..2],
                sha
            );
            routes.insert(
                url.clone(),
                if path.starts_with("gone") {
                    Route::Gone
                } else if path.starts_with("flaky") {
                    Route::Flaky(bytes.to_vec())
                } else {
                    Route::Bytes(bytes.to_vec())
                },
            );
            urls.insert(path.to_string(), url);
        }
        if escape {
            entries.push(Entry {
                path: "../evil.txt".into(),
                kind: Kind::File,
                size: 1,
                mode: 0o644,
                mtime: 0,
                target: None,
                sha256: None,
                ..Default::default()
            });
        }
        let manifest = Manifest {
            version: 1,
            name: "test".into(),
            created_at: 0,
            host: "test".into(),
            root: dir.display().to_string(),
            counts: Counts::default(),
            uploaded_bytes: 0,
            skipped: vec![],
            entries,
            shards: vec![],
        };
        routes.insert(
            format!("/buckets/u/b/resolve/{prefix}/latest.json"),
            Route::Bytes(b"{\"name\":\"test\"}".to_vec()),
        );
        routes.insert(
            format!("/buckets/u/b/resolve/{prefix}/manifests/test.json"),
            Route::Bytes(serde_json::to_vec(&manifest).expect("a manifest")),
        );
        let (endpoint, hits) = fake_bucket(routes);
        Tree {
            cfg: Config {
                token: "t".into(),
                bucket: "u/b".into(),
                endpoint,
                root: dir.clone(),
                prefix: prefix.into(),
                excludes: vec![],
                ..Default::default()
            },
            dir,
            urls,
            hits,
        }
    }

    fn hit(hits: &Arc<Mutex<HashMap<String, usize>>>, url: &str) -> usize {
        *hits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(url)
            .unwrap_or(&0)
    }

    /// The whole point of the failure rules: a file the bucket does not have
    /// must not cost the other three their place on disk, must not be asked for
    /// four times, and must be named afterwards.
    #[test]
    fn a_missing_object_ends_the_run_with_the_rest_of_the_tree_written() {
        let tree = tree("partial");
        let reporter = Reporter::new("restore");
        let report = restore(&tree.cfg, &tree.dir, &reporter).expect("the run goes on");

        assert_eq!(report.files, 3, "every object the hub served landed");
        assert_eq!(report.failed.len(), 1, "one file is named as missing");
        assert!(
            report.failed[0].contains("gone.txt") && report.failed[0].contains("404"),
            "the failure says which file and why: {}",
            report.failed[0]
        );
        assert_eq!(fs::read(tree.dir.join("a.txt")).unwrap(), b"first");
        assert_eq!(fs::read(tree.dir.join("sub/b.txt")).unwrap(), b"second");
        assert_eq!(
            fs::read(tree.dir.join("flaky.txt")).unwrap(),
            b"retried",
            "a dropped stream is asked for again"
        );
        assert_eq!(
            hit(&tree.hits, &tree.urls["gone.txt"]),
            1,
            "an object that is not there is not chased"
        );
        assert_eq!(hit(&tree.hits, &tree.urls["flaky.txt"]), 2);
        assert_eq!(reporter.snapshot().phase, Phase::Failed);
    }

    /// A second restore onto the tree it just wrote is the ordinary case — the
    /// machine came back and only some of it was wrong — so it must ask for
    /// nothing it already holds.
    #[test]
    fn a_repeat_restore_asks_for_nothing_it_already_holds() {
        let tree = tree("repeat");
        let reporter = Reporter::new("restore");
        restore(&tree.cfg, &tree.dir, &reporter).expect("the first run");
        let before: HashMap<String, usize> = tree
            .urls
            .iter()
            .map(|(path, url)| (path.clone(), hit(&tree.hits, url)))
            .collect();

        let report = restore(&tree.cfg, &tree.dir, &reporter).expect("the second run");

        assert_eq!(report.unchanged, 3, "the tree is already current");
        assert_eq!(report.bytes, 0, "and it cost no bytes to learn that");
        for (path, url) in &tree.urls {
            if path == "gone.txt" {
                continue;
            }
            assert_eq!(
                hit(&tree.hits, url),
                before[path],
                "{path} was not fetched again"
            );
        }
    }

    /// A manifest naming a path outside the root is refused entry by entry, not
    /// run by run: the other files are still what the machine needs.
    #[test]
    fn one_refused_path_does_not_stop_the_rest() {
        let tree = tree_with("escape", true);
        let report = restore(&tree.cfg, &tree.dir, &Reporter::new("restore")).expect("the run");
        assert_eq!(report.files, 3, "the writable entries were still written");
        assert!(
            report.failed.iter().any(|f| f.contains("evil.txt")),
            "and the refused one is named: {:?}",
            report.failed
        );
    }

    #[test]
    fn a_retry_waits_a_short_and_bounded_time() {
        assert_eq!(retry_delay(1, false), Duration::from_millis(200));
        assert_eq!(retry_delay(2, false), Duration::from_millis(400));
        assert_eq!(retry_delay(4, false), Duration::from_millis(1600));
        assert_eq!(
            retry_delay(40, false),
            Duration::from_millis(1600),
            "it stops growing"
        );
    }

    /// A `429` retried inside its own window is a lost file, and at sixty-four
    /// workers that is how 3,779 of them went: the whole budget for one file
    /// ran out in under two seconds while the hub was only pausing.
    #[test]
    fn a_throttle_is_answered_with_a_wait_that_outlasts_it() {
        assert_eq!(retry_delay(1, true), Duration::from_millis(2_000));
        assert_eq!(retry_delay(2, true), Duration::from_millis(4_000));
        assert_eq!(retry_delay(3, true), Duration::from_millis(8_000));
        assert!(Error::Hub {
            operation: "download".into(),
            key: "k".into(),
            status: 429,
            body: "too many".into(),
        }
        .is_throttled());
        assert!(
            !Error::Hub {
                operation: "download".into(),
                key: "k".into(),
                status: 404,
                body: "gone".into(),
            }
            .is_throttled(),
            "an object the bucket does not have is never worth waiting for"
        );
    }

    /// A 404 will be a 404 when asked again, and a dead stream might not be.
    #[test]
    fn a_refusal_is_not_asked_for_again_but_a_drop_is() {
        let gone = Error::Hub {
            operation: "download".into(),
            key: "k".into(),
            status: 404,
            body: String::new(),
        };
        assert!(gone.is_permanent());
        let rate_limited = Error::Hub {
            operation: "download".into(),
            key: "k".into(),
            status: 429,
            body: String::new(),
        };
        assert!(
            !rate_limited.is_permanent(),
            "a 429 is a moment, not a fact"
        );
        assert!(!Error::Verify {
            path: "p".into(),
            want: "a".into(),
            got: "b".into(),
        }
        .is_permanent());
    }

    /// A manifest of one file big enough to be cut into range requests, and the
    /// bytes the fake bucket holds for it. `honours_range` is the difference
    /// between a CDN and a server that never implemented `Range`.
    fn big_tree(name: &str, honours_range: bool) -> (Tree, Vec<u8>) {
        let dir = std::env::temp_dir().join(format!("aix-range-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a restore root");
        let prefix = "aixodia/root";
        let mut body = Vec::new();
        while (body.len() as u64) < RANGE_MIN_SIZE + 1024 * 1024 {
            body.extend((0..65_536usize).map(|i| (i % 251) as u8));
        }
        let sha = sha(&body);
        let entries = vec![Entry {
            path: "big.bin".into(),
            kind: Kind::File,
            size: body.len() as u64,
            mode: 0o644,
            mtime: 0,
            target: None,
            sha256: Some(sha.clone()),
            ..Default::default()
        }];
        let url = format!(
            "/buckets/u/b/resolve/{prefix}/objects/{}/{}",
            &sha[..2],
            sha
        );
        let mut routes: HashMap<String, Route> = HashMap::new();
        routes.insert(
            url.clone(),
            if honours_range {
                Route::Ranged(body.clone())
            } else {
                Route::Rangeless(body.clone())
            },
        );
        routes.insert(
            format!("/buckets/u/b/resolve/{prefix}/latest.json"),
            Route::Bytes(b"{\"name\":\"test\"}".to_vec()),
        );
        let manifest = Manifest {
            version: 1,
            name: "test".into(),
            created_at: 0,
            host: "test".into(),
            root: dir.display().to_string(),
            counts: Counts::default(),
            uploaded_bytes: 0,
            skipped: vec![],
            entries,
            shards: vec![],
        };
        routes.insert(
            format!("/buckets/u/b/resolve/{prefix}/manifests/test.json"),
            Route::Bytes(serde_json::to_vec(&manifest).expect("a manifest")),
        );
        let (endpoint, hits) = fake_bucket(routes);
        let mut urls = HashMap::new();
        urls.insert("big.bin".to_string(), url);
        let tree = Tree {
            cfg: Config {
                token: "t".into(),
                bucket: "u/b".into(),
                endpoint,
                root: dir.clone(),
                prefix: prefix.into(),
                excludes: vec![],
                ..Default::default()
            },
            dir,
            urls,
            hits,
        };
        (tree, body)
    }

    /// A twenty-minute restore of a tree full of toolchain binaries is not slow
    /// because of its thirty thousand small files: it is the largest file,
    /// carried by one connection at a fifth of a megabyte a second, that decides
    /// when the run ends. So a big object arrives as several ranges at once.
    #[test]
    fn a_large_file_is_carried_by_several_ranges() {
        let (tree, body) = big_tree("split", true);
        let reporter = Reporter::new("restore");
        let report = restore(&tree.cfg, &tree.dir, &reporter).expect("the run finishes");

        assert!(
            report.failed.is_empty(),
            "one file, served: {:?}",
            report.failed
        );
        assert_eq!(report.files, 1);
        assert_eq!(report.bytes, body.len() as u64);
        assert_eq!(fs::read(tree.dir.join("big.bin")).expect("the file"), body);
        assert!(
            hit(&tree.hits, &tree.urls["big.bin"]) >= 3,
            "a nine-megabyte object is fetched in several ranges, not one stream"
        );
    }

    /// A server that answers a range request with the whole object cannot have
    /// its body written at an offset. Refusing it is only right if the file still
    /// comes across, which is the point of the fallback in `run_piece`.
    #[test]
    fn a_bucket_that_ignores_ranges_still_delivers_the_file() {
        let (tree, body) = big_tree("ignorant", false);
        let reporter = Reporter::new("restore");
        let report = restore(&tree.cfg, &tree.dir, &reporter).expect("the run finishes");

        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(fs::read(tree.dir.join("big.bin")).expect("the file"), body);
    }

    /// Where a container is fetched from: the same address as an object's, one
    /// directory over.
    fn shard_url(prefix: &str, sha: &str) -> String {
        format!("/buckets/u/b/resolve/{prefix}/shards/{}/{}", &sha[..2], sha)
    }

    /// A bucket holding containers packed from `bodies`, and a v2 manifest naming
    /// them. `shards` says how many members go in each container, in the order they
    /// are listed; `missing` says which containers the bucket does not have.
    ///
    /// The directories and links are added by the fixture rather than passed in, so
    /// every packed restore test also proves the two kinds that stay outside the
    /// containers.
    fn packed_tree_with(
        name: &str,
        bodies: &[(String, Vec<u8>)],
        shards: &[usize],
        missing: &[usize],
        short: Option<usize>,
        alter: impl FnOnce(&mut Manifest),
    ) -> Tree {
        let source =
            std::env::temp_dir().join(format!("aix-packed-src-{name}-{}", std::process::id()));
        let dir = std::env::temp_dir().join(format!("aix-packed-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&source);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&source).expect("a packing root");
        fs::create_dir_all(&dir).expect("a restore root");
        let prefix = "aixodia/root";

        for (path, body) in bodies {
            let full = source.join(path);
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).expect("a parent directory");
            }
            fs::write(&full, body).expect("a file to pack");
        }
        let mut entries: Vec<Entry> = Vec::new();
        let mut routes: HashMap<String, Route> = HashMap::new();
        let mut urls: HashMap<String, String> = HashMap::new();
        let mut refs: Vec<ShardRef> = Vec::new();
        let mut taken = 0usize;
        for (position, count) in shards.iter().enumerate() {
            let slice = &bodies[taken..taken + count];
            taken += count;
            let members: Vec<pack::Member> = slice
                .iter()
                .map(|(path, body)| pack::Member {
                    path: path.clone(),
                    size: body.len() as u64,
                    sha256: sha(body),
                })
                .collect();
            for (index, member) in members.iter().enumerate() {
                entries.push(Entry {
                    path: member.path.clone(),
                    kind: Kind::File,
                    size: member.size,
                    mode: if member.path.ends_with(".sh") {
                        0o755
                    } else {
                        0o644
                    },
                    mtime: 1_700_000_000 + index as i64,
                    target: None,
                    sha256: Some(member.sha256.clone()),
                    shard: Some(position),
                });
            }
            let out = source.join(format!("container-{position}.shard"));
            let built = pack::build(&members, &source, &out).expect("a container builds");
            assert!(
                built.drifted.is_empty() && built.lost.is_empty(),
                "the fixture's tree does not move while it is packed"
            );
            let (sha256, size) = (built.sha256, built.size);
            let bytes = fs::read(&out).expect("the container it wrote");
            let _ = fs::remove_file(&out);
            refs.push(ShardRef {
                sha256: sha256.clone(),
                fingerprint: pack::fingerprint(&members),
                size,
                members: members.len(),
                raw: members.iter().map(|member| member.size).sum(),
            });
            let url = shard_url(prefix, &sha256);
            routes.insert(
                url.clone(),
                if missing.contains(&position) {
                    Route::Gone
                } else if Some(position) == short {
                    // The container arrives cut in half, which is the same hole a
                    // truncated object is: fewer bytes than the piece asked for.
                    let take = bytes.len() / 2;
                    Route::ShortBody(bytes, take)
                } else {
                    Route::Bytes(bytes)
                },
            );
            urls.insert(format!(":{position}"), url);
        }
        entries.push(Entry {
            path: "sub".into(),
            kind: Kind::Dir,
            size: 0,
            mode: 0o755,
            mtime: 0,
            target: None,
            sha256: None,
            shard: None,
        });
        entries.push(Entry {
            path: "link.txt".into(),
            kind: Kind::Symlink,
            size: 0,
            mode: 0o777,
            mtime: 0,
            target: Some("a.txt".into()),
            sha256: None,
            shard: None,
        });

        let mut manifest = Manifest {
            version: 2,
            name: "test".into(),
            created_at: 0,
            host: "test".into(),
            root: dir.display().to_string(),
            counts: Counts::default(),
            uploaded_bytes: 0,
            skipped: vec![],
            entries,
            shards: refs,
        };
        alter(&mut manifest);
        // The tree the containers were built from has no further use: what is being
        // restored from now on is the bucket.
        let _ = fs::remove_dir_all(&source);
        routes.insert(
            format!("/buckets/u/b/resolve/{prefix}/latest.json"),
            Route::Bytes(b"{\"name\":\"test\"}".to_vec()),
        );
        routes.insert(
            format!("/buckets/u/b/resolve/{prefix}/manifests/test.json"),
            Route::Bytes(serde_json::to_vec(&manifest).expect("a manifest")),
        );
        let (endpoint, hits) = fake_bucket(routes);
        Tree {
            cfg: Config {
                token: "t".into(),
                bucket: "u/b".into(),
                endpoint,
                root: dir.clone(),
                prefix: prefix.into(),
                excludes: vec![],
                ..Default::default()
            },
            dir,
            urls,
            hits,
        }
    }

    /// A tree of one container, all of it served.
    fn packed_tree(name: &str, bodies: &[(String, Vec<u8>)], shards: &[usize]) -> Tree {
        packed_tree_with(name, bodies, shards, &[], None, |_| {})
    }

    /// The claim packing rests on: nineteen thousand files come back from a handful
    /// of requests, and the tree afterwards is what the manifest named — the bytes,
    /// the modes, the stamps, and the two kinds a container cannot hold.
    #[test]
    fn a_packed_restore_writes_the_tree_from_its_containers() {
        let bodies = vec![
            ("a.txt".to_string(), b"first".to_vec()),
            ("sub/b.txt".to_string(), b"second".to_vec()),
            (
                "run.sh".to_string(),
                "echo a very long line\n".repeat(200).into_bytes(),
            ),
        ];
        let tree = packed_tree("whole", &bodies, &[3]);
        let reporter = Reporter::new("restore");
        let report = restore(&tree.cfg, &tree.dir, &reporter).expect("the run finishes");

        assert!(report.failed.is_empty(), "{:?}", report.failed);
        assert_eq!(report.files, 3, "the members of one container");
        assert_eq!(
            report.bytes,
            bodies.iter().map(|(_, b)| b.len() as u64).sum::<u64>()
        );
        for (path, body) in &bodies {
            assert_eq!(
                fs::read(tree.dir.join(path)).expect("the member"),
                *body,
                "{path}"
            );
        }
        assert_eq!(
            fs::metadata(tree.dir.join("run.sh"))
                .expect("the script")
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "a packed member keeps the mode its entry named"
        );
        assert_eq!(
            fs::metadata(tree.dir.join("a.txt"))
                .expect("the file")
                .mtime(),
            1_700_000_000
        );
        assert!(
            tree.dir.join("sub").is_dir(),
            "a directory stays outside the container and still arrives"
        );
        assert_eq!(
            fs::read_link(tree.dir.join("link.txt")).expect("the link"),
            PathBuf::from("a.txt")
        );
        assert_eq!(
            hit(&tree.hits, &tree.urls[":0"]),
            1,
            "three files, one request: that is the whole argument for packing"
        );
        let _ = fs::remove_dir_all(&tree.dir);
    }

    /// One bad container is a bad hour for the files it carried and nothing else:
    /// the tree's other members still land, and every lost path is named rather
    /// than left for the operator to discover.
    #[test]
    fn one_lost_container_costs_its_members_and_no_more() {
        let bodies = vec![
            ("keep1.txt".to_string(), b"one".to_vec()),
            ("keep2.txt".to_string(), b"two".to_vec()),
            ("lost1.txt".to_string(), b"three".to_vec()),
            ("lost2.txt".to_string(), b"four".to_vec()),
        ];
        let tree = packed_tree_with("blast", &bodies, &[2, 2], &[1], None, |_| {});
        let reporter = Reporter::new("restore");
        let report = restore(&tree.cfg, &tree.dir, &reporter).expect("the run finishes");

        assert_eq!(
            fs::read(tree.dir.join("keep1.txt")).expect("the member"),
            b"one"
        );
        assert_eq!(
            fs::read(tree.dir.join("keep2.txt")).expect("the member"),
            b"two"
        );
        assert!(
            !tree.dir.join("lost1.txt").exists(),
            "a member of the container that is not there"
        );
        assert_eq!(report.files, 2);
        assert_eq!(
            report.failed.len(),
            2,
            "the lost container's members, and nothing else: {:?}",
            report.failed
        );
        let named = report.failed.join("\n");
        for path in ["lost1.txt", "lost2.txt"] {
            assert!(named.contains(path), "{path} is not named in {named}");
        }
        for path in ["keep1.txt", "keep2.txt"] {
            assert!(
                !named.contains(path),
                "{path} was written; it is not a failure"
            );
        }
        let _ = fs::remove_dir_all(&tree.dir);
    }

    /// A container that arrives cut short must not be assembled anyway. Its bytes
    /// are a few hundred of the ones the manifest declared, so the length check on
    /// the piece is what turns "half a shard" into a named set of files instead of a
    /// hash error nobody could read.
    #[test]
    fn a_truncated_container_is_refused_and_its_members_are_named() {
        let bodies = vec![
            ("a.txt".to_string(), b"aaaa".repeat(4096)),
            ("b.txt".to_string(), b"bbbb".repeat(4096)),
        ];
        let tree = packed_tree_with("short", &bodies, &[2], &[], Some(0), |_| {});
        let reporter = Reporter::new("restore");
        let report = restore(&tree.cfg, &tree.dir, &reporter).expect("the run finishes");

        assert_eq!(report.files, 0, "nothing of a half container is a file");
        assert_eq!(
            report.failed.len(),
            2,
            "both members are named: {:?}",
            report.failed
        );
        assert!(
            !tree.dir.join("a.txt").exists() && !tree.dir.join("b.txt").exists(),
            "a partial container leaves no partial files"
        );
        let _ = fs::remove_dir_all(&tree.dir);
    }

    /// A file number the shard list cannot answer is that file's problem. The
    /// container it should have come from still delivers whoever is in it.
    #[test]
    fn a_shard_number_past_the_end_names_its_own_entry() {
        let bodies = vec![
            ("a.txt".to_string(), b"first".to_vec()),
            ("b.txt".to_string(), b"second".to_vec()),
        ];
        let tree = packed_tree_with("bogus", &bodies, &[2], &[], None, |manifest| {
            manifest.entries[1].shard = Some(999);
        });
        let reporter = Reporter::new("restore");
        let report = restore(&tree.cfg, &tree.dir, &reporter).expect("the run finishes");

        assert_eq!(
            fs::read(tree.dir.join("a.txt")).expect("the member"),
            b"first"
        );
        assert_eq!(report.files, 1);
        assert_eq!(
            report.failed,
            vec!["b.txt: shard 999 is not in this manifest".to_string()],
            "the bogus index costs one entry, and says which"
        );
        let _ = fs::remove_dir_all(&tree.dir);
    }

    /// A manifest that puts a file inside a container the container does not hold is
    /// the exact shape of the hole that cost the operator a tree: the shard arrives,
    /// verifies, and the tree is still short. So the walk ends by naming whatever it
    /// never spoke for, and a completed run means a whole tree.
    #[test]
    fn a_container_that_holds_fewer_files_than_named_says_which() {
        let bodies = vec![
            ("a.txt".to_string(), b"first".to_vec()),
            ("b.txt".to_string(), b"second".to_vec()),
        ];
        let tree = packed_tree_with("ghost", &bodies, &[2], &[], None, |manifest| {
            manifest.entries.push(Entry {
                path: "ghost.txt".into(),
                kind: Kind::File,
                size: 6,
                mode: 0o644,
                mtime: 0,
                target: None,
                sha256: Some(sha(b"nobody")),
                shard: Some(0),
            });
        });
        let reporter = Reporter::new("restore");
        let report = restore(&tree.cfg, &tree.dir, &reporter).expect("the run finishes");

        assert_eq!(
            fs::read(tree.dir.join("a.txt")).expect("the member"),
            b"first"
        );
        assert_eq!(
            fs::read(tree.dir.join("b.txt")).expect("the member"),
            b"second"
        );
        assert_eq!(report.files, 2, "the two the container really held");
        assert_eq!(
            report.failed,
            vec!["ghost.txt: its container does not hold it".to_string()],
            "the missing one is named, not counted as a success"
        );
        let _ = fs::remove_dir_all(&tree.dir);
    }

    /// Nothing in the v2 reader makes a v1 backup unreadable: a manifest with no
    /// shards has no container to fetch, so every file is still its own object and
    /// the operator's current backup restores exactly as it did before packing.
    #[test]
    fn a_manifest_without_shards_still_arrives_one_object_at_a_time() {
        let tree = tree("v1");
        let reporter = Reporter::new("restore");
        let report = restore(&tree.cfg, &tree.dir, &reporter).expect("the run finishes");

        assert_eq!(report.files, 3, "{:?}", report.failed);
        for path in ["a.txt", "sub/b.txt", "flaky.txt"] {
            let url = &tree.urls[path];
            assert!(
                hit(&tree.hits, url) >= 1,
                "{path} is fetched as its own object"
            );
        }
        let _ = fs::remove_dir_all(&tree.dir);
    }

    /// A packed manifest names two kinds of key, and the pre-publish audit and the
    /// keep-list are only correct if both are there. A file that went into a
    /// container must not also be asked for as an object — that is the difference
    /// between a backup that audits thirty keys and one that audits none.
    #[test]
    fn a_packed_manifest_names_its_containers_and_no_ghost_objects() {
        let packed = sha(b"first");
        let direct = sha(b"second");
        let manifest = Manifest {
            version: 2,
            name: "test".into(),
            created_at: 0,
            host: "test".into(),
            root: String::new(),
            counts: Counts::default(),
            uploaded_bytes: 0,
            skipped: vec![],
            entries: vec![
                Entry {
                    path: "a.txt".into(),
                    kind: Kind::File,
                    size: 5,
                    mode: 0o644,
                    mtime: 0,
                    target: None,
                    sha256: Some(sha(b"first")),
                    shard: Some(0),
                },
                Entry {
                    path: "keep.txt".into(),
                    kind: Kind::File,
                    size: 6,
                    mode: 0o644,
                    mtime: 0,
                    target: None,
                    sha256: Some(sha(b"second")),
                    shard: None,
                },
                Entry {
                    path: "link.txt".into(),
                    kind: Kind::Symlink,
                    size: 0,
                    mode: 0o777,
                    mtime: 0,
                    target: Some("keep.txt".into()),
                    sha256: None,
                    shard: None,
                },
            ],
            shards: vec![ShardRef {
                sha256: "ab".to_string() + &"c".repeat(62),
                fingerprint: "f".repeat(64),
                size: 10,
                members: 1,
                raw: 5,
            }],
        };
        let keys = manifest.object_keys();
        let absolute = manifest.absolute("aixodia/root");
        assert_eq!(keys.len(), 2, "{keys:?}");
        assert!(
            absolute.contains(&manifest.shards[0].object_key("aixodia/root")),
            "the container is on the list, or the audit and the pruning delete it"
        );
        assert!(
            absolute.contains(&format!("aixodia/root/objects/{}/{}", &direct[..2], direct)),
            "a file outside a container keeps its own object"
        );
        assert!(
            !keys.iter().any(|key| key.ends_with(&packed)),
            "a packed file names no object of its own: {keys:?}"
        );
    }

    /// A root whose files are too big to share a container, and the entries a scan
    /// of it would produce. Sizes are chosen so the grouping lands in two shards:
    /// seventeen megabytes cannot share a container with anything else of its size.
    fn packable_root(name: &str, sizes: &[usize]) -> (Config, Vec<Entry>, PathBuf) {
        let dir = std::env::temp_dir().join(format!("aix-build-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a backup root");
        let mut entries = Vec::new();
        for (index, size) in sizes.iter().enumerate() {
            let path = format!("f{index}.bin");
            // One byte of difference per file, and the rest zeros: the content is
            // distinct, which is what a fingerprint is about, and cheap to build.
            let mut body = vec![0u8; *size];
            body[0] = index as u8 + 1;
            fs::write(dir.join(&path), &body).expect("a file to pack");
            entries.push(Entry {
                path,
                kind: Kind::File,
                size: *size as u64,
                mode: 0o644,
                mtime: 0,
                target: None,
                sha256: Some(sha(&body)),
                shard: None,
            });
        }
        let scratch =
            std::env::temp_dir().join(format!("aix-build-scratch-{name}-{}", std::process::id()));
        fs::create_dir_all(&scratch).expect("a scratch directory");
        (
            Config {
                token: "t".into(),
                bucket: "u/b".into(),
                root: dir.clone(),
                prefix: "aixodia/root".into(),
                ..Default::default()
            },
            entries,
            scratch,
        )
    }

    /// What the second run of a warm backup is allowed to cost: nothing. The
    /// member list a shard was built from is its identity, so an unchanged tree
    /// names the containers the bucket already holds and reads no file at all.
    #[test]
    fn an_unchanged_tree_uploads_no_container_bytes() {
        let (cfg, mut entries, scratch) = packable_root("warm", &[17 << 20, 17 << 20, 1 << 20]);
        let reporter = Reporter::new("backup");
        let built = pack_shards(
            &cfg,
            &mut entries,
            None,
            &HashSet::new(),
            &scratch,
            &reporter,
            &mut Vec::new(),
        )
        .expect("the first run packs");
        assert_eq!(
            built.len(),
            2,
            "seventeen megabytes will not share a container"
        );
        assert!(
            built.iter().all(|shard| shard.temp.is_some()),
            "nothing stands in the bucket yet"
        );

        let published = Manifest {
            version: 2,
            name: "n".into(),
            created_at: 0,
            host: "h".into(),
            root: cfg.root.display().to_string(),
            counts: Counts::default(),
            uploaded_bytes: 0,
            skipped: vec![],
            entries: entries.clone(),
            shards: built.iter().map(ShardBuild::reference).collect(),
        };
        let known: HashSet<String> = published.absolute(&cfg.prefix).into_iter().collect();

        let mut again = entries.clone();
        let reused = pack_shards(
            &cfg,
            &mut again,
            Some(&published),
            &known,
            &scratch,
            &reporter,
            &mut Vec::new(),
        )
        .expect("the second run reuses");
        assert_eq!(reused.len(), built.len());
        for (before, after) in built.iter().zip(&reused) {
            assert_eq!(
                before.sha256, after.sha256,
                "the same members, the same container"
            );
            assert!(
                after.temp.is_none(),
                "shard {} was built again",
                after.fingerprint
            );
        }
        let going: u64 = reused
            .iter()
            .filter(|s| s.temp.is_some())
            .map(|s| s.size)
            .sum();
        assert_eq!(going, 0, "a warm backup puts no container on the wire");
        // The numbering the published manifest uses is the one the entries carry.
        for entry in again.iter().filter(|e| e.shard.is_some()) {
            let at = entry.shard.expect("just checked");
            assert!(
                published.shards[at].members > 0,
                "{} names a container that holds nothing",
                entry.path
            );
        }
        let _ = fs::remove_dir_all(&cfg.root);
        let _ = fs::remove_dir_all(&scratch);
    }

    /// The honest cost of packing, and the reason it is survivable: one changed
    /// file rebuilds the container it lives in and leaves every other container
    /// alone. A tree of nineteen thousand files where one binary changed is one
    /// rebuild, not nineteen thousand.
    #[test]
    fn one_changed_member_rebuilds_one_container() {
        let (cfg, mut entries, scratch) = packable_root("shift", &[17 << 20, 17 << 20, 1 << 20]);
        let reporter = Reporter::new("backup");
        let built = pack_shards(
            &cfg,
            &mut entries,
            None,
            &HashSet::new(),
            &scratch,
            &reporter,
            &mut Vec::new(),
        )
        .expect("the first run packs");
        let published = Manifest {
            version: 2,
            name: "n".into(),
            created_at: 0,
            host: "h".into(),
            root: cfg.root.display().to_string(),
            counts: Counts::default(),
            uploaded_bytes: 0,
            skipped: vec![],
            entries: entries.clone(),
            shards: built.iter().map(ShardBuild::reference).collect(),
        };
        let known: HashSet<String> = published.absolute(&cfg.prefix).into_iter().collect();

        // The first file's bytes change, and with them the one container it is in.
        let mut moved = entries.clone();
        let body = vec![7u8; 17 << 20];
        moved[0].sha256 = Some(sha(&body));
        fs::write(cfg.root.join("f0.bin"), &body).expect("a changed file");
        let rebuilt = pack_shards(
            &cfg,
            &mut moved,
            Some(&published),
            &known,
            &scratch,
            &reporter,
            &mut Vec::new(),
        )
        .expect("the changed tree is packed");

        assert_eq!(rebuilt.len(), 2, "the same grouping");
        assert!(rebuilt[0].temp.is_some(), "the container that held it");
        assert!(
            rebuilt[1].temp.is_none(),
            "its neighbours are seventeen and one megabytes of unchanged bytes"
        );
        assert_eq!(rebuilt[1].sha256, built[1].sha256);
        assert_ne!(rebuilt[0].sha256, built[0].sha256);
        assert_eq!(rebuilt[0].members, 1);
        let _ = fs::remove_dir_all(&cfg.root);
        let _ = fs::remove_dir_all(&scratch);
    }

    /// A reuse claim is only good if the bucket agrees: a fingerprint that matches
    /// a shard the bucket no longer holds must be built again, not published as a
    /// hole in the tree.
    #[test]
    fn a_reused_container_is_only_reused_while_the_bucket_holds_it() {
        let (cfg, mut entries, scratch) = packable_root("gap", &[17 << 20, 17 << 20, 1 << 20]);
        let reporter = Reporter::new("backup");
        let built = pack_shards(
            &cfg,
            &mut entries,
            None,
            &HashSet::new(),
            &scratch,
            &reporter,
            &mut Vec::new(),
        )
        .expect("the first run packs");
        let published = Manifest {
            version: 2,
            name: "n".into(),
            created_at: 0,
            host: "h".into(),
            root: cfg.root.display().to_string(),
            counts: Counts::default(),
            uploaded_bytes: 0,
            skipped: vec![],
            entries: entries.clone(),
            shards: built.iter().map(ShardBuild::reference).collect(),
        };
        // Every key but the containers': the bucket lost them, as it lost the
        // objects that cost the operator a tree.
        let known: HashSet<String> = published
            .entries
            .iter()
            .filter_map(|entry| entry.object_key(&cfg.prefix))
            .collect();

        let mut again = entries.clone();
        let rebuilt = pack_shards(
            &cfg,
            &mut again,
            Some(&published),
            &known,
            &scratch,
            &reporter,
            &mut Vec::new(),
        )
        .expect("the run goes on");
        assert!(
            rebuilt.iter().all(|shard| shard.temp.is_some()),
            "a fingerprint match is not enough: the bytes have to be there"
        );
        let _ = fs::remove_dir_all(&cfg.root);
        let _ = fs::remove_dir_all(&scratch);
    }

    /// A backup walks nineteen thousand files and then spends minutes packing them,
    /// and a live tree loses files in between. A file that went is the scan's news,
    /// not the backup's: the container is built from what stayed, the gone file is
    /// named, and the run finishes.
    #[test]
    fn a_file_gone_before_its_container_is_built_is_named_and_not_fatal() {
        let (cfg, mut entries, scratch) = packable_root("gone", &[1 << 20, 1 << 20, 1 << 20]);
        let reporter = Reporter::new("backup");
        fs::remove_file(cfg.root.join("f1.bin")).expect("the file goes");

        let mut lost = Vec::new();
        let built = pack_shards(
            &cfg,
            &mut entries,
            None,
            &HashSet::new(),
            &scratch,
            &reporter,
            &mut lost,
        )
        .expect("a gone file does not cost the backup");
        assert_eq!(lost, vec!["f1.bin".to_string()], "it is named");

        let held: Vec<usize> = built.iter().flat_map(|shard| shard.group.clone()).collect();
        assert_eq!(held.len(), 2, "and two files are in the containers");
        for &at in &held {
            assert_ne!(entries[at].path, "f1.bin", "the gone file is in no list");
        }
        assert!(
            entries[1].shard.is_none(),
            "and it carries no claim on a container"
        );
        let raw: u64 = built.iter().map(|shard| shard.raw).sum();
        assert_eq!(raw, 2 << 20, "the container sizes say two files");
        let survivors: Vec<pack::Member> = held
            .iter()
            .map(|&at| pack::Member {
                path: entries[at].path.clone(),
                size: entries[at].size,
                sha256: entries[at].sha256.clone().unwrap_or_default(),
            })
            .collect();
        assert_eq!(
            built[0].fingerprint,
            pack::fingerprint(&survivors),
            "and the identity a reuse is decided by describes what is inside"
        );
        let _ = fs::remove_dir_all(&cfg.root);
        let _ = fs::remove_dir_all(&scratch);
    }

    /// The live run's own failure, and the reason a backup of a working tree was
    /// naming files it could not restore: a file that grew between the scan and the
    /// pack. The container holds the bytes it read, so the entry has to name those,
    /// and a restore of it asks for nothing that is not inside.
    #[test]
    fn a_file_that_grew_while_it_was_packed_is_named_as_packed() {
        let (cfg, mut entries, scratch) = packable_root("drift", &[1 << 20, 1 << 20]);
        let reporter = Reporter::new("backup");
        fs::write(cfg.root.join("f0.bin"), vec![7u8; (1 << 20) + 4096]).expect("the file grows");

        let mut lost = Vec::new();
        let built = pack_shards(
            &cfg,
            &mut entries,
            None,
            &HashSet::new(),
            &scratch,
            &reporter,
            &mut lost,
        )
        .expect("a moving file does not cost the backup");
        assert!(lost.is_empty(), "and nothing was lost for it");

        let grown = fs::metadata(cfg.root.join("f0.bin"))
            .expect("the file")
            .len();
        assert_eq!(
            entries[0].size, grown,
            "the entry names the length that went in"
        );
        let packed = scan::hash_file(&cfg.root.join("f0.bin")).expect("a hash");
        assert_eq!(
            entries[0].sha256.as_deref(),
            Some(packed.as_str()),
            "and the digest of the bytes that were read"
        );
        assert_eq!(
            entries[1].size,
            1 << 20,
            "the file that stayed is untouched"
        );
        assert_eq!(built[0].members, 2, "both are in the container");
        let raw: u64 = built.iter().map(|shard| shard.raw).sum();
        assert_eq!(
            raw,
            entries.iter().map(|entry| entry.size).sum::<u64>(),
            "and the container's own total adds up to the manifest's"
        );

        // The identity a later reuse is decided by has to be the one this container
        // answers to, or the next warm backup reuses a shard that describes other bytes.
        let as_packed: Vec<pack::Member> = entries
            .iter()
            .map(|entry| pack::Member {
                path: entry.path.clone(),
                size: entry.size,
                sha256: entry.sha256.clone().unwrap_or_default(),
            })
            .collect();
        assert_eq!(
            built[0].fingerprint,
            pack::fingerprint(&as_packed),
            "the fingerprint describes what was packed"
        );
        let _ = fs::remove_dir_all(&cfg.root);
        let _ = fs::remove_dir_all(&scratch);
    }

    /// A file that stops short of the length its range asked for is not a file: it
    /// is a hole that the hash at the end would call a mystery. The run asks again,
    /// and when the bucket keeps giving the same short body, the path is named and
    /// nothing half-written is left standing in the tree.
    #[test]
    fn a_body_that_stops_short_is_not_written_off_as_a_file() {
        let dir = std::env::temp_dir().join(format!("aix-short-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("a restore root");
        let prefix = "aixodia/root";
        let body: Vec<u8> = (0..8192usize).map(|i| (i % 251) as u8).collect();
        let sha = sha(&body);
        let entries = vec![Entry {
            path: "half.bin".into(),
            kind: Kind::File,
            size: body.len() as u64,
            mode: 0o644,
            mtime: 0,
            target: None,
            sha256: Some(sha.clone()),
            shard: None,
        }];
        let url = format!(
            "/buckets/u/b/resolve/{prefix}/objects/{}/{}",
            &sha[..2],
            sha
        );
        let manifest = Manifest {
            version: 1,
            name: "test".into(),
            created_at: 0,
            host: "test".into(),
            root: dir.display().to_string(),
            counts: Counts::default(),
            uploaded_bytes: 0,
            skipped: vec![],
            entries,
            shards: vec![],
        };
        let mut routes: HashMap<String, Route> = HashMap::new();
        routes.insert(url.clone(), Route::ShortBody(body.clone(), 4096));
        routes.insert(
            format!("/buckets/u/b/resolve/{prefix}/latest.json"),
            Route::Bytes(b"{\"name\":\"test\"}".to_vec()),
        );
        routes.insert(
            format!("/buckets/u/b/resolve/{prefix}/manifests/test.json"),
            Route::Bytes(serde_json::to_vec(&manifest).expect("a manifest")),
        );
        let (endpoint, hits) = fake_bucket(routes);
        let cfg = Config {
            token: "t".into(),
            bucket: "u/b".into(),
            endpoint,
            root: dir.clone(),
            prefix: prefix.into(),
            excludes: vec![],
            ..Default::default()
        };
        let report = restore(&cfg, &dir, &Reporter::new("restore")).expect("the run finishes");

        assert_eq!(report.files, 0);
        assert_eq!(
            report.failed.len(),
            1,
            "one path, named: {:?}",
            report.failed
        );
        assert!(
            report.failed[0].contains("gave 4096 of 8192 bytes"),
            "the reason says what came short: {:?}",
            report.failed
        );
        assert!(
            !dir.join("half.bin").exists(),
            "a short body never becomes a file"
        );
        let left: Vec<String> = fs::read_dir(&dir)
            .expect("the root it wrote")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains("aixodia-part"))
            .collect();
        assert!(left.is_empty(), "no partial is left in the tree: {left:?}");
        assert_eq!(
            hit(&hits, &url),
            RESTORE_ATTEMPTS,
            "a short body is asked for again, up to the attempts a file gets"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
