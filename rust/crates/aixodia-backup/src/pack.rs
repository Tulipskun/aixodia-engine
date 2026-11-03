//! The shard container: many small files, one object.
//!
//! A restore's cost used to be its number of files. The bucket shares a
//! fraction of a megabyte per second with one connection and begins refusing
//! requests above thirty-two of them, so twenty-five thousand small files meant
//! twenty-five thousand round trips and a measured 897 seconds for a tree that
//! holds 1.9 GB. This container turns that into a few dozen requests: files are
//! packed into a shard of at most [`SHARD_BUDGET`] raw bytes, each file is
//! compressed on its own, and the result is stored under one object key. The same
//! tree came back in 54 seconds.
//!
//! Five properties are load-bearing here:
//!
//! * **Per-member frames.** One zstd frame per file, not one per shard. A shard
//!   is then decoded front to back with a bounded buffer, and a shard that stops
//!   half way loses its own tail rather than every file in it.
//! * **Determinism.** The bytes a shard holds are a function of its member list,
//!   so [`fingerprint`] — computed from the manifest entries alone, with no file
//!   read — identifies the container exactly. When a fingerprint still matches
//!   the previous backup's, that shard's key is reused as it stands: nothing is
//!   read, compressed, hashed or uploaded. That is what keeps a warm backup cheap
//!   even though a change now costs a rebuild of a whole shard rather than of one
//!   file.
//! * **Content only.** The container stores a member's path, its length and its
//!   bytes, and nothing else. Mode and mtime live in the manifest, which is
//!   rewritten every run, so a `chmod` or a `touch` cannot force a rebuild: the
//!   content did not move, and neither does the shard.
//! * **A bounded blast radius.** [`group`] caps a shard in members as well as in
//!   bytes, so one shard that never arrives costs a known, small number of files.
//! * **The packed bytes are the truth.** Packing re-reads every member minutes
//!   after the scan walked it, and a live tree moves in between — a transcript or
//!   a log that grew. Each member's length and digest are taken from what was
//!   read, and [`build`] reports the members that moved, so the manifest can name
//!   the bytes the container actually holds rather than refuse them at restore.
//!
//! The container is deliberately not tar. The fields it records are tar's, but
//! tar's header carries a checksum over fields this format does not need, and its
//! padding rules make "the same list produces the same bytes" harder to state
//! than it should be.

use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::{Error, Kind, Result};

/// Raw bytes a shard carries before it is closed and another begun.
///
/// Thirty-two megabytes sits far above the median file in this tree, so a shard
/// fills on its byte budget rather than on its member cap, and it is small enough
/// that one shard's failure costs a handful of files and its build costs one
/// buffer rather than its content.
pub const SHARD_BUDGET: u64 = 32 * 1024 * 1024;

/// The most files one shard may name. A shard of empty files would otherwise
/// swallow the tree whole, and a lost shard must cost a knowable number.
pub const SHARD_MAX_MEMBERS: usize = 4_096;

/// The compression level every shard is built at. Level three is where zstd stops
/// paying much more CPU for little more ratio: measured on this tree, members came
/// out between sixteen and forty percent of their raw size.
const LEVEL: i32 = 3;

const MAGIC: &[u8; 6] = b"AIXSH1";
/// The layout of a container. Two: the raw total moved from the header, where it
/// could only state what the scan had measured, to a trailer that states what the
/// container actually holds.
const FORMAT: u8 = 2;
const HEADER_LEN: usize = MAGIC.len() + 1 + 4;
const TRAILER_LEN: usize = 8;
/// The most members a container may claim. A real one is bounded by
/// [`SHARD_MAX_MEMBERS`]; this only has to be far below what a corrupted length
/// field could offer.
const MAX_MEMBERS: u32 = 1 << 20;
/// A path longer than this is not a file in a home directory.
const MAX_PATH_LEN: u32 = 4_096;
/// Both bounds sit well above anything in this tree — its largest member is a
/// couple of hundred megabytes — and well below what a garbage length could ask
/// for, since a declared length is what a reader is told to skip or to copy.
const MAX_MEMBER_SIZE: u64 = 1 << 30;
const MAX_FRAME_SIZE: u64 = 1 << 30;
const BUFFER: usize = 1 << 20;

/// One file as the container sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    /// Relative to the backup root, `/` separated.
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// Where one member's bytes are to be put. The container does not know where the
/// tree is, so it cannot make a parent directory, set a mode, stamp a time or
/// rename a temporary sibling into place; those are the caller's, and it is the
/// caller that answers this.
#[derive(Clone, Debug)]
pub struct Destination {
    /// The length the manifest promised. A member whose frame comes out any other
    /// length is refused, not kept.
    pub size: u64,
    /// `None` when the manifest made no claim, which leaves a member unverified
    /// rather than refused.
    pub sha256: Option<String>,
    /// The sibling to decode into. It is the caller's name, so the caller's rules
    /// about what a partial file may look like still hold, and a member that fails
    /// verification leaves nothing behind at it.
    pub temp: PathBuf,
}

