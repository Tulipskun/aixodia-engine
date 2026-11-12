//! The bucket's HTTP surface: tree listing, path existence, batch add/delete
//! and `resolve` downloads.
//!
//! Uploads are not here. A bucket only accepts content that has gone through
//! Xet, so [`crate::xet`] pushes bytes to the CAS and this module registers the
//! resulting file hashes under a key.

use std::io::Seek;
use std::path::Path;
use std::time::Duration;

use http::header::{ACCEPT_ENCODING, AUTHORIZATION, CONTENT_TYPE, LINK, RANGE, USER_AGENT};
use serde::Deserialize;

use crate::{
    encode_key, Config, Error, Result, CONNECT_TIMEOUT_SECS, DOWNLOAD_TIMEOUT_MAX,
    DOWNLOAD_TIMEOUT_SECS,
};

/// Operations per `/batch` request. The payload is one small line per key, so
/// the limit is about keeping a single failure from muddling a large run.
const BATCH_OPS: usize = 500;
/// Paths per `/paths-info` request, which is what the hub itself uses.
const PATHS_INFO_BATCH: usize = 1000;

/// One file as the bucket reports it.
#[derive(Clone, Debug)]
pub struct FileItem {
    pub path: String,
    pub size: u64,
    /// The Xet Merkle hash of the content, needed to copy an object to another
    /// key without moving its bytes.
    pub xet_hash: Option<String>,
}

#[derive(Deserialize)]
struct TreeItem {
    #[serde(rename = "type")]
    kind: String,
    path: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default, rename = "xetHash")]
    xet_hash: Option<String>,
}

#[derive(Deserialize)]
struct BatchReply {
    #[serde(default)]
    failed: Vec<serde_json::Value>,
}

/// The bucket API, bound to one bucket and one token.
pub struct Hub {
    bucket: String,
    /// The key prefix this backup owns, so callers of a `Hub` need only a
    /// manifest name to name an object.
    prefix: String,
    /// `{endpoint}/api/buckets/{bucket}`.
    api: String,
    /// `{endpoint}/buckets/{bucket}/resolve`.
    resolve: String,
    token: String,
    client: reqwest::blocking::Client,
    /// The download window, in seconds: what [`Config::timeout_secs`] allows,
    /// bounded. See [`Resolver`] and [`object_timeout_secs`].
    window: u64,
}

/// How long one object may take to arrive: enough for its own bytes at a rate a
/// weak uplink sustains (25 KB/s), never under the floor, and never over the
/// window the run allows. This is what stops a forty-eight-byte file from being
/// waited on for a minute and a half — and a tree of small files is what a
/// restore is mostly made of.
fn object_timeout_secs(size: u64, window: u64) -> u64 {
    let needed = 5 + size / 25_000;
    needed
        .max(DOWNLOAD_TIMEOUT_SECS)
        .min(window)
        .min(DOWNLOAD_TIMEOUT_MAX)
}

/// How long a download may run: the configured allowance, bounded so that a
/// stalled stream is cut and retried rather than waited on for minutes.
fn download_timeout_secs(timeout_secs: u64) -> u64 {
    timeout_secs.clamp(DOWNLOAD_TIMEOUT_SECS, DOWNLOAD_TIMEOUT_MAX)
}

