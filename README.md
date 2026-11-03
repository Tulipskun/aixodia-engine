# AIxodia

The AIxodia engine: serves the chat API on `127.0.0.1:8787`, puts a Cloudflare
quick tunnel in front of it, and tells the phone the tunnel URL via D1.

## Install / update

```bash
curl -fsSL https://raw.githubusercontent.com/Tulipskun/aixodia-engine/main/install.sh | bash
```

Installs to `/usr/local/bin`: `aixodia-engine`, `aixodia`, and `cloudflared` (if it
is not already on your PATH). Linux x86_64 and arm64.

Updating is the same command: `aixodia update` installs the published build and
hands it to the running engine, which swaps the binary and carries on under the
same pid. A running build too old to accept the handshake is restarted instead.

## Run

```bash
export CF_TOKEN=xxxxx    # the only secret; used to publish the tunnel url to D1
aixodia
```

`aixodia` starts the engine in the background, waits for the tunnel URL, prints
it, and gives the prompt back.

```bash
aixodia status      # up? at which url
aixodia logs        # tail the engine log
aixodia stop        # shut it down
aixodia update      # install the published build and update in place
```

`aixodia update` does not stop the engine. It sends `SIGUSR1` to the pid the
engine reports on `GET /healthz`, and the engine installs the build and execs
itself: same pid, same arguments, same session. A chat in flight is not cut
short — the engine notes the request and applies the build when the turn ends.
Only a build that predates the handshake (no `update` key on `/healthz`) is
stopped and started, which ends the attached session.

The engine also installs a published build by itself, ten minutes at a time and
only while no chat is in flight (`selfupdate: installing the build published
...` in the log); `aixodia update` is the same swap on demand.

Only local state lives in `~/.aixodia/` (`aixodia.pid`, `aixodia.log`,
`aixodia-logs.db`, `uploads/`). Chat history and settings are in D1; providers
and their API keys are in Workers KV.

## Configuration

Only `CF_TOKEN` is required. No other environment variables are used.


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
