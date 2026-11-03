//! The cloudflared quick tunnel, and the heartbeat that guards the daemon keys.
//!
//! Port of the engine's Go `tunnel.go`. The heartbeat at the bottom comes from
//! `main.go`, but it only exists to read the tunnel's liveness, so it lives here
//! beside the thing it asks.
//!
//! `cloudflared` announces a `https://*.trycloudflare.com` URL before the edge
//! accepts it, so the URL is only published once the log line
//! [`REGISTERED_LINE`] has been seen: publishing the earlier one is the bug that
//! left D1 naming a URL no phone could dial.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

/// What cloudflared prints once it has actually joined the edge. The URL is
/// announced earlier and only starts resolving after this line, so this is the
/// moment a URL is safe to publish.
pub const REGISTERED_LINE: &str = "Registered tunnel connection";

/// Release tag baked in at build time; the daemon writes it with the start
/// second so a live service can be identified.
pub const VERSION_TAG: &str = "aixodia-engine/1.1";

/// How long to wait for the tunnel to register before giving up.
const REGISTER_TIMEOUT: Duration = Duration::from_secs(45);

/// Delay before restarting a tunnel that exited on its own.
const RESTART_DELAY: Duration = Duration::from_secs(5);

/// How long a tunnel left by a previous process image gets to leave after
/// SIGTERM before it is killed outright.
const STALE_REAP_WAIT: Duration = Duration::from_millis(1500);

/// The start of every quick-tunnel URL.
const URL_PREFIX: &str = "https://";

/// The suffix every quick-tunnel hostname ends with.
const URL_SUFFIX: &str = ".trycloudflare.com";

/// One running cloudflared quick tunnel.
#[derive(Clone)]
pub struct Tunnel {
    url: String,
    /// The cloudflared process behind this tunnel. The keeper records it so the
    /// next process image can reap it: `execve` keeps the engine's pid but
    /// leaves the children of the image it replaced running, and a crash leaves
    /// them to init — either way the old quick tunnel stays up and keeps
    /// answering on a URL D1 no longer names.
    pid: Option<u32>,
    done: watch::Receiver<bool>,
    kill: watch::Sender<bool>,
}

impl Tunnel {
    /// The public address cloudflared announced, once it registered.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Whether cloudflared is still running. It is the daemon's real health:
    /// the HTTP server can keep serving while the tunnel that makes it
    /// reachable is already gone.
    pub fn alive(&self) -> bool {
        !*self.done.borrow()
    }

    /// Resolves once cloudflared has exited.
    pub async fn done(&self) {
        let mut done = self.done.clone();
        let _ = done.changed().await;
    }

    /// Asks cloudflared to exit.
    pub fn kill(&self) {
        let _ = self.kill.send(true);
    }
}

/// Finds the leftmost `https://*.trycloudflare.com` URL in one log line.
///
/// This is the Go `tunnelURL` regexp (`https://[A-Za-z0-9.-]+\.trycloudflare\.com`)
/// written out by hand: the workspace has no `regex` dependency, and the
/// leftmost match the regexp produces is easy to reproduce directly — take the
/// longest run of URL characters after `https://` that still ends in the
/// trycloudflare suffix.
pub fn find_tunnel_url(line: &str) -> Option<&str> {
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(offset) = line[from..].find(URL_PREFIX) {
        let start = from + offset;
        let after = start + URL_PREFIX.len();
        let mut end = after;
        while end < bytes.len() && is_url_char(bytes[end]) {
            end += 1;
        }
        // The `+` in the regexp needs at least one character before the suffix.
        let min_end = after + 1 + URL_SUFFIX.len();
        let mut stop = end;
        while stop >= min_end {
            if &line[stop - URL_SUFFIX.len()..stop] == URL_SUFFIX {
                return Some(&line[start..stop]);
            }
            stop -= 1;
        }
        from = after;
    }
    None
}

/// Whether one byte may appear in the hostname part of a quick-tunnel URL.
fn is_url_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'.' || byte == b'-'
}

