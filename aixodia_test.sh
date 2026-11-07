#!/usr/bin/env bash
# Launcher tests: detached start. Run: bash aixodia_test.sh
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
LAUNCHER="$HERE/aixodia"
# Nothing listens here, so healthz fails at once instead of timing out.
DEAD_URL="http://127.0.0.1:1"
# curl reads a file over the file:// scheme, so a stub engine can answer healthz
# by publishing its own pid, the way the real engine publishes its JSON body.
fails=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1"; fails=$((fails + 1)); }

tmpdir() { mktemp -d; }

make_cloudflared() {
  mkdir -p "$1"
  printf '#!/bin/sh\nexit 0\n' > "$1/cloudflared"
  chmod +x "$1/cloudflared"
}

# A stub engine that records its environment and leaves with a distinct status.
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
echo "engine stdout: redirected"
echo "engine stderr: redirected" >&2
exit 37
STUB
  chmod +x "$1"
}

# A stub engine that stays up until it is asked to leave, the way the real
# engine runs until SIGTERM, and answers healthz with its own pid.
make_resident_engine() {
  cat > "$1" <<'STUB'
#!/usr/bin/env bash
mkdir -p "$(dirname "$ENGINE_HEALTH_FILE")"
printf '%s\n' "$$" > "$ENGINE_PID_FILE"
printf '%s\n' "$PWD" > "$ENGINE_CWD_FILE"
printf '%s\n' "$AIXODIA_HOME" > "$ENGINE_HOME_FILE"
printf '{"pid":%s,"status":"ok"}\n' "$$" > "$ENGINE_HEALTH_FILE"
echo "engine boot: detached"
trap 'rm -f "$ENGINE_HEALTH_FILE"; echo "engine stopped cleanly" > "$ENGINE_STOP_FILE"; exit 0' TERM
# A retired generation leaves this marker in the same state directory; the next
# engine must not read it as an order to leave.
rm -f "$ENGINE_STOP_FILE"
while [ ! -e "$ENGINE_STOP_FILE" ]; do sleep 0.1; done
STUB
  chmod +x "$1"
}

setup() {
  home=$(tmpdir)
  fakebin=$(tmpdir)
  bin="$fakebin/aixodia-engine"
  TEST_URL="$DEAD_URL"
  make_cloudflared "$fakebin"
}

wait_for_file() {
  for _ in $(seq 1 100); do
    [ -s "$1" ] && return 0
    sleep 0.05
  done
  return 1
}

wait_gone() {
  for _ in $(seq 1 100); do
    kill -0 "$1" 2>/dev/null || return 0
    sleep 0.05
  done
  return 1
}

run_launcher() {
  CF_TOKEN=test-token AIXODIA_HOME="$home/state" AIXODIA_ENGINE_BIN="$bin" \
    AIXODIA_LOCAL_URL="${TEST_URL:-$DEAD_URL}" AIXODIA_START_WAIT="${START_WAIT:-3}" \
    PATH="$fakebin:$PATH" \
    ENGINE_PID_FILE="$home/pid" ENGINE_CWD_FILE="$home/cwd" \
    ENGINE_HEALTH_FILE="$home/hz/healthz" \
    ENGINE_HOME_FILE="$home/engine-home" ENGINE_STOP_FILE="$home/stopped" \
    bash "$LAUNCHER" "$@"
}

# healthz appends /healthz to the base URL, so the stub publishes under it.
live_url() { TEST_URL="file://$home/hz"; }
dead_url() { TEST_URL="$DEAD_URL"; }

stop_engine() {
  local pid="$1"
  [ -n "$pid" ] || return 0
  run_launcher stop >/dev/null 2>&1
  wait_gone "$pid" || fail "test engine $pid outlived stop"
}

