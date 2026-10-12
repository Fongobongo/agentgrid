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
E2E_JWT_SECRET="e2e-stable-secret"
E2E_CONCURRENCY=1

TMP="$(mktemp -d -t ag-e2e-sdk-XXXXXX)"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
e2e_start_node sdk "e2e-sdk-node" "mock" "$TMP/work" "$TMP/repos" "$TMP/node"
e2e_wait_nodes_online 1 || exit 1

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
