#!/usr/bin/env bash
# E2E: one workflow manifest runs unchanged on two execution backends
# (Stage 12 / Exit 12 shape, process-based CP+node, docker only for the
# isolated leg).
#
# Leg 1 (native): node with AGENTGRID_SANDBOX=none runs a 2-step mock
# workflow (worker → verifier) to `succeeded`. Leg 2 (isolated): the SAME
# template id starts a second run while the node restarts under
# AGENTGRID_SANDBOX=docker (image with the mock adapter inside) — the run
# must reach `succeeded` with the same step verdicts. Only placement (the
# backend) differs, never the manifest.
#
# Requires docker + $IMAGE (default ag-node:test, built by the compose-job
# run.sh); skips (exit 77) without them. A real-binary variant needs no
# changes — the manifest is backend-agnostic by construction.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"
IMAGE="${AGENTGRID_SANDBOX_IMAGE:-ag-node:test}"

command -v docker >/dev/null 2>&1 || { echo "skip: docker not on PATH (isolated leg needs it)"; exit 77; }
docker image inspect "$IMAGE" >/dev/null 2>&1 || { echo "skip: image $IMAGE missing (build it via the compose job first)"; exit 77; }

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7818}"
PORT="${AGENTGRID_PORT:-7818}"
USER="admin"
PASS="changeme"
source "$ROOT/tests/e2e/lib-bootstrap.sh"

TMP="$(mktemp -d -t ag-e2e-wfbe-XXXXXX)"
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
  [ -n "$CP_PID" ] && kill "$CP_PID" 2>/dev/null
  pkill -f "$BIN/agentgrid-control-plane" 2>/dev/null
  pkill -f "$BIN/agentgrid-node-daemon" 2>/dev/null
  sleep 0.3
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

wait_ready() {
  for _ in $(seq 1 40); do
    curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}

login() {
  jwt=$(curl -fsS -X POST "$BASE/v1/auth/login" \
    -H 'content-type: application/json' \
    -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  [ -n "$jwt" ] || { echo "login failed"; cat "$TMP/cp.log"; exit 1; }
}

mint_token() {
  ENROLL_TOKEN=$(curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" \
    -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  [ -n "$ENROLL_TOKEN" ] || { echo "mint token failed"; exit 1; }
}

start_cp() {
  AGENTGRID_LISTEN="127.0.0.1:$PORT" \
  AGENTGRID_DB="$CP_DB" \
  AGENTGRID_JWT_SECRET="e2e-wfbe-secret" \
  AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
  nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
  CP_PID=$!
}

# $1 = sandbox mode (none|docker), $2 = log tag
start_node() {
  local extra=()
  if [ "$1" = "docker" ]; then
    extra+=(AGENTGRID_SANDBOX=docker "AGENTGRID_SANDBOX_IMAGE=$IMAGE")
  else
    extra+=(AGENTGRID_SANDBOX=none)
  fi
  env PATH="$BIN:$PATH" \
    AGENTGRID_SERVER="$BASE" \
    AGENTGRID_DATA_DIR="$NODE_DATA" \
    AGENTGRID_NODE_NAME="e2e-wfbe" \
    AGENTGRID_WORKSPACE_ROOT="$WORK" \
    AGENTGRID_REPOSITORY_ROOT="$REPOS" \
    AGENTGRID_ADAPTERS="mock" \
    AGENTGRID_MAX_CONCURRENCY="1" \
    RUST_LOG="info" \
    AGENTGRID_ENROLL_TOKEN="$ENROLL_TOKEN" \
    "${extra[@]}" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/node-$2.log" 2>&1 &
  NODE_PID=$!
}

stop_node() {
  [ -n "$NODE_PID" ] && kill -9 "$NODE_PID" 2>/dev/null || true
  wait "$NODE_PID" 2>/dev/null || true
  NODE_PID=""
  sleep 1
}

wait_node_online() {
  for _ in $(seq 1 60); do
    st=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" 2>/dev/null \
      | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["status"] if ns else "none")' 2>/dev/null) || st="none"
    [ "$st" = "online" ] && return 0
    sleep 0.5
  done
  echo "node never came online; status=$st"; return 1
}

# Same manifest both legs: worker writes a file, verifier checks it.
# Verifier steps run read-only, so the verifier prompt must not write.
make_template() {
  python3 -c 'import json;print(json.dumps({"name":"e2e-backends","steps":[
    {"id":"w","prompt":"write:hello.txt:backend-ok","role":"worker","depends_on":[]},
    {"id":"v","prompt":"verify hello.txt exists","role":"verifier","depends_on":["w"]}]}))'
}

run_to_terminal() {  # $1 = template id, $2 = leg tag; prints final status
  local rid status
  rid=$(curl -fsS -X POST "$BASE/v1/workflows/$1/runs" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' -d '{}' \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
  status=""
  for _ in $(seq 1 150); do
    curl -fsS -X POST "$BASE/v1/workflow-runs/$rid/tick" -H "authorization: Bearer $jwt" >/dev/null
    status=$(curl -fsS "$BASE/v1/workflow-runs/$rid" -H "authorization: Bearer $jwt" \
      | python3 -c 'import sys,json;print(json.load(sys.stdin)["run"]["status"])')
    case "$status" in
      succeeded|failed|cancelled|blocked) break;;
    esac
    sleep 2
  done
  echo "$status $rid"
}

echo ">> workflow-backends E2E (image $IMAGE for the isolated leg)"
start_cp
wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
login
mint_token

echo ">> defining the shared manifest once"
TID=$(curl -fsS -X POST "$BASE/v1/workflows" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' -d "$(make_template)" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
echo "  template $TID"

echo ">> leg 1: native backend (sandbox=none)"
start_node none native
wait_node_online || { cat "$TMP/node-native.log"; exit 1; }
read -r ST1 R1 <<<"$(run_to_terminal "$TID" native)"
[ "$ST1" = "succeeded" ] || { echo "  FAILED: native leg -> $ST1"; exit 1; }
echo "  native leg succeeded (run $R1)"

echo ">> leg 2: isolated backend (sandbox=docker)"
stop_node
mint_token
start_node docker isolated
wait_node_online || { cat "$TMP/node-isolated.log"; exit 1; }
read -r ST2 R2 <<<"$(run_to_terminal "$TID" isolated)"
[ "$ST2" = "succeeded" ] || { echo "  FAILED: isolated leg -> $ST2"; cat "$TMP/node-isolated.log"; exit 1; }
echo "  isolated leg succeeded (run $R2)"

echo ">> comparing step verdicts across backends"
V1=$(curl -fsS "$BASE/v1/workflow-runs/$R1/projection" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print(",".join(s["step_id"]+"="+s["verdict"] for s in json.load(sys.stdin)["steps"]))')
V2=$(curl -fsS "$BASE/v1/workflow-runs/$R2/projection" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json;print(",".join(s["step_id"]+"="+s["verdict"] for s in json.load(sys.stdin)["steps"]))')
echo "  native:   $V1"
echo "  isolated: $V2"
[ "$V1" = "$V2" ] || { echo "  FAILED: step verdicts differ across backends"; exit 1; }

echo ">> workflow-backends E2E OK (same manifest, same verdicts, two backends)"
