# task.md — Handoff tracker

> **กฎเหล็กจาก operator:** ทุกครั้งที่มีการเปลี่ยนแปลงไฟล์ใดๆ (รวมไฟล์นี้) ต้อง `git add` → `commit` → `push` **ทันที**
> เหตุผล: อาจมี AI ตัวอื่นมารับช่วงต่อหาก session นี้ติดลิมิต — ห้ามมี work ที่ยังไม่ถูก push ค้างอยู่
> **MAINTAIN:** อัปเดตไฟล์นี้ทุก checkpoint ให้บอกสถานะล่าสุด / สิ่งที่ทำไปแล้ว / ขั้นต่อไป

## Handoff (EN — read this first)

**Status:** `PARTIAL DECISION RECEIVED / WORKING ASSUMPTION PENDING CONFIRMATION / NO CODE CHANGED`
**Decisions:** both clients direct; engine issues a short-lived token instead of exposing `CF_TOKEN`. Exact worker scope still unconfirmed — see *Operator decisions*.
**Goal:** Remove the worker middleman so engine and client talk directly.
**Hard constraint:** commit + push after EVERY change; no unpushed local work, ever.
**Repos (all `main`, all public, owner `Tulipskun`):**
`Tulipskun/aixodia-engine`, `Tulipskun/AIxodia`, `Tulipskun/aixodia-dashboard`
**Local clones:** `/workspace/aixodia-work/<repo-name>`
**Auth:** `gh auth status` OK as `Tulipskun` (OAuth `gho_…`, scopes repo/read:org/gist); per-repo git identity set to `Tulipskun <Tulipskun@users.noreply.github.com>`.
**Blocker:** "worker" is ambiguous — see *Decision needed*. Do NOT guess; ask the operator.
**Repo conventions:** `AGENTS.md` requires updating `MAIN.md` in the SAME turn as any non-read/search tool call. `MAIN.md` must stay under 60 lines.

## เป้าหมาย (จาก operator)

ใน `aixodia-engine`, `AIxodia`, `aixodia-dashboard`: เอา **worker** ออก ให้ **engine กับ client คุยกันโดยตรง** และสร้าง `task.md` ลง repo พร้อม **commit + push ทุกครั้ง** เพื่อให้ AI ตัวอื่นสานต่อได้หากติดลิมิต

## ผลสำรวจ (ภาพรวม 3 repo)

| ความหมายของ "worker" | ที่อยู่ | สถานะในเส้นทางจริง |
| --- | --- | --- |
| A. Cloudflare Worker + Durable Object (stale) | `aixodia-engine/relay/` | ไม่ได้ deploy แล้ว (`rust/MILESTONES.md:34-36`) |
| B. Cloudflare Worker ที่เป็นตัวกลางจริง | `aixodia-dashboard/worker/` | **อยู่ในเส้นทาง**: browser → `/api/*` → Worker → engine |
| C. ผลิตภัณฑ์ Cloudflare ที่ไม่ใช่ Worker script | Workers KV `AIXODIA_CONFIG`, D1 `aixodia`, Named Tunnel | พึ่งพาอยู่ทุกเส้นทาง |
| D. OS thread / agent label | backup crates, `agent == "worker"` ใน UI | ไม่ใช่ network ไม่ต้องแตะ |

**ข้อค้นพบที่ขัดสมมติฐาน:** ฝั่ง Android **ไม่ได้ผ่าน Worker อยู่แล้ว** — `AIxodia/README.md:9` (D-011), `PRODUCT.md:25,46` ยืนยันว่า client ค้น URL จาก Workers KV แล้วยิง `https://aixodia.dpdns.org` (Named Tunnel) ไป engine `127.0.0.1:8787` ตรงๆ

## Repo-specific findings — aixodia-engine

