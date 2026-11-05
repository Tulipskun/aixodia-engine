# AIxodia

The AIxodia engine: serves the chat API on `127.0.0.1:8787`, puts a Cloudflare
quick tunnel in front of it, and tells the phone the tunnel URL via D1.

## Remote filesystem API

The engine exposes a root-level Linux file manager over the existing authenticated
API. It operates with the operating-system permissions of the engine process;
when the engine runs as root, these endpoints can access the whole host filesystem.
Treat the engine token as a root password: anyone holding it can read, replace or
delete system files.

All requests use `Authorization: Bearer $CF_TOKEN` and absolute Linux paths.

| Method | Endpoint | Purpose |
|---|---|---|
| GET | `/api/files?path=/` | List a directory and file metadata |
| GET | `/api/files/download?path=/path/file` | Stream a file to the client |
| POST | `/api/files/upload?path=/target/dir&name=file.bin` | Stream a file into a directory |
| POST | `/api/files/upload?path=/target/dir&name=file.bin&overwrite=true` | Replace an existing file |
| POST | `/api/files/mkdir` | Create a directory; JSON body: `{"path":"/target/new-dir"}` |
| POST | `/api/files/rename` | Move or rename; JSON body: `{"from":"/old","to":"/new"}` |
| PUT | `/api/files/permissions` | Change Unix mode; JSON body: `{"path":"/file","mode":493}` (493 = 0755) |
| DELETE | `/api/files?path=/target` | Delete a file or empty directory |
| DELETE | `/api/files?path=/target&recursive=true` | Recursively delete a directory |

Uploads and downloads stream data instead of buffering complete files in memory.
Uploads are written to a temporary file and published only after the transfer
finishes. Existing files are not overwritten unless `overwrite=true` is sent.
The API does not restrict paths to HOME; `/`, `/content`, `/tmp`, and other
mounted filesystems are available subject to Linux permissions.

## Install / update

```bash
curl -fsSL https://raw.githubusercontent.com/Tulipskun/aixodia-engine/main/install.sh | bash
```

Installs to `/usr/local/bin`: `aixodia-engine`, `aixodia`, and `cloudflared` (if it
is not already on your PATH). Linux x86_64 and arm64.

Updating replaces the published binary without detaching or restarting
the running foreground process. The engine swaps its executable image in place,
keeping the same pid, working directory, standard streams and active session.

## Run

```bash
export CF_TOKEN=xxxxx    # used to publish the tunnel URL to D1
aixodia                 # foreground start
# or: aixodia start
```

**The engine always runs in the foreground.** `aixodia start` replaces the
launcher shell with `aixodia-engine`; it does not fork, use `nohup`, redirect
output to a file, or return the prompt. Logs are written directly to stdout and
stderr, so they appear in the same terminal or Kaggle/Colab cell. Ctrl-C sends
SIGINT and stops the engine.

```bash
aixodia status      # read the running engine's health
aixodia logs        # fetch recent engine logs from its local API
aixodia stop        # send SIGTERM to the foreground engine
aixodia update      # update the running engine in place
```

`status`, `logs`, `stop` and `update` are control commands; none starts a
second engine. The engine does not use D1 timestamps or discovery ownership
checks to suppress its tunnel publication or terminate its process. Start only
one engine per account unless multiple active tunnels are intentionally
required, because each active instance publishes its own current tunnel URL.
`stop` sends SIGTERM to the process reported by the local `/healthz` endpoint.
`logs` reads the engine's local `/api/logs` endpoint. `update` sends SIGUSR1
to the running process; the engine waits for active chats to finish, installs
the published build and execs the new image in the same process. Its output
remains attached to the original terminal or notebook.

The engine also checks for a published build every ten minutes and applies it
only while no chat is in flight. The update is an in-place exec, not a
background restart.

Only local state lives in `~/.aixodia/` (`aixodia-logs.db`, `uploads/` and
the cloudflared child-process pid file). There is no engine pid file or
background log file. Chat history and settings are in D1; providers and their
API keys are in Workers KV.

## Configuration

`CF_TOKEN` is required. `AIXODIA_HOME` can override the local state directory;
`AIXODIA_LOCAL_URL` can override the local control API address.


Chat history, settings, run logs and the note a session leaves for the one that
replaces it live in **D1** (`chat_sessions`, `chat_turns`, `system_state`,
`runs`, `run_events`). Providers and their API keys live in
**Workers KV**: namespace `aixodia-providers`, one key `providers` holding the
whole list. D1's `system_state` also carries the daemon discovery keys
(`daemon/url`, `daemon/version`, `daemon/heartbeat`), which is how the phone
finds the current tunnel. `CF_TOKEN` therefore needs D1 and Workers KV
read/write.

## Runs and terminal

Chat is a run, not a request that dies with the phone. The phone creates one
with `POST /v1/runs`, receives `id` immediately, and reads numbered events from
`GET /v1/runs/{id}/events?after=<seq>` (SSE, replay first, live afterwards).
Closing the app does not stop the worker. `POST /v1/runs/{id}/cancel` stops a
turn. Tools run without asking for confirmation. Every event is in
D1 before it can be missed; a restart marks unfinished runs `interrupted` rather
than pretending a goroutine survived.

Terminals use a dedicated WebSocket at `/terminal`, not the old shared relay
socket. The phone asks `POST /v1/terminals/ticket` for a single-use, 90-second
ticket, then the WebView's xterm.js opens `/terminal?ticket=...&id=...`. Output
is replayed from a 4 MiB ring and the shell is kept in tmux (`aix-<id>`, private
`socket aixodia`) when tmux is installed and `AIXODIA_TMUX` is not `0`.
`GET /v1/terminals` lists live and surviving sessions; `DELETE
/v1/terminals/{id}` ends one. The old `/ws` relay remains for compatibility with
the previous app build; this build's chat and terminal no longer use it.

## Development

```bash
cd rust
cargo build --workspace
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets
bash ../aixodia_test.sh   # launcher
```

The engine is Rust (`rust/`). The `aixodia-engine` binary is built by
`.github/workflows/release.yml` and published to the rolling `engine-latest`
release, which is what `install.sh` downloads.

## Terminal transport

There is no P2P/WebRTC/TURN path. The terminal is a normal WebSocket over the
same Cloudflare quick tunnel as the API, with a short-lived single-use ticket
instead of putting the long-lived API token in a WebView URL. Shell ownership
stays in the engine and, with tmux, survives an engine restart.
