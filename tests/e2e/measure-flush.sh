#!/usr/bin/env bash
# One-off throughput probe (not CI): time a chatty mock task end-to-end.
set -euo pipefail
cd "$(dirname "$0")/../.."
BIN="$PWD/target/debug"
BASE="http://127.0.0.1:7831"; PORT=7831
source tests/e2e/lib-bootstrap.sh
TMP="$(mktemp -d -t ag-flush-measure-XXXXXX)"
trap 'kill $(cat $TMP/pids) 2>/dev/null; sleep 0.3; rm -rf "$TMP"' EXIT

AGENTGRID_LISTEN="127.0.0.1:$PORT" AGENTGRID_DB="$TMP/cp.db" \
AGENTGRID_JWT_SECRET="e2e-flush-secret" AGENTGRID_ARTIFACT_ROOT="$TMP/art" \
nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 & echo -n "$! " >>$TMP/pids
for _ in $(seq 1 40); do curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && break; sleep 0.5; done
bootstrap_first_user "$TMP/cp.log" "$BASE" admin changeme
jwt=$(curl -fsS -X POST "$BASE/v1/auth/login" -H 'content-type: application/json' \
  -d '{"username":"admin","password":"changeme"}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
tok=$(curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
env PATH="$BIN:$PATH" AGENTGRID_SERVER="$BASE" AGENTGRID_DATA_DIR="$TMP/n" \
  AGENTGRID_NODE_NAME="m" AGENTGRID_WORKSPACE_ROOT="$TMP/w" AGENTGRID_REPOSITORY_ROOT="$TMP/r" \
  AGENTGRID_ADAPTERS="mock" AGENTGRID_ENROLL_TOKEN="$tok" \
  nohup "$BIN/agentgrid-node-daemon" >"$TMP/n.log" 2>&1 & echo "$!" >>$TMP/pids
for _ in $(seq 1 60); do
  st=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["status"] if ns else "none")' 2>/dev/null) || st=none
  [ "$st" = online ] && break; sleep 0.5
done

start=$(date +%s)
TID=$(curl -fsS -X POST "$BASE/v1/tasks" -H "authorization: Bearer $jwt" \
  -H 'content-type: application/json' \
  -d '{"prompt":"spam:20000","repository":"*","adapter":"mock","timeout_secs":600}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
while :; do
  st=$(curl -fsS "$BASE/v1/tasks/$TID" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
  [ "$st" = succeeded ] && break
  [ "$st" = failed ] && { echo FAILED; break; }
  sleep 2
done
end=$(date +%s)
echo "20000 events: $((end-start))s => $((20000 / (end-start) )) events/s end-to-end"
curl -fsS "$BASE/metrics" | grep -E "write_txn|write_lock|event_ingest|rate_limit"