- stack: Rust workspace, `axum 0.7` + `tokio`, WebSocket เขียนเอง (`aixodia-wsock`), `cloudflared` Named Tunnel, เรียก Cloudflare REST (D1 / Workers KV)
- entry: `rust/crates/aixodia-engine/src/main.rs:43-49`; launcher `aixodia:8` (`LOCAL_URL=http://127.0.0.1:8787`)
- **server พร้อมอยู่แล้ว**: bind `127.0.0.1:8787` (`main.rs:212-213`), router `gateway.rs:375-453` (`/v1/*`, `/api/*`), WS `/ws`, `/room`, `/terminal`, bearer auth `require_token` (`gateway.rs:908-923`, ข้าม `/healthz`, `/terminal`, `/room`)
- **สิ่งที่ขาดสำหรับ "คุยตรงกับ browser"**: ไม่มี CORS layer เลย (grep `cors|Access-Control|tower_http|allow_origin` ไม่พบ), bind loopback เท่านั้น, และไม่มีกลไก auth ที่ไม่เปิดเผย `CF_TOKEN` ต่อ browser
- `relay/` = Worker ทางเลือก stale: `relay/wrangler.toml:1-4`, `relay/src/index.ts`, `relay/src/room.ts`, `relay/README.md`, `relay/test/helpers.mjs:3`; engine ยังมี `rust/crates/aixodia-relay/` ที่เป็น **engine-side `/room`** (คนละอย่างกับ Cloudflare Worker)
- เอกสาร/CI ที่อ้าง: `README.md:37,50-52,97-108`, `MAIN.md`, `.github/workflows/ci.yml:25-48`

## Decision needed (ห้ามเดา)

| ทางเลือก | ขอบเขต | ผลกระทบ |
| --- | --- | --- |
| 1 | ลบเฉพาะ `aixodia-engine/relay/` | ต่ำมาก — เป็น stale code/docs |
| 2 | ลบ `aixodia-dashboard/worker/` แล้วให้ browser ยิง engine ตรง | สูง — ต้องออกแบบใหม่: SPA hosting, GitHub OAuth/session, CSRF, `CF_TOKEN` ไม่ให้หลุดไป browser, route allowlist, CORS ที่ engine, MCP gateway |
| 3 | ลบทั้งสอง Worker script | รวม 1+2 |
| 4 | ตัด Cloudflare ทั้งระบบ (KV + D1 + Tunnel) | รุนแรงสุด — engine start ไม่ติด, discovery/history/providers หาย, ต้อง rewrite |

ยังต้องระบุด้วยว่า **client** หมายถึง Android อย่างเดียว / dashboard อย่างเดียว / ทั้งคู่

## Operator decisions (received)

- **Client เป้าหมาย:** ทั้งคู่ — Android + Dashboard ต้องชี้ไป engine โดยตรง
- **Auth เมื่อตัด dashboard Worker:** ให้ **engine ออก short-lived token** แทนการเปิดเผย `CF_TOKEN` ต่อ browser (ต้องเพิ่ม endpoint ออก session token ที่จำกัดอายุ/scope ฝั่ง engine)
- **ยังไม่มีคำตอบชัดเจน:** ขอบเขตว่า "worker" คือ `dashboard/worker/` / `engine/relay/` / ทั้งสอง / ตัด Cloudflare ทั้งระบบ
  - **Working assumption (รอยืนยัน):** เริ่มที่ `aixodia-dashboard/worker/` (ตัวกลางจริง) + ลบ stale `aixodia-engine/relay/` เป็น cleanup; **ไม่แตะ** Workers KV / D1 / Named Tunnel
  - ถ้า operator เลือกต่างจากนี้ ให้แก้ assumption นี้ใน 3 repo ก่อน implement ต่อ

## Checkpoint log

| # | สถานะ | สิ่งที่ทำ | Commit |
| --- | --- | --- | --- |
| 1 | ✅ | clone 3 repo, สำรวจสถาปัตยกรรม, สร้าง `task.md` (+ `MAIN.md`), push ทั้ง 3 repo | `engine afd4f94` / `AIxodia ad939f9` / `dashboard e9ffba7` |
| 2 | ✅ | บันทึกคำตอบ operator (client = ทั้งคู่, auth = engine short-lived token) ลง `task.md` + `MAIN.md` | (this commit) |
| 3 | ✅ | operator ปฏิเสธการตอบเพิ่ม → ถือ working assumption (dashboard worker + stale relay, เก็บ KV/D1/Tunnel) | (this commit) |
| 4 | ✅ | engine: CORS + `POST /v1/auth/token` (outstanding: cargo check) + `client_guard` แทน `require_token` ที่ outer mux | (this commit) |
| 5 | 🔄 | `cargo check` ครั้งแรก fail เพราะ **MSRV ของ sandbox** (rustc 1.93 < sysinfo 0.39.6 ที่ต้อง 1.95 / wreq 0.16.3 ที่ต้อง 1.98) ไม่ใช่ error ในโค้ด → กำลังรันใหม่ด้วย `--ignore-rust-version`; `Cargo.lock` +3 บรรทัดจาก reqwest/uuid ถูก commit แล้ว | (this commit) |
| 6 | ⏳ | dashboard: เปลี่ยน SPA เป็น GitHub OAuth PKCE + ยิง engine ตรง แล้วลบ `worker/` | — |
| 7 | ⏳ | AIxodia: อัปเดต docs ให้สอดคล้อง short-lived token (โค้ดยังใช้ CF_TOKEN ตรงอยู่) | — |
| 8 | ✅ | ลบ stale `engine/relay/` (Cloudflare Worker + Durable Object) + แก้ comment stale ใน `aixodia-wsock/src/client.rs`; ไม่มี CI/workspace อ้างถึง | (this commit) |

