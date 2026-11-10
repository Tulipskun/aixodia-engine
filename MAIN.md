GOAL:
[IN_PROCESS] Remove the worker middleman so engine and client talk directly; track every step in task.md (v1.176)

PLAN:
[COMPLETED] Operator: both clients direct; engine issues short-lived token instead of exposing CF_TOKEN (see task.md).
[IN_PROCESS] Engine CORS + POST /v1/auth/token shipped; dashboard/Android wiring is next; commit + push after EVERY change.

ARCHIVE:
v1.175 Engine runs detached: `aixodia start` daemonizes with setsid, aixodia.pid and a rotated engine.log; no attached mode.
Launcher daemonizes, shipped at v1.175 with no engine bump: `cmd_start` setsids the engine, the child writes `aixodia.pid` before it execs, output appends to `engine.log` (rotated past `AIXODIA_LOG_MAX_KB`, default 8 MB), then `start` waits on healthz; `stop`/`status`/`logs`/`update` read healthz and fall back to the `/proc`-validated pidfile, `run`/`up` alias `start`, and no code path execs the engine in the caller's shell.
v1.175 root-capable file manager API (`/api/files` list/delete/download/upload/mkdir/rename) behind the existing API token.
v1.173 `aixodia start` execs the engine in the caller's foreground; no nohup, forked launcher, engine pidfile or redirected log. A newer D1 discovery owner gates only URL/version/heartbeat writes; it never shuts down the older foreground process. When the newer heartbeat expires, the still-running process can republish its current tunnel URL.
v1.172 `tunnel::boot_time()` (now_unix - /proc/uptime, falling back to `started_at()` where there is no kernel clock) rides the same `daemon/version` column as `started=`; `started=` is written first so the reader that stops at the first number still gets the process start. Live: the attach replay that keeps the phone's page is the client's job — the engine's replay is unchanged and still starts with `RIS`
v1.171 `tmux::capture_history` (capture-pane -p -e -S -2000, tail-trimmed) replays the pane's history at attach; `replay_frame` takes the history instead of dropping the ring; v1.169's `mouse on` reverted to `disable_mouse` at boot — mouse reporting made every drag a copy-mode round trip. Live: an attach answered with 1849 replay lines where the pane held 1851, `show-options -g mouse` reads `off`, and both panes survived the restart
v1.170 `DEFAULT_EXCLUDES` loses `core.*`; `sanitize_excludes` drops it from a stored scope on read, and the live scope really did carry it — 9 registry files (`tokio/…/task/core.rs`, both `miniz_oxide` cores, `serde_json/src/io/core.rs`) were missing because nothing ever backed them up; `cc/src/target/*` was named in the same list and only came back in v1.171's registry sweep (8 files, 2 crates)
v1.169 `tmux::enable_mouse` at boot plus a chained `set -g mouse on` on session start; superseded by v1.171 — the pane entered copy-mode on a wheel, and copy-mode paints on tmux's clock
v1.168 `Config.includes` allowlist + `/api/backup/browse` + `/api/backup/preview`; live: preview 47,930 files = the run's 47,930, narrowed scope 328 = 328
v1.167 auto-backup on CPU idle: poll /proc/stat, track streak, trigger via claim+spawn_job
v1.166 packed shards: 39 zstd containers for the tree; cold restore 47.9s (18x), in-place over /root 34.6s 0 failed, warm backup 5.7s reusing 33/39
v1.165 range-split restore: 32 workers, throttle-aware 429 backoff, byte-level progress reporting
v1.164 backup scope excludes reporting to phone
v1.163 backup/restore finish-and-mean-it: verify packed bytes, live-tree drift handling, manifest v2
v1.162 fail-fast 429 with retry_after; D1 as backup not step
v1.161 invalid JSON arguments fix; tool-block done-ness
v1.160 goals/plans/compaction surfaced to user
v1.159 terminal pid across restart; close kills tmux session
v1.158 self-update stale CDN guard; terminal toggle
v1.157 keyless Opencode default route
v1.156 catalog parse + keyless retry + self-update digest check
v1.155-54 JA3 TLS match, Opencode provider in-tree
v1.153+ shell-card, terminal Backspace, bare aixodia start fix
v1.152 set_directory tool, OSC title in terminal drawer
v1.151 backup: aixodia-backup crate (hub client, pack, scan, ops) + API routes + phone UI
v1.150 tunnel keeper: pidfile recording, reap stale cloudflared on execve handover
v1.149 circuit breaker ignores client errors; session ID validation
v1.148+ provider registry, tool rounds, keyless attempts, idle check, restart endpoint
earlier: gateway port from Go, session store, chat SSE, D1 history, mobile hub, terminal ws

