# AIxodia — Handoff

เขียนเมื่อจบเซสชันที่ทำงานเรื่อง zero-downtime update, cache ของ config และ terminal scroll
ใช้เป็นจุดเริ่มต้นของแชทใหม่ — อ่านไฟล์นี้ก่อน แล้วค่อยอ่าน `AGENTS.md`

---

## 0. กติกาที่ตกลงกันไว้ (สำคัญ)

- ผู้ใช้สื่อสารภาษาไทย ตอบเป็นไทย
- **ต้องอ่าน `AGENTS.md` (`/root/AGENTS.md` → symlink ไป `aixodia-app/AGENTS.md`) ก่อนแก้โค้ด**
  - §1 โครงสร้างก่อน / อ่านไฟล์เต็ม / ห้าม grep เดาสุ่ม
  - §2 ห้ามเชื่อความจำ ตรวจ ground truth ก่อน
  - §3 ห้ามเพิ่ม dependency เองโดยไม่ได้รับอนุญาต
  - §4 root cause ก่อนแก้, ห้าม fallback/โค้ดตาย, ห้ามปิด error
  - §5 เทสต์เฉพาะจุด, circuit breaker 3 ครั้ง
  - §6 commit → push → deploy/release ทุกครั้งโดยไม่ต้องถาม (ยกเว้นผู้ใช้สั่งข้ามเฉพาะเทิร์นนั้น)
- ห้ามแตะมือถือผ่าน adb ถ้าไม่ได้สั่ง
- ห้ามแตะ tmux pane ของผู้ใช้ / ห้าม kill process ที่ตัวเองไม่ได้เริ่ม
- ห้ามทิ้ง Xvfb ค้าง
- ใช้ `aixodia update` เสมอ ห้าม `stop` + `cp`
- ไม่เพิ่มเทสต์ที่ผู้ใช้ไม่ได้ขอ

---

## 1. สถานะปัจจุบัน (working)

| ส่วน | สถานะ |
|---|---|
| Engine รันบนเครื่องนี้ | ✅ `/usr/local/bin/aixodia-engine` (pid ใน `/root/.aixodia/aixodia.pid`) |
| Zero-downtime update | ✅ swap จริงแล้ว (exec) pid ไม่เปลี่ยน, probe 473×200 มี FAIL 1 |
| Boot หลัง update | 28 วินาที → 7.0 → **0.015 วินาที** (listen → routes) |
| `/healthz` ระหว่าง boot | ตอบได้ใน **0.40 วินาที** หลัง start |
| Terminal (holder, ไม่มี tmux) | ✅ shell อยู่รอดข้าม engine ตาย |
| Terminal scroll (TUI) | ✅ ส่งเป็น wheel/ลูกศร, batching + fling แล้ว (แอป `9580e80`) |
| Terminal scrollback หลัง reattach | ✅ replay 2,000 บรรทัดท้าย (ไม่เกิน 256 KiB) แทนหนึ่งหน้าจอ |
| WM | ✅ `_NET_SUPPORTING_WM_CHECK` + เคารพ `WM_NORMAL_HINTS` |
| Chat / sign-in / screen / undo | ทำงาน (undo ยังไม่เคยทดสอบกับ D1 จริง) |

---

## 2. สถาปัตยกรรม update ที่ทำให้ downtime = 0

### 2.1 ส่งต่อ listening socket ข้าม `execve`
- `crates/aixodia-selfupdate/src/lib.rs`: `keep_listener_across_update(fd)` + `HANDOVER_FD_ENV`,
  `exec_in_place` ล้าง `FD_CLOEXEC` แล้ว `execve`
- `crates/aixodia-engine/src/main.rs`: `inherited_listener()` รับ fd จาก
  `AIXODIA_HANDOVER_LISTENER_FD`, image ใหม่ตั้ง `FD_CLOEXEC` กลับทันที
- port ไม่ถูก unbind เลย → request ระหว่าง swap รอใน accept queue ไม่ถูก refuse

