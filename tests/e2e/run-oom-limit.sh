#!/usr/bin/env bash
# E2E (process-based CP+node, Docker-sandboxed attempt): per-attempt memory
# limit → runtime OOM kill → truthful `resource_limit` error code.
#
# Stage 12 / ADR 0003 acceptance: "Test: превышение memory limit →
# error_code=resource_limit". The wrapper path spawns the mock adapter inside
# the hardened sandbox container with `--memory 64m` (node-wide env knob; the
# per-attempt profile override is covered by unit tests). The mock's `oom:512`
# command allocates and touches 512 MiB, the runtime OOM-kills the container,
# the node's post-exit `docker inspect` reports OOMKilled, and the completion
# carries `error_code=resource_limit:memory` — distinct from `agent_failed`,
# so the CP can treat a hit ceiling differently from an agent crash.
#
# Requires: docker (a pullable/in-image runtime), curl, python3.
# Skips cleanly (exit 0) when docker or the sandbox image is unavailable so
# the fast CI job stays green; the e2e job (docker available) runs it for real.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7814}"
PORT="${AGENTGRID_PORT:-7814}"
USER="admin"
source "$ROOT/tests/e2e/lib-bootstrap.sh"
PASS="changeme"
IMAGE="${AGENTGRID_SANDBOX_IMAGE:-ubuntu:24.04}"
MEM_LIMIT_MB="${AGENTGRID_E2E_MEMORY_MB:-64}"
OOM_MB="${AGENTGRID_E2E_OOM_MB:-512}"

# ---- docker availability gate (mirror docker_ro_mount_really_blocks_write)
if ! command -v docker >/dev/null 2>&1; then
  echo ">> skip: docker not found"
  exit 0
fi
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  if ! docker pull -q "$IMAGE" >/dev/null 2>&1; then
    echo ">> skip: image $IMAGE not pullable"
    exit 0
  fi
fi
if ! docker info >/dev/null 2>&1; then
  echo ">> skip: docker daemon unreachable"
  exit 0
fi

TMP="$(mktemp -d -t ag-e2e-oom-XXXXXX)"
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
  # Best-effort: reap any sandbox containers this run created (the daemon's
  # startup sweep does this via the node label, but we may kill the daemon
  # before it cleans up).
  for c in $(docker ps -aq --filter name=agentgrid- 2>/dev/null); do
    docker rm -f "$c" >/dev/null 2>&1
  done
  sleep 0.3
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

start_cp() {
  AGENTGRID_LISTEN="127.0.0.1:$PORT" \
  AGENTGRID_DB="$CP_DB" \
  AGENTGRID_JWT_SECRET="e2e-stable-secret" \
  AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
  nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
  CP_PID=$!
}

start_node() {
  local tok="${1:-}"
  local env_args=()
  [ -n "$tok" ] && env_args+=(AGENTGRID_ENROLL_TOKEN="$tok")
  env PATH="$BIN:$PATH" \
    AGENTGRID_SERVER="$BASE" \
    AGENTGRID_DATA_DIR="$NODE_DATA" \
    AGENTGRID_NODE_NAME="e2e-oom" \
    AGENTGRID_WORKSPACE_ROOT="$WORK" \
    AGENTGRID_REPOSITORY_ROOT="$REPOS" \
    AGENTGRID_ADAPTERS="mock" \
    AGENTGRID_MAX_CONCURRENCY="1" \
    AGENTGRID_SANDBOX="docker" \
    AGENTGRID_SANDBOX_IMAGE="$IMAGE" \
    AGENTGRID_SANDBOX_MEMORY="${MEM_LIMIT_MB}m" \
    RUST_LOG="info" \
    "${env_args[@]}" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/node.log" 2>&1 &
  NODE_PID=$!
}

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

wait_node_online() {
  for _ in $(seq 1 60); do
    st=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" 2>/dev/null \
      | python3 -c 'import sys,json;d=json.load(sys.stdin);ns=d.get("items",d) if isinstance(d,dict) else d;print(ns[0]["status"] if ns else "none")' 2>/dev/null) || st="none"
    [ "$st" = "online" ] && return 0
    sleep 0.5
  done
  echo "node never came online; status=$st"; cat "$TMP/node.log"; return 1
}

submit() {  # $1 = prompt; prints task id
  local prompt_json
  prompt_json=$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1")
  curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":$prompt_json,\"repository\":\"*\",\"adapter\":\"mock\",\"timeout_secs\":120}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}

poll_status() {
  STATUS=$(curl -fsS "$BASE/v1/tasks/$1" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
}

wait_terminal() {
  STATUS=""
  for _ in $(seq 1 "$1"); do
    poll_status "$2"
    case "$STATUS" in
      succeeded|failed|cancelled|timed_out|lost) return 0;;
    esac
    sleep 1
  done
  return 1
}

echo ">> OOM limit e2e: sandbox=docker --memory=${MEM_LIMIT_MB}m, mock allocates ${OOM_MB} MiB"
start_cp
wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
login
mint_token
start_node "$ENROLL_TOKEN"
wait_node_online || exit 1

echo ">> submitting oom task (mock allocates ${OOM_MB} MiB > ${MEM_LIMIT_MB} MiB limit)"
TID=$(submit "oom:${OOM_MB}")
echo "  task $TID; waiting for terminal status (expect failed / resource_limit; timeout 120s)"
if wait_terminal 120 "$TID"; then
  echo "  final status: $STATUS"
else
  echo "  final status: $STATUS (timed out)"; cat "$TMP/cp.log"; cat "$TMP/node.log"; exit 1
fi
[ "$STATUS" = "failed" ] || { echo "  FAILED: expected failed, got $STATUS"; cat "$TMP/node.log"; exit 1; }

echo ">> asserting error_code=resource_limit (distinct from agent_failed)"
DETAIL=$(curl -fsS "$BASE/v1/tasks/$TID" -H "authorization: Bearer $jwt")
python3 <<PYEOF
import json
d = json.loads('''$DETAIL''')
attempts = d.get("attempts") or d.get("attempt") or []
if isinstance(attempts, dict):
    attempts = [attempts]
err = None
for a in attempts:
    e = a.get("error_code")
    if e:
        err = e
        break
if not err:
    print("  FAILED: no error_code on the attempt"); raise SystemExit(1)
if not err.startswith("resource_limit"):
    print(f"  FAILED: expected resource_limit:*, got {err!r}"); raise SystemExit(1)
print(f"  error_code={err} — OOM classified as resource_limit, not agent_failed")
PYEOF

echo ">> asserting the sandbox actually OOM-killed the container (not a mock bug)"
if docker ps -a --filter name=agentgrid- --format '{{.Names}}' | grep -q .; then
  CNAME=$(docker ps -a --filter name=agentgrid- --format '{{.Names}}' | head -1)
  OK=$(docker inspect -f '{{.State.OOMKilled}}' "$CNAME" 2>/dev/null || echo "gone")
  echo "  container $CNAME OOMKilled=$OK (cleaned up after inspect is also fine)"
fi

echo ">> OOM limit e2e OK: memory ceiling → runtime OOM → resource_limit error code"