## Implementation status — engine (checkpoint 4)

- ไฟล์ใหม่ `rust/crates/aixodia-engine/src/client_auth.rs`: `ClientAuth` เก็บ session token แบบ opaque ใน memory (`HashMap` + `RwLock`), ออกผ่าน `POST /v1/auth/token` โดยแลกจาก GitHub OAuth access token (`GET https://api.github.com/user`) แล้วตรวจ allowlist
- `client_guard` แทน `aixodia_io::require_token` เฉพาะ **outer mux**: ยังรับ `CF_TOKEN` เดิม (Android/CLI/in-process), รับ short-lived token เพิ่ม, ตอบ OPTIONS preflight, เติม CORS headers ทุก response; open paths คงเดิม (`/healthz`, `/terminal`, `/room`) + `/v1/auth/token`
- `aixodia_relay::router(relay_cfg)` ภายในยังใช้ `require_token` เดิมกับ CF_TOKEN → ไม่กระทบ `/room`
- **env ใหม่ที่ต้องตั้งบน engine** (fail-closed ถ้าไม่ตั้ง = ไม่มีใครล็อกอินผ่าน browser ได้):
  - `ALLOWED_GITHUB_USERS=Tulipskun` (comma-separated)
  - `ALLOWED_ORIGINS=https://dashboard.aixodia.dpdns.org,http://localhost:5173`
  - `AIXODIA_CLIENT_TOKEN_TTL=900` (optional, 60–86400 วินาที)
- **ข้อจำกัดที่ตั้งใจ**: token อยู่ใน memory → restart engine ทำให้ session หลุด (ยอมรับได้สำหรับ short-lived); engine ยัง bind `127.0.0.1:8787` และเข้าได้ผ่าน Named Tunnel เท่านั้น; `CF_TOKEN` **ไม่เคยถูกส่งให้ browser**
- **build status:** ครั้งแรก `cargo check -p aixodia-engine` หยุดที่ MSRV (`rustc 1.93.0 is not supported … sysinfo@0.39.6 requires rustc 1.95, wreq@0.16.3 requires rustc 1.98`) ก่อน compile โค้ดจริง → รันใหม่ด้วย `cargo check --ignore-rust-version` + `LIBCLANG_PATH=/usr/local/lib/python3.13/dist-packages/clang/native` + `BINDGEN_EXTRA_CLANG_ARGS="-isystem /usr/lib/gcc/x86_64-linux-gnu/13/include"`; เครื่อง deploy จริงใช้ rustc 1.99.1 ตาม MAIN.md

## Next steps สำหรับ AI ที่รับช่วง

1. อ่าน `task.md` ใน **ทั้ง 3 repo** (เนื้อหาเหมือนกันต่างกันเฉพาะ findings)
2. ถาม operator ให้ชัดเรื่อง *Decision needed* ด้านบน ถ้ายังไม่มีคำตอบ
3. ทำทีละขั้นเล็กๆ: แก้ → update `task.md` (checkpoint) → `git add -A` → `commit` → `git push origin main` **ทุกขั้น**; ใน engine/AIxodia ต้อง update `MAIN.md` พร้อมกันตาม `AGENTS.md`
4. ตรวจทดสอบตาม repo: `cargo test --workspace` / `./gradlew :app:testDebugUnitTest` (under `unshare -Cm` ถ้าเจอ jmod cgroup warning) / `npm test && npm run build`
5. อย่า deploy (`wrangler deploy`, `aixodia start`) โดยไม่มีคำสั่งชัดเจน

## Useful commands

```bash
cd /workspace/aixodia-work/<repo>
git status --short --branch
git add -A && git commit -m "task: <step>" && git push origin main
gh repo view Tulipskun/<repo>
```