### 2.2 SIGUSR1
- bug เดิม: launcher ทิ้ง `SIGUSR1` ไว้ใน blocked mask → `SigBlk` มี USR1
- แก้: `arm()` ทำ `sigemptyset`/`sigaddset`/`pthread_sigmask(SIG_UNBLOCK, …)` ก่อน `libc::signal`
- handler เป็นการ `write` ลง self-pipe อย่างเดียว (`SIGNAL_WRITE_FD`), `spawn_signal_reader`
  คือ thread ที่ทำงานจริง

### 2.3 รับ port ก่อน แล้วค่อย boot (สำคัญที่สุดของรอบนี้)
`main.rs` ลำดับใหม่:
1. `DbClient::new()` (ไม่ทำ I/O)
2. `let listener = listen();` ← รับ/blind port เป็นอย่างแรก
3. `axum::serve` เริ่มทันทีด้วย **boot router**: `/healthz` ตอบ 200 (`starting: true`),
   path อื่นตอบ 503 "engine is starting"
4. อ่าน config (KV/D1) ต่อ
5. สร้าง `mux` จริง แล้ว `*routes.write().await = Some(mux)` → connection ที่ค้างอยู่ถูกตอบด้วย router จริง
6. `tracing::info!("routes installed")`

`SharedRoutes = Arc<tokio::sync::RwLock<Option<Router>>>` + `tower::ServiceExt::oneshot`
(`tower` เพิ่มใน `crates/aixodia-engine/Cargo.toml` เป็น `{ version = "0.5", features = ["util"] }`
— เป็น dependency ที่ workspace ใช้อยู่แล้ว ไม่ใช่ของใหม่จากภายนอก)

---

## 3. Cache — ต้นเหตุจริงของ boot ช้า

เดิมทุก start อ่าน ~23 round-trip ผ่าน tunnel (D1 API ~6.7 วินาทีต่อครั้ง — วัด `SELECT 1` ได้ 6.74s)

เพิ่มใน `crates/aixodia-db/src/lib.rs`:
- `config_kv_cache` (memory) + `config_kv_fetch` (network) — `config_kv_put` invalidate key ที่เขียน
- `DiskConfigCache` → `~/.aixodia/config-cache.json` (namespace ผูกไว้, เปลี่ยน namespace = ทิ้งทั้งไฟล์)
- `kv_namespaces` — list namespace ครั้งเดียวต่อ process (เดิม `verify_kv` + `verify_config_kv`
  ยิง request เดียวกันซ้ำ)
- `DiskIdentity` → `~/.aixodia/identity.json` เก็บ **fingerprint ของ token (ไม่ใช่ token)**
  + account/db/kv/config_kv ids → `verify_detailed` / `verify_kv` / `verify_config_kv` ไม่ยิง network เลย
- `crates/aixodia-db/src/schema.rs`: `claim()` / `record()` + `~/.aixodia/schema.json`
  ใช้ gate DDL ที่ idempotent: `session-turns` (`aixodia-session`), `objectives` (`aixodia-goals`),
  `runs` (`aixodia-run` `D1Store::ensure`, 5 statements = ~13.5 วินาที)

ทั้งหมดนี้อยู่รอดข้าม `execve` เพราะอยู่บนดิสก์

### 3.1 บั๊กที่เจอระหว่างทาง
`need_update()` เดิมเทียบแค่ `release.updated_at` กับ `selfupdate.state` → ถ้า state ตรง
จะตอบ "already the published build" ทับ binary ที่ไม่ใช่ build ที่ publish
แก้: เทียบ `release.digest` (`sha256:…`) กับ `current_sha256()` ด้วย (`release_matches_running`)

---

## 4. Terminal (holder) — แทน tmux ทั้งหมด

- `crates/aixodia-terminal/src/holder.rs`: process แยก ถือ pty, `setsid`, อยู่ต่อได้แม้ engine ตาย
- socket: `$HOME/.aixodia/terminals/<id>.sock` (0600) + `<id>.pid`
- frame protocol: 1 byte kind (`o`/`i`/`r`/`x`) + 4-byte LE length
- ring 4 MiB, replay ให้ client ที่ attach ใหม่
- `Shell` enum: `Direct(Arc<Pty>)` / `Held(Arc<Holder>)`
- **ข้อควรระวังที่แก้ไปแล้ว**
  - หลัง `exec_in_place` ล้าง `FD_CLOEXEC` ทำให้ child (holder → shell) สืบทอด listening socket
    → แก้ 3 จุด: image ใหม่ตั้ง CLOEXEC กลับ, `close_inheritable_sockets()` ใน `pre_exec`,
    `is_listening_socket()` เช็ค `SO_ACCEPTCONN` ก่อน `from_raw_fd`
  - `pre_exec` ต้องใช้แต่ syscall ขนาดคงที่ (ห้าม allocate/lock/`read_dir`) — เคยพังเป็น
    `holder: no /proc/self/exe available` เฉพาะตอน spawn จาก process ที่มีหลาย thread