# Missing credentials must fail before the engine is executed.
setup
bin="$fakebin/aixodia-engine"
make_engine "$bin"
out_file="$home/out"
env -u CF_TOKEN CF_TOKEN= AIXODIA_HOME="$home/state" AIXODIA_ENGINE_BIN="$bin" \
  AIXODIA_LOCAL_URL="$DEAD_URL" PATH="$fakebin:$PATH" \
  ENGINE_PID_FILE="$home/pid" ENGINE_CWD_FILE="$home/cwd" \
  ENGINE_HOME_FILE="$home/engine-home" ENGINE_STOP_FILE="$home/stopped" \
  bash "$LAUNCHER" start >"$out_file" 2>&1
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

# start detaches: the launcher returns while the engine keeps running in its own
# session with no terminal, inside AIXODIA_HOME, logging to a file.
setup
bin="$fakebin/aixodia-engine"
make_resident_engine "$bin"
live_url
out_file="$home/out"
run_launcher start >"$out_file" 2>&1
rc=$?
if [ "$rc" -eq 0 ]; then
  pass "start returns without waiting for the engine"
else
  fail "start blocked or failed (rc=$rc, output=$(cat "$out_file"))"
fi
engine_pid=""
if wait_for_file "$home/pid"; then
  engine_pid="$(cat "$home/pid")"
  if kill -0 "$engine_pid" 2>/dev/null; then
    pass "engine survives after the launcher exits"
  else
    fail "engine died with the launcher (pid $engine_pid)"
  fi
  read -r engine_ppid engine_sid engine_tty < <(ps -o ppid=,sid=,tty= -p "$engine_pid" 2>/dev/null | tr -s ' ')
  if [ "$engine_ppid" = "1" ]; then
    pass "detached engine is reparented to init"
  else
    fail "detached engine still parented (ppid=$engine_ppid)"
  fi
  if [ "$engine_sid" = "$engine_pid" ] && [ "$engine_tty" = "?" ]; then
    pass "detached engine owns a session with no controlling terminal"
  else
    fail "detached engine session/tty (sid=$engine_sid pid=$engine_pid tty=$engine_tty)"
  fi
  if [ "$(cat "$home/cwd" 2>/dev/null || true)" = "$home/state" ] &&
     [ "$(cat "$home/engine-home" 2>/dev/null || true)" = "$home/state" ]; then
    pass "engine starts inside AIXODIA_HOME"
  else
    fail "engine working directory/state path mismatch (cwd=$(cat "$home/cwd" 2>/dev/null))"
  fi
  if grep -q "engine boot: detached" "$home/state/engine.log" 2>/dev/null; then
    pass "engine output is redirected to AIXODIA_HOME/engine.log"
  else
    fail "engine log missing (log=$(cat "$home/state/engine.log" 2>/dev/null))"
  fi
  if ! grep -q "engine boot: detached" "$out_file" &&
     grep -q "engine running (pid $engine_pid)" "$out_file"; then
    pass "launcher reports the engine pid and does not forward its output"
  else
    fail "launcher reporting wrong (output=$(cat "$out_file"))"
  fi
  if [ "$(cat "$home/state/aixodia.pid" 2>/dev/null || true)" = "$engine_pid" ]; then
    pass "AIXODIA_HOME/aixodia.pid holds the engine pid"
  else
    fail "pid file missing or wrong (file=$(cat "$home/state/aixodia.pid" 2>/dev/null))"
  fi
else
  fail "detached engine never started (output=$(cat "$out_file"))"
fi

# stop signals the detached engine, waits for it, and clears the pid file.
if [ -n "$engine_pid" ]; then
  out_file="$home/stop"
  run_launcher stop >"$out_file" 2>&1
  rc=$?
  if [ "$rc" -eq 0 ] && grep -q "engine stopped (pid $engine_pid" "$out_file"; then
    pass "stop reports the pid it terminated"
  else
    fail "stop reporting (rc=$rc, output=$(cat "$out_file"))"
  fi
  if wait_gone "$engine_pid"; then
    pass "stop waits for the engine to leave"
  else
    fail "engine still alive after stop (pid $engine_pid)"
  fi
  if [ -e "$home/stopped" ]; then
    pass "engine received SIGTERM"
  else
    fail "engine never saw SIGTERM"
  fi
  if [ ! -e "$home/state/aixodia.pid" ]; then
    pass "stop removes the pid file"
  else
    fail "stale pid file left behind"
  fi
