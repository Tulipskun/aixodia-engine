# Task: screen stream — verify the earlier fixes, fix what remains, add a window manager (aixodia-app + aixodia-engine/crates/aixodia-screen)

Rewritten against the code currently in /root/aixodia-app and /root/aixodia-engine on this device (localhost). Follow each repo's AGENTS.md. Do the steps in order and report after each. Nothing outside these steps.

Original symptom (phone, JPEG mode, 1280x720 @ 15fps): old terminal windows left as ghosts, black blocks, a 16px full-width stripe near y~176, and stutter on zoom/drag/click.

## Already in the code (found by reading it; verify, do not redo)
- Tile placement: `ScreenSurface.tileRect(col, row, cell, width, height)` positions by the grid size; `tile` is in the frame header; TilePlacementTest covers clipped bottom/right tiles.
- Client never loses a patch silently: decoding moved off the OkHttp reader into `drain()`; a backlog skip, a `seq` gap, a failed `trySend` or a failed decode each send `request_keyframe`; the engine answers it with `force_all` in JPEG mode too.
- Input no longer waits on capture: `ScreenHub` has a separate `input` handle; `cursor_in` and `apply` use it; Drag runs in `spawn_blocking`; the screen lock is held only while pixels are read.
- One-finger moves are throttled by MOVE_INTERVAL_MS (except see R4).

## R1. Verify, on this device
Run `cargo test -p aixodia-screen` and the app's unit tests (TilePlacementTest and the ScreenSocket tests). Then measure, with the engine running against a real display: actual fps, ms per grab, whether `shm` is true in `/api/screen/info`, and click-to-effect latency while the capture loop runs. Measure on the display the engine itself starts (display.rs, Xvfb) with real, changing content open on it (a terminal being typed in, a browser scrolling, a window being moved), not an idle display with one xterm: numbers from an idle display say nothing about the ghosting, stutter or fps seen on the phone. Report the numbers; these fixes have never been run against a phone. (An earlier note measured ~250 ms per XGetImage grab on a different machine; do not assume it holds here.)

## R2. Join race (server.rs `connection()`)
It sends `hub.full_frame()` and only then calls `hub.frames.subscribe()`. A diff the loop publishes between the two is never seen by this client, but the loop's baseline has moved on, so that tile stays stale until it changes again. Fix: subscribe first, then send the full frame. Add a test if one is practical.

## R3. Stream freezes after a size change (server.rs)
`update()` sets `*screen = None` when width/height arrive; `publish()` then returns `ok_or("display is not open")`, `run_capture` breaks and logs "screen capture stopped" while viewers remain; the loop only restarts when `viewers` goes 0 -> 1. Fix: reopen the display in `publish()` (or from `update()`), so the stream continues after a Settings change. Also check whether `ScreenSocket.connect()`'s `width`/`height`/`fps` query params go through `update()`; if they do, this bug is reachable from a normal connect. Add a test.

## R4. Unthrottled drag moves (ScreenView.moveOneFinger)
While `held`, `socket.move(wantX, wantY)` is sent for every motion event and returns before the MOVE_INTERVAL_MS check. Apply the same gate there (send the latest position, and always send the final one on release).

## R5. H.264 mode (only matters if the client uses it; verify before changing)
- `VideoSurface.submit` returns silently when `dequeueInputBuffer` is negative; the following P-frames then decode wrong. It should request a keyframe when it drops an access unit.
- Server: `h264_keyframe_due` is true only for the first frame and on request. A client that joins mid-stream gets no keyframe (`connection()` sends nothing in video mode and sets nothing), and there is no periodic keyframe. Make the server send one when a viewer joins and every few seconds.
- First read `ScreenSocket.kt` (~line 394, `if (frame == null) requestKeyframe()`) and VideoSurface to see what already covers this, and report before changing.

## R6. Capture rate (report only)
If R1 shows `shm` false or a slow grab, find out why XShm is refused (X server and engine in different IPC namespaces?) and list options. Do not change how anything is deployed without asking.

## R7. A window manager for the engine-started display
Since commit cb00851 the engine starts a bare Xvfb when `$DISPLAY` points at nothing (display.rs). Nothing starts a window manager, so apps on that display have no title bars and windows cannot be moved, resized or closed. Pick the smallest window manager that installs easily on this machine and have display.rs start it on the engine-started display only, tied to the engine like the Xvfb (pid held, killed on shutdown, not stacked on restart). Do not start one on a display the operator set through `$DISPLAY`. If the window manager is not installed, report it the way a missing Xvfb is reported. Do not touch the xfce session that is running on this machine. Report which one you chose and why, with a test for the start/stop handling in the style of the existing display.rs tests.

## Deliver
Code, passing tests, the R1 numbers (and the same measurements after any change that could affect them), and a short summary per step.
