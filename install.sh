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
#   aixodia         the detached launcher (start / stop / status / logs / update)
#   cloudflared     the tunnel helper, if it is not already installed
#
# When CF_TOKEN is exported for the install, the launcher is also set to start
# the engine on its own:
#   aixodia autostart on   remembers the token and starts the engine at login
#
#
# Updating is the same command again: it replaces the files and stops there.
# A running engine picks the new build up in place with `aixodia update`, which
# signals it to exec itself (same pid, session intact); a request sent during a
# chat is queued and applied when the chat ends.
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
# The same engine with the screen pane left out. The pane links X libraries, and
# a host without them cannot load the engine at all — the terminal and the chat
# die over a tab that host could not have used anyway.
ENGINE_URL_NOSCREEN="$ENGINE_URL-noscreen"
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

# engine_can_load <binary> — false when a shared library it needs is missing.
engine_can_load() {
  ! ldd "$1" 2>/dev/null | grep -q "not found"
}

# glibc_required <binary> — the newest GLIBC_x.y the binary asks the loader for,
# or nothing. A minimal host may have none of these tools, and an installer that
# died because `objdump` was absent would be worse than one that says nothing.
glibc_required() {
  command -v objdump >/dev/null 2>&1 || command -v readelf >/dev/null 2>&1 || return 0
  { objdump -T "$1" 2>/dev/null || readelf -sW "$1" 2>/dev/null; } 2>/dev/null |
    grep -oE 'GLIBC_[0-9]+\.[0-9]+' 2>/dev/null | sed 's/GLIBC_//' |
    sort -uV 2>/dev/null | tail -1 || true
}

# glibc_host — the glibc this machine runs.
glibc_host() {
  { ldd --version 2>/dev/null || getconf GNU_LIBC_VERSION 2>/dev/null; } 2>/dev/null |
    grep -oE '[0-9]+\.[0-9]+' 2>/dev/null | head -1 || true
}

# Installs the X libraries the screen pane links, if this host has a package
# manager that knows them. Returns non-zero when it cannot.
install_x_libraries() {
  command -v apt-get >/dev/null 2>&1 || return 1
  local attempt
  for attempt in 1 2 3; do
    if apt-get update -qq >/dev/null 2>&1 &&
      DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
        libx11-6 libxext6 libxtst6 libxfixes3 >/dev/null 2>&1; then
      return 0
    fi
    log "apt attempt $attempt for the X libraries failed; retrying in 5s"
    sleep 5
  done
  return 1
}

fetch "$ENGINE_URL" "$PREFIX/aixodia-engine"
needed_glibc="$(glibc_required "$PREFIX/aixodia-engine")"
host_glibc="$(glibc_host)"
if [ -n "$needed_glibc" ] && [ -n "$host_glibc" ] &&
  [ "$(printf '%s\n%s\n' "$needed_glibc" "$host_glibc" | sort -V | tail -1)" != "$host_glibc" ]; then
  # Not a missing library: this build was linked against a newer C library than
  # this machine has, and no package will fix that. Measured as "installed
  # cleanly, then died at start with 'version `GLIBC_2.38' not found'".
  die "this build needs glibc $needed_glibc and this host has $host_glibc; build the engine on this machine (cargo build --release) or install on a host with glibc $needed_glibc or newer"
fi
if ! engine_can_load "$PREFIX/aixodia-engine"; then
  log "the screen pane needs X libraries this host does not have"
  if install_x_libraries && engine_can_load "$PREFIX/aixodia-engine"; then
    log "installed the X libraries; the screen pane is available"
  else
    log "installing the build without the screen pane instead (terminal, chat and tunnel are unaffected)"
    fetch "$ENGINE_URL_NOSCREEN" "$PREFIX/aixodia-engine"
  fi
fi
fetch "$RAW_BASE/aixodia" "$PREFIX/aixodia"

if command -v cloudflared >/dev/null 2>&1; then
  log "cloudflared already installed, keeping it"
else
  fetch "$CLOUDFLARED_URL" "$PREFIX/cloudflared"
fi

log "installed: $PREFIX/{aixodia-engine, aixodia}$(command -v cloudflared >/dev/null 2>&1 && echo "" || echo ", cloudflared")"
echo
echo "Start it with:"
echo "  export CF_TOKEN=xxxxx   # reads Named Tunnel config and publishes endpoint to Workers KV"
echo "  aixodia start"

# An unattended host — a chroot with no init, Termux, a container — has nothing
# to keep the engine up. When the operator already has the token exported, this
# is the one moment it can be remembered safely, so asking for it twice would be
# the wrong default.
if [ -n "${CF_TOKEN:-}" ]; then
  echo
  if "$PREFIX/aixodia" autostart on; then
    echo "aixodia: the engine will also start whenever a shell opens"
  else
    echo "aixodia: could not set up autostart; run 'aixodia autostart on' yourself" >&2
  fi
fi
