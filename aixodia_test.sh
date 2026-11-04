#!/usr/bin/env bash
# Foreground launcher tests. Run: bash aixodia_test.sh
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
LAUNCHER="$HERE/aixodia"
fails=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1"; fails=$((fails + 1)); }

tmpdir() { mktemp -d; }

make_cloudflared() {
  mkdir -p "$1"
  printf '#!/bin/sh\nexit 0\n' > "$1/cloudflared"
  chmod +x "$1/cloudflared"
}

make_engine() {
  cat > "$1" <<'STUB'
#!/usr/bin/env bash
if [ "$#" -gt 0 ] && [ "$1" = "update" ]; then
  echo "installed the build published"
  exit 0
fi
printf '%s\n' "$$" > "$ENGINE_PID_FILE"
printf '%s\n' "$PWD" > "$ENGINE_CWD_FILE"
printf '%s\n' "$AIXODIA_HOME" > "$ENGINE_HOME_FILE"
echo "engine stdout: attached"
echo "engine stderr: attached" >&2
exit 37
STUB
  chmod +x "$1"
}

# Missing credentials must fail before the engine is executed.
home=$(tmpdir)
fakebin=$(tmpdir)
bin="$fakebin/aixodia-engine"
make_engine "$bin"
make_cloudflared "$fakebin"
out_file="$home/out"
env -u CF_TOKEN AIXODIA_HOME="$home/state" AIXODIA_ENGINE_BIN="$bin" \
  ENGINE_PID_FILE="$home/pid" ENGINE_CWD_FILE="$home/cwd" ENGINE_HOME_FILE="$home/engine-home" \
  PATH="$fakebin:$PATH" bash "$LAUNCHER" start >"$out_file" 2>&1
rc=$?
if [ "$rc" -ne 0 ] && grep -q "CF_TOKEN is not set" "$out_file"; then
  pass "start refuses to run without CF_TOKEN"
else
  fail "missing CF_TOKEN (rc=$rc, output=$(cat "$out_file"))"
fi
if [ ! -e "$home/pid" ]; then
  pass "engine was not executed without CF_TOKEN"
else
  fail "engine executed without CF_TOKEN"
fi

# start must exec the engine, keep the caller's pid, and return only when the
# engine exits. Its stdout and stderr must remain attached to the caller.
home=$(tmpdir)
fakebin=$(tmpdir)
bin="$fakebin/aixodia-engine"
make_engine "$bin"
make_cloudflared "$fakebin"
out_file="$home/out"
CF_TOKEN=test-token AIXODIA_HOME="$home/state" AIXODIA_ENGINE_BIN="$bin" \
  ENGINE_PID_FILE="$home/pid" ENGINE_CWD_FILE="$home/cwd" ENGINE_HOME_FILE="$home/engine-home" \
  PATH="$fakebin:$PATH" bash "$LAUNCHER" start >"$out_file" 2>&1 &
launcher_pid=$!

for _ in $(seq 1 40); do
  [ -s "$home/pid" ] && break
  sleep 0.05
done
engine_pid="$(cat "$home/pid" 2>/dev/null || true)"
if [ -n "$engine_pid" ] && [ "$engine_pid" = "$launcher_pid" ]; then
  pass "launcher execs the engine in the same foreground process"
else
  fail "engine pid differs from launcher (launcher=$launcher_pid engine=$engine_pid)"
fi

wait "$launcher_pid"
rc=$?
if [ "$rc" -eq 37 ]; then
  pass "launcher stays attached until the engine exits and preserves its exit code"
else
  fail "foreground exit status (rc=$rc, expected 37)"
fi
if grep -q "engine stdout: attached" "$out_file" && grep -q "engine stderr: attached" "$out_file"; then
  pass "engine stdout and stderr reach the caller directly"
else
  fail "engine output was not forwarded (output=$(cat "$out_file"))"
fi
if [ "$(cat "$home/cwd" 2>/dev/null || true)" = "$home/state" ] &&
   [ "$(cat "$home/engine-home" 2>/dev/null || true)" = "$home/state" ]; then
  pass "engine starts inside AIXODIA_HOME"
else
  fail "engine working directory/state path mismatch"
fi
if [ ! -e "$home/state/aixodia.pid" ] && [ ! -e "$home/state/aixodia.log" ]; then
  pass "launcher creates no detached-engine pid or redirected log file"
else
  fail "legacy daemon files were created"
fi

# A stopped engine can still use the updater command to install the published
# binary; this command does not start a service.
home=$(tmpdir)
fakebin=$(tmpdir)
bin="$fakebin/aixodia-engine"
make_engine "$bin"
out=$(AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" PATH="$fakebin:$PATH" \
  bash "$LAUNCHER" update 2>&1)
rc=$?
if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q "installed the build published"; then
  pass "update installs the published build without starting the engine"
else
  fail "offline update (rc=$rc, output=$out)"
fi

# The obsolete opt-in switch is no longer accepted: foreground is the only mode.
out=$(AIXODIA_ENGINE_BIN="$bin" bash "$LAUNCHER" --foreground 2>&1)
rc=$?
if [ "$rc" -eq 2 ] && printf '%s' "$out" | grep -q "unknown command"; then
  pass "there is no separate foreground mode flag"
else
  fail "obsolete --foreground flag (rc=$rc, output=$out)"
fi

# Guard against reintroducing launcher-level detachment or file redirection.
if ! grep -Eq 'nohup|PID_FILE|LOG_FILE|>[[:space:]]*&|[[:space:]]&[[:space:]]*$' "$LAUNCHER"; then
  pass "launcher contains no daemonization, pidfile, or log redirection"
else
  fail "daemonization or redirected logging remains in launcher"
fi

echo
if [ "$fails" -eq 0 ]; then
  echo "all foreground launcher tests passed"
else
  echo "$fails launcher test(s) failed"
  exit 1
fi
