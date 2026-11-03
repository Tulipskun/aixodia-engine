//! Getting bytes into the bucket's Xet store.
//!
//! A bucket has no plain object PUT: content is ingested by a Xet client, which
//! answers with a hash, and only then can a path be registered for it. This
//! module does the ingest half and reports `key -> hash`; [`crate::Hub`] does
//! the registration half.
//!
//! `hf-xet`'s blocking entry points panic when they are called from inside a
//! tokio runtime, so every call here happens on a thread this module owns.

use std::path::PathBuf;

use http::header::{AUTHORIZATION, USER_AGENT};
use xet::xet_session::XetSessionBuilder;
use xet_data::processing::Sha256Policy;

use xet_runtime::config::XetConfig;

use crate::{Config, Error, Reporter, Result};

/// Files per commit. Small enough that one bad file costs one chunk of retry,
/// large enough that the hub is not asked for a new write token per file.
const COMMIT_CHUNK: usize = 1000;

/// Upload connections the CAS client opens straight away.
///
/// The client's own default is two, and its adaptive controller only steps that
/// up once twenty megabytes of transfers have completed — which a tree of thirty
/// thousand files of a few kilobytes each takes a long time to reach, because
/// every one of them is a round trip rather than a stream. A first backup of a
/// Rust toolchain measured four minutes and fourteen seconds for 901 MB at two
/// connections; the wide start is what makes a bulk backup feel like a copy
/// instead of a queue.
const UPLOAD_CONCURRENCY: usize = 16;

/// The floor the controller may retreat to when the link turns bad.
const UPLOAD_CONCURRENCY_MIN: usize = 4;

/// The client's own preset for high-performance mode.
const UPLOAD_ADJUSTMENT_BYTES: &str = "1mb";

/// The session config, with the upload path tuned for many small objects. A
/// wrong dotted path would keep the client's default silently, so the values
/// are read back and the paths are pinned by a test.
fn upload_config() -> Result<XetConfig> {
    XetConfig::new()
        .with_config("client.ac_initial_upload_concurrency", UPLOAD_CONCURRENCY)
        .and_then(|c| c.with_config("client.ac_min_upload_concurrency", UPLOAD_CONCURRENCY_MIN))
        .and_then(|c| {
            c.with_config(
                "client.ac_min_bytes_required_for_adjustment",
                UPLOAD_ADJUSTMENT_BYTES,
            )
        })
        .map_err(|e| Error::Xet(format!("upload concurrency: {e}")))
}

/// What to ingest for one key.
pub struct Job {
    pub key: String,
    pub source: Source,
    /// The file's sha256 when the scan already computed it, so the ingest
    /// client does not have to read the file twice.
    pub sha256: Option<String>,
    pub size: u64,
}

pub enum Source {
    File(PathBuf),
    Bytes(Vec<u8>),
}

/// One ingested object, ready to register under `key`.
pub struct Done {
    pub key: String,
    pub xet_hash: String,
    pub size: u64,
}

/// Ingest every job and return what the bucket now stores.
///
/// Progress is published to `reporter` per completed chunk, and the total byte
/// count is set from the jobs before the first chunk starts.
pub fn ingest(cfg: &Config, jobs: Vec<Job>, reporter: Reporter) -> Result<Vec<Done>> {
    if jobs.is_empty() {
        return Ok(Vec::new());
    }
    let endpoint = cfg.endpoint.trim_end_matches('/').to_string();
    let token = cfg.token.clone();
    let bucket = cfg.bucket.clone();
    // The client itself is built here rather than in the caller so that no
    // tokio context can reach it by accident.
    let worker = std::thread::Builder::new()
        .name("aixodia-backup-xet".into())
        .spawn(move || run(&endpoint, &token, &bucket, jobs, &reporter))
        .map_err(|source| Error::Api {
            operation: "spawn xet worker".into(),
            message: source.to_string(),
        })?;
    worker
        .join()
        .map_err(|_| Error::Xet("the xet worker panicked".into()))?
}