fi

# status without an engine fails and names the pid file it found.
setup
bin="$fakebin/aixodia-engine"
make_engine "$bin"
mkdir -p "$home/state"
printf '424242\n' > "$home/state/aixodia.pid"
out=$(run_launcher status 2>&1)
rc=$?
if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -q "engine is not running; .*aixodia.pid names pid 424242"; then
  pass "status explains a dead engine and its pid file"
else
  fail "status while stopped (rc=$rc, output=$out)"
fi

# A pid file left by an unrelated process must never be signalled.
setup
bin="$fakebin/aixodia-engine"
make_resident_engine "$bin"
live_url
mkdir -p "$home/state"
sleep 120 &
squatter=$!
printf '%s\n' "$squatter" > "$home/state/aixodia.pid"
out_file="$home/out"
run_launcher start >"$out_file" 2>&1
rc=$?
if [ "$rc" -eq 0 ] && kill -0 "$squatter" 2>/dev/null; then
  pass "start refuses to signal a pid that is not the engine"
else
  fail "foreign pid was treated as the engine (rc=$rc, alive=$(kill -0 "$squatter" 2>/dev/null && echo yes || echo no), output=$(cat "$out_file"))"
fi
new_pid=""
if wait_for_file "$home/pid"; then
  new_pid="$(cat "$home/pid")"
  if [ -n "$new_pid" ] && [ "$new_pid" != "$squatter" ] && kill -0 "$new_pid" 2>/dev/null; then
    pass "start proceeds with a fresh engine after ignoring the stale pid"
  else
    fail "no fresh engine after a stale pid file (pid=$new_pid)"
  fi
else
  fail "engine never recorded a pid after a stale pid file"
fi
kill "$squatter" 2>/dev/null || true
wait "$squatter" 2>/dev/null
stop_engine "$new_pid"

# A second start takes over an engine that is up but not answering healthz.
setup
bin="$fakebin/aixodia-engine"
make_resident_engine "$bin"
dead_url
run_launcher start >"$home/out" 2>&1 || true
first_pid="$(cat "$home/pid" 2>/dev/null || true)"
: > "$home/pid"
run_launcher start >"$home/out2" 2>&1
rc=$?
second_pid="$(cat "$home/pid" 2>/dev/null || true)"
if [ -n "$first_pid" ] && [ -n "$second_pid" ] && [ "$first_pid" != "$second_pid" ] &&
   ! kill -0 "$first_pid" 2>/dev/null && kill -0 "$second_pid" 2>/dev/null; then
  pass "start stops an unresponsive engine and replaces it"
else
  fail "takeover (rc=$rc first=${first_pid:-none} second=${second_pid:-none} output=$(cat "$home/out2"))"
fi
stop_engine "$second_pid"

# An engine that dies during boot is reported with its log and leaves no pid file.
setup
bin="$fakebin/aixodia-engine"
make_engine "$bin"
out_file="$home/out"
run_launcher start >"$out_file" 2>&1
rc=$?
if [ "$rc" -ne 0 ] && grep -q "engine exited during startup" "$out_file" &&
   grep -q "engine stdout: redirected" "$out_file"; then
  pass "a boot failure surfaces with the log tail and a non-zero status"
else
  fail "boot failure reporting (rc=$rc, output=$(cat "$out_file"))"
fi
if [ ! -e "$home/state/aixodia.pid" ]; then
  pass "no pid file is left for an engine that exited"
else
  fail "pid file left for a dead engine"
fi

