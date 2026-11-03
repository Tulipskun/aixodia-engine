#!/usr/bin/env bash
# Tests for the aixodia launcher. Run: bash aixodia_test.sh
# No framework: a stub engine stands in for the real binary so the launcher's
# own logic (env check, pidfile, wait-for-url, stop/status) is what is tested.
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
LAUNCHER="$HERE/aixodia"

fails=0
pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1"; fails=$((fails + 1)); }

tmp_home() { mktemp -d; }

# wait_for_port prints the port a handshake stub bound, once its log reports it.
# The stub prints its tunnel url before it binds, so `start` can answer first and
# a single read of the log races itself.
wait_for_port() {
  local log="$1" port="" i
  for i in $(seq 1 20); do
    port=$(sed -n 's/.*listening on \([0-9][0-9]*\).*/\1/p' "$log" | tail -1)
    [ -n "$port" ] && break
    sleep 0.25
  done
  printf '%s' "$port"
}


# Stub engine: logs the tunnel url it published, then stays alive until killed.
make_stub_engine() {
  cat > "$1" <<'STUB'
#!/usr/bin/env bash
if [ "${1:-}" = "update" ]; then
  echo "installed the build published 2026-10-21T00:00:00Z"
  exit 0
fi
echo "engine started"
echo "tunnel url: https://stub-tunnel.trycloudflare.com"
echo "daemon url saved"
while true; do sleep 0.5; done
STUB
  chmod +x "$1"
}

# A cloudflared stand-in: the launcher only checks that the binary exists.
make_cloudflared() {
  mkdir -p "$1"
  printf '#!/bin/sh\nexit 0\n' > "$1/cloudflared"
  chmod +x "$1/cloudflared"
}