#### 4.2 TUI หลัง reattach (รอบล่าสุด — `8bd5997`, `04f0a34`, `d5f1911`)

อาการ: แท็บที่รัน full-screen program (TUI) หลัง disconnected กลับมาเป็นจอว่าง/ค้าง เปิดใหม่ก็ไม่ขึ้น
ต้นเหตุมี 3 อย่าง ทั้งหมดอยู่ที่ crate `aixodia-terminal`:

1. **replay ตอน attach ใช้ tail ของ ring** — ใช้ได้กับ shell ที่พิมพ์เป็นบรรทัด แต่ TUI วาดด้วย
   การย้าย cursor ทำให้ ring ของ session นั้น **ไม่มี newline เลย** → `tail_lines` ตัดไม่ได้
   → ส่งทั้ง 4 MiB (`len: 4194370` วัดได้) ให้มือถือ parse ก่อนจะวาดอะไรได้
   แก้: session ที่ `modes.alt_screen()` → replay = `RIS + modes` เท่านั้น (`replay_reset`)
2. **kick ไม่เคยถูกส่ง** — `Shell::Held::kick()` เป็น `Ok(())` เปล่า ๆ (comment เดิมบอกว่า holder
   ส่ง SIGWINCH เองตอน resize) ทั้งที่ `reattach_kick()` อาศัยมัน
   แก้: เพิ่มเฟรม `KIND_KICK` (`k`) ในโพรโตคอล holder → `pty.kick()` (SIGWINCH เข้า fg pgrp)
   **ยืนยัน:** trap WINCH ในเชลล์ยิงเมื่อมี client attach ใหม่ (TUI repaint จริง)
3. **ปิดแท็บไม่ได้ฆ่า shell** — `KIND_EXIT` ที่ `holder::close` ส่งไปถูก holder มองข้าม (`_ => {}`)
   และ `drop_session` กับ `Shell::Held` แค่ shutdown socket (= detach) → shell + holder ค้างตลอดกาล
   ซ้ำร้าย `serve()` ทำ `frame(replay)?` **ก่อน** อ่านคำสั่ง ถ้า client ปิดไปแล้ว write พัง → return
   ทิ้งคำสั่งนั้นไม่เคยอ่าน (นี่คือเหตุที่เจอ: ไฟล์ socket หายแต่ holder ยังอยู่)
   แก้: เพิ่ม `KIND_END` (`e`) → `pty.close()` + `exit(0)`; `drop_session` เรียก `holder::close(id)`;
   replay ที่เขียนไม่สำเร็จไม่ทำให้เลิกอ่านคำสั่ง
   **ยืนยัน:** `DELETE /api/terminals/<id>` → holder 1 → 0, socket 0

ข้อจำกัดที่ตั้งใจ: SIGWINCH เข้า **foreground pgrp** เท่านั้น (เหมือน tmux) — ถ้าโปรแกรมอยู่ใน
alt screen แต่ fg pgrp ตอนนั้นเป็น child ที่ ignore SIGWINCH ก็จะไม่ repaint

## 4.1 Replay = หน้าจอ + history ที่มีขอบเขต (`history_tail`)
เดิม replay ring ทั้งก้อน (300 KB) ทำให้จอเลื่อนและเห็น prompt เก่าซ้อน + ช่องว่าง
จากนั้นตัดเหลือหนึ่งหน้าจอ (`tail_lines`) ซึ่งแก้อาการจอเพี้ยนได้ แต่ทำให้ scrollback ของ client
ว่างเปล่า — ผู้ใช้ย้อน history ไม่ได้เลยหลัง attach

