//! Self-update and in-place restart.
//!
//! This is a direct port of the engine's Go `selfupdate` package. It lets a
//! running engine replace its own binary and restart without ending the host
//! session:
//!
//! * [`latest`] compares the published release asset against the running
//!   executable and reports whether they differ.
//! * [`install`] downloads the published asset, verifies it by sha256 against
//!   the running copy, and swaps it into place. `aixodia update` uses this and
//!   lets the launcher decide when the engine restarts.
//! * [`apply`] installs and, when the binary changed, replaces this process
//!   with it: same pid, same arguments, same environment.
//! * [`notify_update`] makes `kill -USR1 <pid>` mean "install the published build
//!   and continue as it"; the signal is the whole handshake, so no inbound port
//!   has to be this engine's.
//! * [`run_loop`] checks the published build every ten minutes, once the engine
//!   has had two minutes to finish starting.
//!
//! On Linux the running bytes are always read from `/proc/self/exe`, which
//! stays bound to the inode that was loaded even after `install.sh` renames a
//! new binary over the path. [`running_image`] deliberately does not use
//! `std::env::current_exe`: it would report the path of the *new* binary and
//! [`install`] would then conclude the published build is already running.

use std::ffi::CString;
use std::fmt;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::Duration;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

/// The published release this engine tracks.
pub const RELEASE_URL: &str =
    "https://api.github.com/repos/Tulipskun/aixodia-engine/releases/tags/engine-latest";

/// The binary that is swapped in place.
pub const TARGET_PATH: &str = "/usr/local/bin/aixodia-engine";

/// `User-Agent` sent to GitHub and to the asset host.
pub const USER_AGENT: &str = "aixodia-engine/1.0";

/// How long `run_loop` waits before its first check.
pub const START_GRACE: Duration = Duration::from_secs(2 * 60);

/// How often `run_loop` checks the published build.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Errors from the self-update path.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("github release: {0}")]
    ReleaseStatus(String),
    #[error("asset {0} not found")]
    AssetNotFound(String),
    #[error("download: {0}")]
    DownloadStatus(String),
    #[error("cannot locate the running executable")]
    NoRunningImage,
    #[error("{operation} failed: {message}")]
    Io { operation: String, message: String },
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// The one release asset the engine publishes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Release {
    pub url: String,
    pub updated_at: String,
    pub size: i64,
    /// GitHub's `sha256:…` for the asset, empty when the release predates it.
    pub digest: String,
}

impl fmt::Display for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (published {})", self.url, self.updated_at)
    }
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    #[serde(default)]
    name: String,
    #[serde(default, rename = "browser_download_url")]
    url: String,
    #[serde(default, rename = "updated_at")]
    updated_at: String,
    #[serde(default)]
    size: i64,
    #[serde(default)]
    digest: String,
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    #[serde(default)]
    assets: Vec<GhAsset>,
}

/// The asset name this host downloads, chosen from the architecture Go's
/// `runtime.GOARCH` reports.
pub fn asset_name() -> String {
    match std::env::consts::ARCH {
        "aarch64" => "aixodia-engine-linux-arm64",
        _ => "aixodia-engine-linux-amd64",
    }
    .to_string()
}

/// Finds the published release asset for this host.
pub async fn latest() -> Result<Release> {
    let response = reqwest::Client::new()
        .get(RELEASE_URL)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await?;

    let status = response.status();
    if status != reqwest::StatusCode::OK {
        return Err(Error::ReleaseStatus(status.to_string()));
    }
    let release: GhRelease = response.json().await?;

    let name = asset_name();
    release
        .assets
        .into_iter()
        .find(|asset| asset.name == name)
        .map(|asset| Release {
            url: asset.url,
            updated_at: asset.updated_at,
            size: asset.size,
            digest: asset.digest,
        })
        .ok_or_else(|| Error::AssetNotFound(name))
}

/// The hex sha256 of the bytes at `path`.
pub fn sha256_file(path: &str) -> Result<String> {
    let file = fs::File::open(path).map_err(|e| Error::Io {
        operation: format!("open {path}"),
        message: e.to_string(),
    })?;
    let mut reader = io::BufReader::new(file);
    let mut hasher = Sha256::new();
    io::copy(&mut reader, &mut hasher).map_err(|e| Error::Io {
        operation: format!("hash {path}"),
        message: e.to_string(),
    })?;
    Ok(hex::encode(hasher.finalize()))
}