/// `Ok(bytes written)` once a member's content is on disk at `temp`, `Err(why)`
/// when it never got there. Putting it at its real name is the caller's.
pub type Settled = std::result::Result<u64, String>;

/// The identity of a shard's content, from its member list alone.
///
/// Every field the container records is here, so two lists with the same
/// fingerprint build to the same bytes — which is what lets a key from the
/// previous manifest be reused without reading a single file. The list is taken
/// as given, so the caller's ordering is part of the identity.
///
/// A note on portability: this is an identity for *this* build of zstd. The pinned
/// version reproduces its output byte for byte, so the fingerprint means the same
/// thing on every run and on every host this engine builds on. Were that ever to
/// change, the next backup would rebuild each shard once and be cheap again after
/// it, because a shard's key is the hash of the bytes actually built rather than
/// of this fingerprint.
pub fn fingerprint(members: &[Member]) -> String {
    let mut hash = Sha256::new();
    for member in members {
        hash.update(member.path.as_bytes());
        hash.update([0]);
        hash.update(member.size.to_le_bytes());
        hash.update(member.sha256.as_bytes());
        hash.update([10]);
    }
    hex::encode(hash.finalize())
}

/// Split `sizes` into shards: consecutive indices whose raw total fits
/// [`SHARD_BUDGET`] and whose count fits [`SHARD_MAX_MEMBERS`].
///
/// The cut depends on sizes alone, never on content, so a file that changes
/// without changing length leaves every boundary where it was and costs exactly
/// one shard. A file that grows moves the boundaries after it, and those shards
/// are rebuilt too — that is the honest price of packing, and it is why the files
/// that change size on their own (logs, the engine's state) are grouped apart from
/// everything else.
pub fn group(sizes: &[u64]) -> Vec<Vec<usize>> {
    let mut shards: Vec<Vec<usize>> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut bytes = 0u64;
    for (index, size) in sizes.iter().enumerate() {
        let full = bytes + size > SHARD_BUDGET || current.len() == SHARD_MAX_MEMBERS;
        if !current.is_empty() && full {
            bytes = 0;
            shards.push(std::mem::take(&mut current));
        }
        bytes += size;
        current.push(index);
    }
    if !current.is_empty() {
        shards.push(current);
    }
    shards
}

