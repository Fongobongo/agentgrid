#!/usr/bin/env bash
# Regression: deleting a running control plane's SQLite database must make
# the daemon exit, release its instance lock, and close the stdout descriptor.
#
# Incident this guards: a temp-dir CP kept running after its DB directory was
# deleted. SQLite reconnects failed every few seconds and warnings appended to
# an unlinked log inode — 703 MiB invisible to `du`, still consuming disk.
#
# Test:
#   1. start CP on a temp DB and wait for readiness;
#   2. delete the DB while the process runs;
#   3. assert the process exits non-zero within a bounded time;
#   4. assert the instance lock can be acquired again (flock released);
#   5. assert the log file did not keep growing after exit.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"
PORT="${AGENTGRID_PORT:-7891}"
BASE="http://127.0.0.1:$PORT"
TMP="$(mktemp -d -t ag-runtime-cleanup-XXXXXX)"
DB="$TMP/cp.db"
LOG="$TMP/cp.log"
CP_PID=""

cleanup() {
  set +e
  if [ -n "$CP_PID" ]; then
    kill -TERM -- "-$CP_PID" 2>/dev/null || kill -TERM "$CP_PID" 2>/dev/null
    sleep 0.2
    kill -KILL -- "-$CP_PID" 2>/dev/null || kill -9 "$CP_PID" 2>/dev/null
    wait "$CP_PID" 2>/dev/null
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

[ -x "$BIN/agentgrid-control-plane" ] || {
  echo "build debug binaries first (agentgrid-control-plane missing)" >&2
  exit 1
}

# Own process group, so cleanup reaches helper children too.
setsid bash -c 'echo $$ > "$1"; shift; exec "$@"' _ "$TMP/cp.pid" -- \
  env AGENTGRID_LISTEN="127.0.0.1:$PORT" \
  AGENTGRID_DB="$DB" \
  AGENTGRID_JWT_SECRET="runtime-cleanup-secret-32-bytes-minimum" \
  AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
  "$BIN/agentgrid-control-plane" >"$LOG" 2>&1 &
i=0
while [ ! -s "$TMP/cp.pid" ] && [ "$i" -lt 100 ]; do
  sleep 0.05
  i=$((i + 1))
done
[ -s "$TMP/cp.pid" ] || { echo "control plane did not report pid" >&2; exit 1; }
CP_PID=$(cat "$TMP/cp.pid")

ready=0
for _ in $(seq 1 60); do
  if curl -fsS "$BASE/health/ready" >/dev/null 2>&1; then
    ready=1
    break
  fi
  kill -0 "$CP_PID" 2>/dev/null || break
  sleep 0.25
done
[ "$ready" = "1" ] || {
  echo "control plane never became ready" >&2
  cat "$LOG" >&2
  exit 1
}

rm -f "$DB"

exited=0
for _ in $(seq 1 80); do
  if ! kill -0 "$CP_PID" 2>/dev/null; then
    exited=1
    break
  fi
  sleep 0.25
done
if [ "$exited" != "1" ]; then
  echo "control plane kept running after its database was deleted" >&2
  cat "$LOG" >&2
  exit 1
fi

set +e
wait "$CP_PID"
status=$?
set -e
CP_PID=""
[ "$status" -ne 0 ] || {
  echo "control plane exited zero after runtime files vanished" >&2
  exit 1
}

# The flock must be released. Taking the same lock proves it.
python3 - "$DB.lock" <<'PY'
import fcntl, os, sys
path = sys.argv[1]
fd = os.open(path, os.O_RDWR | os.O_CREAT, 0o600)
try:
    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
except BlockingIOError:
    print("instance lock still held after exit", file=sys.stderr)
    raise SystemExit(1)
finally:
    os.close(fd)
PY

# The log must not have grown after exit.
size_after=$(stat -c%s "$LOG")
sleep 0.5
size_later=$(stat -c%s "$LOG")
[ "$size_after" = "$size_later" ] || {
  echo "log kept growing after control-plane exit" >&2
  exit 1
}

echo "E2E OK (deleted DB -> CP exits, lock released, log stopped)"