# --- missing CF_TOKEN: refuse before starting anything ------------------------
home=$(tmp_home); bin=$(tmp_home)/aixodia-engine; make_stub_engine "$bin"
out=$(env -u CF_TOKEN AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" bash "$LAUNCHER" start 2>&1); rc=$?
if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -qi "CF_TOKEN"; then
  pass "start without CF_TOKEN fails with a message"
else
  fail "start without CF_TOKEN (rc=$rc, out=$out)"
fi
if [ ! -f "$home/aixodia.pid" ]; then
  pass "no pidfile written when CF_TOKEN is missing"
else
  fail "pidfile written without CF_TOKEN"
fi

# --- the bare command: it says what is here, and starts nothing --------------
home=$(tmp_home); bin=$(tmp_home)/aixodia-engine; make_stub_engine "$bin"
out=$(env -u CF_TOKEN AIXODIA_HOME="$home" AIXODIA_LOCAL_URL="http://127.0.0.1:1" \
      AIXODIA_ENGINE_BIN="$bin" bash "$LAUNCHER" 2>&1); rc=$?
if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q "^usage: aixodia" && printf '%s' "$out" | grep -q "start"; then
  pass "aixodia with no command prints the usage"
else
  fail "bare aixodia (rc=$rc, out=$out)"
fi
if printf '%s' "$out" | grep -qi "not running"; then
  pass "aixodia with no command says there is no engine to report a version of"
else
  fail "bare aixodia version line (out=$out)"
fi
if [ ! -f "$home/aixodia.pid" ]; then
  pass "aixodia with no command does not start an engine"
else
  fail "bare aixodia started an engine"
fi

# --- status when not running -------------------------------------------------
home=$(tmp_home)
out=$(AIXODIA_HOME="$home" bash "$LAUNCHER" status 2>&1); rc=$?
if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -qi "not running"; then
  pass "status reports not running"
else
  fail "status not-running (rc=$rc, out=$out)"
fi

# --- already running: do not start a second engine ---------------------------
home=$(tmp_home); bin=$(tmp_home)/aixodia-engine; make_stub_engine "$bin"
echo "$$" > "$home/aixodia.pid"
out=$(CF_TOKEN=x AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" bash "$LAUNCHER" start 2>&1); rc=$?
if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -qi "already running"; then
  pass "second start reports already running"
else
  fail "already-running (rc=$rc, out=$out)"
fi
if [ "$(cat "$home/aixodia.pid")" = "$$" ]; then
  pass "already-running keeps the existing pid"
else
  fail "already-running overwrote the pid"
fi

# --- start, then status, stop, status ---------------------------------------
home=$(tmp_home); fakebin=$(tmp_home); bin=$fakebin/aixodia-engine
make_cloudflared "$fakebin"; make_stub_engine "$bin"
out=$(CF_TOKEN=x AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" AIXODIA_TUNNEL_WAIT=15 PATH="$fakebin:$PATH" bash "$LAUNCHER" start 2>&1); rc=$?
if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q "https://stub-tunnel.trycloudflare.com"; then
  pass "start prints the tunnel address and returns"
else
  fail "start (rc=$rc, out=$out)"
fi
pid=$(cat "$home/aixodia.pid" 2>/dev/null || echo "")
if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
  pass "engine is left running in the background"
else
  fail "engine not running after start (pid=$pid)"
fi

out=$(AIXODIA_HOME="$home" bash "$LAUNCHER" status 2>&1); rc=$?
if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q "https://stub-tunnel.trycloudflare.com"; then
  pass "status reports the running url"
else
  fail "status running (rc=$rc, out=$out)"
fi

AIXODIA_HOME="$home" bash "$LAUNCHER" stop >/dev/null 2>&1
sleep 0.5
if ! kill -0 "$pid" 2>/dev/null; then
  pass "stop terminates the engine"
else
  fail "stop left the engine alive (pid=$pid)"
fi
if [ ! -f "$home/aixodia.pid" ]; then
  pass "stop removes the pidfile"
else
  fail "stop left the pidfile"
fi

# --- start on a terminal: it prints the log instead of giving the prompt back --
# `script` hands the launcher a real terminal, which is the only case that
# follows; `timeout` stands in for the Ctrl-C that ends the follow, and the
# engine has to outlive it.
home=$(tmp_home); fakebin=$(tmp_home); bin=$fakebin/aixodia-engine
make_cloudflared "$fakebin"; make_stub_engine "$bin"
out=$(timeout -k 2 10 env CF_TOKEN=x AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" \
      AIXODIA_TUNNEL_WAIT=15 PATH="$fakebin:$PATH" \
      script -qec "bash $LAUNCHER start" /dev/null 2>&1)
if printf '%s' "$out" | grep -q "engine started" && printf '%s' "$out" | grep -q "stub-tunnel"; then
  pass "start on a terminal prints the log"
else
  fail "start follow on a terminal (out=$out)"
fi
pid=$(cat "$home/aixodia.pid" 2>/dev/null || echo "")
if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
  pass "the engine outlives the log follow"
else
  fail "engine died with the follow (pid=$pid)"
fi
AIXODIA_HOME="$home" bash "$LAUNCHER" stop >/dev/null 2>&1

# --- update with no engine running: installs and stops there ----------------
home=$(tmp_home); fakebin=$(tmp_home); bin=$fakebin/aixodia-engine
make_cloudflared "$fakebin"; make_stub_engine "$bin"
out=$(AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" PATH="$fakebin:$PATH" bash "$LAUNCHER" update 2>&1); rc=$?
if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q "installed the build published"; then
  pass "update installs the published build"
else
  fail "update install (rc=$rc, out=$out)"
fi
if printf '%s' "$out" | grep -q "the next start uses the new build"; then
  pass "update says the next start picks it up"
else
  fail "update without a running engine (out=$out)"
fi

# --- update with the engine running: it is restarted into the new build -----
home=$(tmp_home); fakebin=$(tmp_home); bin=$fakebin/aixodia-engine
make_cloudflared "$fakebin"; make_stub_engine "$bin"
env CF_TOKEN=x AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" AIXODIA_TUNNEL_WAIT=15 \
  PATH="$fakebin:$PATH" bash "$LAUNCHER" start >/dev/null 2>&1
before=$(cat "$home/aixodia.pid" 2>/dev/null || echo "")
out=$(CF_TOKEN=x AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" AIXODIA_TUNNEL_WAIT=15 \
      PATH="$fakebin:$PATH" bash "$LAUNCHER" update 2>&1); rc=$?
after=$(cat "$home/aixodia.pid" 2>/dev/null || echo "")
if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q "restarting (pid $before)"; then
  pass "update restarts the running engine"
else
  fail "update restart (rc=$rc, out=$out)"
fi
if [ -n "$after" ] && [ "$after" != "$before" ] && kill -0 "$after" 2>/dev/null; then
  pass "the restarted engine is a new pid"
else
  fail "restarted pid (before=$before after=$after)"
fi
AIXODIA_HOME="$home" bash "$LAUNCHER" stop >/dev/null 2>&1

# --- update without CF_TOKEN while running leaves the engine up --------------
home=$(tmp_home); fakebin=$(tmp_home); bin=$fakebin/aixodia-engine
make_cloudflared "$fakebin"; make_stub_engine "$bin"
env CF_TOKEN=x AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" AIXODIA_TUNNEL_WAIT=15 \
  PATH="$fakebin:$PATH" bash "$LAUNCHER" start >/dev/null 2>&1
pid=$(cat "$home/aixodia.pid" 2>/dev/null || echo "")
out=$(env -u CF_TOKEN AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" PATH="$fakebin:$PATH" \
      bash "$LAUNCHER" update 2>&1); rc=$?
if [ "$rc" -ne 0 ] && printf '%s' "$out" | grep -q "still runs the old build"; then
  pass "update without CF_TOKEN warns instead of stopping the engine"
else
  fail "update without CF_TOKEN (rc=$rc, out=$out)"
fi
if kill -0 "$pid" 2>/dev/null; then
  pass "the engine is still up after a tokenless update"
else
  fail "tokenless update killed the engine (pid=$pid)"
fi
AIXODIA_HOME="$home" bash "$LAUNCHER" stop >/dev/null 2>&1

# Stub engine that speaks the in-place update handshake. It execs into its
# server so the pid the launcher started is the pid /healthz reports — the real
# engine keeps its pid across an update by execing itself, and the launcher
# refuses to signal anything whose pid does not match the pidfile.
make_handshake_engine() {
  cat > "$1" <<'STUB'
#!/usr/bin/env bash
if [ "${1:-}" = "update" ]; then
  echo "installed the build published 2026-10-21T00:00:00Z"
  exit 0
fi
echo "engine started"
echo "tunnel url: https://stub-tunnel.trycloudflare.com"
echo "daemon url saved"
exec python3 - <<'PY'
import http.server, json, os, signal

pid = os.getpid()

class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = json.dumps({"ok": True, "version": "aixodia-engine/1.1 started=1",
                           "pid": pid, "update": "signal:USR1", "idle": True}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass

def on_usr1(signum, frame):
    # The two lines the launcher waits for: the build is swapped and the same
    # pid carries on. A stub that is current prints the first line instead.
    print("selfupdate: installing the build published 2026-10-21T00:00:00Z", flush=True)
    print("engine started", flush=True)

srv = http.server.HTTPServer(("127.0.0.1", 0), H)
print("listening on %d" % srv.server_port, flush=True)
signal.signal(signal.SIGUSR1, on_usr1)
srv.serve_forever()
PY
STUB
  chmod +x "$1"
}

# --- update asks a handshake-capable engine to swap instead of restarting ----
home=$(tmp_home); fakebin=$(tmp_home); bin=$fakebin/aixodia-engine
make_cloudflared "$fakebin"; make_handshake_engine "$bin"
error_log=$home/stub.err
CF_TOKEN=x AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" AIXODIA_TUNNEL_WAIT=15 \
  PATH="$fakebin:$PATH" bash "$LAUNCHER" start >"$home/start.out" 2>"$error_log"
pid=$(cat "$home/aixodia.pid" 2>/dev/null || echo "")
# The stub serves /healthz on a port it picks itself and prints into the log.
port=$(wait_for_port "$home/aixodia.log")
if [ -n "$port" ]; then
  out=$(CF_TOKEN=x AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" AIXODIA_LOCAL_URL="http://127.0.0.1:$port" \
        AIXODIA_UPDATE_WAIT=10 PATH="$fakebin:$PATH" bash "$LAUNCHER" update 2>&1); rc=$?
  if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q "asking the running engine (pid $pid) to update in place"; then
    pass "update asks the running engine instead of stopping it"
  else
    fail "update in place (rc=$rc, out=$out)"
  fi
  if printf '%s' "$out" | grep -q "updated in place (pid $pid)"; then
    pass "update reports the in-place swap"
  else
    fail "update did not report the swap (out=$out)"
  fi
  if [ "$(cat "$home/aixodia.pid")" = "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    pass "the engine keeps its pid across an in-place update"
  else
    fail "in-place update changed the process (pid=$pid)"
  fi
else
  fail "handshake stub did not report a port (log: $(tail -3 "$home/aixodia.log" 2>/dev/null))"
fi
AIXODIA_HOME="$home" bash "$LAUNCHER" stop >/dev/null 2>&1

# --- a pidfile that disagrees with /healthz is not signalled -----------------
home=$(tmp_home); fakebin=$(tmp_home); bin=$fakebin/aixodia-engine
make_cloudflared "$fakebin"; make_handshake_engine "$bin"
CF_TOKEN=x AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" AIXODIA_TUNNEL_WAIT=15 \
  PATH="$fakebin:$PATH" bash "$LAUNCHER" start >/dev/null 2>&1
engine_pid=$(cat "$home/aixodia.pid" 2>/dev/null || echo "")
port=$(wait_for_port "$home/aixodia.log")
echo "$$" > "$home/aixodia.pid"   # claims to be this test, not the engine
if [ -n "$port" ]; then
  out=$(env -u CF_TOKEN AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" \
        AIXODIA_LOCAL_URL="http://127.0.0.1:$port" PATH="$fakebin:$PATH" bash "$LAUNCHER" update 2>&1); rc=$?
  if printf '%s' "$out" | grep -q "does not answer the update handshake"; then
    pass "a pidfile that disagrees with /healthz falls back to a restart"
  else
    fail "pidfile mismatch (rc=$rc, out=$out)"
  fi
  # The mismatch must not have been signalled: the engine is still the one that
  # was started, and the tokenless fallback left it running.
  if kill -0 "$engine_pid" 2>/dev/null; then
    pass "a mismatched pidfile never signals the engine"
  else
    fail "the engine was killed by a mismatched pidfile (pid=$engine_pid)"
  fi
else
  fail "handshake stub did not report a port for the mismatch case"
fi
kill "$engine_pid" 2>/dev/null
rm -f "$home/aixodia.pid"

# --- the bare command names the build that is running ------------------------
home=$(tmp_home); fakebin=$(tmp_home); bin=$fakebin/aixodia-engine
make_cloudflared "$fakebin"; make_handshake_engine "$bin"
CF_TOKEN=x AIXODIA_HOME="$home" AIXODIA_ENGINE_BIN="$bin" AIXODIA_TUNNEL_WAIT=15 \
  PATH="$fakebin:$PATH" bash "$LAUNCHER" start >/dev/null 2>&1
port=$(wait_for_port "$home/aixodia.log")
if [ -n "$port" ]; then
  out=$(AIXODIA_HOME="$home" AIXODIA_LOCAL_URL="http://127.0.0.1:$port" bash "$LAUNCHER" 2>&1); rc=$?
  if [ "$rc" -eq 0 ] && printf '%s' "$out" | grep -q "aixodia-engine/1.1" \
     && printf '%s' "$out" | grep -q "^usage: aixodia"; then
    pass "aixodia with no command reports the running version and the usage"
  else
    fail "bare aixodia with an engine up (rc=$rc, out=$out)"
  fi
else
  fail "handshake stub did not report a port for the version case (log: $(tail -3 "$home/aixodia.log" 2>/dev/null))"
fi
AIXODIA_HOME="$home" bash "$LAUNCHER" stop >/dev/null 2>&1

echo
if [ "$fails" -eq 0 ]; then
  echo "all launcher tests passed"
else
  echo "$fails launcher test(s) failed"
  exit 1
fi
