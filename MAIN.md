GOAL:
[COMPLETED] Stop an exclude rule from eating source (v1.170): `core.*` read like a core-dump rule and globed onto every `core.rs` in the tree, so no manifest ever named them and no restore could bring them back.

PLAN:
(no active plan)

ARCHIVE:
v1.170 `DEFAULT_EXCLUDES` loses `core.*`; `sanitize_excludes` drops it from a stored scope on read, and the live scope really did carry it — 9 registry files (`tokio/…/task/core.rs`, both `miniz_oxide` cores, `serde_json/src/io/core.rs`, `cc/src/target/*`) were missing because nothing ever backed them up
v1.169 `tmux::enable_mouse` at boot plus a chained `set -g mouse on` on session start; live probe: three wheel notches put the pane in copy-mode and showed lines 183-185 of 200
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
- An exclusion is a data-loss rule in disguise. A restore only ever adds and overwrites, so a file the scope hides does not come back as an error at restore time — it comes back later as a build that cannot compile.
- A backup's minutes are spent reading the tree, not on the wire: 53.4 s idle for 4.6 GB was 37.9 s packing, 5.5 s uploading 517 MB (~94 MB/s), 6.8 s walking, 3.4 s publishing.
- Backup slot is single: auto-backup cannot start during a manual run.
- Cooldown (default 30 min) prevents rapid-fire on CPU threshold bouncing.
- rustc 1.99.1 requires cc >= 1.11; a truncated registry src dir (e.g. tokio missing `runtime/task/core.rs`) is repaired by extracting the members the dir lacks from the intact `.crate` in `registry/cache`, keeping the tarball mtimes so cargo's fingerprints stay valid — no `rm -rf`, no rebuild.
- `latest` tag points to main; phone builds from main branch.
- Building the Android client here: this container makes every HotSpot process print a cgroup warning to stderr, and AGP feeds that blob to `jmod --module-version`, which fails with "Version string does not start with a number". Run gradle under `unshare -Cm` (cgroup namespace) — /proc/self/cgroup then reads `/` and the warning is gone. The SDK lives at /opt/android-sdk (platform 35, build-tools 35, platform-tools); `/data` is not mounted here.
- Rust builds here with `LIBCLANG_PATH=/usr/local/lib/python3.13/dist-packages/clang/native` and `BINDGEN_EXTRA_CLANG_ARGS="-isystem /usr/lib/gcc/x86_64-linux-gnu/13/include"` — there is no llvm-18 or clang on this box, and btls-sys' bindgen needs gcc's `stddef.h` to go with the wheel's libclang.
- Deploying here: `aixodia-engine update` installs the CI-published build; nothing supervises the daemon and `POST /restart` only writes `~/.exit`, which no process on this box reads — restart is `kill <pid>` then relaunch from `/root/.aixodia` with stdout appended to `aixodia.log`. The tmux server is reparented to init, so a restart keeps every pane's history.
- MAIN.md policy: current goal + one active plan, archive past as one-liners, operational notes only if relevant to next session. No prose paragraphs for completed work.
- Bucket batch API: `POST /api/buckets/{bucket}/batch` with NDJSON `{"type":"deleteFile","path":…}`; 500 ops per request is fine, and deleting a key that is already gone reports success. List with `GET /api/buckets/{bucket}/tree/{prefix}?recursive=true` — leave `/` unencoded, send a User-Agent, follow the `Link: rel="next"` header (it caps at 1000 entries).
- Restore's remaining cost is container skew, not round trips: one 32 MiB container was 16.1s of a 32s transfer. Grouping by encoded bytes instead of raw is the next win.
