# task.md — Handoff tracker

> **กฎเหล็กจาก operator:** ทุกครั้งที่แก้ไฟล์ใดๆ (รวมไฟล์นี้) ต้อง `git add` → `commit` → `push` **ทันที** — ไม่มี unpushed work ค้างอยู่
> **ขอบเขต:** ทำงานเฉพาะ repo นี้ (`Tulipskun/aixodia-engine`, branch `main`); ห้ามแตะ repo อื่น; ห้าม deploy
> **จบใน session เดียว:** งานทั้ง 5 ข้อทำเสร็จทีละขั้น (≥4 commits) ไม่มี handoff ไป AI อื่น ไม่มีงานรอ operator

## Status (EN)
`IN PROGRESS (all work stays in this session) — step 1/4 shipped; steps 2–4 follow, each committed + pushed. No operator input needed.`
Every push succeeds; at the end `git status --short --branch` is clean and ahead 0.

## งาน (5 ข้อ, จาก operator)
1. ทำให้ build ผ่านจริง (toolchain 1.99.1, btls-sys, check/test)
2. ย้าย device/MCP gateway มา engine (`/v1/devices` ฯลฯ)
3. `POST /v1/auth/token` รับ `master_token` (Android ไม่ต้องมี GitHub PAT)
4. env fallback จาก Workers KV `AIXODIA_CONFIG` + default ที่ใช้งานได้
5. Sync `MAIN.md` (<60 บรรทัด ตาม AGENTS.md) + `task.md` ทุก checkpoint → completed

## Endpoints ใหม่ (contract)
| Method | Path | หน้าที่ |
| --- | --- | --- |
| POST | `/v1/auth/token` | body `{"github_token":...}` หรือ `{"master_token":"<CF_TOKEN>"}` → short-lived opaque token (ไม่ส่ง CF_TOKEN กลับ) |
| GET | `/v1/devices` | รายชื่อ device จาก KV `DEVICE_TUNNELS` / config + probe status ผ่าน MCP `initialize`; ว่าง → `{devices: [], note}` ไม่ใช่ 500 |
| GET | `/v1/devices/{id}/tools` | MCP `tools/list` → `{tools:[...]}` |
| POST | `/v1/devices/{id}/action` | body `{tool, arguments}` → MCP `tools/call` (map เป็น `act_and_observe` เมื่อเหมาะ) → `{ok, device, tool, text, screenshot, at}` |

ทุก route รวมเข้า outer mux ผ่าน `client_guard` (รับ short-lived token หรือ CF_TOKEN). Upstream `https://mcp.aixodia.dpdns.org/{device}` ด้วย reqwest jsonrpc 2.0, timeout ต่อ request + retry 1 ครั้ง, error มี `error`/`code` อ่านรู้เรื่อง (`DEVICE_UNAVAILABLE`, `DEVICE_ACTION_FAILED`, `DEVICE_REGISTRY_UNAVAILABLE`, `INVALID_TOOL`, `INVALID_ARGUMENTS`, `INVALID_REQUEST`).

## env (engine)
- `CF_TOKEN` — master token (จำเป็น; ไม่เคยส่งให้ browser)
- `ALLOWED_GITHUB_USERS` — comma-separated; ไม่ตั้ง → KV `AIXODIA_CONFIG` key `client/allowed_users`; ไม่มีจริง → fail-closed เฉพาะ github path (master_token ยังทำงาน)
- `ALLOWED_ORIGINS` — ไม่ตั้ง → KV `client/allowed_origins` → default `https://dashboard.aixodia.dpdns.org,http://localhost:5173`
- `AIXODIA_CLIENT_TOKEN_TTL` — optional 60–86400 วิ (default 900)
- `AIXODIA_DEVICE_TUNNELS` — optional comma-separated device names (fallback เมื่อ KV ว่าง)

## Build verification (ผ่านจริง)
```
rustup toolchain install 1.99.1 && rustup update stable   # stable = 1.99.1 (>=1.98); rust-toolchain.toml pins it
cd rust
LIBCLANG_PATH=/usr/lib/llvm-18/lib BINDGEN_EXTRA_CLANG_ARGS="-isystem /usr/lib/gcc/x86_64-linux-gnu/13/include" \
  cargo +1.99.1 check --workspace        # Finished dev profile, exit 0 (เฉพาะ dead_code warnings เดิมใน d1log.rs)
LIBCLANG_PATH=/usr/lib/llvm-18/lib BINDGEN_EXTRA_CLANG_ARGS="-isystem /usr/lib/gcc/x86_64-linux-gnu/13/include" \
  cargo +1.99.1 test -p aixodia-engine     # ok: 16 passed; 0 failed
```
`cargo check --workspace` ธรรมดาก็ผ่าน (rust-toolchain.toml + rustup default 1.99.1; clang-sys หา libclang ผ่าน `llvm-config` ได้เอง). MSRV เดิม `wreq 0.16.3`(1.98) / `sysinfo 0.39.6`(1.95) / `btls-sys 0.5.6` build script — แก้ด้วย toolchain 1.99.1 + LLVM-18 libclang/cmake/perl/clang ที่มีอยู่; ไม่ต้อง `cargo update` หรือ `--ignore-rust-version`.

## Checkpoint log
| # | สถานะ | สิ่งที่ทำ | Commit |
| --- | --- | --- | --- |
| 1 | ✅ | toolchain fix: `rust-toolchain.toml`, `cargo check --workspace` + `cargo test -p aixodia-engine` ผ่านบน 1.99.1; sync MAIN.md/task.md | (this commit) |

## กฎ
- `git -C /workspace/aixodia-work/aixodia-engine add -A && git -C ... commit -m "engine: ..." && git -C ... push origin main` ทุกขั้น (≥4 commits)
- ท้ายสุด: tree สะอาด, ahead 0, ไม่มีงานค้าง, ไม่มี deploy
