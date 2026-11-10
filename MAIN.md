GOAL:
[IN_PROCESS] Migrate the device/MCP gateway, master_token exchange and KV env fallback into the engine; workspace build green on rustc 1.99.1 (v1.177)

PLAN:
[COMPLETED] toolchain: `rustup toolchain install 1.99.1` + `rustup update stable`; `rust-toolchain.toml` pins 1.99.1; `cargo check --workspace` and `cargo test -p aixodia-engine` pass.
[COMPLETED] /v1/devices, /v1/devices/{id}/tools, /v1/devices/{id}/action (reqwest -> mcp.aixodia.dpdns.org/{device}, timeout+retry); device names via `DbClient::device_names()` over KV DEVICE_TUNNELS.
[IN_PROCESS] 3) master_token + ALLOWED_* KV fallback. 4) MAIN.md/task.md sync to completed. commit+push after EVERY change.

ARCHIVE:
v1.176 CORS + `POST /v1/auth/token` short-lived client token + `client_guard`; removed stale relay/; both clients talk to the engine directly.
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
- task.md is the handoff tracker: update + push in the SAME commit as any edit; operator requires zero unpushed work.
- Build (this sandbox): `cargo check --workspace` / `cargo test -p aixodia-engine` pass on 1.99.1. btls-sys builds against LLVM-18 libclang found via `llvm-config`; optional belt-and-suspenders env: `LIBCLANG_PATH=/usr/lib/llvm-18/lib BINDGEN_EXTRA_CLANG_ARGS="-isystem /usr/lib/gcc/x86_64-linux-gnu/13/include"`.
- `--ignore-rust-version` no longer needed: wreq 0.16.3 (MSRV 1.98) and sysinfo 0.39.6 (1.95) compile on 1.99.1; deploy box runs 1.99.1 too.
- engine binds 127.0.0.1:8787 behind Named Tunnel aixodia.dpdns.org; CF_TOKEN must be exported, never passed as argv.
- `client_guard` accepts CF_TOKEN or a short-lived token from `POST /v1/auth/token`; CF_TOKEN is never sent to browsers.
- Cooldown (default 30 min) prevents rapid-fire on CPU threshold bouncing.
- `latest` tag points to main; phone builds from main branch.
- Self-update looks a release up by published tag; a draft 404s with no hint, so `engine-latest` must stay published.
- Deployment: `aixodia start` detaches the engine itself; `start` on a live engine adopts it. instabox box installs to ~/.local/bin (must be on PATH); a second engine republishes `daemon/url` and moves every phone's discovery.
- Do not deploy (`wrangler deploy`, `aixodia start`) without an explicit order.
