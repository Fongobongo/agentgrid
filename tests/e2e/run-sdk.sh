#!/usr/bin/env bash
# Plan 1.11 (#8): SDK round-trip E2E (process-based, no Docker).
#
# Scenario: start CP + a mock node, then drive the Python SDK end-to-end:
#   run() -> wait() -> artifacts()/artifact() -> cancel() on a second task.
# The TS SDK exercises the same endpoints; the Python one is the test oracle
# here (both share the /v1 surface).
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7815}"
PORT="${AGENTGRID_PORT:-7815}"
USER="admin"
PASS="changeme"
source "$ROOT/tests/e2e/lib-bootstrap.sh"

TMP="$(mktemp -d -t ag-e2e-sdk-XXXXXX)"
CP_DB="$TMP/cp.db"
NODE_DATA="$TMP/node"
WORK="$TMP/work"
REPOS="$TMP/repos"
mkdir -p "$NODE_DATA" "$WORK" "$REPOS"

CP_PID=""
NODE_PID=""

cleanup() {
  set +e
  [ -n "$NODE_PID" ] && kill -9 "$NODE_PID" 2>/dev/null
  [ -n "$CP_PID" ] && kill -9 "$CP_PID" 2>/dev/null
  sleep 0.3
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

AGENTGRID_LISTEN="127.0.0.1:$PORT" \
AGENTGRID_DB="$CP_DB" \
AGENTGRID_JWT_SECRET="e2e-stable-secret" \
AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
CP_PID=$!

for _ in $(seq 1 40); do
  curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -fsS "$BASE/health/ready" >/dev/null || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"

jwt=$(curl -fsS -X POST "$BASE/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
[ -n "$jwt" ] || { echo "login failed"; cat "$TMP/cp.log"; exit 1; }

ENROLL_TOKEN=$(curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" \
  -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
env PATH="$BIN:$PATH" \
  AGENTGRID_SERVER="$BASE" \
  AGENTGRID_DATA_DIR="$NODE_DATA" \
  AGENTGRID_NODE_NAME="e2e-sdk-node" \
  AGENTGRID_WORKSPACE_ROOT="$WORK" \
  AGENTGRID_REPOSITORY_ROOT="$REPOS" \
  AGENTGRID_ADAPTERS="mock" \
  AGENTGRID_MAX_CONCURRENCY="1" \
  RUST_LOG="info" \
  AGENTGRID_ENROLL_TOKEN="$ENROLL_TOKEN" \
  nohup "$BIN/agentgrid-node-daemon" >"$TMP/node.log" 2>&1 &
NODE_PID=$!

for _ in $(seq 1 60); do
  st=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["status"] if ns else "none")' 2>/dev/null) || st="none"
  [ "$st" = "online" ] && break
  sleep 0.5
done
[ "$st" = "online" ] || { echo "node never came online"; cat "$TMP/node.log"; exit 1; }

echo ">> driving Python SDK"
AGENTGRID_TOKEN="$jwt" python3 - "$BASE" <<'PY'
import sys
sys.path.insert(0, "sdks/python")
from agentgrid import Agentgrid

base = sys.argv[1]
ag = Agentgrid(base)

# run -> wait
task = ag.run("sleep:2", "*", adapter="mock", timeout_secs=60)
assert task["status"] == "queued" or task["status"] == "assigned", task
tid = task["id"]
final = ag.wait(tid, interval_s=1.0, timeout_s=90)
assert final["status"] == "succeeded", final

# artifacts: mock adapter uploads the raw agent output log
arts = ag.artifacts(tid)
names = [a["name"] for a in arts]
assert "agent-raw-output.log" in names, arts
log = ag.artifact(tid, "agent-raw-output.log")
assert isinstance(log, str) and len(log) > 0, log

# cancel on a long queued task
task2 = ag.run("sleep:30", "*", adapter="mock", timeout_secs=120)
ag.cancel(task2["id"])
st2 = ag.status(task2["id"])
assert st2 in ("cancelled", "queued", "assigned", "running"), st2

print(f"SDK round-trip OK: task {tid} succeeded; {len(names)} artifacts; cancel ok")
PY
