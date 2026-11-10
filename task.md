# task.md — Handoff tracker

> **กฎเหล็กจาก operator:** ทุกครั้งที่แก้ไฟล์ใดๆ (รวมไฟล์นี้) ต้อง `git add` → `commit` → `push` **ทันที** — ไม่มี unpushed work ค้างอยู่
> **ขอบเขต:** ทำงานเฉพาะ repo นี้ (`Tulipskun/aixodia-engine`, branch `main`); ห้ามแตะ repo อื่น; ห้าม deploy
> **จบใน session เดียว:** งานทั้ง 5 ข้อทำเสร็จครบ ≥4 commits ทุก push สำเร็จ

## Status & checkpoints
Status: `COMPLETED — all 5 engine tasks done; every change committed + pushed; tree clean & ahead 0; no pending work, no operator input needed, no deploy.`

| # | สถานะ | สิ่งที่ทำ | Commit |
| --- | --- | --- | --- |
| 1 | ✅ | toolchain fix: `rust-toolchain.toml`, `cargo check --workspace` + `cargo test -p aixodia-engine` ผ่านบน 1.99.1; sync central Agents repository/task.md | df71f1e |
| 2 | ✅ | devices endpoints: `devices.rs` + `DbClient::device_names()` (KV `DEVICE_TUNNELS`), 3 routes รวมเข้า mux ผ่าน `client_guard`; sync central Agents repository/task.md | 9c34413 |
| 3 | ✅ | `master_token` ใน `/v1/auth/token` + env→KV(`AIXODIA_CONFIG`)→default fallback; GitHub fail-closed, master ยังทำงาน; unit tests; sync central Agents repository/task.md | cc025de |
| 4 | ✅ | ผล verify สุดท้าย (`check --workspace`, `test -p aixodia-engine`, `test --workspace`) + sync `central Agents repository`/`task.md` เป็น completed | (this commit) |

## งานทั้ง 5 ข้อ — completed
1. **Build ผ่านจริง** — ติดตั้ง toolchain `1.99.1` (`rustup toolchain install 1.99.1` + `rustup update stable`) และ pin ด้วย `rust-toolchain.toml`; แก้ MSRV (`wreq 0.16.3` ต้อง 1.98 / `sysinfo 0.39.6` ต้อง 1.95) และ `btls-sys 0.5.6` build ผ่านด้วย LLVM-18 libclang/cmake/perl/clang. ไม่ต้อง `cargo update` หรือ `--ignore-rust-version`.
2. **ย้าย device/MCP gateway มา engine** — `GET /v1/devices`, `GET /v1/devices/{id}/tools`, `POST /v1/devices/{id}/action` รวมเข้า outer mux ผ่าน `client_guard`; เรียก upstream ด้วย reqwest + timeout/retry + error ที่อ่านรู้เรื่อง (ไม่ใช่ stub). รายชื่อ device จาก KV `DEVICE_TUNNELS` ผ่าน `DbClient`; ถ้าไม่มีข้อมูล → `{devices: [], note}` ไม่ใช่ 500.
3. **Android แลก token ได้โดยไม่มี GitHub PAT** — `POST /v1/auth/token` รับ `{"master_token":"<CF_TOKEN>"}` นอกเหนือจาก `{"github_token":...}`; ตรวจเทียบ `CF_TOKEN` แล้วออก short-lived token เดียวกัน (ไม่ส่ง CF_TOKEN กลับ).
4. **env fallback ใช้งานได้จริง** — `ALLOWED_GITHUB_USERS` / `ALLOWED_ORIGINS` อ่านจาก Workers KV `AIXODIA_CONFIG` ผ่าน DbClient เมื่อไม่ได้ตั้ง env; `ALLOWED_ORIGINS` มี default `https://dashboard.aixodia.dpdns.org,http://localhost:5173`; `ALLOWED_GITHUB_USERS` fail-closed เฉพาะ github path (master_token ยังทำงาน).
5. **Sync เอกสาร** — `central Agents repository` ตาม AGENTS.md (<60 บรรทัด, GOAL=COMPLETED, no active plan, v1.177 ใน ARCHIVE) และ `task.md` ทุก checkpoint เป็น completed พร้อมผล build และคำอธิบาย endpoints/env ใหม่.

## Endpoints ใหม่ (completed)
| Method | Path | หน้าที่ |
| --- | --- | --- |
| POST | `/v1/auth/token` | body `{"github_token":...}` หรือ `{"master_token":"<CF_TOKEN>"}` → short-lived opaque token (ไม่ส่ง CF_TOKEN กลับ; master path ไม่ต้องมี allowlist) |
| GET | `/v1/devices` | รายชื่อ device จาก KV `DEVICE_TUNNELS` / config + probe status ผ่าน MCP `initialize`; ว่าง → `{devices: [], items: [], note}` ไม่ใช่ 500 |
| GET | `/v1/devices/{id}/tools` | MCP `tools/list` → `{tools:[...]}` |
| POST | `/v1/devices/{id}/action` | body `{tool, arguments}` → MCP `tools/call` (map เป็น `act_and_observe` เมื่อเหมาะ) → `{ok, device, tool, text, screenshot, at}` |