/// Runs cloudflared and waits until the quick tunnel has registered with the
/// edge, so the URL it returns is one that actually resolves. It needs the
/// cloudflared binary on the given path.
pub async fn start_cloudflared(cancel: CancellationToken, path: &Path) -> anyhow::Result<Tunnel> {
    let mut command = tokio::process::Command::new(path);
    command.args([
        "tunnel",
        "--no-autoupdate",
        "--metrics",
        "127.0.0.1:0",
        "--url",
        "http://127.0.0.1:8787",
    ]);
    command.stderr(std::process::Stdio::piped());
    command.kill_on_drop(true);
    let mut child = command.spawn()?;
    let pid = child.id();

    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("cloudflared: no stderr pipe"))?;

    // The URL is announced before the tunnel is reachable; only the
    // registration line means the hostname actually resolves.
    let (ready_tx, ready_rx) = oneshot::channel::<String>();
    tokio::spawn(async move {
        let mut lines = BufReader::with_capacity(64 * 1024, stderr).lines();
        let mut url = String::new();
        let mut ready_tx = Some(ready_tx);
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(found) = find_tunnel_url(&line) {
                url = found.to_string();
            }
            if !url.is_empty() && line.contains(REGISTERED_LINE) {
                // The Go reader sends into a buffered channel with a `default`
                // arm, so only the first registration is published.
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(url.clone());
                }
            }
        }
    });

    let (done_tx, done_rx) = watch::channel(false);
    let (kill_tx, kill_rx) = watch::channel(false);
    let done_task = done_tx.clone();
    tokio::spawn(async move {
        let mut child = child;
        let mut kill_rx = kill_rx;
        let killed = {
            let wait = child.wait();
            let stop = async {
                let _ = kill_rx.changed().await;
            };
            tokio::pin!(wait, stop);
            tokio::select! {
                _ = &mut wait => false,
                _ = &mut stop => true,
            }
        };
        if killed {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
        let _ = done_task.send(true);
    });

    let mut done_wait = done_rx.clone();
    let mut kill = Some(kill_tx);
    let url = tokio::select! {
        biased;
        ready = ready_rx => ready.ok(),
        _ = done_wait.changed() => None,
        _ = tokio::time::sleep(REGISTER_TIMEOUT) => {
            if let Some(tx) = kill.take() {
                let _ = tx.send(true);
            }
            anyhow::bail!("timed out waiting for the trycloudflare URL");
        }
        _ = cancel.cancelled() => {
            if let Some(tx) = kill.take() {
                let _ = tx.send(true);
            }
            anyhow::bail!("cloudflared cancelled");
        }
    };
    let Some(url) = url else {
        if let Some(tx) = kill.take() {
            let _ = tx.send(true);
        }
        anyhow::bail!("cloudflared exited before the tunnel registered");
    };

    Ok(Tunnel {
        url,
        pid,
        done: done_rx,
        kill: kill.expect("kill sender"),
    })
}

/// The name the keeper records its cloudflared pid under, beside the launcher's
/// `aixodia.pid`.
pub const PIDFILE: &str = "cloudflared.pid";

/// Whether `pid` is a running cloudflared quick tunnel. A zombie has an empty
/// command line and a recycled pid names whatever took its place, so both read
/// false. Every argument is matched rather than only the first so that a
/// `#!/bin/sh` stand-in in a test reads as the binary it is named for.
pub fn is_stale_tunnel(pid: u32) -> bool {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    let argv: Vec<String> = raw
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    let names_cloudflared = argv.iter().any(|arg| {
        Path::new(arg)
            .file_name()
            .map(|name| name == "cloudflared")
            .unwrap_or(false)
    });
    names_cloudflared && argv.iter().any(|arg| arg == "tunnel")
}

/// The pid a keeper recorded as its tunnel, when the file holds one.
fn recorded_pid(pidfile: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(pidfile).ok()?;
    raw.trim().parse().ok()
}