impl Hub {
    /// A client for `cfg`'s bucket. Must be built off any tokio runtime, like
    /// everything else in this crate.
    pub fn new(cfg: &Config) -> Result<Self> {
        cfg.validate()?;
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(cfg.timeout_secs.max(10)))
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .build()?;
        Ok(Self {
            bucket: cfg.bucket.clone(),
            prefix: cfg.prefix.clone(),
            api: format!(
                "{}/api/buckets/{}",
                trim_endpoint(&cfg.endpoint),
                cfg.bucket
            ),
            resolve: format!(
                "{}/buckets/{}/resolve",
                trim_endpoint(&cfg.endpoint),
                cfg.bucket
            ),
            token: cfg.token.clone(),
            window: download_timeout_secs(cfg.timeout_secs),
            client,
        })
    }

    /// A download window of its own, for one restore worker.
    ///
    /// A `reqwest` blocking client drives every request it is handed from a
    /// single internal thread, so a client shared by twelve workers makes them
    /// queue behind each other — and behind whatever one of them is stuck on.
    /// Call this once per worker.
    pub fn downloader(&self) -> Result<Resolver> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(self.window))
            .connect_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .tcp_nodelay(true)
            .build()?;
        Ok(Resolver {
            resolve: self.resolve.clone(),
            token: self.token.clone(),
            window: self.window,
            client,
        })
    }

    pub fn manifest_key(&self, name: &str) -> String {
        format!("{}/manifests/{}.json", self.prefix, name)
    }

    pub fn latest_key(&self) -> String {
        format!("{}/latest.json", self.prefix)
    }

    pub fn manifests_prefix(&self) -> String {
        format!("{}/manifests", self.prefix)
    }

    pub fn objects_prefix(&self) -> String {
        format!("{}/objects", self.prefix)
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    fn get(&self, path: &str) -> reqwest::blocking::RequestBuilder {
        self.client
            .get(path)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
    }

    fn post(&self, path: &str) -> reqwest::blocking::RequestBuilder {
        self.client
            .post(path)
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
    }

    /// `key` must already be percent-encoded for the URL, and `operation` is
    /// what a failure names itself as.
    fn check(status: reqwest::StatusCode, body: String, operation: &str, key: &str) -> Result<()> {
        if status.is_success() {
            return Ok(());
        }
        Err(Error::Hub {
            operation: operation.to_string(),
            key: key.to_string(),
            status: status.as_u16(),
            body: trim_body(&body),
        })
    }

    /// Every file under `prefix`, recursively. Pagination is the hub's `Link`
    /// header, the same as its other list endpoints.
    pub fn list_files(&self, prefix: &str) -> Result<Vec<FileItem>> {
        let mut out = Vec::new();
        let mut url = format!("{}/tree/{}?recursive=true", self.api, encode_key(prefix));
        loop {
            let response = self
                .get(&url)
                .header(USER_AGENT, "aixodia-engine-backup")
                .send()?;
            let status = response.status();
            let link = link_next(response.headers());
            let body = response.text()?;
            Self::check(status, body.clone(), "list", prefix)?;
            let page: Vec<TreeItem> = serde_json::from_str(&body)
                .map_err(|e| Error::Manifest(prefix.to_string(), e.to_string()))?;
            for item in page {
                if item.kind == "file" {
                    out.push(FileItem {
                        path: item.path,
                        size: item.size.unwrap_or(0),
                        xet_hash: item.xet_hash,
                    });
                }
            }
            match link {
                Some(next) => url = next,
                None => return Ok(out),
            }
        }
    }

    /// The keys among `keys` that the bucket actually holds. Missing keys are
    /// not an error, so this is how a caller asks "is this content already up?".
    pub fn existing_keys(&self, keys: &[String]) -> Result<Vec<String>> {
        let mut found = Vec::new();
        for chunk in keys.chunks(PATHS_INFO_BATCH) {
            let response = self
                .post(&format!("{}/paths-info", self.api))
                .json(&serde_json::json!({ "paths": chunk }))
                .send()?;
            let status = response.status();
            let body = response.text()?;
            Self::check(
                status,
                body.clone(),
                "paths-info",
                &format!("{} keys", chunk.len()),
            )?;
            let items: Vec<TreeItem> = serde_json::from_str(&body)
                .map_err(|e| Error::Manifest("paths-info".into(), e.to_string()))?;
            found.extend(
                items
                    .into_iter()
                    .filter(|i| i.kind == "file")
                    .map(|i| i.path),
            );
        }
        Ok(found)
    }

    /// `GET /buckets/{bucket}/resolve/{key}`. Returns `None` when the key is
    /// not there; the hub redirects to the CDN and the redirect is followed.
    ///
    /// A manifest is read through here too, and it is the one request a run
    /// cannot proceed without: it gets the download window, so a stall shows up
    /// as a failed run in seconds rather than as silence for two minutes.
    pub fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let url = format!("{}/{}", self.resolve, encode_key(key));
        let response = self
            .get(&url)
            .timeout(Duration::from_secs(self.window))
            .send()?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            return Err(Error::Hub {
                operation: "download".into(),
                key: key.to_string(),
                status: status.as_u16(),
                body: trim_body(&body),
            });
        }
        let body = response.bytes()?;
        Ok(Some(body.to_vec()))
    }

    /// Registers `items` as files: `(key, xet hash, modification time in
    /// milliseconds, content type)`. This is the step that makes uploaded
    /// content visible under a path.
    pub fn add_files(&self, items: &[(String, String, i64, Option<&str>)]) -> Result<()> {
        for chunk in items.chunks(BATCH_OPS) {
            let mut payload = String::new();
            for (key, hash, mtime_ms, content_type) in chunk {
                let mut line = serde_json::json!({
                    "type": "addFile",
                    "path": key,
                    "xetHash": hash,
                    "mtime": mtime_ms,
                });
                if let Some(ct) = content_type {
                    line["contentType"] = serde_json::Value::String(ct.to_string());
                }
                payload.push_str(&line.to_string());
                payload.push('\n');
            }
            self.batch("add", &payload, chunk.len())?;
        }
        Ok(())
    }

    /// Deletes keys. Buckets are not versioned, so this is the end of them.
    pub fn delete_keys(&self, keys: &[String]) -> Result<()> {
        for chunk in keys.chunks(BATCH_OPS) {
            let mut payload = String::new();
            for key in chunk {
                let line = serde_json::json!({"type": "deleteFile", "path": key});
                payload.push_str(&line.to_string());
                payload.push('\n');
            }
            self.batch("delete", &payload, chunk.len())?;
        }
        Ok(())
    }

    /// The URL the Xet client calls for a fresh write token.
    pub fn write_token_url(&self) -> String {
        format!("{}/xet-write-token", self.api)
    }

    fn batch(&self, operation: &str, payload: &str, sent: usize) -> Result<()> {
        let response = self
            .post(&format!("{}/batch", self.api))
            .header(CONTENT_TYPE, "application/x-ndjson")
            .body(payload.to_string())
            .send()?;
        let status = response.status();
        let body = response.text()?;
        // A bucket batch answers 200 with the operations it refused, so the
        // status alone never tells the whole story.
        if status.is_success() || status.as_u16() == 422 {
            if let Ok(reply) = serde_json::from_str::<BatchReply>(&body) {
                if !reply.failed.is_empty() {
                    return Err(Error::Api {
                        operation: format!("{operation} batch"),
                        message: format!(
                            "{} of {sent} operations failed: {}",
                            reply.failed.len(),
                            trim_body(&serde_json::to_string(&reply.failed).unwrap_or_default())
                        ),
                    });
                }
            }
        }
        Self::check(status, body, operation, &format!("{sent} keys"))
    }
}

