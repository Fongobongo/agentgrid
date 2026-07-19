#!/usr/bin/env bash
# Stage 8 E2E: two containers, one workflow's roles spread across both nodes.
# Brings up control-plane + node-1 + node-2, defines a workflow that pins the
# worker steps to node A and the integrator + verifier to node B, runs it, and
# asserts it reaches `succeeded`. Tears the stack down on exit.
#
# The same workflow manifest runs unchanged on one machine or two; the only
# difference is placement (requested_node_id) and the resulting provenance
# shown by the projection endpoint. This is the Stage 8 release gate.
set -euo pipefail

cd "$(dirname "$0")/../.."

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7800}"
USER="${AGENTGRID_BOOTSTRAP_USER:-admin}"
PASS="${AGENTGRID_BOOTSTRAP_PASSWORD:-changeme}"
TIMEOUT="${E2E_TIMEOUT:-120}"

# Build images if missing so the script is self-contained in CI.
docker image inspect ag-cp:test >/dev/null 2>&1 || docker build -t ag-cp:test -f Dockerfile.control-plane .
docker image inspect ag-node:test >/dev/null 2>&1 || docker build -t ag-node:test -f Dockerfile.node-daemon .

cleanup() { bash deploy/compose/down.sh; }
trap cleanup EXIT

echo ">> bringing up stack (control plane + 2 nodes)"
bash deploy/compose/up.sh >/dev/null

echo ">> waiting for health"
for _ in $(seq 1 30); do
  curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && break
  sleep 1
done
curl -fsS "$BASE/health/ready" >/dev/null || { echo "control plane never became ready"; exit 1; }

JWT=$(curl -fsS -X POST "$BASE/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
[ -n "$JWT" ] || { echo "login failed"; exit 1; }

echo ">> discovering node ids"
readarray -t NODES < <(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $JWT" \
  | python3 -c 'import sys,json;print("\n".join(n["id"] for n in json.load(sys.stdin)))')
[ "${#NODES[@]}" -ge 2 ] || { echo "expected >=2 enrolled nodes, got ${#NODES[@]}"; exit 1; }
NODE_A="${NODES[0]}"; NODE_B="${NODES[1]}"
echo ">> node A = $NODE_A   node B = $NODE_B"

echo ">> defining workflow (workers on A, integrator+verifier on B)"
DEF=$(python3 -c 'import json,sys
a,b=sys.argv[1],sys.argv[2]
steps=[
  {"id":"arch","prompt":"design","role":"architect","depends_on":[]},
  {"id":"w1","prompt":"impl a","role":"worker","depends_on":["arch"],"requested_node_id":a},
  {"id":"w2","prompt":"impl b","role":"worker","depends_on":["arch"],"requested_node_id":a},
  {"id":"int","prompt":"merge","role":"integrator","depends_on":["w1","w2"],"requested_node_id":b},
  {"id":"ver","prompt":"verify","role":"verifier","depends_on":["int"],"requested_node_id":b},
]
print(json.dumps({"name":"e2e-2node","steps":steps}))' "$NODE_A" "$NODE_B")
TID=$(curl -fsS -X POST "$BASE/v1/workflows" \
  -H "authorization: Bearer $JWT" -H 'content-type: application/json' -d "$DEF" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

echo ">> starting run"
RID=$(curl -fsS -X POST "$BASE/v1/workflows/$TID/runs" \
  -H "authorization: Bearer $JWT" -H 'content-type: application/json' -d '{"repository":"demo"}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

echo ">> ticking workflow (nodes run mock adapters via long-poll)"
status=""
for _ in $(seq 1 "$TIMEOUT"); do
  curl -fsS -X POST "$BASE/v1/workflow-runs/$RID/tick" -H "authorization: Bearer $JWT" >/dev/null
  status=$(curl -fsS "$BASE/v1/workflow-runs/$RID" -H "authorization: Bearer $JWT" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["run"]["status"])')
  case "$status" in
    succeeded|failed|cancelled|blocked) break;;
  esac
  sleep 2
done

echo ">> final status: $status"
echo ">> step provenance (projection):"
curl -fsS "$BASE/v1/workflow-runs/$RID/projection" -H "authorization: Bearer $JWT" \
  | python3 -c 'import sys,json
for s in json.load(sys.stdin)["steps"]:
    print(f"  step {s[\"step_id\"]:4} role={s[\"role\"]:11} node={s.get(\"node_id\")} verdict={s[\"verdict\"]}")'

[ "$status" = "succeeded" ] || { echo "E2E FAILED: run $RID -> $status"; exit 1; }
echo "E2E OK: workflow ran across two nodes"
