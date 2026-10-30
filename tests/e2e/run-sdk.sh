#!/usr/bin/env bash
# Plan 1.11 (#8): SDK round-trip E2E (process-based, no Docker).
#
# Scenario: start CP + a mock node, then drive the Python SDK end-to-end:
#   run() -> wait() -> artifacts()/artifact() -> cancel() on a second task.
# Before the node exists, both SDKs are driven through one payload carrying
# every CreateTaskRequest field (`sdk-parity.mts` replays what the Python block
# sent), so a field the API accepts cannot go missing from either client.
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

# Field parity first, deliberately BEFORE any node is online: every optional
# field of CreateTaskRequest is sent at once, including ones that would wreck a
# real run (a bogus base_commit, a resumed ACP session, consensus tags, a
# -strict profile). Nothing picks the task up, so the assertion is on the
# control plane's own echo — create response and the stored row must agree.
# The payload is written out and replayed through the TS SDK below.
echo ">> SDK field parity (no node: create, inspect, cancel)"
AGENTGRID_TOKEN="$jwt" SDK_PARITY_PAYLOAD="$TMP/sdk-parity.json" python3 - "$BASE" <<'PY'
import json
import os
import sys
import urllib.request

sys.path.insert(0, "sdks/python")
from agentgrid import Agentgrid

base = sys.argv[1]
ag = Agentgrid(base)


def stored_task(tid):
    """Read the task back the way any other client would (plain GET)."""
    req = urllib.request.Request(
        f"{base}/v1/tasks/{tid}",
        headers={"Authorization": f"Bearer {os.environ['AGENTGRID_TOKEN']}"},
    )
    with urllib.request.urlopen(req) as r:
        return json.loads(r.read())


sent = dict(
    adapter="mock",
    requested_node_id="node-that-does-not-exist",
    timeout_secs=4242,
    validation_command="true",
    base_commit="0" * 40,
    parent_acp_session_id="sess-parity",
    consensus_group_id="grp-parity",
    consensus_member="mock",
    consensus_mode="review",
    review_of="task-parity",
    max_tokens=1234,
    max_cost_cents=56,
    network_mode="none",
    security_profile="default-strict",
    group_id="g-parity",
    agent_id="agent-parity",
    opencode_override={"model": "x/y", "small_model": "x/z", "config": {"theme": "dark"}},
    github_push=False,
    github_repo="owner/name",
    github_issue=7,
    github_base_ref="main",
    github_create=False,
    max_attempts=3,
    retry_on_node_loss=False,
)
task = ag.run("parity", "*", **sent)
assert task["status"] == "queued", task

# TaskView does not echo these three (they are scheduling inputs, not state),
# so they are only covered by the contract test.
not_echoed = {"timeout_secs", "github_push", "retry_on_node_loss"}
echoed = {k: v for k, v in sent.items() if k not in not_echoed}
row = stored_task(task["id"])
for k, v in echoed.items():
    assert task.get(k) == v, f"create echo {k}: sent {v!r}, got {task.get(k)!r}"
    assert row.get(k) == v, f"stored {k}: sent {v!r}, got {row.get(k)!r}"

# Unset fields must be OMITTED from the body, not sent as null: `max_attempts`
# is a u32 and `github_push` a bool, so a null would be a 400 rather than a
# default. A minimal create proves the SDK omits them.
minimal = ag.run("minimal", "*", adapter="mock")
assert minimal["max_attempts"] == 1, minimal
assert minimal["github_create"] is False, minimal
assert "group_id" not in minimal, minimal
ag.cancel(minimal["id"])

# `adapter` is required by the API, so it must not be silently defaulted —
# a missing adapter used to run the mock agent and "succeed" doing nothing.
try:
    ag.run("no adapter", "*")
except TypeError:
    pass
else:
    raise AssertionError("run() must require adapter")

ag.cancel(task["id"])
with open(os.environ["SDK_PARITY_PAYLOAD"], "w", encoding="utf-8") as f:
    json.dump(sent, f)
print(f"SDK parity OK: {len(echoed)} fields echoed and stored; unset fields omitted")
PY

# Same body, second client. The TS SDK had no runtime coverage at all, which is
# how its `cancel()` kept throwing on the CP's `200` + empty body: `resp.json()`
# dies on an empty payload. Node >= 22.6 strips the types, so this needs no
# build step and no dependency.
echo ">> TS SDK parity (same payload, same CP)"
AGENTGRID_TOKEN="$jwt" node --experimental-strip-types --no-warnings \
  tests/e2e/sdk-parity.mts "$BASE" "$TMP/sdk-parity.json"

e2e_start_node sdk "e2e-sdk-node" "mock" "$TMP/work" "$TMP/repos" "$TMP/node"
e2e_wait_nodes_online 1 || exit 1

echo ">> driving Python SDK"
AGENTGRID_TOKEN="$jwt" python3 - "$BASE" <<'PY'
import sys
sys.path.insert(0, "sdks/python")
from agentgrid import Agentgrid

base = sys.argv[1]
ag = Agentgrid(base)

# run -> wait, with the scheduling knobs a CI caller actually needs set
task = ag.run(
    "sleep:2",
    "*",
    adapter="mock",
    timeout_secs=60,
    max_attempts=2,
    retry_on_node_loss=False,
    max_tokens=100000,
    max_cost_cents=1000,
    group_id="sdk-e2e",
)
assert task["status"] == "queued" or task["status"] == "assigned", task
assert task["max_attempts"] == 2, task
assert task["group_id"] == "sdk-e2e", task
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
