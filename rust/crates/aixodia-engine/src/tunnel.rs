//! Cloudflare Named Tunnel lifecycle. The stable hostname is read from Workers KV;
//! the tunnel token is never taken from process arguments or printed to logs.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

/// What cloudflared prints once it has actually joined the Named Tunnel edge.
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

/// One running Cloudflare Named Tunnel.
#[derive(Clone)]
pub struct Tunnel {
    url: String,
    /// The cloudflared process behind this tunnel. The keeper records it so the
    /// next process image can reap it: `execve` keeps the engine's pid but
    /// leaves the children of the image it replaced running, and a crash leaves
    /// them to init — either way the old Named Tunnel stays up and keeps
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

/// Starts the configured Named Tunnel and waits until cloudflared registers an edge connection.
pub async fn start_cloudflared(
    cancel: CancellationToken,
    path: &Path,
    token: &str,
    url: &str,
) -> anyhow::Result<Tunnel> {
    let token = token.trim();
    if token.is_empty() {
        anyhow::bail!("Workers KV key tunnel_token is empty");
    }
    let url = url.trim().trim_end_matches('/').to_string();
    let parsed = url::Url::parse(&url)
        .map_err(|_| anyhow::anyhow!("Workers KV key tunnel_url is not a valid URL"))?;
    if parsed.scheme() != "https" || parsed.host_str().is_none() {
        anyhow::bail!("Workers KV key tunnel_url must be an https:// URL");
    }

    let mut command = tokio::process::Command::new(path);
    command
        .args(["tunnel", "--no-autoupdate", "--metrics", "127.0.0.1:0", "run"])
        .env("TUNNEL_TOKEN", token)
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn()?;
    let pid = child.id();

    let stderr = child.stderr.take()
        .ok_or_else(|| anyhow::anyhow!("cloudflared: no stderr pipe"))?;
    let (ready_tx, ready_rx) = oneshot::channel::<String>();
    let ready_url = url.clone();
    tokio::spawn(async move {
        let mut lines = BufReader::with_capacity(64 * 1024, stderr).lines();
        let mut ready_tx = Some(ready_tx);
        while let Ok(Some(line)) = lines.next_line().await {
            if line.contains(REGISTERED_LINE) {
                if let Some(tx) = ready_tx.take() {
                    let _ = tx.send(ready_url.clone());
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
            let stop = async { let _ = kill_rx.changed().await; };
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
            if let Some(tx) = kill.take() { let _ = tx.send(true); }
            anyhow::bail!("timed out waiting for the Named Tunnel to register");
        }
        _ = cancel.cancelled() => {
            if let Some(tx) = kill.take() { let _ = tx.send(true); }
            anyhow::bail!("cloudflared cancelled");
        }
    };
    let Some(url) = url else {
        if let Some(tx) = kill.take() { let _ = tx.send(true); }
        anyhow::bail!("cloudflared exited before the Named Tunnel registered");
    };

    Ok(Tunnel {
        url,
        pid,
        done: done_rx,
        kill: kill.expect("kill sender"),
    })
}

/// The name the keeper records its cloudflared child pid under in engine state.
pub const PIDFILE: &str = "cloudflared.pid";

/// Whether `pid` is a running cloudflared tunnel. A zombie has an empty
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
/// `execve` handover nor a crash can pile a second live Named Tunnel onto the
/// one KV names. Returns the pid it ended, or none when there was nothing of
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

/// Owns the cloudflared Named Tunnel. It starts one, hands every (re)start's
/// URL to publish, and starts a fresh one when it exits: the daemon row must
/// never keep naming a URL whose tunnel has died.
pub struct Keeper {
    path: PathBuf,
    token: String,
    url: String,
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
        token: impl Into<String>,
        url: impl Into<String>,
        publish: Option<Arc<dyn Fn(String) + Send + Sync>>,
        pidfile: impl Into<PathBuf>,
    ) -> Self {
        Self {
            path: path.into(),
            token: token.into(),
            url: url.into(),
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
            match start_cloudflared(cancel.clone(), &self.path, &self.token, &self.url).await {
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

/// When this machine started, in unix seconds, read off its own uptime clock.
///
/// The phone shows uptime from this, because the count a reader wants is the box's:
/// a machine that has been up for nine days should say nine days after an engine
/// update restarted the process an hour ago. Where there is no `/proc/uptime` the
/// process start is the best answer left.
pub fn boot_time() -> i64 {
    let uptime = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|raw| {
            raw.split_whitespace()
                .next()
                .and_then(|head| head.parse::<f64>().ok())
        })
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map(|seconds| seconds as i64);
    match uptime {
        Some(seconds) => now_unix() - seconds,
        None => started_at(),
    }
}

/// The version column the daemon writes: the tag, the unix second this process
/// started, and the unix second the machine booted. The phone reads the uptime it
/// shows off the boot time and keeps the process start for telling one engine from
/// the next.
pub fn version_string() -> String {
    format!(
        "{VERSION_TAG} started={} boot={}",
        started_at(),
        boot_time()
    )
}

/// The current unix second.
pub fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
        use std::time::Instant;

    /// The clock the phone counts on is the machine's, not the process's: an
    /// engine update restarts the process and must not restart the answer.
    #[test]
    fn the_boot_time_is_the_machines_not_the_engines() {
        let Ok(raw) = std::fs::read_to_string("/proc/uptime") else {
            assert_eq!(
                boot_time(),
                started_at(),
                "without a kernel clock the process start is the best answer"
            );
            return;
        };
        let uptime = raw
            .split_whitespace()
            .next()
            .and_then(|head| head.parse::<f64>().ok())
            .expect("kernel uptime") as i64;
        let measured = now_unix() - boot_time();
        assert!(
            (measured - uptime).abs() <= 1,
            "engine read {measured}, the kernel says {uptime}"
        );
    }

    #[test]
    fn the_version_column_carries_both_clocks() {
        let version = version_string();
        assert!(version.contains("started="), "{version}");
        let boot = version
            .split("boot=")
            .nth(1)
            .unwrap_or_default()
            .parse::<i64>()
            .expect("a boot time the phone can read");
        assert!(boot > 0 && boot <= now_unix(), "{version}");
        assert!(
            version.find("started=").unwrap() < version.find("boot=").unwrap(),
            "the reader that stops at the first number must still find the process start"
        );
    }

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

        let tunnel = start_cloudflared(cancel, &path, "test-token", "https://aixodia.dpdns.org")
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

        let keeper = Arc::new(Keeper::new(path, "test-token", "https://aixodia.dpdns.org", Some(publish), dir.path().join(PIDFILE)));
        let task = {
            let keeper = Arc::clone(&keeper);
            let cancel = cancel.clone();
            tokio::spawn(async move { keeper.run(cancel).await })
        };

        assert_eq!(wait_url(&mut urls).await, "https://aixodia.dpdns.org");
        assert_eq!(wait_url(&mut urls).await, "https://aixodia.dpdns.org");

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
    // init. Either way every update used to pile one more live Named Tunnel onto
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

        let tunnel = start_cloudflared(cancel.clone(), &path, "test-token", "https://aixodia.dpdns.org")
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

    // The app reads a fresh service heartbeat as "online". While the tunnel is
    // down the heartbeat must not be refreshed, or the phone adopts a dead URL.
    #[tokio::test]
    async fn heartbeat_skips_the_write_while_the_tunnel_is_down() {
        let touched = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&touched);
        let deps = HeartbeatDeps {
            publish: Box::new(move |_version: String, _at: i64| {
                let count = Arc::clone(&count);
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), anyhow::Error>(())
                })
            }),
            now: Box::new(|| Box::pin(async { 1_i64 })),
            announce: None,
        };

        heartbeat_once(|| false, deps).await;
        assert_eq!(touched.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn heartbeat_refreshes_while_the_tunnel_is_up() {
        let touched = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&touched);
        let deps = HeartbeatDeps {
            publish: Box::new(move |_version: String, _at: i64| {
                let count = Arc::clone(&count);
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), anyhow::Error>(())
                })
            }),
            now: Box::new(|| Box::pin(async { 1_i64 })),
            announce: None,
        };

        heartbeat_once(|| true, deps).await;
        assert_eq!(touched.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn heartbeat_publishes_without_discovery_ownership_checks() {
        let touched = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&touched);
        let deps = HeartbeatDeps {
            publish: Box::new(move |_version: String, _at: i64| {
                let count = Arc::clone(&count);
                Box::pin(async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), anyhow::Error>(())
                })
            }),
            now: Box::new(|| Box::pin(async { 1_i64 })),
            announce: None,
        };

        heartbeat_once(|| true, deps).await;
        assert_eq!(touched.load(Ordering::SeqCst), 1);
    }
}