NOTE:
- task.md is the cross-repo handoff tracker: update it and push in the SAME commit as any edit; operator requires zero unpushed work (another AI may take over mid-task).
- Pending operator decision (see task.md): which "worker" to remove. Android already bypasses Workers via Named Tunnel (D-011); the live middleman is aixodia-dashboard/worker/.
- cargo check here needs --ignore-rust-version: rustc 1.93 < sysinfo 0.39.6 (1.95) / wreq 0.16.3 (1.98); the deploy box runs 1.99.1.
- Cooldown (default 30 min) prevents rapid-fire on CPU threshold bouncing.
- rustc 1.99.1 requires cc >= 1.11; a truncated registry src dir (e.g. tokio missing `runtime/task/core.rs`) is repaired by extracting the members the dir lacks from the intact `.crate` in `registry/cache`, keeping the tarball mtimes so cargo's fingerprints stay valid — no `rm -rf`, no rebuild.
- `latest` tag points to main; phone builds from main branch.
- Building the Android client here: this container makes every HotSpot process print a cgroup warning to stderr, and AGP feeds that blob to `jmod --module-version`, which fails with "Version string does not start with a number". Run gradle under `unshare -Cm` (cgroup namespace) — /proc/self/cgroup then reads `/` and the warning is gone. The SDK lives at /opt/android-sdk (platform 35, build-tools 35, platform-tools); `/data` is not mounted here.
- Rust builds here with `LIBCLANG_PATH=/usr/local/lib/python3.13/dist-packages/clang/native` and `BINDGEN_EXTRA_CLANG_ARGS="-isystem /usr/lib/gcc/x86_64-linux-gnu/13/include"` — there is no llvm-18 or clang on this box, and btls-sys' bindgen needs gcc's `stddef.h` to go with the wheel's libclang.
- Deployment: `aixodia start` detaches the engine itself, so no `setsid`/`nohup` wrapper belongs anywhere. `aixodia update` execs a new engine image in place and never touches the launcher script — a launcher change only lands through `install.sh`. `start` on a live engine adopts it (records the pidfile, no restart), so reinstalling while it runs is safe. `CF_TOKEN` must be exported in the shell, never passed as `env CF_TOKEN=…` argv, because argv is readable in `ps`.
- The instabox engine box (`ssh -p 2222 ibssh_…@ssh.instabox.sh`, x86_64, 4 cores / 7 GB): `install.sh` lands `aixodia-engine`, `aixodia` and `cloudflared` in `~/.local/bin`, **not** `/usr/local/bin`, so that dir must be on PATH. Both the launcher and the binary refuse to start without `CF_TOKEN`. Starting a second engine for the same account republishes `daemon/url` in `AIXODIA_CONFIG`, which moves every phone's discovery to this box.
- `aixodia.dpdns.org` is meant to be authoritative at **Cloudflare** with no hardcoded records: `PATCH /domains/{d}/nameservers` sets the registry delegation to `keenan/elsa.ns.cloudflare.com` (GET on that path returns 403, PATCH succeeds) and `POST /dns/switch-external` sets `mode=external` and archives the Hosted DNS zone. The archived zone is still *served* by `dns1/dns2.digitalplat.org` until their nodes purge it, which is why every switch so far left the phone online.
- The DigitalPlat parent cluster (`ns1..ns4.digitalplat.org`) is anycast with inconsistent nodes — repeated queries to one IP return different data, so a single reading is not state; sample a dozen. Cloudflare's own side is solid: `elsa` 8/8 and `keenan` 8/8 return the flattened A of the proxied `CNAME → <tunnel-uuid>.cfargotunnel.com`.
- Apex `CNAME` is rejected by DigitalPlat DNS (`apex_cname_not_allowed`), `<tunnel-uuid>.cfargotunnel.com` has no public A, and records cannot be deleted while `mode=external` (`DELETE /dns/records/{id}` → `409 hosted_dns_zone_read_only`). There is **no rollback left**: the archived Hosted DNS zone was destroyed when the Cloudflare zone was deleted and recreated, so if delegation flips back or the zone is lost again the records must be rebuilt by hand from the tunnel uuid.
- DigitalPlat API base is `https://domain-api.digitalplat.org/api/v1` (`Authorization: Bearer dp_live_…`, writes require `Idempotency-Key`); `dashboard.digitalplat.org` is a different product and sits behind a Cloudflare challenge.
- Self-update looks a release up by tag, and `GET /repos/{repo}/releases/tags/{tag}` does **not** answer for a draft: a bare 404 with no hint of why. The rolling `engine-latest` must stay published. The workflow creates it published, so only a hand-made draft row breaks it — and one that was orphaned by a squashed history stays unlinked even after the tag is pushed again.
- `daemon/version` is the phone's only transport for the daemon's clocks: `D1Api` regexes `started=` and `boot=` out of that one column, so a new clock costs no endpoint, no schema change and no protocol version. Anything added must therefore keep parsing for an older client — which is why `started=` stays the first number in the string.
- `handover.rs` writes `kernel_started: started_at()`, which is not a kernel start. Nothing reads the field, and `tunnel::boot_time()` is the real one; left alone rather than widened into this change.
- MAIN.md policy: current goal + one active plan, archive past as one-liners, operational notes only if relevant to next session. No prose paragraphs for completed work.
- Bucket batch API: `POST /api/buckets/{bucket}/batch` with NDJSON `{"type":"deleteFile","path":…}`; 500 ops per request is fine, and deleting a key that is already gone reports success. List with `GET /api/buckets/{bucket}/tree/{prefix}?recursive=true` — leave `/` unencoded, send a User-Agent, follow the `Link: rel="next"` header (it caps at 1000 entries).
- Restore's remaining cost is container skew, not round trips: one 32 MiB container was 16.1s of a 32s transfer. Grouping by encoded bytes instead of raw is the next win.