/// One worker's window on the bucket's `resolve` endpoint: its own connection
/// pool and its own thread driving them, with a timeout short enough that a
/// stalled stream costs a retry rather than minutes.
pub struct Resolver {
    resolve: String,
    token: String,
    client: reqwest::blocking::Client,
    window: u64,
}

impl Resolver {
    /// `GET /buckets/{bucket}/resolve/{key}` into `path`, which the caller owns:
    /// it is the temporary sibling a half-received file is written to, so no
    /// piece of this code has to know where the final name is.
    ///
    /// `range` is `(first, last)` byte indices, inclusive. A large object is
    /// fetched as several ranges at once because the bucket serves roughly a
    /// fifth of a megabyte per second per connection, so one ninety-megabyte
    /// binary is a seven-minute tail on an otherwise finished restore. `None`
    /// asks for the whole object.
    pub fn download_piece(
        &self,
        key: &str,
        range: Option<(u64, u64)>,
        want_bytes: u64,
        path: &Path,
    ) -> Result<u64> {
        let url = format!("{}/{}", self.resolve, encode_key(key));
        let mut request = self
            .client
            .get(&url)
            .timeout(Duration::from_secs(object_timeout_secs(
                want_bytes,
                self.window,
            )))
            .header(AUTHORIZATION, format!("Bearer {}", self.token))
            .header(ACCEPT_ENCODING, "identity");
        if let Some((first, last)) = range {
            request = request.header(RANGE, format!("bytes={first}-{last}"));
        }
        let response = request.send()?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::Hub {
                operation: "download".into(),
                key: key.to_string(),
                status: 404,
                body: "the object is not in the bucket".into(),
            });
        }
        if !status.is_success() {
            let body = response.text().unwrap_or_default();
            return Err(Error::Hub {
                operation: "download".into(),
                key: key.to_string(),
                status: status.as_u16(),
                body: trim_body(&body),
            });
        }
        // A 200 to a ranged request means the bucket answered for the whole
        // object. Writing that body at an offset would corrupt the file in a
        // way the hash would only catch at the end, so it is refused instead.
        let offset = range.map_or(0, |(first, _)| first);
        if offset > 0 && status != reqwest::StatusCode::PARTIAL_CONTENT {
            return Err(Error::Api {
                operation: "download".into(),
                message: format!("{key}: the bucket ignored a range request at byte {offset}"),
            });
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            // Pieces of the same file are written by different workers, so what
            // another one already landed has to stay.
            .truncate(false)
            .open(path)
            .map_err(|source| Error::Io {
                path: path.display().to_string(),
                source,
            })?;
        file.seek(std::io::SeekFrom::Start(offset))
            .map_err(|source| Error::Io {
                path: path.display().to_string(),
                source,
            })?;
        let mut stream = response;
        let written = std::io::copy(&mut stream, &mut file).map_err(|source| {
            let _ = std::fs::remove_file(path);
            Error::Io {
                path: path.display().to_string(),
                source,
            }
        })?;
        Ok(written)
    }
}