ตอนนี้ `history_tail(ring, rows)` = `REPLAY_LINES` (2,000) บรรทัดท้าย และไม่เกิน `REPLAY_BYTES`
(256 KiB) โดยตัดที่ขอบบรรทัดเสมอ; `rows` คือขั้นต่ำ (จอใหญ่กว่า 2,000 บรรทัดได้)
การวาดยังลงเอยที่หน้าจอเดิม เพราะบรรทัดที่เกิน viewport ถูกเลื่อนเข้า scrollback ของ client

**ข้อยกเว้นที่ตั้งใจ:** session ที่อยู่ใน alternate screen ยัง replay แค่ `RIS + modes` (ดู §4.2)

---

## 5. แอป Android (`/root/aixodia-app`)

- package `com.tulipskun.aixodia`, `DEFAULT_DAEMON_URL = https://engine.aixodia.dpdns.org`
- terminal เป็น native View: `app/src/main/kotlin/.../term/TerminalView.kt`
- path เลื่อนหน้า: `onTouchEvent` → `DragAxis` → `scrollTopPx` + `OverScroller`
- `pansLocalHistory()` = ไม่มี mouse reporting && ไม่ใช่ alternate screen && scrollback ไม่ว่าง
- **งานรอบนี้ (`9580e80`)**
  - `dispatchWheelAt`: รวม notch ของหนึ่ง frame เป็น WebSocket **message เดียว**
    (เดิม 1 notch = 1 message = 1 pty write → โปรแกรมตื่นและ repaint ต่อ notch)
  - step = `cellHeight` (1 แถวต่อ notch, เดิม 2 แถว)
  - `releaseWheelFling()` + `computeScroll()`: fling ต่อหลังปล่อยนิ้วด้วย scroller เดียวกับหน้า history
  - `wheelBytes()` แทน `sendWheel()`: mouse mode → wheel report; alt screen → `ESC[A`/`ESC[B`;
    prompt เปล่าไม่มี history → ไม่ส่งอะไร
- โครงสร้าง input: `TerminalPanel.kt` ต่อ `inputSink = { socket.send(it) }` →
  `TerminalSocket.TabLink.send` → OkHttp `WebSocket.send`
- **เทสต์:** `ANDROID_HOME=/opt/android-sdk ./gradlew :app:testDebugUnitTest`
  - Paparazzi รันในเครื่องนี้ไม่ได้ (`UninitializedPropertyAccessException: sessionParamsBuilder`)
    — **เป็นปัญหาเดิมก่อนการแก้ของเรา** ยืนยันด้วย `git stash` แล้วรันซ้ำ; CI เป็นคนรัน

---

## 6. ตัวเลขที่วัดได้ (หลักฐาน)

```
boot ก่อนแก้                 ~28 s  (listen → routes installed)
boot หลัง cache              ~7.0 s
boot หลัง dd3a12b            ~0.015 s
healthz ตอบครั้งแรก           0.40 s หลัง start
exec → listening             ~9.9 s (รวม download/rename ของ asset 27 MB)
listening → routes           0.015 s (เดิม 6.7 s)
interrupt_stale หลัง routes  6.5 s  (ไม่บล็อกการตอบแล้ว)
update จริง (swap + exec)    473 × 200, FAIL 1 ครั้ง, pid เดิม
D1 round trip (SELECT 1)      6.74 s   ← เพดานที่เหลืออยู่
```

---

## 7. งานค้าง / ข้อเสนอ

1. ~~**boot ยัง ~7 วินาที** เพราะ `Manager::start` → `interrupt_stale()` เป็น UPDATE จริง (D1 ~6.5s)~~
   **แก้แล้ว `dd3a12b`** — `Manager::start` เหลือแค่ `ensure()` (schema gate = อ่านไฟล์),
   `interrupt_stale` spawn หลัง `routes installed`
   - ต้อง scope ด้วย `created_at < started_at` เท่านั้น: เทิร์นที่ถูกรับระหว่าง UPDATE
     กำลังบินอยู่บน tunnel (insert ใช้ ~6.7s เท่ากัน) จะถูก mark ทับเป็น interrupted
   - `MemoryStore`/`D1Store` รับ `created_before` แล้ว, มีเทสต์คุมทั้งสองด้าน