/// The path to the bytes this process is actually executing. On Linux that is
/// `/proc/self/exe`; elsewhere the executable the process started from.
pub fn running_image() -> String {
    if cfg!(target_os = "linux") {
        return "/proc/self/exe".to_string();
    }
    std::env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(str::to_string))
        .unwrap_or_default()
}

/// The sha256 of the running image, or an error when it cannot be located.
pub fn current_sha256() -> Result<String> {
    let image = running_image();
    if image.is_empty() {
        return Err(Error::NoRunningImage);
    }
    sha256_file(&image)
}

/// Where the last applied release is remembered.
pub fn state_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".aixodia/selfupdate.state")
}

/// The `updated_at` of the release this engine last applied, empty when none.
pub fn read_state() -> String {
    fs::read_to_string(state_path())
        .map(|raw| raw.trim().to_string())
        .unwrap_or_default()
}

/// Records the release that is now installed, so the next check does not
/// download it again.
fn write_state(value: &str) {
    let path = state_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(&path, format!("{value}\n"));
}

/// Whether the published asset differs from the running copy. The asset's
/// `updated_at` is the change signal; [`install`] confirms by sha256 of the
/// downloaded bytes.
pub async fn need_update() -> Result<(bool, Release)> {
    let release = latest().await?;
    if release.updated_at.is_empty() || release.updated_at == read_state() {
        return Ok((false, release));
    }
    Ok((true, release))
}

/// The two halves of an update, behind one lock so a test can swap them and
/// exercise the signal path without the network or an exec of the test binary
/// over itself.
type NeedFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(bool, Release)>> + Send>>;
type ApplyFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>;
type NeedFn = Arc<dyn Fn() -> NeedFuture + Send + Sync>;
type ApplyFn = Arc<dyn Fn(Release, String) -> ApplyFuture + Send + Sync>;

static NEED_FN: RwLock<Option<NeedFn>> = RwLock::new(None);
static APPLY_FN: RwLock<Option<ApplyFn>> = RwLock::new(None);

fn default_need() -> NeedFn {
    Arc::new(|| Box::pin(async { need_update().await }))
}

fn default_apply() -> ApplyFn {
    Arc::new(|release, target| Box::pin(async move { apply(release, &target).await }))
}

fn halves() -> (NeedFn, ApplyFn) {
    let need = NEED_FN.read().unwrap().clone().unwrap_or_else(default_need);
    let apply = APPLY_FN
        .read()
        .unwrap()
        .clone()
        .unwrap_or_else(default_apply);
    (need, apply)
}

/// Overrides the network and exec halves of an update. Only tests use this.
#[cfg(test)]
fn with_halves<R>(f: impl FnOnce(&mut Option<NeedFn>, &mut Option<ApplyFn>) -> R) -> R {
    let mut need = NEED_FN.write().unwrap();
    let mut apply = APPLY_FN.write().unwrap();
    f(&mut need, &mut apply)
}

/// The published size and digest are part of the asset metadata, and the
/// download URL is served through a CDN that can still hand back the previous
/// build after a clobber. A payload that does not match them is stale, not a
/// build to run: swapping it in would install old code and record the new
/// release as applied, so nothing would ever retry it.
fn verify_payload(release: &Release, payload: &[u8]) -> Result<()> {
    if release.size > 0 && payload.len() as i64 != release.size {
        return Err(Error::DownloadStatus(format!(
            "asset is {} bytes but the release lists {} — the download served a stale build",
            payload.len(),
            release.size
        )));
    }
    let Some(expected) = release.digest.strip_prefix("sha256:") else {
        return Ok(());
    };
    let got = hex::encode(Sha256::digest(payload));
    if !expected.eq_ignore_ascii_case(&got) {
        return Err(Error::DownloadStatus(format!(
            "asset hashes to {got} but the release lists {expected} — the download served a stale build"
        )));
    }
    Ok(())
}