fn trim_endpoint(endpoint: &str) -> String {
    endpoint.trim_end_matches('/').to_string()
}

fn trim_body(body: &str) -> String {
    const KEEP: usize = 400;
    let flat = body.replace('\n', " ");
    if flat.len() <= KEEP {
        flat
    } else {
        format!("{}…", &flat[..KEEP])
    }
}

/// GitHub-style `Link: <url>; rel="next"`.
fn link_next(headers: &http::HeaderMap) -> Option<String> {
    for value in headers.get_all(LINK) {
        let Ok(text) = value.to_str() else { continue };
        for part in text.split(',') {
            let mut bits = part.splitn(2, ';');
            let url = bits.next()?.trim();
            let rest = bits.next().unwrap_or("").trim();
            if rest.replace(' ', "").contains("rel=\"next\"") {
                return Some(url.trim_matches('<').trim_end_matches('>').to_string());
            }
        }
    }
    None
}

pub(crate) fn temp_path(path: &Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "object".to_string());
    path.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(".{name}.aixodia-part"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A download is cut in seconds, not in the caller's patience: four
    /// attempts each allowed two minutes is how a 12 MB tree became an
    /// eight-minute restore.
    #[test]
    fn a_download_window_is_bounded_whatever_the_configuration_allows() {
        assert_eq!(download_timeout_secs(120), DOWNLOAD_TIMEOUT_MAX);
        assert_eq!(download_timeout_secs(3600), DOWNLOAD_TIMEOUT_MAX);
        assert_eq!(download_timeout_secs(1), DOWNLOAD_TIMEOUT_SECS);
        assert_eq!(download_timeout_secs(45), 45);
    }

    /// A small file's wait is bounded by its size, not by the allowance; a large
    /// one still gets the time its bytes need.
    #[test]
    fn an_object_waits_for_its_own_bytes_and_no_longer() {
        assert_eq!(object_timeout_secs(48, 90), DOWNLOAD_TIMEOUT_SECS);
        assert_eq!(object_timeout_secs(1_000_000, 90), 45);
        assert_eq!(object_timeout_secs(5_000_000, 90), 90);
        assert_eq!(object_timeout_secs(20_000_000, 3600), DOWNLOAD_TIMEOUT_MAX);
    }
}