fn signal(pid: u32, sig: i32) {
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

/// Ends the cloudflared a previous process image left behind, so neither an
/// `execve` handover nor a crash can pile a second live quick tunnel onto the
/// one D1 names. Returns the pid it ended, or none when there was nothing of
/// ours to end.
pub fn reap_stale_tunnel(pidfile: &Path) -> Option<u32> {
    let pid = recorded_pid(pidfile)?;
    // The engine's own pid survives a handover, so a pidfile that names it — or
    // nothing else that could legitimately be a cloudflared — is never signalled.
    if pid <= 1 || pid == std::process::id() || !is_stale_tunnel(pid) {
        return None;
    }
    signal(pid, libc::SIGTERM);
    let deadline = Instant::now() + STALE_REAP_WAIT;
    while is_stale_tunnel(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    if is_stale_tunnel(pid) {
        signal(pid, libc::SIGKILL);
    }
    // Take the exit so the dead tunnel does not sit as a zombie: after a
    // handover it is a child of this same pid, and nothing else will wait for it.
    unsafe {
        libc::waitpid(pid as libc::pid_t, std::ptr::null_mut(), libc::WNOHANG);
    }
    Some(pid)
}

/// Records the pid of the tunnel just started, for the next process image.
fn record_pid(pidfile: &Path, pid: Option<u32>) {
    let Some(pid) = pid else { return };
    if let Some(dir) = pidfile.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(err) = std::fs::write(pidfile, format!("{pid}\n")) {
        tracing::warn!("tunnel pidfile: {err}");
    }
}

/// Owns the cloudflared quick tunnel. It starts one, hands every (re)start's
/// URL to publish, and starts a fresh one when it exits: the daemon row must
/// never keep naming a URL whose tunnel has died.
pub struct Keeper {
    path: PathBuf,
    publish: Option<Arc<dyn Fn(String) + Send + Sync>>,
    pidfile: PathBuf,
    mu: Mutex<Option<Tunnel>>,
}

impl Keeper {
    /// Builds a keeper for the cloudflared binary at `path`. `publish` receives
    /// the URL of every (re)start, and may be `None`. `pidfile` is where each
    /// start records its cloudflared pid, so the next process image can reap a
    /// tunnel that outlived the one that dialed it.
    pub fn new(
        path: impl Into<PathBuf>,
        publish: Option<Arc<dyn Fn(String) + Send + Sync>>,
        pidfile: impl Into<PathBuf>,
    ) -> Self {
        Self {
            path: path.into(),
            publish,
            pidfile: pidfile.into(),
            mu: Mutex::new(None),
        }
    }

    /// Whether the tunnel is up right now. The heartbeat asks before refreshing
    /// the daemon keys: a dead tunnel is not a live daemon.
    pub fn alive(&self) -> bool {
        self.current().map(|tunnel| tunnel.alive()).unwrap_or(false)
    }

    /// The tunnel this keeper currently has up, empty while it has none.
    pub fn url(&self) -> String {
        self.current()
            .map(|tunnel| tunnel.url().to_string())
            .unwrap_or_default()
    }

    fn current(&self) -> Option<Tunnel> {
        self.mu.lock().unwrap().clone()
    }

    fn set(&self, tunnel: Option<Tunnel>) {
        *self.mu.lock().unwrap() = tunnel;
    }

    /// Keeps a tunnel up for the life of `cancel`.
    pub async fn run(&self, cancel: CancellationToken) {
        if let Some(pid) = reap_stale_tunnel(&self.pidfile) {
            tracing::info!("tunnel: reaped cloudflared {pid} left by the previous build");
        }
        while !cancel.is_cancelled() {
            match start_cloudflared(cancel.clone(), &self.path).await {
                Ok(tunnel) => {
                    record_pid(&self.pidfile, tunnel.pid);
                    let url = tunnel.url().to_string();
                    self.set(Some(tunnel));
                    tracing::info!("tunnel url: {url}");
                    if let Some(publish) = &self.publish {
                        publish(url);
                    }
                    let tunnel = self.current().expect("tunnel was just set");
                    tokio::select! {
                        _ = cancel.cancelled() => {
                            tunnel.kill();
                            return;
                        }
                        _ = tunnel.done() => {}
                    }
                    self.set(None);
                    tracing::warn!("tunnel: cloudflared exited; restarting");
                }
                Err(err) => {
                    if cancel.is_cancelled() {
                        return;
                    }
                    tracing::warn!("tunnel: {err}");
                    if !sleep_ctx(&cancel, RESTART_DELAY).await {
                        return;
                    }
                }
            }
        }
    }
}

/// Waits for `d`, reporting false if `cancel` ended first.
async fn sleep_ctx(cancel: &CancellationToken, d: Duration) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(d) => true,
    }
}

// --- the engine clock ----------------------------------------------------