/// Downloads `release` into `target` and swaps it into place, unless the
/// published bytes are the running executable's. Returns whether `target` now
/// holds a different binary.
pub async fn install(release: &Release, target: &str) -> Result<bool> {
    let response = reqwest::Client::new()
        .get(&release.url)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await?;

    let status = response.status();
    if status != reqwest::StatusCode::OK {
        return Err(Error::DownloadStatus(status.to_string()));
    }

    let payload = response.bytes().await?.to_vec();
    verify_payload(release, &payload)?;
    let got = hex::encode(Sha256::digest(&payload));

    let current = current_sha256().unwrap_or_default();
    if !current.is_empty() && got == current {
        // The published build is already running; remember its timestamp so
        // the next check does not download it again. Without this the engine
        // re-downloads the whole asset every tick and `aixodia update` waits
        // for an answer that never comes.
        write_state(&release.updated_at);
        return Ok(false);
    }

    let tmp = format!("{target}.new");
    write_executable(&tmp, &payload).map_err(|e| Error::Io {
        operation: format!("write {tmp}"),
        message: e.to_string(),
    })?;
    if let Err(e) = fs::rename(&tmp, target) {
        let _ = fs::remove_file(&tmp);
        return Err(Error::Io {
            operation: format!("rename {tmp}"),
            message: e.to_string(),
        });
    }
    write_state(&release.updated_at);
    Ok(true)
}

/// Creates `path` with 0755 and writes `payload` into it.
pub(crate) fn write_executable(path: &str, payload: &[u8]) -> io::Result<()> {
    fs::write(path, payload)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

/// Installs `release` and, when the binary changed, replaces this process with
/// it: same pid, same arguments, same environment.
///
/// This does not return on success; the new binary takes over with this pid.
pub async fn apply(release: Release, target: &str) -> Result<()> {
    if !install(&release, target).await? {
        return Ok(());
    }
    exec_in_place(target)
}

/// The path [`apply`] swaps: the installed binary when it exists, otherwise
/// the executable this process started from.
pub fn target_path() -> String {
    if fs::metadata(TARGET_PATH)
        .map(|meta| meta.is_file())
        .unwrap_or(false)
    {
        return TARGET_PATH.to_string();
    }
    std::env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(str::to_string))
        .unwrap_or_else(|| TARGET_PATH.to_string())
}

/// Replaces this process with the binary at `target`. Same pid, same arguments,
/// same environment.
#[cfg(unix)]
pub fn exec_in_place(target: &str) -> Result<()> {
    use std::os::unix::ffi::OsStringExt;

    let args: Vec<CString> = std::env::args_os()
        .map(|arg| CString::new(arg.into_vec()).unwrap_or_default())
        .collect();
    let env: Vec<CString> = std::env::vars_os()
        .map(|(key, value)| {
            let mut raw = key.into_vec();
            raw.push(b'=');
            raw.extend_from_slice(&value.into_vec());
            CString::new(raw).unwrap_or_default()
        })
        .collect();
    let target = CString::new(target).map_err(|e| Error::Io {
        operation: "exec".to_string(),
        message: e.to_string(),
    })?;
    let arg_ptrs: Vec<*const libc::c_char> = args
        .iter()
        .map(|arg| arg.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    let env_ptrs: Vec<*const libc::c_char> = env
        .iter()
        .map(|var| var.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();
    unsafe {
        libc::execve(target.as_ptr(), arg_ptrs.as_ptr(), env_ptrs.as_ptr());
    }
    // Only reached when execve fails.
    Err(Error::Io {
        operation: "exec".to_string(),
        message: io::Error::last_os_error().to_string(),
    })
}

#[cfg(not(unix))]
pub fn exec_in_place(_target: &str) -> Result<()> {
    Err(Error::Io {
        operation: "exec".to_string(),
        message: "in-place exec is only supported on unix".to_string(),
    })
}

/// The signal a launcher sends to ask a running engine to update in place.
#[cfg(unix)]
pub const UPDATE_SIGNAL: i32 = libc::SIGUSR1;

#[cfg(not(unix))]
pub const UPDATE_SIGNAL: i32 = 0;

/// Makes `kill -USR1 <pid>` mean "install the published build and continue as
/// it". `idle` reports whether a chat is in flight; when it is, the signal only
/// notes the request and the next [`run_loop`] tick applies the same build as
/// soon as the chat ends.
///
/// The returned guard stops the handler when dropped.
#[cfg(unix)]
pub fn notify_update<F>(idle: F) -> UpdateSignalGuard
where
    F: Fn() -> bool + Send + Sync + 'static,
{
    let idle = Arc::new(idle);
    let handler = Arc::new(move || {
        tracing::info!("selfupdate: update requested on signal");
        if !idle() {
            tracing::info!("selfupdate: a chat is in flight; applying when it ends");
            return;
        }
        let (need, apply) = halves();
        let release = match block_on(need()) {
            Ok((true, release)) => release,
            Ok((false, _)) => {
                tracing::info!("selfupdate: already the published build");
                return;
            }
            Err(err) => {
                tracing::warn!(%err, "selfupdate:");
                return;
            }
        };
        tracing::info!(published = %release.updated_at, "selfupdate: installing the build");
        // On success this does not return: the new binary takes over with
        // this pid, so the session the caller holds survives.
        let target = target_path();
        if let Err(err) = block_on(apply(release, target)) {
            tracing::warn!(%err, "selfupdate:");
        } else {
            // `apply` only returns when the published bytes already match the
            // running image; a real swap execs and never comes back. Say so,
            // because the launcher reads this line to end `aixodia update`
            // without waiting for its timeout.
            tracing::info!("selfupdate: already the published build");
        }
    });
    UpdateSignalGuard::arm(handler)
}

#[cfg(not(unix))]
pub fn notify_update<F>(_idle: F) -> UpdateSignalGuard
where
    F: Fn() -> bool + Send + Sync + 'static,
{
    UpdateSignalGuard::arm()
}

/// Stops the [`notify_update`] handler when dropped.
pub struct UpdateSignalGuard {
    #[cfg(unix)]
    handler: Arc<dyn Fn() + Send + Sync>,
}

impl UpdateSignalGuard {
    #[cfg(unix)]
    fn arm(handler: Arc<dyn Fn() + Send + Sync>) -> Self {
        SIGNAL_HANDLER.get_or_init(|| Mutex::new(Vec::new()));
        unsafe {
            libc::signal(
                UPDATE_SIGNAL,
                handle_signal as *const () as libc::sighandler_t,
            );
        }
        SIGNAL_HANDLER
            .get()
            .unwrap()
            .lock()
            .unwrap()
            .push(handler.clone());
        Self { handler }
    }

    #[cfg(not(unix))]
    fn arm() -> Self {
        Self {}
    }

    /// Delivers the update signal to this process, the way
    /// `kill -USR1 <pid>` does.
    #[cfg(unix)]
    pub fn raise() -> io::Result<()> {
        let result = unsafe { libc::kill(libc::getpid(), UPDATE_SIGNAL) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(not(unix))]
    pub fn raise() -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "no signal on this platform",
        ))
    }
}

