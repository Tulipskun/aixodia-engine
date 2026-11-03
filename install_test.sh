#!/usr/bin/env bash
# Tests for install.sh. Run: bash install_test.sh
# Downloads are served by a fake curl, so nothing touches the network and
# nothing is written outside a temp prefix.
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
fails=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1"; fails=$((fails + 1)); }

# --- fresh install drops the binaries and the tunnel helper in the prefix ----
prefix=$(mktemp -d)
fakecurl=$(mktemp -d)
cat > "$fakecurl/curl" <<'FAKE'
#!/usr/bin/env bash
# stand-in curl: writes a dummy file and succeeds
out=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -*) shift ;;
    *) shift ;;
  esac
done
[ -n "$out" ] && printf 'x\n' > "$out"
FAKE
chmod +x "$fakecurl/curl"
out=$(PATH="$fakecurl:/usr/bin:/bin" bash "$HERE/install.sh" "$prefix" 2>&1); rc=$?
if [ "$rc" -eq 0 ]; then
  pass "install exits 0"
else
  fail "install rc=$rc ($out)"
fi
for f in aixodia-engine aixodia cloudflared; do
  if [ -x "$prefix/$f" ]; then
    pass "installed $f (executable)"
  else
    fail "missing or not executable: $prefix/$f"
  fi
done

# --- a failed download fails the install ------------------------------------
prefix=$(mktemp -d)
fakecurl=$(mktemp -d)
cat > "$fakecurl/curl" <<'FAKE'
#!/usr/bin/env bash
# stand-in curl: always fails
die() { printf 'curl: %s\n' "$*" >&2; exit 1; }
die "simulated failure"
FAKE
chmod +x "$fakecurl/curl"
out=$(PATH="$fakecurl:/usr/bin:/bin" bash "$HERE/install.sh" "$prefix" 2>&1); rc=$?
if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -qi "cannot download"; then
  pass "a failed download fails loudly"
else
  fail "failed download (rc=$rc, out=$out)"
fi

# --- downloads match the host architecture ----------------------------------
record=$(mktemp)
fakecurl=$(mktemp -d)
cat > "$fakecurl/curl" <<'FAKE'
#!/usr/bin/env bash
# stand-in curl: records the requested url and writes a dummy file
out=""; url=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2 ;;
    -*) shift ;;
    *) url="$1"; shift ;;
  esac
done
printf '%s\n' "$url" >> "${RECORD:?}"
[ -n "$out" ] && printf 'x\n' > "$out"
FAKE
chmod +x "$fakecurl/curl"
want=amd64
case "$(uname -m)" in aarch64|arm64) want=arm64 ;; esac
prefix=$(mktemp -d)
PATH="$fakecurl:/usr/bin:/bin" RECORD="$record" bash "$HERE/install.sh" "$prefix" >/dev/null 2>&1
if grep -q "aixodia-engine-linux-$want" "$record"; then
  pass "engine download matches the host arch ($want)"
else
  fail "engine url for $want: $(tr '\n' ' ' < "$record")"
fi
if grep -q "cloudflared-linux-$want" "$record"; then
  pass "cloudflared download matches the host arch ($want)"
else
  fail "cloudflared url for $want: $(tr '\n' ' ' < "$record")"
fi

echo
if [ "$fails" -eq 0 ]; then
  echo "all installer tests passed"
else
  echo "$fails installer test(s) failed"
  exit 1
fi
