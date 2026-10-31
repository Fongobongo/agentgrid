#!/usr/bin/env bash
# ACP northbound gateway E2E: the real `agentgrid-acp-agent` binary against a
# real control plane and a real node (no Docker).
#
# `agentgrid-acp-agent` is the northbound half of ACP — it lets an external ACP
# client drive Agentgrid: `session/prompt` mints a task on the control plane and
# streams that task's events back as `session/update` until the task is
# terminal. The binary ships in every release tarball, and until this script
# existed nothing ran it against a real CP: the only coverage was four
# in-process unit tests against a fake control plane.
#
# What is asserted:
#   1. the JSON-RPC lifecycle completes over stdio — `initialize` ->
#      `session/new` -> `session/prompt`;
#   2. `session/prompt` streams at least one `session/update` notification and
#      returns `status: succeeded`;
#   3. the task exists on the control plane under the prompt we sent, and a
#      node actually ran it (the ACP answer is the gateway's own view; this is
#      the CP's).
#
# The client half lives in `acp-gateway-client.py`; it is a plain stdio
# JSON-RPC speaker, so the assertions stay readable.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="${AGENTGRID_BIN:-$ROOT/target/debug}"

PORT="${AGENTGRID_PORT:-7833}"
BASE="http://127.0.0.1:$PORT"
USER="admin"
PASS="changeme"
# Unique per run so the CP-side lookup cannot match a task from another script.
PROMPT="acp-gateway-e2e-$(date +%s)-$$"

TMP="$(mktemp -d -t ag-e2e-acp-XXXXXX)"
mkdir -p "$TMP/work" "$TMP/repos" "$TMP/node"

# shellcheck disable=SC1091
source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

e2e_check_bins
if [ ! -x "$BIN/agentgrid-acp-agent" ]; then
  echo "build debug binaries first (agentgrid-acp-agent missing)"
  exit 1
fi

echo "== 1. control plane + one node"
e2e_start_cp
e2e_wait_ready || { echo "control plane did not become ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
e2e_start_node n1 "acp-n1" "mock" "$TMP/work" "$TMP/repos" "$TMP/node"
e2e_wait_nodes_online 1 || { echo "node did not come online"; cat "$TMP/n1.log"; exit 1; }

echo "== 2. ACP lifecycle over stdio"
SUMMARY=$(python3 "$ROOT/tests/e2e/acp-gateway-client.py" \
  --bin "$BIN/agentgrid-acp-agent" \
  --server "$BASE" \
  --token "$jwt" \
  --agent mock \
  --cwd "$TMP/work" \
  --prompt "$PROMPT" \
  --timeout 120) || { echo "ACP client failed"; exit 1; }
echo "  $SUMMARY"

python3 - "$SUMMARY" <<'PY'
import json, sys
s = json.loads(sys.argv[1])
assert s["status"] == "succeeded", f"task did not succeed: {s}"
assert s["updates"] > 0, f"no session/update was streamed: {s}"
assert s["session_id"], f"no session id: {s}"
print(f"  streamed {s['updates']} session/update notification(s), status={s['status']}")
PY

echo "== 3. the control plane agrees, and the node's adapter saw the prompt"
TASK_JSON=$(curl -fsS "$BASE/v1/tasks?limit=100" -H "authorization: Bearer $jwt")
TASK_ID=$(python3 - "$TASK_JSON" "$PROMPT" <<'PY'
import json, sys
data = json.loads(sys.argv[1])
tasks = data["items"] if isinstance(data, dict) else data
match = [t for t in tasks if t.get("prompt") == sys.argv[2]]
assert match, f"no task with the ACP prompt on the control plane ({len(tasks)} tasks)"
print(match[0]["id"])
PY
)
EVENTS=$(curl -fsS "$BASE/v1/tasks/$TASK_ID/events" -H "authorization: Bearer $jwt")
python3 - "$EVENTS" "$PROMPT" "$TASK_ID" <<'PY'
import json, sys
events, prompt, task_id = json.loads(sys.argv[1]), sys.argv[2], sys.argv[3]
assert events, "the task has no events — nothing ran it"
attempts = {e["attempt_id"] for e in events if e.get("attempt_id")}
assert attempts, f"no event carries an attempt id: {events[0]}"
# The adapter echoes the prompt on stdout; seeing it here proves the prompt
# travelled ACP client -> gateway -> CP -> node -> adapter and back, rather
# than the gateway merely echoing what we sent.
echoed = [
    e for e in events
    if prompt in json.dumps(e.get("payload", {}))
]
assert echoed, f"the adapter never echoed {prompt!r}; payloads: " + json.dumps(
    [e.get("payload") for e in events][:6]
)
print(f"  task {task_id}: {len(events)} events from {len(attempts)} attempt(s), prompt echoed by the adapter")
PY

echo "OK: agentgrid-acp-agent drove a task end to end"