/// The handlers armed for `SIGUSR1`. A process may arm more than one in tests,
/// so they are a list rather than a single slot.
#[cfg(unix)]
type SignalHandlers = Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>;

#[cfg(unix)]
static SIGNAL_HANDLER: OnceLock<SignalHandlers> = OnceLock::new();

#[cfg(unix)]
impl Drop for UpdateSignalGuard {
    fn drop(&mut self) {
        // Keep the Arc alive across the reset so a signal delivered right at
        // this moment cannot drop the closure mid-call.
        let _ = &self.handler;
        unsafe {
            libc::signal(UPDATE_SIGNAL, libc::SIG_DFL);
        }
        if let Some(list) = SIGNAL_HANDLER.get() {
            list.lock().unwrap().pop();
        }
    }
}

#[cfg(unix)]
extern "C" fn handle_signal(_signal: libc::c_int) {
    // Only async-signal-safe work happens here: take a clone of the handler
    // and run it on a fresh thread, so the handler may do I/O and allocate.
    let Some(list) = SIGNAL_HANDLER.get() else {
        return;
    };
    let Ok(list) = list.lock() else {
        return;
    };
    let Some(handler) = list.last().cloned() else {
        return;
    };
    let _ = std::thread::spawn(move || handler()).join();
}

/// Runs a future to completion on a private single-threaded runtime. Used only
/// from the signal handler, which cannot await.
#[cfg(unix)]
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("signal handler runtime")
        .block_on(future)
}

/// Checks the published build every ten minutes, once the engine has had two
/// minutes to finish starting, and installs it when it changed and no chat is
/// in flight. A check that fails repeats every tick, so it is logged when the
/// error changes, not on every attempt.
pub async fn run_loop<F>(cancel: CancellationToken, idle: F)
where
    F: Fn() -> bool,
{
    let mut last_err = String::new();

    tokio::select! {
        _ = cancel.cancelled() => return,
        _ = tokio::time::sleep(START_GRACE) => {}
    }
    loop {
        if idle() {
            match need_update().await {
                Ok((true, release)) => {
                    tracing::info!(published = %release.updated_at, "selfupdate: installing the build");
                    // On success this does not return: the new binary takes over.
                    match apply(release, &target_path()).await {
                        Ok(()) => note(&mut last_err, None),
                        Err(err) => note(&mut last_err, Some(&err)),
                    }
                }
                Ok((false, _)) => note(&mut last_err, None),
                Err(err) => note(&mut last_err, Some(&err)),
            }
        }
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep(CHECK_INTERVAL) => {}
        }
    }
}

