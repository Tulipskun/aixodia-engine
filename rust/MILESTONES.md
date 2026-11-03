# Rust port — milestones

The engine is Rust. The Go tree has been removed; `rust/` is the only
implementation and the release workflow builds it for Linux x86_64 and arm64.

## Verify

```bash
cd rust
cargo build --workspace
cargo test --workspace
cargo fmt --check
cargo clippy --workspace --all-targets   # rustup component add clippy
```

## Status

| Piece | Crate | Status |
|---|---|---|
| `db/` (D1 REST + Workers KV + `system_state`) | `aixodia-db` | ported |
| `config/` (provider store, built-ins) | `aixodia-config` | ported |
| `provider/` types, OpenAI, OpenCode, Anthropic | `aixodia-provider` | ported |
| `tools/` declarations + execution | `aixodia-tools` | ported |
| `session/` (D1 store, trimming, blocks) | `aixodia-session` | ported |
| `io/gateway/` (HTTP API, chat loop, SSE) | `aixodia-io` | ported |
| `io/terminal/` (pty, tmux, ring, tickets) | `aixodia-terminal` | ported |
| `io/mobile/`, `io/wsock/` | `aixodia-mobile`, `aixodia-wsock` | ported |
| `io/relay/` (direct `/room` transport) | `aixodia-relay` | ported |
| `run/` (runs, events) | `aixodia-run` | ported |
| `selfupdate/` (handover, exec) | `aixodia-selfupdate` | ported |
| `imageprep/`, `goals/` | `aixodia-imageprep`, `aixodia-goals` | ported |
| `main.go` + `tunnel.go` + `handover.go` | `aixodia-engine` (bin) | ported |

The relay's old outbound-dial mode (`relay.Run`) is not part of this deployment:
the engine serves `/room` directly behind the Cloudflare quick tunnel, as its
`main` always did. The direct transport is the one the phone uses.
