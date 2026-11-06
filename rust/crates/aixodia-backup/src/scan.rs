//! Walking the tree and turning it into manifest entries.

use std::collections::{HashMap, HashSet};
use std::fs::{self, Metadata};
use std::io::{BufReader, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{Error, Kind, Manifest, Result};

/// No paths are excluded unless the operator explicitly selects an exclusion.
pub const DEFAULT_EXCLUDES: &[&str] = &[];

/// One filesystem entry as found on disk, before it is decided whether its
/// content has to be read.
#[derive(Clone, Debug)]
pub struct Scanned {
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub mode: u32,
    pub mtime: i64,
    pub target: Option<String>,
    pub dev: u64,
}

fn mode_of(meta: &Metadata) -> u32 {
    meta.permissions().mode() & 0o7777
}

/// `true` when `rel` matches an exclusion. Component patterns prune a whole
/// subtree; glob patterns are matched against the relative path.
pub fn is_excluded(rel: &str, excludes: &[String]) -> bool {
    for pattern in excludes {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            continue;
        }
        // A trailing slash is the UI's explicit directory pick. Match that
        // exact path and its descendants, not every same-named folder elsewhere.
        if pattern.ends_with('/') {
            let directory = pattern.trim_end_matches('/');
            if !directory.is_empty() && (rel == directory || rel.starts_with(pattern)) {
                return true;
            }
            continue;
        }
        if pattern.contains('/') {
            if glob_match(pattern, rel) {
                return true;
            }
            continue;
        }
        if pattern.contains('*') || pattern.contains('?') {
            if rel
                .rsplit('/')
                .next()
                .map(|name| glob_match(pattern, name))
                .unwrap_or(false)
            {
                return true;
            }
            continue;
        }
        if rel
            .split('/')
            .any(|component| component == pattern || glob_match(pattern, component))
        {
            return true;
        }
    }
    false
}

/// A rule is a selected path, or an excluded path prefixed with !.
/// The most specific matching rule wins, so selecting root and excluding
/// !root/.cache backs up the whole tree except that subtree.
fn rule_matches_or_reaches(pattern: &str, rel: &str) -> bool {
    if rel == pattern || rel.starts_with(&format!("{pattern}/")) {
        return true;
    }
    if pattern.contains('*') || pattern.contains('?') {
        prefix_matches(pattern, rel) || leads_to_match(rel, pattern)
    } else {
        false
    }
}

fn selected_by_rules(rel: &str, includes: &[String]) -> bool {
    includes.iter()
        .filter_map(|raw| {
            let raw = raw.trim();
            let (excluded, path) = raw.strip_prefix('!')
                .map(|path| (true, path))
                .unwrap_or((false, raw));
            if path.is_empty() || !rule_matches_or_reaches(path, rel) {
                return None;
            }
            Some((path.len(), !excluded))
        })
        .max_by_key(|(length, _)| *length)
        .map(|(_, selected)| selected)
        .unwrap_or(false)
}

/// true when rel is selected or is an ancestor needed to reach a selected path.
/// An empty list selects nothing.
pub fn is_included(rel: &str, includes: &[String]) -> bool {
    if selected_by_rules(rel, includes) {
        return true;
    }
    includes.iter().any(|raw| {
        let raw = raw.trim();
        if raw.starts_with('!') || raw.is_empty() {
            return false;
        }
        let pattern = raw;
        if pattern.starts_with(&format!("{rel}/")) {
            return selected_by_rules(pattern, includes);
        }
        (pattern.contains('*') || pattern.contains('?'))
            && leads_to_match(rel, pattern)
    })
}

/// `true` when `pattern` names `path` itself or one of its ancestors, so
/// everything under a name the pattern matches is inside the scope too. Without
/// this a wildcard pick would keep the entries it spells and lose their
/// contents, which a literal pick does not do.
fn prefix_matches(pattern: &str, path: &str) -> bool {
    if glob_match(pattern, path) {
        return true;
    }
    let mut cut = 0usize;
    while let Some(at) = path[cut..].find('/') {
        cut += at + 1;
        if glob_match(pattern, &path[..cut - 1]) {
            return true;
        }
    }
    false
}