fn run(
    endpoint: &str,
    token: &str,
    bucket: &str,
    jobs: Vec<Job>,
    reporter: &Reporter,
) -> Result<Vec<Done>> {
    let mut headers = http::HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        format!("Bearer {token}")
            .parse()
            .map_err(|e: http::header::InvalidHeaderValue| Error::Xet(e.to_string()))?,
    );
    headers.insert(USER_AGENT, "aixodia-engine-backup".parse().unwrap());
    let refresh_url = format!("{endpoint}/api/buckets/{bucket}/xet-write-token");

    let session = XetSessionBuilder::new_with_config(upload_config()?)
        .build()
        .map_err(|e| Error::Xet(e.to_string()))?;
    let mut out = Vec::with_capacity(jobs.len());
    let total = jobs.iter().map(|j| j.size).sum();
    reporter.set_totals(jobs.len(), total);

    for chunk in jobs.chunks(COMMIT_CHUNK) {
        out.extend(ingest_chunk(
            &session,
            &refresh_url,
            headers.clone(),
            chunk,
            reporter,
        )?);
    }
    Ok(out)
}

fn ingest_chunk(
    session: &xet::xet_session::XetSession,
    refresh_url: &str,
    headers: http::HeaderMap,
    chunk: &[Job],
    reporter: &Reporter,
) -> Result<Vec<Done>> {
    let commit = session
        .new_upload_commit()
        .map_err(|e| Error::Xet(e.to_string()))?
        .with_token_refresh_url(refresh_url.to_string(), headers.clone())
        .with_custom_headers(headers)
        .build_blocking()
        .map_err(|e| Error::Xet(e.to_string()))?;

    // The commit report is keyed by task id, which is how a finished hash is
    // matched back to the key it belongs to.
    let mut by_task: std::collections::HashMap<u64, (&str, u64)> = std::collections::HashMap::new();
    let mut handles = Vec::with_capacity(chunk.len());
    for job in chunk {
        let policy = match &job.sha256 {
            Some(sha) => Sha256Policy::from_hex(sha),
            None => Sha256Policy::Skip,
        };
        let handle = match &job.source {
            Source::File(path) => commit
                .upload_from_path_blocking(path.clone(), policy)
                .map_err(|e| Error::Xet(format!("{}: {e}", job.key)))?,
            Source::Bytes(bytes) => commit
                .upload_bytes_blocking(bytes.clone(), policy, Some(job.key.clone()))
                .map_err(|e| Error::Xet(format!("{}: {e}", job.key)))?,
        };
        by_task.insert(handle.task_id().0, (job.key.as_str(), job.size));
        handles.push(handle);
    }
    let report = commit
        .commit_blocking()
        .map_err(|e| Error::Xet(format!("{} files: {e}", chunk.len())))?;
    drop(handles);

    let mut done = Vec::with_capacity(chunk.len());
    for (task, metadata) in report.uploads.iter() {
        let Some((key, size)) = by_task.get(&task.0) else {
            continue;
        };
        done.push(Done {
            key: (*key).to_string(),
            xet_hash: metadata.xet_info.hash.clone(),
            size: *size,
        });
        reporter.advance(1, *size);
    }
    if done.len() != chunk.len() {
        return Err(Error::Xet(format!(
            "the bucket ingested {} of {} objects",
            done.len(),
            chunk.len()
        )));
    }
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dotted path the client does not know is accepted silently by most
    /// config layers, and would leave the upload at two connections forever.
    #[test]
    fn the_upload_paths_are_the_ones_the_client_reads() {
        let cfg = upload_config().expect("the concurrency paths are real config paths");
        assert_eq!(cfg.client.ac_initial_upload_concurrency, UPLOAD_CONCURRENCY);
        assert_eq!(cfg.client.ac_min_upload_concurrency, UPLOAD_CONCURRENCY_MIN);
        let adjustment_bytes: u64 = cfg.client.ac_min_bytes_required_for_adjustment.into();
        // The client reads "1mb" as a decimal megabyte, which is the point: the
        // threshold is what has to be transferred before the controller is
        // allowed to react, and twenty of them never arrives in a small-file tree.
        assert_eq!(adjustment_bytes, 1_000_000);
    }
}
