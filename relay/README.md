# aixodia-relay

Worker + Durable Object ที่จับคู่ engine กับมือถือ (อย่างละตัว) แล้วส่งต่อ frame ระหว่างกัน — ไม่มี tunnel, URL นิ่ง

- `GET /room/<name>` (WebSocket upgrade, `Authorization: Bearer <RELAY_TOKEN>`) — engine 1 ตัว + phone 1 ตัวต่อ room
- frame แรก: `{"role":"engine","started":<unix>,"version":"..."}` หรือ `{"role":"phone"}`
- หลังจากนั้น relay ส่งต่อ frame โปร่งทั้งสองทาง (text/binary) — ไม่ parse เนื้อ
- `{"ctl":"superseded"}` = engine ใหม่กว่าแย่งที่; `{"ctl":"superseded-phone"}` = มือถือเครื่องใหม่แย่งที่; `{"ctl":"presence","engine":bool}` = สถานะ engine
- free plan: SQLite-backed DO + Hibernation API

## ดีพลอย

```bash
export CLOUDFLARE_API_TOKEN=...   # deploy token (Workers Scripts Write)
cd relay && npx wrangler deploy
printf '<shared-token>' | npx wrangler secret put RELAY_TOKEN
```

URL: `https://aixodia-relay.aixodia.workers.dev/room/<room>`

## ทดสอบ

**ต้อง deploy ก่อน** — `workerd` รันในเครื่อง dev ที่ใช้พัฒนาไม่ได้ (aarch64 + PRoot: TCMalloc ขอ mmap alignment 1 GiB แล้วล้ม) จึงไม่มี `wrangler dev` และไม่มี vitest workers-pool; เทสต์คือ Node client ยิงเข้า Worker จริง

```bash
cd relay && npm install
export CLOUDFLARE_API_TOKEN=...
npx wrangler deploy
RELAY_TOKEN=<shared-token> npm test
```

## ไฟล์

- `src/index.ts` — Worker: `/healthz`, auth, route `/room/<name>`
- `src/room.ts` — Durable Object: slot engine/phone, handover, presence, forward
- `test/` — Node test runner + `ws` client

## ฝั่ง engine และแอพ

engine ต่อออกมาที่ห้องเดียวและถือ socket ไว้ตลอด; แอพถืออีกฝั่งของห้องเดียวกัน
ทุกอย่างจึงผ่าน WS เส้นเดียว ไม่มี inbound port และไม่มี cloudflared

| ตัวแปร | ค่าเริ่มต้น | ความหมาย |
|---|---|---|
| `AIXODIA_RELAY` | `wss://aixodia-relay.aixodia.workers.dev` | base ของ Worker |
| `AIXODIA_ROOM` | `aixodia` | ชื่อห้อง (Durable Object) |
| `AIXODIA_RELAY_TOKEN` | ค่าของ `CF_TOKEN` | bearer ที่ engine ส่งให้ Worker |

Worker ต้องตั้ง `RELAY_TOKEN` ให้ตรงกับค่านี้ ถ้าใช้ค่าเริ่มต้น (CF token)
ก็ `printf '<CF_TOKEN>' | npx wrangler secret put RELAY_TOKEN` ครั้งเดียว

ตอน rotate token ให้ใส่หลายค่า คั่นด้วย comma (`เก่า,ใหม่`) ได้ Worker จะรับ
ทั้งคู่จนกว่าทั้ง engine และแอพจะย้ายเสร็จ แล้วค่อยเหลือค่าใหม่ค่าเดียว

engine เขียน `daemon/url` ใน D1 เป็น `https://<worker>/room/<room>` ตอนเชื่อมต่อได้
และหยุดเขียน heartbeat ตอนหลุด แอพจึงรู้ว่า online/offline เหมือนเดิม