/// `true` when `above` is a component-wise prefix of `below`, which still has
/// components left — i.e. a wildcard entry could match something under `above`.
fn leads_to_match(above: &str, below: &str) -> bool {
    let upper: Vec<&str> = above.split('/').collect();
    let lower: Vec<&str> = below.split('/').collect();
    upper.len() < lower.len() && upper.iter().zip(&lower).all(|(a, b)| glob_match(b, a))
}

/// Iterative glob: `*` spans everything including `/`, `?` is one character.
fn glob_match(pattern: &str, text: &str) -> bool {
    let (pat, txt) = (pattern.as_bytes(), text.as_bytes());
    let (mut p, mut t) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut star_t = 0usize;
    while t < txt.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == txt[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = Some(p);
            star_t = t;
            p += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            star_t += 1;
            t = star_t;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

/// Walks `root` and returns entries matching the selected paths and any
/// explicit exclusions, sorted by path. Directories are traversed across
/// filesystem boundaries. Symlinks are recorded as symlinks, not traversed.
///
/// Only metadata is read. That is what makes a preview cheap: the walk a preview
/// does is the walk a backup would do anyway, minus the hashing.
pub fn scan(
    root: &Path,
    excludes: &[String],
    includes: &[String],
    skipped: &mut Vec<String>,
) -> Result<Vec<Scanned>> {
    fs::metadata(root).map_err(|source| Error::Io {
        path: root.display().to_string(),
        source,
    })?;

    let mut out: Vec<Scanned> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let rel_dir = relative(root, &dir);
        let read = match fs::read_dir(&dir) {
            Ok(read) => read,
            Err(source) => {
                skipped.push(format!("{rel_dir}/ (unreadable: {source})"));
                continue;
            }
        };
        for item in read {
            let item = match item {
                Ok(item) => item,
                Err(source) => {
                    skipped.push(format!("{} (unreadable entry: {source})", dir.display()));
                    continue;
                }
            };
            let name = item.file_name().to_string_lossy().to_string();
            let child = item.path();
            let rel = if rel_dir.is_empty() {
                name.clone()
            } else {
                format!("{rel_dir}/{name}")
            };
            if is_excluded(&rel, excludes) || !is_included(&rel, includes) {
                continue;
            }
            let meta = match fs::symlink_metadata(&child) {
                Ok(meta) => meta,
                Err(source) => {
                    skipped.push(format!("{rel} (unreadable: {source})"));
                    continue;
                }
            };
            let file_type = meta.file_type();
            let mode = mode_of(&meta);
            let mtime = meta.mtime();

            if file_type.is_symlink() {
                let target = fs::read_link(&child)
                    .map(|t| t.to_string_lossy().to_string())
                    .unwrap_or_default();
                out.push(Scanned {
                    path: rel,
                    kind: Kind::Symlink,
                    size: 0,
                    mode,
                    mtime,
                    target: Some(target),
                    dev: meta.dev(),
                });
                continue;
            }
            if file_type.is_dir() {
                out.push(Scanned {
                    path: rel,
                    kind: Kind::Dir,
                    size: 0,
                    mode,
                    mtime,
                    target: None,
                    dev: meta.dev(),
                });
                stack.push(child);
                continue;
            }
            if !file_type.is_file() {
                skipped.push(format!("{rel} (not a regular file)"));
                continue;
            }
            out.push(Scanned {
                path: rel,
                kind: Kind::File,
                size: meta.len(),
                mode,
                mtime,
                target: None,
                dev: meta.dev(),
            });
        }
    }

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn relative(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default()
}

pub fn hash_file(path: &Path) -> Result<String> {
    let file = fs::File::open(path).map_err(|source| Error::Io {
        path: path.display().to_string(),
        source,
    })?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = reader.read(&mut buffer).map_err(|source| Error::Io {
            path: path.display().to_string(),
            source,
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// What a backup run still has to do: the entries whose bytes are not in the
/// bucket yet, and the entries that could reuse a previous manifest's hash.
#[derive(Debug, Default)]
pub struct Plan {
    pub entries: Vec<crate::Entry>,
    /// Absolute paths whose content must be uploaded, parallel to `uploads`.
    pub uploads: Vec<(PathBuf, String)>,
    /// Files whose size and mtime still match, so their hash was reused unread.
    pub reused: usize,
    pub bytes_reused: u64,
    pub bytes_upload: u64,
}

/// Decides which files need reading at all. A file whose size and mtime match
/// the previous manifest keeps that manifest's sha256 and is not opened; a file
/// the bucket already holds under the same content key is not uploaded.
///
/// The files that must be read are hashed on several threads, because a first
/// full backup of a large tree is bound by hashing, not by the wire.
///
/// `known_keys` is the set of object keys already present in the bucket.
/// Files that cannot be read are recorded in `unreadable` and left out of the
/// manifest; a tree being written while it is scanned must not fail the run.
pub fn plan(
    root: &Path,
    scanned: Vec<Scanned>,
    previous: Option<&Manifest>,
    known_keys: &HashSet<String>,
    prefix: &str,
    unreadable: &mut Vec<String>,
) -> Result<Plan> {
    let previous_by_path: HashMap<&str, &crate::Entry> = previous
        .map(|m| m.entries.iter().map(|e| (e.path.as_str(), e)).collect())
        .unwrap_or_default();

    // Reused hashes need no read; everything else is hashed in parallel.
    let mut reused_sha: HashMap<usize, String> = HashMap::new();
    let mut to_hash: Vec<usize> = Vec::new();
    for (i, item) in scanned.iter().enumerate() {
        if item.kind != Kind::File {
            continue;
        }
        if let Some(old) = previous_by_path.get(item.path.as_str()) {
            if old.kind == Kind::File && old.size == item.size && old.mtime == item.mtime {
                if let Some(known) = old.sha256.as_ref() {
                    reused_sha.insert(i, known.clone());
                    continue;
                }
            }
        }
        to_hash.push(i);
    }

    let mut hashed: HashMap<usize, String> = HashMap::new();
    let mut read_errors: Vec<(usize, String)> = Vec::new();
    if !to_hash.is_empty() {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .min(to_hash.len())
            .min(16);
        let scanned_ref = &scanned;
        let root_ref = root;
        let results: Vec<Vec<(usize, std::result::Result<String, String>)>> =
            std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(threads);
                for chunk in to_hash.chunks(to_hash.len().div_ceil(threads)) {
                    let chunk = chunk.to_vec();
                    handles.push(scope.spawn(move || {
                        chunk
                            .iter()
                            .map(|&i| {
                                let path = root_ref.join(&scanned_ref[i].path);
                                let out = hash_file(&path).map_err(|err| err.to_string());
                                (i, out)
                            })
                            .collect()
                    }));
                }
                handles
                    .into_iter()
                    .map(|h| h.join().unwrap_or_default())
                    .collect()
            });
        for batch in results {
            for (i, outcome) in batch {
                match outcome {
                    Ok(sha) => {
                        hashed.insert(i, sha);
                    }
                    Err(err) => read_errors.push((i, err)),
                }
            }
        }
    }

    let mut plan = Plan::default();
    for (i, item) in scanned.into_iter().enumerate() {
        match item.kind {
            Kind::Dir | Kind::Symlink => {
                plan.entries.push(crate::Entry {
                    path: item.path,
                    kind: item.kind,
                    size: 0,
                    mode: item.mode,
                    mtime: item.mtime,
                    target: item.target,
                    sha256: None,
                    // Only regular files are ever packed: a directory or a link is
                    // a few bytes of metadata, and the pre-pass makes it before any
                    // shard is opened.
                    shard: None,
                });
                continue;
            }
            Kind::File => {}
        }
        let sha = match reused_sha.remove(&i) {
            Some(sha) => {
                plan.reused += 1;
                plan.bytes_reused += item.size;
                sha
            }
            None => match hashed.remove(&i) {
                Some(sha) => sha,
                None => {
                    let reason = read_errors
                        .iter()
                        .find(|(at, _)| *at == i)
                        .map(|(_, why)| why.clone())
                        .unwrap_or_else(|| "not hashed".to_string());
                    unreadable.push(format!("{} (cannot read: {reason})", item.path));
                    continue;
                }
            },
        };

        let key = format!("{prefix}/objects/{}/{}", &sha[..2], sha);
        if !known_keys.contains(&key) {
            plan.uploads.push((root.join(&item.path), key.clone()));
            plan.bytes_upload += item.size;
        }
        plan.entries.push(crate::Entry {
            path: item.path,
            kind: Kind::File,
            size: item.size,
            mode: item.mode,
            mtime: item.mtime,
            target: None,
            sha256: Some(sha),
            // `backup` decides which shard a file lands in, once it has every
            // file's hash and none of its bytes still open.
            shard: None,
        });
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_exclusion_patterns_prune_any_depth() {
        let excludes: Vec<String> = ["target", "__pycache__", ".cache", "*.pyc", "*.aixodia-part"]
            .iter().map(|s| s.to_string()).collect();
        assert!(is_excluded("aixodia-engine/rust/target/debug/x", &excludes));
        assert!(is_excluded(".cache/pip/wheel.whl", &excludes));
        assert!(is_excluded("a/b/__pycache__/c.pyc", &excludes));
        assert!(!is_excluded("aixodia-engine/rust/Cargo.toml", &excludes));
        assert!(!is_excluded(".aixodia/aixodia.log", &excludes));
        assert!(is_excluded(".ssh/config.aixodia-part", &excludes));
        // A restored machine has to be able to build again, so the toolchain is
        // in the scope and not in this list.
        assert!(!is_excluded(".cargo/bin/cargo", &excludes));
        assert!(!is_excluded(
            ".rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/rustc",
            &excludes
        ));
    }

    /// A source file is not a core dump. `core.*` read like one and hid every
    /// `core.rs` in the tree, and a file that never enters a manifest cannot come
    /// back on a restore — so the defaults lost them on every machine built from
    /// a backup, cargo registries included.
    #[test]
    fn a_source_file_named_core_is_backed_up() {
        let excludes: Vec<String> = Vec::new();
        for path in [
            "src/core.rs",
            "app/core.py",
            "include/core.h",
            ".cargo/registry/src/index.crates.io-1/tokio-1.54.1/src/runtime/task/core.rs",
        ] {
            assert!(
                !is_excluded(path, &excludes),
                "{path} is source, not a dump"
            );
        }
    }

    #[test]
    fn path_pattern_and_glob() {
        let excludes = vec!["a/b/*".to_string(), "*.tmp".to_string()];
        assert!(is_excluded("a/b/c", &excludes));
        assert!(is_excluded("keep/x.tmp", &excludes));
        assert!(!is_excluded("a/x/c", &excludes));
        assert!(!is_excluded("keep/x.txt", &excludes));
    }

    #[test]
    fn explicit_directory_exclusion_only_matches_that_path() {
        let excludes = vec!["projects/cache/".to_string(), "cache/".to_string()];
        assert!(is_excluded("cache", &excludes));
        assert!(is_excluded("cache/pip/wheel.whl", &excludes));
        assert!(is_excluded("projects/cache", &excludes));
        assert!(is_excluded("projects/cache/item", &excludes));
        assert!(!is_excluded("other/cache/item", &excludes));
        assert!(!is_excluded("projects/cache-old/item", &excludes));
    }

    #[test]
    fn empty_includes_selects_nothing() {
        // No selection means no entries are selected.
        let includes: Vec<String> = Vec::new();
        assert!(!is_included("anything/at/all", &includes));
        assert!(!is_included("target/x", &includes));
    }

    #[test]
    fn selected_parent_excludes_only_the_unchecked_subtree() {
        let rules = vec!["root".to_string(), "!root/.cache".to_string()];
        assert!(is_included("root", &rules));
        assert!(is_included("root/.config", &rules));
        assert!(!is_included("root/.cache", &rules));
        assert!(!is_included("root/.cache/pip", &rules));
        assert!(!is_included("root-sibling/file", &rules));
    }

    #[test]
    fn a_nested_positive_rule_can_reinclude_a_subtree() {
        let rules = vec![
            "root".to_string(),
            "!root/.cache".to_string(),
            "root/.cache/needed".to_string(),
        ];
        assert!(is_included("root/.cache/needed", &rules));
        assert!(is_included("root/.cache/needed/file", &rules));
        assert!(!is_included("root/.cache/other", &rules));
    }

    #[test]
    fn selected_files_and_independent_paths_are_scanned_without_siblings() {
        let dir = std::env::temp_dir().join(format!("aix-file-picks-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("first")).unwrap();
        fs::create_dir_all(dir.join("second/bin")).unwrap();
        fs::write(dir.join("first/.profile"), b"profile").unwrap();
        fs::write(dir.join("first/other.txt"), b"other").unwrap();
        fs::write(dir.join("second/bin/tool"), b"tool").unwrap();
        fs::write(dir.join("second/bin/unselected"), b"other").unwrap();

        let includes = vec!["first/.profile".to_string(), "second/bin/tool".to_string()];
        let mut skipped = Vec::new();
        let found = scan(&dir, &[], &includes, &mut skipped).unwrap();
        let paths: Vec<&str> = found.iter().map(|entry| entry.path.as_str()).collect();

        assert!(paths.contains(&"first"));
        assert!(paths.contains(&"first/.profile"));
        assert!(paths.contains(&"second"));
        assert!(paths.contains(&"second/bin"));
        assert!(paths.contains(&"second/bin/tool"));
        assert!(!paths.contains(&"first/other.txt"));
        assert!(!paths.contains(&"second/bin/unselected"));
        assert!(skipped.is_empty(), "{skipped:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn included_folder_pulls_its_subtree() {
        let includes = vec![".codex".to_string()];
        assert!(is_included(".codex", &includes));
        assert!(is_included(".codex/config.json", &includes));
        assert!(is_included(".codex/sessions/a/b.jsonl", &includes));
        assert!(!is_included("projects/.codex/x", &includes));
        assert!(!is_included("projects", &includes));
    }

    #[test]
    fn include_ancestor_is_walked_for_a_nested_pick() {
        // Picking projects/aixodia/app means the walk has to get through the two
        // folders above it; pruning them would back up nothing at all.
        let includes = vec!["projects/aixodia/app".to_string()];
        assert!(is_included("projects", &includes));
        assert!(is_included("projects/aixodia", &includes));
        assert!(is_included("projects/aixodia/app/src/main.rs", &includes));
        assert!(!is_included("projects/other", &includes));
        assert!(!is_included("home", &includes));
    }

    #[test]
    fn wildcard_include_reaches_through_its_prefix() {
        let includes = vec!["projects/*/app".to_string()];
        assert!(is_included("projects", &includes));
        assert!(is_included("projects/aixodia", &includes));
        assert!(is_included("projects/aixodia/app/x", &includes));
        assert!(!is_included("projects/aixodia/other/x", &includes));
        assert!(!is_included("other", &includes));
    }

    #[test]
    fn excludes_outrank_includes() {
        // Picking a parent folder cannot pull the caches back in, and cannot
        // reach outside itself: the exclusions still apply inside it, and a
        // folder nobody picked stays out.
        let dir = std::env::temp_dir().join(format!("aix-include-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("keep/target")).unwrap();
        fs::write(dir.join("keep/a.txt"), b"a").unwrap();
        fs::write(dir.join("keep/target/junk"), b"j").unwrap();
        fs::create_dir_all(dir.join("drop")).unwrap();
        fs::write(dir.join("drop/b.txt"), b"b").unwrap();

        let excludes = vec!["target".to_string()];
        let includes = vec!["keep".to_string()];
        let mut skipped = Vec::new();
        let found = scan(&dir, &excludes, &includes, &mut skipped).unwrap();
        let paths: Vec<&str> = found.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"keep"), "the pick itself is in the tree");
        assert!(
            paths.contains(&"keep/a.txt"),
            "its contents follow: {paths:?}"
        );
        assert!(
            !paths.contains(&"keep/target/junk"),
            "an exclusion inside a pick still excludes"
        );
        assert!(!paths.contains(&"drop/b.txt"), "nobody picked drop");
        assert!(skipped.is_empty(), "{skipped:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_matches_simple_forms() {
        assert!(glob_match("*.pyc", "x.pyc"));
        assert!(glob_match("a*b", "a--b"));
        assert!(!glob_match("a*b", "axxc"));
        assert!(glob_match("a/b*", "a/bzzz"));
        assert!(glob_match("*", "anything/at/all"));
    }

    #[test]
    fn scan_records_dirs_files_and_links_without_following() {
        let dir = std::env::temp_dir().join(format!("aix-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub/deep")).unwrap();
        fs::write(dir.join("sub/a.txt"), b"hello").unwrap();
        fs::create_dir(dir.join("target")).unwrap();
        fs::write(dir.join("target/junk"), b"x").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc", dir.join("sub/escape")).unwrap();

        let excludes = vec!["target".to_string()];
        let includes = vec!["sub".to_string(), "target".to_string()];
        let mut skipped = Vec::new();
        let found = scan(&dir, &excludes, &includes, &mut skipped).unwrap();
        let paths: Vec<&str> = found.iter().map(|e| e.path.as_str()).collect();
        assert!(paths.contains(&"sub"), "got {paths:?}");
        assert!(paths.contains(&"sub/a.txt"));
        assert!(!paths.contains(&"target/junk"), "target must be pruned");
        let link = found.iter().find(|e| e.path == "sub/escape").unwrap();
        assert_eq!(link.kind, Kind::Symlink);
        assert_eq!(link.target.as_deref(), Some("/etc"));
        assert!(skipped.is_empty(), "{skipped:?}");
        assert!(
            paths.windows(2).all(|w| w[0] <= w[1]),
            "paths must be sorted"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_reuses_an_unchanged_files_hash_and_skips_known_keys() {
        let dir = std::env::temp_dir().join(format!("aix-plan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("same.txt"), b"same").unwrap();
        fs::write(dir.join("new.txt"), b"new").unwrap();
        let meta = fs::metadata(dir.join("same.txt")).unwrap();

        let scanned = vec![
            Scanned {
                path: "same.txt".into(),
                kind: Kind::File,
                size: meta.len(),
                mode: 0o644,
                mtime: meta.mtime(),
                target: None,
                dev: meta.dev(),
            },
            Scanned {
                path: "new.txt".into(),
                kind: Kind::File,
                size: 3,
                mode: 0o644,
                mtime: 1,
                target: None,
                dev: meta.dev(),
            },
        ];
        let same_sha = hash_file(&dir.join("same.txt")).unwrap();
        let previous = Manifest {
            version: 1,
            name: "old".into(),
            created_at: 0,
            host: "h".into(),
            root: "/x".into(),
            counts: Default::default(),
            uploaded_bytes: 0,
            skipped: vec![],
            entries: vec![crate::Entry {
                path: "same.txt".into(),
                kind: Kind::File,
                size: 4,
                mode: 0o644,
                mtime: meta.mtime(),
                target: None,
                sha256: Some(same_sha.clone()),
                shard: None,
            }],
            shards: vec![],
        };
        let new_sha = hash_file(&dir.join("new.txt")).unwrap();
        let mut known = HashSet::new();
        known.insert(format!("p/objects/{}/{}", &new_sha[..2], new_sha));

        let mut unreadable = Vec::new();
        let plan = plan(&dir, scanned, Some(&previous), &known, "p", &mut unreadable).unwrap();
        assert!(unreadable.is_empty());
        assert_eq!(plan.reused, 1);
        assert_eq!(plan.entries.len(), 2);
        // `same.txt`'s key is unknown to the bucket set above, so it is the only
        // upload; `new.txt` was already there.
        assert_eq!(plan.uploads.len(), 1);
        assert!(plan.uploads[0].0.ends_with("same.txt"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn plan_skips_a_file_that_vanished_after_the_scan() {
        let dir = std::env::temp_dir().join(format!("aix-ghost-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("ghost.txt"), b"gone").unwrap();
        let meta = fs::metadata(dir.join("ghost.txt")).unwrap();
        fs::remove_file(dir.join("ghost.txt")).unwrap();
        fs::write(dir.join("kept.txt"), b"here").unwrap();
        let kept = fs::metadata(dir.join("kept.txt")).unwrap();
        let scanned = vec![
            Scanned {
                path: "ghost.txt".into(),
                kind: Kind::File,
                size: meta.len(),
                mode: 0o644,
                mtime: meta.mtime(),
                target: None,
                dev: meta.dev(),
            },
            Scanned {
                path: "kept.txt".into(),
                kind: Kind::File,
                size: kept.len(),
                mode: 0o644,
                mtime: kept.mtime(),
                target: None,
                dev: kept.dev(),
            },
        ];
        let mut unreadable = Vec::new();
        let plan = plan(&dir, scanned, None, &HashSet::new(), "p", &mut unreadable).unwrap();
        assert_eq!(
            plan.entries.len(),
            1,
            "only the surviving file is in the manifest"
        );
        assert_eq!(unreadable.len(), 1);
        assert!(unreadable[0].contains("ghost.txt"), "{unreadable:?}");
        let _ = fs::remove_dir_all(&dir);
    }
}