/// What a container ended up holding for one member that was not what it was asked
/// for: the length and hash of the bytes that went in.
#[derive(Clone, Debug)]
pub struct Drift {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

/// A container as built: its own hash and length, the members left out because they
/// could not be opened, and the members whose bytes were not what was promised.
#[derive(Debug)]
pub struct Built {
    /// The container's sha256 — the key the bucket stores it under. Empty when no
    /// member survived to be packed, which means there is no object to name.
    pub sha256: String,
    pub size: u64,
    pub lost: Vec<String>,
    pub drifted: Vec<Drift>,
}

/// Write the container for `members` to `out`.
///
/// `root` resolves each member's path. A file that has gone by the time its container
/// is built is that file's loss and not the run's: the scan already names unreadable
/// files rather than failing over them, and packing re-reads every file a rebuilt
/// shard holds — twenty-five thousand of them on a live tree — so one vanished
/// temporary file cannot be allowed to cost a backup.
///
/// A file that *changed* since it was scanned is not a loss either. Every member is
/// hashed as it is compressed, so the container declares the length and digest of the
/// bytes it actually holds and names the members that moved in `drifted`. What is in
/// the container is what its hash describes, and what the manifest ends up naming is
/// what the container holds: a log that grew while the backup ran restores as the file
/// it was at that moment, rather than being refused for not being the file it was at
/// the scan. A member that opened and then broke part way through still fails the
/// whole container, because half a read is a shard that lies, and a lying shard is
/// reused by every later run.
pub fn build(members: &[Member], root: &Path, out: &Path) -> Result<Built> {
    let mut lost: Vec<String> = Vec::new();
    let keep: Vec<&Member> = members
        .iter()
        .filter(|member| match fs::metadata(root.join(&member.path)) {
            Ok(_) => true,
            Err(_) => {
                lost.push(member.path.clone());
                false
            }
        })
        .collect();
    if keep.is_empty() {
        // Nothing to hold, so nothing to name a container by: the caller drops this
        // group rather than publishing an object nobody can restore a file from.
        return Ok(Built {
            sha256: String::new(),
            size: 0,
            lost,
            drifted: Vec::new(),
        });
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(out)
        .map_err(|source| Error::Io {
            path: out.display().to_string(),
            source,
        })?;
    let mut sink = Tracked::new(BufWriter::with_capacity(BUFFER, file));
    sink.put(MAGIC, out)?;
    sink.put(&[FORMAT], out)?;
    // The count is known here and the raw total is not, so the total is a trailer:
    // what a member actually measured is only settled once it has been read, and a
    // streaming hash cannot go back and rewrite the twelve bytes at the front.
    sink.put(&(keep.len() as u32).to_le_bytes(), out)?;

    // Each member's frame is compressed into a sibling of the container first: the
    // container declares a frame's length before its bytes, and zstd does not know
    // that length until they are written. One file, rewritten per member, so the
    // cost on disk is one frame rather than a shard's whole content held in memory.
    let staged = out.with_extension("frame");
    let io = |source: std::io::Error| Error::Io {
        path: staged.display().to_string(),
        source,
    };
    let mut stage = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&staged)
        .map_err(io)?;

    let mut drifted: Vec<Drift> = Vec::new();
    let mut total_raw = 0u64;
    let mut buffer = vec![0u8; BUFFER];
    for member in keep {
        if member.size > MAX_MEMBER_SIZE {
            return Err(Error::Api {
                operation: "pack".into(),
                message: format!("{}: {size} bytes", member.path, size = member.size),
            });
        }
        stage.seek(SeekFrom::Start(0)).map_err(io)?;
        stage.set_len(0).map_err(io)?;
        // The frame first, and its length and digest with it: a member's record
        // declares what was read, which is not always what was scanned.
        let (raw, sha256) = write_frame(&root.join(&member.path), &mut stage, &mut buffer)?;
        let frame_len = stage.seek(SeekFrom::End(0)).map_err(io)?;
        if frame_len > MAX_FRAME_SIZE {
            return Err(Error::Api {
                operation: "pack".into(),
                message: format!("{}: compressed beyond any member", member.path),
            });
        }
        if raw != member.size || sha256 != member.sha256 {
            drifted.push(Drift {
                path: member.path.clone(),
                size: raw,
                sha256: sha256.clone(),
            });
        }
        let path = member.path.as_bytes();
        if path.len() as u32 > MAX_PATH_LEN {
            return Err(Error::Api {
                operation: "pack".into(),
                message: format!("{}: a path {} bytes long", member.path, path.len()),
            });
        }
        sink.put(&(path.len() as u32).to_le_bytes(), out)?;
        sink.put(path, out)?;
        // Only regular files are ever packed: directories and symlinks are a few
        // bytes of metadata each and the manifest already names them.
        sink.put(&[Kind::File as u8], out)?;
        sink.put(&raw.to_le_bytes(), out)?;
        sink.put(&frame_len.to_le_bytes(), out)?;
        stage.seek(SeekFrom::Start(0)).map_err(io)?;
        std::io::copy(&mut stage, &mut sink).map_err(io)?;
        total_raw += raw;
    }
    // The trailer declares what the walk measured, which is the one total the loop
    // above could not know before it ran.
    sink.put(&total_raw.to_le_bytes(), out)?;
    drop(stage);
    // The staging file is a means to a length, and nothing else ever reads it.
    let _ = fs::remove_file(&staged);
    // The key is the hash of the object as the bucket will hold it, and every byte
    // of it passed through `sink`, so the digest is taken here rather than by
    // reading 310 megabytes back off the disk.
    let (hash, written) = sink.into_bytes().map_err(|source| Error::Io {
        path: out.display().to_string(),
        source,
    })?;
    Ok(Built {
        sha256: hex::encode(hash),
        size: written,
        lost,
        drifted,
    })
}

/// One member's bytes, as a single zstd frame appended to `sink`, and the length and
/// digest of what was read. Reading through a buffer of our own rather than
/// `io::copy` is what makes the digest possible; the copy is the same one either way.
fn write_frame<W: Write>(from: &Path, sink: &mut W, buffer: &mut [u8]) -> Result<(u64, String)> {
    let io = |source: std::io::Error| Error::Io {
        path: from.display().to_string(),
        source,
    };
    let mut source = File::open(from).map_err(io)?;
    let mut encoder = zstd::stream::write::Encoder::new(sink, LEVEL).map_err(io)?;
    let mut hash = Sha256::new();
    let mut raw = 0u64;
    loop {
        let got = source.read(buffer).map_err(io)?;
        if got == 0 {
            break;
        }
        hash.update(&buffer[..got]);
        encoder.write_all(&buffer[..got]).map_err(io)?;
        raw += got as u64;
    }
    // `finish` writes the frame's tail and hands the writer back. Dropping the
    // encoder instead would leave a frame unfinished, and silent about it.
    let _ = encoder.finish().map_err(io)?;
    Ok((raw, hex::encode(hash.finalize())))
}

/// Walk the container at `from`, putting each member's bytes where the caller said
/// and reporting every one of them through `settled`.
///
/// A member is only ever written to a name the caller offered as a temporary
/// sibling, and only reported as settled once its hash matches what was promised,
/// so a shard that came back scrambled cannot put a wrong file in the tree, and the
/// rename that makes a file visible stays the caller's to decide.
///
/// A container whose framing breaks — a bad magic, a length that no member could
/// produce, a frame that runs out of bytes — stops the pass and returns the error,
/// because nothing after the break can be found again. The caller then names every
/// member of that shard that did not settle, which is the whole distance it was
/// short. A member that is simply not the file it claimed to be is that member's own
/// problem, reported and passed over.
///
/// `lookup` answers whether a member is still wanted. One nobody asked for is
/// stepped over and costs nothing: the tree only ever gains what the manifest
/// names. This is how a shard rebuilt under a fingerprint that moved is survived.
pub fn extract(
    from: &Path,
    lookup: &mut dyn FnMut(&str) -> Option<Destination>,
    settled: &mut dyn FnMut(&str, Settled),
) -> Result<()> {
    let io = |source: std::io::Error| Error::Io {
        path: from.display().to_string(),
        source,
    };
    let mut reader = BufReader::with_capacity(BUFFER, File::open(from).map_err(io)?);
    let mut header = [0u8; HEADER_LEN];
    read_exact(&mut reader, &mut header, from)?;
    if &header[..MAGIC.len()] != MAGIC {
        return Err(Error::Manifest(
            from.display().to_string(),
            "not an aixodia shard".into(),
        ));
    }
    if header[MAGIC.len()] != FORMAT {
        return Err(Error::Manifest(
            from.display().to_string(),
            format!("shard format {} is not known", header[MAGIC.len()]),
        ));
    }
    // `count` cannot exceed `MAX_MEMBERS`, so the loop is bounded whatever the
    // object claimed.
    let count = {
        let mut field = [0u8; 4];
        field.copy_from_slice(&header[MAGIC.len() + 1..MAGIC.len() + 5]);
        u32::from_le_bytes(field)
    };
    if count > MAX_MEMBERS {
        return Err(Error::Manifest(
            from.display().to_string(),
            format!("{count} members is beyond any shard"),
        ));
    }
    // The declared raw total is a trailer, and tells a reader what it is looking at:
    // the members' own lengths have to add up to it.
    let mut seen = 0u64;
    for _ in 0..count {
        let mut field = [0u8; 4];
        read_exact(&mut reader, &mut field, from)?;
        let path_len = u32::from_le_bytes(field);
        if path_len > MAX_PATH_LEN {
            return Err(Error::Manifest(
                from.display().to_string(),
                format!("a member path {path_len} bytes long"),
            ));
        }
        let mut name = vec![0u8; path_len as usize];
        reader.read_exact(&mut name).map_err(io)?;
        let path = String::from_utf8_lossy(&name).to_string();
        let mut field = [0u8; 1];
        read_exact(&mut reader, &mut field, from)?;
        if field[0] != Kind::File as u8 {
            return Err(Error::Manifest(
                from.display().to_string(),
                format!("{path}: kind {} is not a file", field[0]),
            ));
        }
        let mut field = [0u8; 8];
        read_exact(&mut reader, &mut field, from)?;
        let size = u64::from_le_bytes(field);
        read_exact(&mut reader, &mut field, from)?;
        let frame_len = u64::from_le_bytes(field);
        if size > MAX_MEMBER_SIZE || frame_len > MAX_FRAME_SIZE {
            return Err(Error::Manifest(
                from.display().to_string(),
                format!("{path}: claims {size} bytes in {frame_len} compressed"),
            ));
        }
        seen += size;
        let Some(want) = lookup(&path) else {
            // Not wanted: step over its frame and go on.
            reader
                .seek(SeekFrom::Current(frame_len as i64))
                .map_err(io)?;
            continue;
        };
        // A `bufread` decoder borrows the buffer this reader already owns, so it
        // takes exactly the frame's bytes and what it did not take is still here for
        // the next member. That is why no repositioning is needed between members,
        // and why one member cannot reach into the next.
        let mut decoder = zstd::stream::read::Decoder::with_buffer(&mut reader)
            .map_err(io)?
            .single_frame();
        match write_member(&path, size, want, &mut decoder, from) {
            Ok(outcome) => settled(&path, outcome),
            Err(err) => {
                settled(&path, Err(format!("the shard ended here: {err}")));
                return Err(err);
            }
        }
        let _ = decoder.finish();
    }
    let mut field = [0u8; TRAILER_LEN];
    read_exact(&mut reader, &mut field, from)?;
    let total = u64::from_le_bytes(field);
    if seen != total {
        // The walk found a different amount of content than the object declared for
        // itself, so something in the middle of it was not read as framing. Every
        // member of this shard is named by the caller from that.
        return Err(Error::Manifest(
            from.display().to_string(),
            format!("its members total {seen} bytes and its trailer declares {total}"),
        ));
    }
    Ok(())
}

/// Decode one member into the temporary name the caller offered, checking it on the
/// way. An `Ok(Ok(_))` means the bytes at `temp` are the bytes the manifest named;
/// putting them at their real name stays the caller's. Any refusal removes `temp`,
/// because a restore that leaves a wrong file at a path is worse than one that leaves
/// the path empty.
fn write_member<R: Read>(
    path: &str,
    size: u64,
    want: Destination,
    decoder: &mut R,
    from: &Path,
) -> Result<Settled> {
    let io = |source: std::io::Error| Error::Io {
        path: format!("{}: {path}", from.display()),
        source,
    };
    let mut sink = Tracked::new(BufWriter::with_capacity(
        BUFFER,
        File::create(&want.temp).map_err(io)?,
    ));
    // A frame that stops short of its declared length is the container breaking,
    // not the member being wrong: the bytes after it cannot be found again.
    let copied = std::io::copy(decoder, &mut sink);
    let (hash, written) = match sink.into_bytes() {
        Ok(finished) => finished,
        Err(source) => {
            let _ = fs::remove_file(&want.temp);
            return Err(io(source));
        }
    };
    let got = hex::encode(hash);
    if copied.is_err() || written != size || want.size != size {
        let _ = fs::remove_file(&want.temp);
    }
    match copied {
        Err(source) => return Err(io(source)),
        Ok(_) if written != size || want.size != size => {
            return Ok(Err(format!(
                "the shard holds {written} bytes and the manifest names {}",
                want.size
            )))
        }
        Ok(_) => {}
    }
    if let Some(claim) = &want.sha256 {
        if claim != &got {
            let _ = fs::remove_file(&want.temp);
            return Ok(Err(format!(
                "content is not what the manifest names (want {claim}, got {got})"
            )));
        }
    }
    Ok(Ok(written))
}

/// Fill `buffer` from `reader`, or say which object ran out.
fn read_exact<R: Read>(reader: &mut R, buffer: &mut [u8], from: &Path) -> Result<()> {
    reader.read_exact(buffer).map_err(|source| {
        Error::Manifest(from.display().to_string(), format!("truncated: {source}"))
    })
}

/// A writer that counts and hashes everything passing through it, so an object's
/// key can be taken from the bytes as they were written rather than by reading them
/// back.
struct Tracked<W> {
    inner: W,
    hash: Sha256,
    written: u64,
}

impl<W: Write> Tracked<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hash: Sha256::new(),
            written: 0,
        }
    }

    /// `write_all` with the failure named after the object that was being built.
    fn put(&mut self, bytes: &[u8], at: &Path) -> Result<()> {
        self.write_all(bytes).map_err(|source| Error::Io {
            path: at.display().to_string(),
            source,
        })
    }

    /// Flush, finish, and hand back the digest and the byte count.
    fn into_bytes(mut self) -> std::io::Result<([u8; 32], u64)> {
        self.inner.flush()?;
        let digest = self.hash.finalize();
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&digest);
        Ok((hash, self.written))
    }
}

