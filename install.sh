#!/usr/bin/env bash
# aixodia installer — one line:
#
#   curl -fsSL https://raw.githubusercontent.com/Tulipskun/aixodia-engine/main/install.sh | bash
#
# (run with sudo to install to /usr/local/bin; without sudo it installs to
#  ~/.local/bin)
#
# Installs:
#   aixodia-engine  the engine binary (it serves 127.0.0.1:8787)
#   aixodia         the launcher (start / stop / status / logs / update)
#   cloudflared     the tunnel helper, if it is not already installed
#
# Updating is the same command again: it replaces the files and stops there.
# A running engine picks the new build up in place with `aixodia update`, which
# signals it to exec itself (same pid, session intact).
set -euo pipefail

REPO_SLUG="Tulipskun/aixodia-engine"
REF="main"
RAW_BASE="https://raw.githubusercontent.com/$REPO_SLUG/$REF"

# An optional first argument overrides the install prefix (used by tests). In
# normal use the prefix is chosen from the invoking user.
if [ -n "${1:-}" ]; then
  PREFIX="$1"
elif [ "$(id -u)" -eq 0 ]; then
  PREFIX="/usr/local/bin"
else
  PREFIX="$HOME/.local/bin"
fi

log() { printf '==> %s\n' "$*"; }
die() { printf 'install: %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || die "curl is required"

mkdir -p "$(dirname "$PREFIX")" 2>/dev/null || true

# Writing to /usr/local/bin needs root; a writable prefix (tests, custom
# installs) does not.
if [ "$(id -u)" -ne 0 ] && [ ! -w "$PREFIX" ] && [ ! -w "$(dirname "$PREFIX")" ]; then
  die "needs root to write $PREFIX — run it as: curl -fsSL <url> | sudo bash"
fi

os="$(uname -s)"
arch="$(uname -m)"
case "$os/$arch" in
  Linux/x86_64) PLATFORM=linux-amd64 ;;
  Linux/aarch64 | Linux/arm64) PLATFORM=linux-arm64 ;;
  *) die "unsupported platform: $os/$arch (only Linux x86_64 and arm64 builds are published)" ;;
esac

ENGINE_URL="https://github.com/$REPO_SLUG/releases/download/engine-latest/aixodia-engine-$PLATFORM"
CLOUDFLARED_URL="https://github.com/cloudflare/cloudflared/releases/latest/download/cloudflared-$PLATFORM"

mkdir -p "$PREFIX"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# fetch <url> <dest> — downloads to a temp name and moves it into place, so a
# binary that is currently running can still be replaced.
fetch() {
  local url="$1" dest="$2"
  log "download $(basename "$dest")"
  curl -fsSL --retry 3 --retry-delay 2 -o "$TMP/download" "$url" \
    || die "cannot download $url"
  chmod +x "$TMP/download"
  mv -f "$TMP/download" "$dest"
}

fetch "$ENGINE_URL" "$PREFIX/aixodia-engine"
fetch "$RAW_BASE/aixodia" "$PREFIX/aixodia"

if command -v cloudflared >/dev/null 2>&1; then
  log "cloudflared already installed, keeping it"
else
  fetch "$CLOUDFLARED_URL" "$PREFIX/cloudflared"
fi

log "installed: $PREFIX/{aixodia-engine, aixodia}$(command -v cloudflared >/dev/null 2>&1 && echo "" || echo ", cloudflared")"
echo
echo "Start it with:"
echo "  export CF_TOKEN=xxxxx   # the engine publishes its tunnel url to D1 with this"
echo "  aixodia"