2. ~~**scrollback หลัง attach ว่าง**~~ **แก้แล้ว `446d18a`** — replay เป็น history ที่มีขอบเขต
   (2,000 บรรทัด / 256 KiB) บรรทัดที่เกิน viewport เลื่อนเข้า scrollback ของ client
3. **TUI scroll latency** — ลดแล้วด้วย batching + fling แต่ยังมี RTT ของ tunnel เป็นพื้น
   (เทียบกับ screen stream ที่ได้ feedback ต่อเนื่อง) ถ้ายังช้า พิจารณา map flick แรง ๆ เป็น PgUp/PgDn
   — ยังไม่ได้ทำ เพราะต้องทดสอบบนเครื่องที่มี Android SDK
4. **ยังไม่ได้ทดสอบ**: undo end-to-end กับ D1 จริง (`POST /api/sessions/:id/undo`)
   — และยังไม่มีใครทดสอบ TUI reattach **จากมือถือจริง** (ยืนยันฝั่ง engine ด้วย client ทดสอบแทน)
5. ~~**counter `Backups::owed` ค้าง**~~ **แก้แล้ว `a187786`** — decrement ทำใน `Drop` guard
   job ที่ panic จึงไม่ทำให้ counter ค้าง (ตัว counter ไม่มีใครใช้ตัดสิน update แล้ว — §2 comment ใน
   `main.rs`; shutdown ก็มี timeout 5 วินาที)
6. **ยังไม่ได้แก้**: AgentRouter `gpt-6.1-sol` ตอบ `content-blocked` HTTP 400 บาง prompt
   — ต้องมี body ของ 400 จริงถึงจะรู้ต้นเหตุ (log เครื่องนี้ไม่มีเคสนี้)
7. ~~**minimal WM ไม่ได้ตั้ง `_NET_SUPPORTING_WM_CHECK`, ไม่สน `WM_NORMAL_HINTS`**~~ **แก้แล้ว `50248dd`**
8. **ยังไม่ได้แก้**: ความกว้าง Thai cluster ใน terminal (เลือกไม่แก้โดยเจตนา)
9. **แอปอาจต้อง sign-in ใหม่** — ห้ามทำบนเครื่องผู้ใช้ ให้ผู้ใช้ทำเอง

---

## 8. กับดักที่เจอ (อ่านก่อนทำงาน)

- `pkill -f <pattern>` ฆ่า shell ของตัวเองได้ (pattern ไปแมตช์ command line ของ bash)
  → ใช้ `pgrep -af … | grep -v "bash -c"` แล้ว `kill <pid>`
- `cp` ทับ binary ที่กำลังรัน → `Text file busy` ต้อง `aixodia stop` → `rm -f` → `cp` → `aixodia start`
  (หรือใช้ `aixodia update`)
- asset `aixodia-engine` (ชื่อเปล่า) เป็น **amd64** — เครื่องนี้ aarch64 ต้องใช้
  `aixodia-engine-linux-arm64` ไม่งั้น `Exec format error`
- `aixodia update` จะไม่ swap ถ้า digest ตรงกับ binary ที่รันอยู่ — ถ้าต้องทดสอบ exec จริง
  ต้องมี build ใหม่จริง (หรือ binary บนดิสก์ต่างจาก release)
- หลัง download asset ต้อง `chmod +x`
- engine ต้องมี `CF_TOKEN` — launcher อ่านจาก `/root/.aixodia/cf.env` (mode 600, ห้ามพิมพ์ค่าออกมา)
- `/proc` เป็น `hidepid=2` แต่ engine รันเป็น uid 0 อ่าน `/proc/<pid>/exe` ได้

---

## 9. คำสั่งที่ใช้บ่อย

เครื่องใหม่ (Debian trixie x86_64) ต้องติดตั้งก่อน build — rustup จะลง toolchain 1.99.1 ตาม
`rust-toolchain.toml` เอง:

```bash
rustup component add rustfmt clippy
apt-get install -y cmake clang libclang-dev \
    libx11-dev libxext-dev libxtst-dev      # btls-sys (boringssl ผ่าน wreq) และ x11 crate
```