/// When this process started, in unix seconds. A later start is a newer engine.
/// Zero means "not recorded", so [`started_at`] falls back to now.
static STARTED_AT: AtomicI64 = AtomicI64::new(0);

/// The unix second this process started.
pub fn started_at() -> i64 {
    let started = STARTED_AT.load(Ordering::Relaxed);
    if started != 0 {
        started
    } else {
        now_unix()
    }
}

/// Records the unix second this process started.
pub fn set_started_at(started: i64) {
    STARTED_AT.store(started, Ordering::Relaxed);
}

/// The version column the daemon writes: the tag plus the unix second this
/// process started, which the phone parses for uptime.
pub fn version_string() -> String {
    format!("{VERSION_TAG} started={}", started_at())
}

/// The current unix second.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// --- heartbeat -----------------------------------------------------------

/// The boxed future the heartbeat's injected calls return.
pub type BoxFuture<T> = Pin<Box<dyn std::future::Future<Output = T> + Send>>;

/// The daemon-key calls a heartbeat makes. They are a struct so a test can watch
/// the write without D1.
pub struct HeartbeatDeps {
    /// Whether another engine started after this one.
    pub newer: Box<dyn Fn(i64) -> BoxFuture<anyhow::Result<bool>> + Send + Sync>,
    /// Refreshes `daemon/version` and `daemon/heartbeat`.
    pub touch: Box<dyn Fn(String, i64) -> BoxFuture<anyhow::Result<()>> + Send + Sync>,
    /// The current unix second.
    pub now: Box<dyn Fn() -> BoxFuture<i64> + Send + Sync>,
    /// Pushes the daemon state to the phones after a heartbeat beat lands, so
    /// discovery moves from reading the D1 row to listening for the daemon
    /// instead of polling.
    pub announce: Option<Box<dyn Fn(String, i64) + Send + Sync>>,
}

