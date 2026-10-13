#!/usr/bin/env bash
# E2E (process-based, no Docker, no GitHub): project assembly across hosts.
#
# One workflow — two workers on DIFFERENT nodes, each writing its own part
# into a shared local-path repo, then a collector step merging both parts
# into one commit. Exercises the whole decompose → distribute → collect loop:
# per-node mirrors (cherry-pick where objects are local, patch-bundle
# fallback across hosts), finalize commit, and changes.patch artifacts.
#
# No GitHub remote here (no token in CI): the remote-create/push path is
# unit-covered against a dummy API (`github::tests`) and shares the push
# code with the long-standing write-back. With a token + `gh`, the same
# manifest plus run-level github_push/github_repo/github_create publishes
# the assembled project (see ADR 0019).
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7819}"
PORT="${AGENTGRID_PORT:-7819}"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-assemble-secret"
E2E_CONCURRENCY=2

TMP="$(mktemp -d -t ag-e2e-assemble-XXXXXX)"
mkdir -p "$TMP/n1" "$TMP/w1" "$TMP/r1" "$TMP/n2" "$TMP/w2" "$TMP/r2"

# Shared local-path repo both nodes clone (file path = offline "remote").
git init -q -b main "$TMP/origin"
git -C "$TMP/origin" -c user.name=t -c user.email=t@x commit -q --allow-empty -m init

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

echo ">> assemble E2E: two workers on two nodes + collector"
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
e2e_start_node n1 "e2e-assemble-n1" "mock" "$TMP/w1" "$TMP/r1" "$TMP/n1"
# Enrollment tokens are one-shot: n1 consumed the minted token, so n2 needs
# a freshly minted one (same pattern as run-cp-restart.sh).
e2e_mint_token
e2e_start_node n2 "e2e-assemble-n2" "mock" "$TMP/w2" "$TMP/r2" "$TMP/n2"
e2e_wait_nodes_online 2 || exit 1

echo ">> registering the shared local repo"
curl -fsS -X POST "$BASE/v1/repositories" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d "{\"name\":\"asm\",\"git_url\":\"$TMP/origin\",\"default_branch\":\"main\"}" >/dev/null \
  || { echo "repo register failed"; exit 1; }

echo ">> discovering node ids"
readarray -t NODES < <(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print("\n".join(n["id"] for n in (lambda d: d.get("items",d) if isinstance(d,dict) else d)(json.load(sys.stdin))))')
NODE_A="${NODES[0]}"; NODE_B="${NODES[1]}"
echo "  A=$NODE_A B=$NODE_B"

echo ">> defining the assembly manifest (w1 on A, w2 on B, collector merges)"
DEF=$(python3 -c 'import json,sys
a,b=sys.argv[1],sys.argv[2]
steps=[
  {"id":"w1","prompt":"write:part-a.txt:AAA","role":"worker","depends_on":[],"requested_node_id":a},
  {"id":"w2","prompt":"write:part-b.txt:BBB","role":"worker","depends_on":[],"requested_node_id":b},
  {"id":"c","prompt":"assemble the parts","role":"collector","depends_on":["w1","w2"]},
]
print(json.dumps({"name":"e2e-assemble","steps":steps}))' "$NODE_A" "$NODE_B")
TID=$(curl -fsS -X POST "$BASE/v1/workflows" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' -d "$DEF" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

echo ">> starting run"
RID=$(curl -fsS -X POST "$BASE/v1/workflows/$TID/runs" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' -d '{"repository":"asm"}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

echo ">> ticking to terminal"
status=""
for _ in $(seq 1 150); do
  curl -fsS -X POST "$BASE/v1/workflow-runs/$RID/tick" -H "authorization: Bearer $jwt" >/dev/null
  status=$(curl -fsS "$BASE/v1/workflow-runs/$RID" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["run"]["status"])')
  case "$status" in
    succeeded|failed|cancelled|blocked) break;;
  esac
  sleep 2
done
[ "$status" = "succeeded" ] || { echo "  FAILED: run $RID -> $status"; exit 1; }
echo "  run succeeded"

echo ">> collector task must carry both parts in its patch"
CTID=$(curl -fsS "$BASE/v1/workflow-runs/$RID/projection" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;ss=json.load(sys.stdin)["steps"];print([s["task_id"] for s in ss if s["step_id"]=="c"][0])')
[ -n "$CTID" ] && [ "$CTID" != "None" ] || { echo "  FAILED: collector has no task"; exit 1; }
curl -fsS "$BASE/v1/tasks/$CTID/artifacts/changes.patch" -H "authorization: Bearer $jwt" -o "$TMP/collector.patch" \
  || { echo "  FAILED: no collector patch artifact"; exit 1; }
grep -q "part-a.txt" "$TMP/collector.patch" || { echo "  FAILED: part-a missing from collector patch"; exit 1; }
grep -q "part-b.txt" "$TMP/collector.patch" || { echo "  FAILED: part-b missing from collector patch"; exit 1; }
grep -q "AAA" "$TMP/collector.patch" || { echo "  FAILED: part-a content missing"; exit 1; }
grep -q "BBB" "$TMP/collector.patch" || { echo "  FAILED: part-b content missing"; exit 1; }
echo "  collector patch carries both parts with contents"

echo ">> assemble E2E OK (decompose, distribute, collect, merge)"