/// Logs a failed check when the message changes, not on every attempt.
fn note(last_err: &mut String, err: Option<&Error>) {
    match err {
        None => {
            if !last_err.is_empty() {
                tracing::info!("selfupdate: the release is reachable again");
                last_err.clear();
            }
        }
        Some(err) => {
            let message = err.to_string();
            if message != *last_err {
                tracing::warn!(%message, "selfupdate:");
                *last_err = message;
            }
        }
    }
}

// --- tests ------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use axum::routing::get;
    use axum::Router;
    use std::os::unix::fs::PermissionsExt as _;

    /// The self-update tests share the process-wide HOME and SIGUSR1 handler,
    /// so they must run one at a time: a guard dropped by one test would leave
    /// the next test's signal on the default action, which kills the process.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|err| err.into_inner())
    }

    /// The published bytes land in the target, and the asset's `updated_at` is
    /// what stops the next check from downloading them again.
    #[tokio::test]
    async fn install_writes_the_published_build() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());

        let payload = std::sync::Arc::new(b"#!/bin/sh\necho new\n".to_vec());
        let app = Router::new().route(
            "/engine",
            get({
                let payload = payload.clone();
                move || {
                    let payload = payload.clone();
                    async move {
                        (
                            [(reqwest::header::CONTENT_LENGTH, payload.len().to_string())],
                            (*payload).clone(),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let target = dir.path().join("aixodia-engine");
        write_executable(target.to_str().unwrap(), b"#!/bin/sh\necho old\n").unwrap();

        let rel = Release {
            url: format!("http://{addr}/engine"),
            updated_at: "2026-10-21T00:00:00Z".to_string(),
            size: 0,
            digest: String::new(),
        };
        let changed = install(&rel, target.to_str().unwrap())
            .await
            .expect("install");
        assert!(changed, "a different binary should report a change");

        assert_eq!(fs::read(&target).unwrap(), *payload);

        let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);

        assert_eq!(read_state(), rel.updated_at);
    }

    /// The outage this pins: after a clobbered asset upload the download URL
    /// still served the previous build, whose bytes matched the running image,
    /// so install() reported "already running" and recorded the new release as
    /// applied. Wrong-length payloads must fail instead.
    #[tokio::test]
    async fn install_rejects_a_stale_payload_size() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());

        let payload = b"#!/bin/sh\necho new\n".to_vec();
        let served = payload.clone();
        let app = Router::new().route(
            "/engine",
            get(move || {
                let served = served.clone();
                async move { served }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let target = dir.path().join("aixodia-engine");
        let old = b"#!/bin/sh\necho old\n";
        fs::write(&target, old).unwrap();

        let rel = Release {
            url: format!("http://{addr}/engine"),
            updated_at: "2026-10-21T00:00:00Z".to_string(),
            size: payload.len() as i64 + 12_288,
            digest: String::new(),
        };
        let result = install(&rel, target.to_str().unwrap()).await;

        assert!(result.is_err(), "a stale build must not install");
        assert_eq!(fs::read(&target).unwrap(), old, "the binary was replaced");
        assert!(
            read_state().is_empty(),
            "a stale download must not be recorded as applied"
        );
    }

    /// Consecutive builds can pad to the same length, so the size check alone
    /// cannot tell them apart; the published digest is what pins the bytes.
    #[tokio::test]
    async fn install_rejects_a_stale_payload_with_the_published_size() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());

        let payload = b"#!/bin/sh\necho new\n".to_vec();
        let served = payload.clone();
        let app = Router::new().route(
            "/engine",
            get(move || {
                let served = served.clone();
                async move { served }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let target = dir.path().join("aixodia-engine");
        let old = b"#!/bin/sh\necho old\n";
        fs::write(&target, old).unwrap();

        let rel = Release {
            url: format!("http://{addr}/engine"),
            updated_at: "2026-10-21T00:00:00Z".to_string(),
            size: payload.len() as i64,
            digest: format!("sha256:{}", "0".repeat(64)),
        };
        let result = install(&rel, target.to_str().unwrap()).await;

        assert!(result.is_err(), "a stale build must not install");
        assert_eq!(fs::read(&target).unwrap(), old, "the binary was replaced");
        assert!(
            read_state().is_empty(),
            "a stale download must not be recorded as applied"
        );
    }

    /// A download that fails leaves the running binary alone.
    #[tokio::test]
    async fn install_keeps_the_binary_when_the_download_fails() {
        let _serial = serial();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());

        let app = Router::new().route(
            "/engine",
            get(|| async { axum::http::StatusCode::NOT_FOUND }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let target = dir.path().join("aixodia-engine");
        let old = b"#!/bin/sh\necho old\n";
        fs::write(&target, old).unwrap();

        let result = install(
            &Release {
                url: format!("http://{addr}/engine"),
                ..Release::default()
            },
            target.to_str().unwrap(),
        )
        .await;
        assert!(result.is_err(), "a 404 should fail the install");

        assert_eq!(fs::read(&target).unwrap(), old);
        assert!(
            !dir.path().join("aixodia-engine.new").exists(),
            "a partial download was left behind"
        );
    }

    /// The signal is what `aixodia update` sends instead of stopping the daemon:
    /// it must reach `apply` with the published release.
    #[test]
    fn update_signal_applies_the_published_build() {
        let _serial = serial();
        let rel = Release {
            url: "http://example.invalid/engine".to_string(),
            updated_at: "2026-10-21T00:00:00Z".to_string(),
            size: 0,
            digest: String::new(),
        };
        let applied = Arc::new(Mutex::new(None::<String>));
        let seen = applied.clone();
        let need_rel = rel.clone();
        let apply_rel = rel.clone();

        with_halves(move |need, apply| {
            *need = Some(Arc::new(move || {
                let rel = need_rel.clone();
                Box::pin(async move { Ok((true, rel)) })
            }));
            *apply = Some(Arc::new(move |_release, _target| {
                let seen = seen.clone();
                let stamp = apply_rel.updated_at.clone();
                Box::pin(async move {
                    *seen.lock().unwrap() = Some(stamp);
                    Ok(())
                })
            }));
        });

        let _guard = notify_update(|| true);
        UpdateSignalGuard::raise().unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while applied.lock().unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the update signal did not reach apply"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            applied.lock().unwrap().as_deref(),
            Some(rel.updated_at.as_str())
        );

        with_halves(|need, apply| {
            *need = None;
            *apply = None;
        });
    }

    /// A chat in flight must not be cut short by a deploy: the request is noted
    /// and left for the next tick, so nothing is installed while the turn runs.
    #[test]
    fn update_signal_waits_for_an_idle_engine() {
        let _serial = serial();
        let applied = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = applied.clone();

        with_halves(|need, apply| {
            *need = Some(Arc::new(|| {
                Box::pin(async {
                    Ok((
                        true,
                        Release {
                            updated_at: "2026-10-21T00:00:00Z".to_string(),
                            ..Release::default()
                        },
                    ))
                })
            }));
            *apply = Some(Arc::new(move |_release, _target| {
                let seen = seen.clone();
                Box::pin(async move {
                    seen.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
            }));
        });

        let _guard = notify_update(|| false);
        UpdateSignalGuard::raise().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !applied.load(std::sync::atomic::Ordering::SeqCst),
            "a busy engine must not apply an update"
        );

        with_halves(|need, apply| {
            *need = None;
            *apply = None;
        });
    }

    /// Nothing published means nothing to do: the signal must not fall through
    /// to `apply`, which would swap a binary for the one already running.
    #[test]
    fn update_signal_does_nothing_when_the_build_matches() {
        let _serial = serial();
        let called = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = called.clone();

        with_halves(|need, apply| {
            *need = Some(Arc::new(|| {
                Box::pin(async { Ok((false, Release::default())) })
            }));
            *apply = Some(Arc::new(move |_release, _target| {
                let seen = seen.clone();
                Box::pin(async move {
                    seen.store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(())
                })
            }));
        });

        let _guard = notify_update(|| true);
        UpdateSignalGuard::raise().unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "apply ran for a build that was already in place"
        );

        with_halves(|need, apply| {
            *need = None;
            *apply = None;
        });
    }

    // The bug this pins: install.sh renames a new binary over the running one,
    // so the path this process was started from now holds *different* bytes.
    // Hashing `current_exe` would then hash the new file, conclude the
    // published build is already running, and never exec.
    #[test]
    fn running_image_is_the_linux_proc_self_exe() {
        let _serial = serial();
        if !cfg!(target_os = "linux") {
            // The replaced-binary case is Linux-specific (/proc/self/exe).
            return;
        }
        assert_eq!(running_image(), "/proc/self/exe");
        assert!(!current_sha256()
            .expect("the running image hashes")
            .is_empty());
    }
}