/// Performs one heartbeat and reports whether this engine should keep running.
/// While the tunnel is down it stops refreshing `daemon/heartbeat`: the app
/// reads a fresh heartbeat as "online", so touching it with a dead tunnel is
/// exactly what makes the phone adopt a URL it cannot dial.
pub async fn heartbeat_once(started: i64, alive: impl Fn() -> bool, deps: HeartbeatDeps) -> bool {
    if !alive() {
        tracing::info!("heartbeat: relay is down; not refreshing the daemon keys");
        return true;
    }
    match (deps.newer)(started).await {
        Ok(true) => {
            tracing::info!("heartbeat: a newer aixodia-engine took over; stopping");
            return false;
        }
        Ok(false) => {}
        Err(err) => tracing::warn!("heartbeat guard: {err}"),
    }
    if let Err(err) = (deps.touch)(version_string(), (deps.now)().await).await {
        tracing::warn!("heartbeat: {err}");
    } else if let Some(announce) = &deps.announce {
        announce(version_string(), (deps.now)().await);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Instant;

    /// Writes an executable stand-in for cloudflared. It logs to stderr, the
    /// stream cloudflared itself logs to, so the tests drive the real process
    /// plumbing without Cloudflare.
    fn fake_cloudflared(script: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("cloudflared");
        std::fs::write(&path, format!("#!/bin/sh\n{script}")).expect("write script");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");
        (dir, path)
    }

    /// Waits for the next URL the keeper publishes, failing the test when none
    /// arrives in time.
    async fn wait_url(rx: &mut tokio::sync::mpsc::Receiver<String>) -> String {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv()).await {
            Ok(Some(url)) => url,
            _ => panic!("no tunnel url was published in time"),
        }
    }

    #[test]
    fn the_url_matcher_takes_the_leftmost_quick_tunnel_address() {
        let line =
            "INF Your quick Tunnel has been created! Visit it at https://live.trycloudflare.com";
        assert_eq!(
            find_tunnel_url(line),
            Some("https://live.trycloudflare.com")
        );
    }

    #[test]
    fn the_url_matcher_ignores_anything_else() {
        assert_eq!(
            find_tunnel_url("INF Registered tunnel connection connIndex=0"),
            None
        );
        assert_eq!(
            find_tunnel_url("INF serving on http://127.0.0.1:8787"),
            None
        );
        assert_eq!(find_tunnel_url("https://example.com"), None);
        assert_eq!(find_tunnel_url("https://.trycloudflare.com"), None);
    }

    #[test]
    fn the_url_matcher_stops_before_a_trailing_path() {
        assert_eq!(
            find_tunnel_url("INF see https://a.trycloudflare.com/path?x=1 now"),
            Some("https://a.trycloudflare.com")
        );
    }

    // A URL is announced before the tunnel is reachable; only the registration
    // line means the hostname actually resolves. Publishing the earlier one is
    // the bug that left D1 naming a URL no phone could dial.
    #[tokio::test]
    async fn start_cloudflared_publishes_only_once_registered() {
        let (_dir, path) = fake_cloudflared(
            "echo 'INF Your quick Tunnel has been created! Visit it at https://live.trycloudflare.com' >&2\n\
             echo 'INF Registered tunnel connection connIndex=0' >&2\n\
             sleep 30\n",
        );
        let cancel = CancellationToken::new();

        let tunnel = start_cloudflared(cancel, &path)
            .await
            .expect("startCloudflared");
        assert_eq!(tunnel.url(), "https://live.trycloudflare.com");
        assert!(tunnel.alive(), "a running cloudflared must read as alive");
    }

    #[tokio::test]
    async fn start_cloudflared_fails_when_the_tunnel_never_registers() {
        let (_dir, path) = fake_cloudflared(
            "echo 'INF Your quick Tunnel has been created! Visit it at https://dead.trycloudflare.com' >&2\n\
             echo 'ERR no more connections active and exiting' >&2\n\
             exit 1\n",
        );
        let cancel = CancellationToken::new();

        assert!(
            start_cloudflared(cancel, &path).await.is_err(),
            "a tunnel that never registered must not yield a URL"
        );
    }

    #[tokio::test]
    async fn tunnel_is_not_alive_after_cloudflared_exits() {
        let (_dir, path) = fake_cloudflared(
            "echo 'INF Your quick Tunnel has been created! Visit it at https://gone.trycloudflare.com' >&2\n\
             echo 'INF Registered tunnel connection connIndex=0' >&2\n\
             sleep 1\n\
             exit 1\n",
        );
        let cancel = CancellationToken::new();

        let tunnel = start_cloudflared(cancel, &path)
            .await
            .expect("startCloudflared");
        let deadline = Instant::now() + Duration::from_secs(5);
        while tunnel.alive() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            !tunnel.alive(),
            "cloudflared has exited; the tunnel must read as not alive"
        );
    }

    // The daemon row names the URL of the live tunnel: when cloudflared exits
    // and a new one comes up with a different address, that address must be
    // republished.
    #[tokio::test]
    async fn keeper_republishes_the_url_when_cloudflared_restarts() {
        let (dir, path) = fake_cloudflared(
            "n=$(cat \"$0.n\" 2>/dev/null || echo 0)\n\
             n=$((n+1))\n\
             echo $n > \"$0.n\"\n\
             echo \"INF Your quick Tunnel has been created! Visit it at https://t$n.trycloudflare.com\" >&2\n\
             echo 'INF Registered tunnel connection connIndex=0' >&2\n\
             if [ \"$n\" -ge 2 ]; then sleep 30; else sleep 1; fi\n",
        );
        let (tx, mut urls) = tokio::sync::mpsc::channel::<String>(8);
        let publish = Arc::new(move |url: String| {
            let _ = tx.try_send(url);
        });
        let cancel = CancellationToken::new();

        let keeper = Arc::new(Keeper::new(path, Some(publish), dir.path().join(PIDFILE)));
        let task = {
            let keeper = Arc::clone(&keeper);
            let cancel = cancel.clone();
            tokio::spawn(async move { keeper.run(cancel).await })
        };

        assert_eq!(wait_url(&mut urls).await, "https://t1.trycloudflare.com");
        assert_eq!(wait_url(&mut urls).await, "https://t2.trycloudflare.com");

        // Every start records its pid, which is what the next image reaps.
        let recorded = std::fs::read_to_string(dir.path().join(PIDFILE))
            .expect("the keeper records its cloudflared pid")
            .trim()
            .to_owned();
        let recorded: u32 = recorded.parse().expect("a numeric pid");
        assert!(
            is_stale_tunnel(recorded),
            "the recorded pid is a live tunnel"
        );

        cancel.cancel();
        task.abort();
    }

    // The defect this closes: `execve` keeps the engine's pid but leaves the
    // children of the image it replaced running, and a crash leaves them to
    // init. Either way every update used to pile one more live quick tunnel onto
    // the address D1 names.
    #[tokio::test]
    async fn a_tunnel_left_by_the_previous_build_is_reaped() {
        let (dir, path) = fake_cloudflared(
            "echo 'INF Your quick Tunnel has been created! Visit it at https://stale.trycloudflare.com' >&2\n\
             echo 'INF Registered tunnel connection connIndex=0' >&2\n\
             sleep 30\n",
        );
        let pidfile = dir.path().join(PIDFILE);
        let cancel = CancellationToken::new();

        let tunnel = start_cloudflared(cancel.clone(), &path)
            .await
            .expect("startCloudflared");
        let pid = tunnel.pid.expect("a spawned cloudflared has a pid");
        assert!(is_stale_tunnel(pid));
        std::fs::write(&pidfile, format!("{pid}\n")).expect("record the pid");

        assert_eq!(reap_stale_tunnel(&pidfile), Some(pid));
        assert!(
            !is_stale_tunnel(pid),
            "the tunnel the previous build left behind must be gone"
        );
        assert_eq!(
            reap_stale_tunnel(&pidfile),
            None,
            "a second reap finds nothing to do"
        );
    }

    #[test]
    fn a_pidfile_naming_anything_else_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join(PIDFILE);

        let mut other = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        std::fs::write(&pidfile, format!("{}\n", other.id())).expect("record the pid");
        assert_eq!(
            reap_stale_tunnel(&pidfile),
            None,
            "a pid recycled onto an unrelated process must never be signalled"
        );
        assert!(
            other.try_wait().expect("try_wait").is_none(),
            "the unrelated process must still be running"
        );
        let _ = other.kill();
        let _ = other.wait();

        std::fs::write(&pidfile, "not a pid\n").expect("write garbage");
        assert_eq!(reap_stale_tunnel(&pidfile), None, "garbage is not a pid");
        assert_eq!(
            reap_stale_tunnel(&dir.path().join("absent.pid")),
            None,
            "a first boot has nothing to reap"
        );
    }

    // The app reads a fresh daemon/heartbeat as "online". While the tunnel is
    // down the heartbeat must not be refreshed, or the phone adopts a URL that
    // is dead.
    #[tokio::test]
    async fn heartbeat_skips_the_write_while_the_tunnel_is_down() {
        let touched = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&touched);
        let deps = HeartbeatDeps {
            newer: Box::new(|_started: i64| Box::pin(async { Ok::<bool, anyhow::Error>(false) })),
            touch: Box::new(move |_version: String, _at: i64| {
                let count = Arc::clone(&count);
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), anyhow::Error>(())
                })
            }),
            now: Box::new(|| Box::pin(async { 1_i64 })),
            announce: None,
        };

        assert!(
            heartbeat_once(100, || false, deps).await,
            "a dead tunnel must not stop the engine"
        );
        assert_eq!(touched.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn heartbeat_refreshes_while_the_tunnel_is_up() {
        let touched = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&touched);
        let deps = HeartbeatDeps {
            newer: Box::new(|_started: i64| Box::pin(async { Ok::<bool, anyhow::Error>(false) })),
            touch: Box::new(move |_version: String, _at: i64| {
                let count = Arc::clone(&count);
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), anyhow::Error>(())
                })
            }),
            now: Box::new(|| Box::pin(async { 1_i64 })),
            announce: None,
        };

        assert!(
            heartbeat_once(100, || true, deps).await,
            "a live tunnel keeps the heartbeat running"
        );
        assert_eq!(touched.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn heartbeat_stops_when_a_newer_engine_took_over() {
        let deps = HeartbeatDeps {
            newer: Box::new(|_started: i64| Box::pin(async { Ok::<bool, anyhow::Error>(true) })),
            touch: Box::new(|_version: String, _at: i64| {
                Box::pin(async { Ok::<(), anyhow::Error>(()) })
            }),
            now: Box::new(|| Box::pin(async { 1_i64 })),
            announce: None,
        };

        assert!(
            !heartbeat_once(100, || true, deps).await,
            "a newer engine must stop this one"
        );
    }
}