impl<W: Write> Write for Tracked<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.hash.update(&buffer[..written]);
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, UNIX_EPOCH};

    fn tree(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aix-pack-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).expect("a tree");
        dir
    }

    fn member(path: &str, root: &Path) -> Member {
        let full = root.join(path);
        Member {
            path: path.to_string(),
            size: fs::metadata(&full).expect("a file").len(),
            sha256: crate::scan::hash_file(&full).expect("a hash"),
        }
    }

    fn destination(member: &Member, at: &Path) -> Destination {
        Destination {
            size: member.size,
            sha256: Some(member.sha256.clone()),
            temp: at.join(&member.path),
        }
    }

    /// A backup walks the tree and then packs it minutes apart, and a live tree
    /// loses files in between. One gone file must not cost the whole backup, so a
    /// member that cannot be opened is dropped and named — the same way the scan
    /// treats a file it cannot read.
    #[test]
    fn a_member_that_went_is_named_and_the_rest_is_packed() {
        let root = tree("vanished");
        fs::write(root.join("gone.txt"), b"here while the tree was walked").unwrap();
        fs::write(root.join("two.txt"), "beta".repeat(900)).unwrap();
        let mut members = vec![member("gone.txt", &root), member("two.txt", &root)];
        fs::remove_file(root.join("gone.txt")).expect("the file goes");

        let out = root.join(".shard");
        let built = build(&members, &root, &out).expect("a shard");
        let (sha256, size, lost) = (built.sha256, built.size, built.lost);
        assert_eq!(lost, vec!["gone.txt".to_string()], "the one that went");
        assert!(!sha256.is_empty(), "and the shard still exists");
        assert!(size > 0, "with the bytes that stayed");
        assert!(
            built.drifted.is_empty(),
            "the file that stayed did not move"
        );

        let mut reached = Vec::new();
        extract(
            &out,
            &mut |path| {
                reached.push(path.to_string());
                let found = members.iter().find(|m| m.path == path)?;
                Some(destination(found, &root))
            },
            &mut |_, _| {},
        )
        .expect("a container of one member is a container");
        assert_eq!(reached, vec!["two.txt"], "and it holds only that");

        // What the container declares has to be what it holds, or a restore reads it
        // as short of the file it is missing.
        members.retain(|member| member.path != "gone.txt");
        let bytes = fs::read(&out).expect("the shard");
        let count = u32::from_le_bytes(
            bytes[MAGIC.len() + 1..MAGIC.len() + 5]
                .try_into()
                .expect("a header"),
        );
        let raw = u64::from_le_bytes(
            bytes[bytes.len() - TRAILER_LEN..]
                .try_into()
                .expect("a trailer"),
        );
        assert_eq!(count as usize, members.len(), "the count is the kept one");
        assert_eq!(raw, members.iter().map(|m| m.size).sum::<u64>());
        let _ = fs::remove_dir_all(&root);
    }

    /// A group whose every member went is not an error and not an object: it names
    /// what is missing and leaves nothing for the bucket to hold.
    #[test]
    fn a_group_that_entirely_went_builds_nothing() {
        let root = tree("all-vanished");
        let members = vec![
            Member {
                path: "one.txt".into(),
                size: 10,
                sha256: String::new(),
            },
            Member {
                path: "two.txt".into(),
                size: 20,
                sha256: String::new(),
            },
        ];
        let out = root.join(".shard");
        let built = build(&members, &root, &out).expect("nothing to pack is not an error");
        let (sha256, size, lost) = (built.sha256, built.size, built.lost);
        assert!(sha256.is_empty(), "no bytes, no identity");
        assert_eq!(size, 0);
        assert_eq!(lost.len(), 2, "and both are named: {lost:?}");
        assert!(!out.exists(), "and no half-written container is left");
        let _ = fs::remove_dir_all(&root);
    }

    /// What the live run actually hit: a session transcript that grew between the
    /// walk of the tree and the packing of it. The container keeps the bytes it read
    /// and says what they were, rather than holding a member whose declared length no
    /// longer matches it — which is the difference between a backup that restores and
    /// one that names four files it could not.
    #[test]
    fn a_member_that_grew_while_it_was_packed_is_recorded_as_read() {
        let root = tree("drift");
        fs::write(root.join("tail.txt"), b"first line\n").unwrap();
        fs::write(root.join("steady.txt"), b"unchanged").unwrap();
        let mut members = vec![member("tail.txt", &root), member("steady.txt", &root)];
        let scanned = members[0].sha256.clone();
        fs::write(root.join("tail.txt"), b"first line\nsecond line\n").unwrap();
        let grown = fs::metadata(root.join("tail.txt")).expect("the file").len();

        let out = root.join(".shard");
        let built = build(&members, &root, &out).expect("a shard");
        assert_eq!(built.drifted.len(), 1, "only the file that moved");
        let drift = &built.drifted[0];
        assert_eq!(drift.path, "tail.txt");
        assert_eq!(drift.size, grown, "and it names the length that went in");
        assert_ne!(drift.sha256, scanned, "with the bytes it read");

        // A caller that adopts the report gets a member that verifies. Refusing it
        // would be the old behaviour, and a restore is not where a moving file is news.
        members[0].size = drift.size;
        members[0].sha256 = drift.sha256.clone();
        let mut settled = Vec::new();
        extract(
            &out,
            &mut |path| {
                members
                    .iter()
                    .find(|member| member.path == path)
                    .map(|member| destination(member, &root))
            },
            &mut |path, outcome| settled.push((path.to_string(), outcome)),
        )
        .expect("the container is well formed");
        assert_eq!(settled.len(), 2, "both members reported");
        for (path, outcome) in &settled {
            assert!(outcome.is_ok(), "{path}: {outcome:?}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// The claim warm reuse rests on: nothing about a shard depends on when it was
    /// built, so the fingerprint can stand in for the bytes.
    #[test]
    fn the_same_members_build_the_same_bytes() {
        let root = tree("determinism");
        fs::write(root.join("sub/one.txt"), b"alpha alpha alpha").unwrap();
        fs::write(root.join("two.txt"), "beta".repeat(900)).unwrap();
        let members = vec![member("two.txt", &root), member("sub/one.txt", &root)];
        let out = root.join(".shard");

        let first_built = build(&members, &root, &out).expect("a shard");
        let (first, first_len) = (first_built.sha256, first_built.size);
        let kept = fs::read(&out).expect("the shard");
        let second_built = build(&members, &root, &out).expect("the other shard");
        let (second, second_len) = (second_built.sha256, second_built.size);

        assert_eq!(first, second, "the hash is an identity");
        assert_eq!(
            kept,
            fs::read(&out).expect("the shard again"),
            "and so are the bytes"
        );
        assert_eq!(first_len, second_len);
        let mut reversed = members.clone();
        reversed.reverse();
        assert_ne!(
            fingerprint(&members),
            fingerprint(&reversed),
            "and the order the caller chose is part of it"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// The container holds content and nothing else, so a mode or a stamp that
    /// moved cannot strand a shard that still describes the bytes exactly.
    #[test]
    fn a_content_change_moves_the_fingerprint_and_nothing_else_does() {
        let root = tree("fingerprint");
        fs::write(root.join("three.txt"), b"gamma").unwrap();
        let base = member("three.txt", &root);
        let before = fingerprint(std::slice::from_ref(&base));

        let mut renamed = base.clone();
        renamed.path = "other.txt".into();
        let mut longer = base.clone();
        longer.size += 1;
        let mut other_content = base.clone();
        other_content.sha256 = "0".repeat(64);

        assert_eq!(
            before,
            fingerprint(std::slice::from_ref(&base)),
            "the same list twice"
        );
        for changed in [renamed, longer, other_content] {
            assert_ne!(
                before,
                fingerprint(std::slice::from_ref(&changed)),
                "{:?} is a different shard",
                changed
            );
        }
        fs::set_permissions(root.join("three.txt"), fs::Permissions::from_mode(0o600)).unwrap();
        let file = fs::OpenOptions::new()
            .write(true)
            .open(root.join("three.txt"))
            .unwrap();
        file.set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1)))
            .unwrap();
        let after = member("three.txt", &root);
        assert_eq!(
            before,
            fingerprint(&[after]),
            "a mode and a stamp that moved leave the content where it was"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_round_trip_writes_what_the_tree_held() {
        let root = tree("roundtrip");
        fs::write(root.join("sub/one.txt"), b"alpha alpha alpha").unwrap();
        fs::write(root.join("two.txt"), "beta".repeat(900)).unwrap();
        fs::write(root.join("empty.txt"), b"").unwrap();
        let members = vec![
            member("empty.txt", &root),
            member("sub/one.txt", &root),
            member("two.txt", &root),
        ];
        let out = root.join(".shard");
        let size = build(&members, &root, &out).expect("a shard").size;
        assert!(size < 8_192, "three small files pack small: {size}");

        // The caller owns the tree it is writing, including the directories: this
        // module is told a name and writes to it, nothing more.
        let restored = root.join("restored");
        fs::create_dir_all(restored.join("sub")).unwrap();
        let mut settled = Vec::new();
        extract(
            &out,
            &mut |path| {
                let member = members.iter().find(|m| m.path == path)?;
                Some(destination(member, &restored))
            },
            &mut |path, outcome| settled.push((path.to_string(), outcome)),
        )
        .expect("the container is well formed");

        assert_eq!(settled.len(), 3, "every member reported");
        for (path, outcome) in &settled {
            assert!(outcome.is_ok(), "{path}: {outcome:?}");
        }
        for member in &members {
            assert_eq!(
                fs::read(restored.join(&member.path)).expect("a restored member"),
                fs::read(root.join(&member.path)).expect("the original"),
                "{} came back as it went in",
                member.path
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    /// A shard is not the tree: it names paths. A member whose bytes are not what
    /// was promised is refused, and nothing is left at the name it was written to.
    #[test]
    fn a_member_that_is_not_what_was_named_is_refused() {
        let root = tree("verify");
        fs::write(root.join("kept.txt"), b"what the manifest says").unwrap();
        let members = vec![member("kept.txt", &root)];
        let out = root.join(".shard");
        build(&members, &root, &out).expect("a shard");

        let temp = root.join(".kept.txt.aixodia-part");
        let mut reported = Vec::new();
        extract(
            &out,
            &mut |_| {
                Some(Destination {
                    size: members[0].size,
                    sha256: Some("0".repeat(64)),
                    temp: temp.clone(),
                })
            },
            &mut |path, outcome| reported.push((path.to_string(), outcome)),
        )
        .expect("the container itself is sound");

        assert_eq!(reported.len(), 1, "the refusal is the only report");
        assert_eq!(reported[0].0, "kept.txt");
        assert!(reported[0].1.is_err(), "{:?}", reported[0].1);
        assert!(!temp.exists(), "and nothing was left at the temporary name");
        let _ = fs::remove_dir_all(&root);
    }

    /// Per-member frames are what makes a short shard a short shard rather than an
    /// unreadable one: the pass stops at the cut, and the caller is left holding the
    /// members that came before it.
    #[test]
    fn a_truncated_shard_stops_at_the_cut_and_says_so() {
        let root = tree("truncated");
        fs::write(root.join("long.txt"), "gamma".repeat(20_000)).unwrap();
        fs::write(root.join("short.txt"), b"delta").unwrap();
        let out = root.join(".shard");
        // The long member first, so the cut falls inside its frame and the file
        // after it is never reached.
        build(
            &[member("long.txt", &root), member("short.txt", &root)],
            &root,
            &out,
        )
        .expect("a shard");
        let whole = fs::read(&out).expect("the shard");
        fs::write(&out, &whole[..whole.len() / 2]).expect("a shorter shard");

        let mut reached = Vec::new();
        let mut settled = Vec::new();
        let err = extract(
            &out,
            &mut |path| {
                reached.push(path.to_string());
                let meta = fs::metadata(root.join(path)).ok()?;
                Some(Destination {
                    size: meta.len(),
                    sha256: None,
                    temp: root.join(format!(".{path}.aixodia-part")),
                })
            },
            &mut |path, outcome| settled.push((path.to_string(), outcome)),
        )
        .expect_err("half a container is not a shard");
        assert!(
            matches!(err, Error::Io { .. }),
            "the bytes ran out, which is what it says: {err}"
        );
        assert_eq!(
            reached,
            vec!["long.txt"],
            "the cut is where the pass stopped"
        );
        assert_eq!(settled.len(), 1, "and the member at the cut is named");
        assert!(settled[0].1.is_err(), "{:?}", settled[0].1);
        assert!(
            !root.join(".long.txt.aixodia-part").exists(),
            "leaving nothing behind at a partial name"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A member the manifest no longer names is a shard built under a fingerprint
    /// that moved. It costs that member and nothing else.
    #[test]
    fn a_member_nobody_asked_for_is_stepped_over() {
        let root = tree("extra");
        fs::write(root.join("asked.txt"), b"epsilon").unwrap();
        fs::write(root.join("unasked.txt"), "zeta".repeat(4_000)).unwrap();
        let out = root.join(".shard");
        build(
            &[member("asked.txt", &root), member("unasked.txt", &root)],
            &root,
            &out,
        )
        .expect("a shard");
        let mut settled = Vec::new();
        extract(
            &out,
            &mut |path| (path == "asked.txt").then(|| destination(&member(path, &root), &root)),
            &mut |path, outcome| settled.push((path.to_string(), outcome)),
        )
        .expect("the container is well formed");
        assert_eq!(
            settled.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            vec!["asked.txt"],
            "the unasked member was skipped and the one after it still found"
        );
        assert!(settled[0].1.is_ok(), "{:?}", settled[0].1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_shard_closes_on_bytes_and_on_count() {
        assert_eq!(
            group(&[SHARD_BUDGET, 1, 1]),
            vec![vec![0], vec![1, 2]],
            "a file larger than the budget stands alone"
        );

        let crowd = group(&vec![0u64; SHARD_MAX_MEMBERS + 5]);
        assert_eq!(crowd.len(), 2, "a thousand empty files are not one shard");
        assert_eq!(crowd[0].len(), SHARD_MAX_MEMBERS);

        // A content change that keeps the length cannot move a boundary, which is
        // the difference between rebuilding one shard and rebuilding the tree.
        let sizes = vec![SHARD_BUDGET / 4; 40];
        let before = group(&sizes);
        let mut shorter = sizes.clone();
        shorter[9] -= 1;
        let after = group(&shorter);
        assert_eq!(before.len(), after.len());
        assert_eq!(before[..9], after[..9], "nothing before the change moved");
    }
}
