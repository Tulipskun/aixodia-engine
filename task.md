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
| 3 | ⏳ | ยืนยันขอบเขต worker กับ operator | — |
| 4 | ⏳ | implement ทีละขั้น + commit/push ทุกขั้น | — |

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
