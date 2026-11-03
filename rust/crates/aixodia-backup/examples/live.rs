//! Live check of the whole path against a real bucket, in a throwaway prefix.
//!
//! `HF_TOKEN=… cargo run -p aixodia-backup --example live -- <scratch-root> <prefix> [purge]`
//!
//! It backs `scratch-root` up, restores it somewhere else, compares the two
//! trees, then deletes what it made.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use aixodia_backup::{scan, Config, Hub, Reporter};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().expect("usage: live <root> <prefix> [purge]"));
    let prefix = args
        .next()
        .unwrap_or_else(|| "aixodia-selftest/live".to_string());
    let purge = args.next().as_deref() == Some("purge");
    let prefix_for_cfg = prefix.clone();
    let token = std::env::var("HF_TOKEN").expect("HF_TOKEN");

    let cfg = Config {
        token,
        bucket: "kyomu53n/Bucket".into(),
        prefix: prefix_for_cfg,
        root: root.clone(),
        ..Default::default()
    };

    // Taken before the walk, so any file newer than this was written after the
    // manifest read it and is allowed to disagree with the restored copy.
    let wrote_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let reporter = Reporter::new("live-backup");
    let report = aixodia_backup::backup(&cfg, &reporter)?;
    println!(
        "backup {}: {} files, {} dirs, {} links, {} bytes, uploaded {}, reused {} files / {} objects, {} skipped, replaced {} manifest(s), {} ms",
        report.name,
        report.files,
        report.dirs,
        report.symlinks,
        report.bytes,
        report.uploaded_bytes,
        report.files_reused,
        report.objects_reused,
        report.skipped.len(),
        report.deleted_manifests,
        report.duration_ms
    );
    for line in &report.skipped {
        println!("  skipped: {line}");
    }

    let again = aixodia_backup::backup(&cfg, &Reporter::new("live-backup-2"))?;
    println!(
        "backup {} (incremental): uploaded {}, reused {} files / {} objects, {} ms",
        again.name,
        again.uploaded_bytes,
        again.files_reused,
        again.objects_reused,
        again.duration_ms
    );

    let into = std::env::temp_dir().join(format!("aix-restore-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&into);
    let started = std::time::Instant::now();
    let restored = aixodia_backup::restore(&cfg, &into, &Reporter::new("live-restore"))?;
    println!(
        "restore {} into {}: {} files, {} dirs, {} links, {} bytes, {} unchanged, {} ms",
        restored.name,
        restored.root,
        restored.files,
        restored.dirs,
        restored.symlinks,
        restored.bytes,
        restored.unchanged,
        started.elapsed().as_millis()
    );

    let before = fingerprint(&root)?;
    let after = fingerprint(&into)?;
    if before == after {
        println!("MATCH: {} entries identical", before.len());
    } else {
        // The source is a live machine: the engine writes its own log while this
        // runs, so a file the manifest read at second N legitimately differs
        // from the same file at second N+6. What must never happen is a restored
        // file that does not match what the manifest recorded, or an entry the
        // manifest named and the restore did not write.
        let mut churned = 0usize;
        let mut wrong = 0usize;
        for (path, hash) in &before {
            match after.get(path) {
                Some(got) if got == hash => {}
                Some(got) => {
                    if touched_after(&root.join(path), wrote_at) {
                        churned += 1;
                        println!("  CHURN {path}: source moved on after the manifest");
                    } else {
                        wrong += 1;
                        println!("  DIFF {path}: wanted {hash}, got {got}");
                    }
                }
                None => {
                    wrong += 1;
                    println!("  MISSING {path}");
                }
            }
        }
        for path in after.keys() {
            if !before.contains_key(path) {
                println!("  EXTRA {path}");
                wrong += 1;
            }
        }
        if wrong > 0 {
            return Err(
                format!("the restored tree does not match the source ({wrong} entries)").into(),
            );
        }
        println!(
            "MATCH: {} entries restored, {} files the source changed after the manifest",
            after.len(),
            churned
        );
    }

    // Re-restore: everything should be recognised as already correct.
    let again = std::time::Instant::now();
    let twice = aixodia_backup::restore(&cfg, &into, &Reporter::new("live-restore-2"))?;
    println!(
        "re-restore: {} unchanged of {} files, {} ms",
        twice.unchanged,
        twice.files,
        again.elapsed().as_millis()
    );

    let _ = std::fs::remove_dir_all(&into);
    if purge {
        let hub = Hub::new(&cfg)?;
        let mut keys: Vec<String> = Vec::new();
        for prefix in [
            format!("{prefix}/manifests"),
            format!("{prefix}/objects"),
            prefix.clone(),
        ] {
            keys.extend(hub.list_files(&prefix)?.into_iter().map(|f| f.path));
        }
        println!("purging {} objects", keys.len());
        hub.delete_keys(&keys)?;
    }
    Ok(())
}

/// `true` when `path` was written after `since`, i.e. the source moved on after
/// the manifest was read rather than the restore being wrong.
fn touched_after(path: &Path, since: i64) -> bool {
    std::fs::symlink_metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64 > since)
        .unwrap_or(false)
}

/// Path → kind and content hash, so a comparison says what actually differs.
///
/// It has to fingerprint with the same exclusions the backup used, or the
/// source side of the comparison counts `target` and the caches the manifest
/// never named and reports a tree full of phantom differences.
fn fingerprint(root: &Path) -> Result<BTreeMap<String, String>, Box<dyn std::error::Error>> {
    let mut out = BTreeMap::new();
    let excludes: Vec<String> = scan::DEFAULT_EXCLUDES
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut skipped = Vec::new();
    for item in scan::scan(root, &excludes, &[], &mut skipped)? {
        let value = match item.kind {
            aixodia_backup::Kind::File => {
                format!("file {}", scan::hash_file(&root.join(&item.path))?)
            }
            aixodia_backup::Kind::Symlink => format!("link {:?}", item.target),
            aixodia_backup::Kind::Dir => format!("dir {:o}", item.mode),
        };
        out.insert(item.path, value);
    }
    Ok(out)
}