# The log file is rotated on start once it passes the size limit.
setup
bin="$fakebin/aixodia-engine"
make_resident_engine "$bin"
live_url
mkdir -p "$home/state"
head -c 2048 /dev/zero | tr '\0' 'x' > "$home/state/engine.log"
out_file="$home/out"
AIXODIA_LOG_MAX_KB=1 run_launcher start >"$out_file" 2>&1
if [ -s "$home/state/engine.log.1" ] && grep -q "rotated" "$out_file"; then
  pass "start rotates engine.log past the size limit"
else
  fail "log rotation (output=$(cat "$out_file"))"
fi
rotated_pid="$(cat "$home/pid" 2>/dev/null || true)"
if ! grep -q "engine boot: detached" "$home/state/engine.log.1"; then
  pass "rotation keeps the live log separate from the previous generation"
else
  fail "rotation copied the new run into the archive"
fi
stop_engine "$rotated_pid"

# logs falls back to the log file when the log table is unreachable.
if [ -s "$home/state/engine.log" ]; then
  out=$(AIXODIA_HOME="$home/state" AIXODIA_LOCAL_URL="$DEAD_URL" CF_TOKEN=test-token \
    bash "$LAUNCHER" logs 5 2>&1)
  if printf '%s' "$out" | grep -q "reading .*engine.log" && printf '%s' "$out" | grep -q "engine boot: detached"; then
    pass "logs falls back to the engine log file"
  else
    fail "logs fallback (output=$out)"
  fi
else
  fail "logs fallback: no log file to read"
fi
out=$(AIXODIA_HOME="$home/state" bash "$LAUNCHER" logs 99999 2>&1)
rc=$?
if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -q "log limit must be"; then
  pass "logs validates its limit"
else
  fail "logs limit validation (rc=$rc, output=$out)"
fi

# A stopped engine can still use the updater command to install the published
# binary; this command does not start a service.
setup
bin="$fakebin/aixodia-engine"
make_engine "$bin"
out=$(AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" AIXODIA_LOCAL_URL="$DEAD_URL" \
  PATH="$fakebin:$PATH" bash "$LAUNCHER" update 2>&1)
rc=$?
if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q "installed the build published"; then
  pass "update installs the published build without starting the engine"
else
  fail "offline update (rc=$rc, output=$out)"
fi

# There is no attached mode left to ask for.
setup
bin="$fakebin/aixodia-engine"
make_resident_engine "$bin"
live_url
out=$(run_launcher run 2>&1)
rc=$?
alias_pid="$(cat "$home/pid" 2>/dev/null || true)"
if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q "starting in the background"; then
  pass "the run alias starts the detached engine too"
else
  fail "run alias (rc=$rc, output=$out)"
fi
stop_engine "$alias_pid"
out=$(AIXODIA_HOME="$home" AIXODIA_LOCAL_URL="$DEAD_URL" bash "$LAUNCHER" --foreground 2>&1)
rc=$?
if [ "$rc" -eq 2 ] && printf '%s' "$out" | grep -q "unknown command"; then
  pass "there is no foreground flag"
else
  fail "foreground flag handling (rc=$rc, output=$out)"
fi

# Detachment lives in the launcher: setsid, a pid file and a redirected log.
if grep -q 'setsid' "$LAUNCHER" && grep -q 'aixodia.pid' "$LAUNCHER" && grep -q 'engine.log' "$LAUNCHER"; then
  pass "launcher daemonizes with setsid, a pid file and a log file"
else
  fail "launcher no longer daemonizes"
fi
if ! grep -qE 'exec "\$ENGINE_RESOLVED"' "$LAUNCHER"; then
  pass "no code path replaces the caller with the engine"
else
  fail "an attached exec path remains in the launcher"
fi

echo
if [ "$fails" -eq 0 ]; then
  echo "all launcher tests passed"
else
  echo "$fails launcher test(s) failed"
  exit 1
fi