```bash
# engine
cd /root/aixodia-engine
export PATH=$PATH:/root/.cargo/bin
cargo fmt --all
cargo clippy -p aixodia-engine --bin aixodia-engine --no-deps -- -D warnings
cargo test -p aixodia-engine            # เทสต์เฉพาะจุดก่อน
cargo test --workspace -j 2             # ยืนยันครั้งสุดท้าย (363 เทสต์; -j สูง ๆ ทำ disk เต็มตอน link)
cargo build --release -p aixodia-engine

# ปล่อย release (CI+Release ทำงานเองเมื่อ push)
git add -A && git commit -F - <<'EOF'
...
EOF
git push origin HEAD

# ดูสถานะ
aixodia update
curl -s http://127.0.0.1:8787/healthz
cat /root/.aixodia/aixodia.pid
sed 's/\x1b\[[0-9;]*m//g' /root/.aixodia/engine.log | tail -30

# แอป
cd /root/aixodia-app
ANDROID_HOME=/opt/android-sdk ./gradlew :app:testDebugUnitTest --offline
git push origin HEAD     # workflow "Release APK" รันเทสต์ + release.sh เอง
```

งานที่เพิ่งทำเสร็จยืนยันด้วย log แบบนี้:
```
selfupdate: installing the build published=…
listening on 127.0.0.1:8787 (inherited from the previous image)
routes installed
```

---

## 10. ข้อมูลระบบ

- เครื่อง: RMX3371 (Android, chroot Debian 13 trixie, aarch64), ~7 cores / 7.5 GB RAM / 4 GB swap, **ไม่มี IPv6**
- Engine port `127.0.0.1:8787`, tunnel ผ่าน cloudflared → `https://engine.aixodia.dpdns.org`
- state dir: `/root/.aixodia/`
  - `aixodia.pid`, `engine.log`, `selfupdate.state`, `cf.env` (600)
  - `config-cache.json`, `identity.json`, `schema.json` ← cache ที่เพิ่งเพิ่ม
  - `terminals/<id>.sock` + `<id>.pid`
- Cloudflare: account `3b953d1cbb784b3424c27798931bc37b`,
  D1 `aixodia` = `e8e746ea-691e-43bf-9fe2-c4e4016f29a1`,
  provider KV `aixodia-providers` = `3db70d16438043debb4f9dc00ea4106c` (key `providers`),
  config KV `AIXODIA_CONFIG` = `b9159aba166f4f5c974250310f4459f7`,
  zone `aixodia.dpdns.org` = `0df5ea46806f151750ec01be67bf365b`
- repos: `Tulipskun/aixodia-engine` (tag `engine-latest`), `Tulipskun/aixodia-app`
- adb `127.0.0.1:5555` — ห้ามใช้ถ้าไม่ได้รับสั่ง

---

## 11. Commit ล่าสุดของเซสชันนี้

engine:
```
a187786  Release the owed count when a write ends, however it ends
50248dd  Be the manager EWMH looks for, and obey the size a client asked for
446d18a  Hand a reattaching client its history, not just the screen it is on
```

engine (เซสชันก่อนหน้า):
d5f1911  Read the order a client sent even when its replay cannot be written
04f0a34  End the shell when a tab is closed, not just the engine's side of it
8bd5997  Let a full-screen program paint the screen a reattaching client is missing
dd3a12b  Mark last start's runs once the engine answers, not before
c8aa0cb  Compare the bytes, not only the timestamp, before saying "already installed"
feaf6cd  Say why the schema version is recorded, not just that it is
2a769ee  Skip the run-table DDL a previous image already applied
0713ec5  Stop reading configuration the engine already has
c6cd3c7  Answer on the inherited socket while the engine is still starting
2ca33af  Listen before verifying the token, not after
a01a601  Take the port before anything that talks to the network
849aa3e  Repaint one screenful, and stop letting a hung write block an update
6b0582b  holder: no /proc/self/exe available
e0ac4c1  fd leak: holder/shell สืบทอด listening socket
d6de467  terminal: holder แทน tmux
```

app:
```
9580e80  terminal: send a frame's wheel notches as one write, and keep flicking
75ba171  chat: undo
f4c21b3  chat: draft clears on send
```