ทุก route รวมเข้า outer mux ผ่าน `client_guard` (รับ short-lived token หรือ CF_TOKEN). Upstream `https://mcp.aixodia.dpdns.org/{device}` ด้วย reqwest jsonrpc 2.0, timeout ต่อ request + retry 1 ครั้ง (transport error / 5xx / 429), error มี `error`/`code` อ่านรู้เรื่อง (`DEVICE_UNAVAILABLE`, `DEVICE_ERROR`, `DEVICE_ACTION_FAILED`, `INVALID_TOOL`, `INVALID_ARGUMENTS`, `INVALID_DEVICE`, `INVALID_REQUEST`, `MASTER_TOKEN_REJECTED`). ไฟล์ใหม่ `src/devices.rs`; รายชื่อ device ผ่าน `aixodia_db::Client::device_names()` (list keys ของ KV namespace `DEVICE_TUNNELS`) + fallback env `AIXODIA_DEVICE_TUNNELS`.

## env (engine)
- `CF_TOKEN` — master token (จำเป็น; ไม่เคยส่งให้ browser)
- `ALLOWED_GITHUB_USERS` — comma-separated; ไม่ตั้ง → KV `AIXODIA_CONFIG` key `client/allowed_users`; ไม่มีจริง → fail-closed เฉพาะ github path (master_token ยังทำงาน)
- `ALLOWED_ORIGINS` — ไม่ตั้ง → KV `client/allowed_origins` → default `https://dashboard.aixodia.dpdns.org,http://localhost:5173`
- `AIXODIA_CLIENT_TOKEN_TTL` — optional 60–86400 วิ (default 900)
- `AIXODIA_DEVICE_TUNNELS` — optional comma-separated device names (fallback เมื่อ KV ว่าง)
- `AIXODIA_MCP_UPSTREAM` — optional base URL (default `https://mcp.aixodia.dpdns.org`)

## Build verification (ผ่านจริง — คำสั่งที่รันจริง)
```
rustup toolchain install 1.99.1 && rustup update stable   # stable = 1.99.1 (>=1.98); rust-toolchain.toml pins it
cd rust
export LIBCLANG_PATH=/usr/lib/llvm-18/lib
export BINDGEN_EXTRA_CLANG_ARGS="-isystem /usr/lib/gcc/x86_64-linux-gnu/13/include"
cargo +1.99.1 check --workspace        # Finished dev profile, exit 0 (เฉพาะ dead_code warnings เดิมใน d1log.rs)
cargo +1.99.1 test -p aixodia-engine     # ok: 20 passed; 0 failed
cargo +1.99.1 test --workspace           # exit 0 ทุก crate
```
`cargo check --workspace` ธรรมดาก็ผ่าน (rust-toolchain.toml + rustup default 1.99.1; clang-sys หา libclang ผ่าน `llvm-config` ได้เอง).

## ไฟล์ที่เปลี่ยน (4 commits)
`rust-toolchain.toml`, `task.md` and the central `Tulipskun/Agents` repository, `rust/Cargo.lock`, `rust/crates/aixodia-db/src/lib.rs`, `rust/crates/aixodia-engine/Cargo.toml`, `src/main.rs`, `src/client_auth.rs`, `src/devices.rs`

## กฎ
- `git -C /workspace/aixodia-work/aixodia-engine add -A && git -C ... commit -m "engine: ..." && git -C ... push origin main` ครบ 4 commits ทุก push สำเร็จ
- สุดท้าย: `git status --short --branch` สะอาด, ahead 0, ไม่มีงานค้าง, ไม่มี deploy

## Push verification (operator ร้องขอ — ให้ push ทั้งหมด + เขียนส่วนที่ push ไม่ได้ลง task)
- วันที่ verify: 2026-11-10
- `git rev-list --left-right --count origin/main...HEAD` → `0 0` (ahead 0, behind 0)
- `git push origin HEAD:main` → `Everything up-to-date`
- สรุป: ไม่มี uncommitted work, ไม่มี unpushed commit; ทุก change ถูก push ครบแล้ว
- **ไม่มีส่วนใด push ไม่ได้** — repo นี้ sync กับ origin/main เรียบร้อย


## Deploy fix session (operator: pull all repos, verify, fix deploys, GitHub client login)
- วันที่: 2026-11-10
- ตรวจพบ: Release/CI failed 5 ครั้งติดกัน — `cargo-fmt is not installed for toolchain 1.99.1` (dtolnay action ติดตั้ง stable แต่ rust-toolchain.toml pin 1.99.1, rustup auto-install ไม่มี components)
- แก้: workflows ติดตั้ง toolchain 1.99.1 + components rustfmt; ลบ dead code (TeeWriter, warn) ที่ clippy -D warnings ติด; bump VERSION v1.176→v1.177 (const ลืม bump ทำให้ release ใหม่รายงานเวอร์ชันเก่า)
- เสร็จสมบูรณ์บน production: release สำเร็จ → selfupdate แทน binary (digest ตรง) → engine restart → KV `client/allowed_users=Tulipskun` มีผล → `POST /v1/auth/token` แลก GitHub token เป็น short-lived token สำเร็จ (ทดสอบด้วย gh auth token จริง; token ไม่ถูกต้องได้ 401 ไม่ใช่ 503)
- ต่อ: operator ต้องการให้ client ล็อกอินผ่าน GitHub โดยไม่เก็บ CF_TOKEN — Android เพิ่ม GitHubDeviceFlow + ClientToken ส่ง github_token แล้ว (2 commits); เหลือ wiring settings/UI ฝั่ง Android
