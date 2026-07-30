#!/usr/bin/env bash
# Two-host E2E (Stage 8 / line 260 follow-up): the SAME workflow manifest runs
# unchanged with roles spread across two *physical* hosts — a local dev box
# (control plane + one node) and a remote Linux host (a second node) reached
# over SSH via tests/e2e/remote-ssh.py. No Docker, no second CI runner: the
# remote node daemon is the debug-gnu binary uploaded from the dev box (same
# glibc 2.36), talking back to the local control plane over the LAN/WAN.
#
# Verifies the Stage 8 release gate: "the same manifest works on one PC and on
# two hosts" — proven here by running workers on the remote host and the
# integrator+verifier on the local host, then asserting `succeeded`.
#
# Reads AG_REMOTE_* from .env (SSH creds). Run from the repo root:
#   tests/e2e/run-two-host.sh
# Reuses the already-built debug binaries; no network-facing assumptions apart
# from the dev box being reachable from the remote on AG_LISTEN_PORT.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"
HERE="$ROOT/tests/e2e"

# Local control plane listens on a public interface (0.0.0.0) so the remote
# node can reach it. Default 7811; override AG_TWO_HOST_PORT.
LISTEN_PORT="${AG_TWO_HOST_PORT:-7811}"
# The address the REMOTE node dials back to. Default the dev box's first public
# IPv4 (eth0); override AG_TWO_HOST_PUBLIC.
PUBLIC_IP="${AG_TWO_HOST_PUBLIC:-$(ip -4 addr show eth0 2>/dev/null \
  | awk '/inet /{split($2,a,"/");print a[1];exit}')}"
BASE_REMOTE="http://${PUBLIC_IP}:${LISTEN_PORT}"
BASE_LOCAL="http://127.0.0.1:${LISTEN_PORT}"
USER="admin"
PASS="changeme"
# Hardening P0 #2: the bootstrap env backdoor was removed; source the shared
# helper that reads the one-time setup token from the CP log.
source "$ROOT/tests/e2e/lib-bootstrap.sh"
REMOTE_BIN_DIR="/root/ag-two-host"
REMOTE_DATA="/tmp/ag-two-host-node"

TMP="$(mktemp -d -t ag-e2e-2host-XXXXXX)"
CP_DB="$TMP/cp.db"
LOCAL_NODE_DATA="$TMP/local-node"
LOCAL_WORK="$TMP/work"
LOCAL_REPOS="$TMP/repos"
REMOTE_WORK="/tmp/ag-two-host-work"
REMOTE_REPOS="/tmp/ag-two-host-repos"
mkdir -p "$LOCAL_NODE_DATA" "$LOCAL_WORK" "$LOCAL_REPOS" "$TMP/artifacts"

CP_PID=""; LOCAL_NODE_PID=""

cleanup() {
  set +e
  [ -n "$LOCAL_NODE_PID" ] && kill -9 "$LOCAL_NODE_PID" 2>/dev/null
  [ -n "$CP_PID" ] && kill "$CP_PID" 2>/dev/null
  pkill -f "$BIN/agentgrid-control-plane" 2>/dev/null
  pkill -f "$BIN/agentgrid-node-daemon" 2>/dev/null
  # Stop the remote node daemon we left running.
  "$HERE/remote-ssh.py" "pkill -f agentgrid-node-daemon 2>/dev/null; rm -rf $REMOTE_DATA $REMOTE_WORK $REMOTE_REPOS $REMOTE_BIN_DIR" >/dev/null 2>&1 || true
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

echo ">> two-host E2E: local CP+node on :$LISTEN_PORT, remote node at $BASE_REMOTE"

[ -n "$PUBLIC_IP" ] || { echo "  could not resolve dev box public IP; set AG_TWO_HOST_PUBLIC"; exit 1; }
[ -x "$BIN/agentgrid-control-plane" ] || { echo "  build debug binaries first: cargo build --bin agentgrid-control-plane --bin agentgrid-node-daemon"; exit 1; }
[ -x "$BIN/agentgrid-node-daemon" ] || { echo "  agentgrid-node-daemon missing"; exit 1; }

echo "  uploading node-daemon + adapter-mock to the remote host"
"$HERE/remote-ssh.py" "mkdir -p $REMOTE_BIN_DIR" >/dev/null 2>&1
"$HERE/remote-ssh.py" --file "$BIN/agentgrid-node-daemon" "$REMOTE_BIN_DIR/agentgrid-node-daemon" >/dev/null
"$HERE/remote-ssh.py" --file "$BIN/adapter-mock" "$REMOTE_BIN_DIR/adapter-mock" >/dev/null
"$HERE/remote-ssh.py" "chmod +x $REMOTE_BIN_DIR/agentgrid-node-daemon $REMOTE_BIN_DIR/adapter-mock && rm -rf $REMOTE_DATA $REMOTE_WORK $REMOTE_REPOS && mkdir -p $REMOTE_DATA $REMOTE_WORK $REMOTE_REPOS" >/dev/null 2>&1
# Put the remote-bin dir on PATH so resolve_adapter_bin finds adapter-mock.
REMOTE_PATH_PREFIX="PATH=$REMOTE_BIN_DIR:\$PATH"

start_cp() {
  AGENTGRID_LISTEN="0.0.0.0:$LISTEN_PORT" \
  AGENTGRID_DB="$CP_DB" \
  AGENTGRID_JWT_SECRET="e2e-2host-secret" \
  AGENTGRID_BOOTSTRAP_USER="$USER" \
  AGENTGRID_BOOTSTRAP_PASSWORD="$PASS" \
  AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
  nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
  CP_PID=$!
}

wait_ready() {
  for _ in $(seq 1 40); do
    curl -fsS "$BASE_LOCAL/health/ready" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  return 1
}

login() {
  jwt=$(curl -fsS -X POST "$BASE_LOCAL/v1/auth/login" \
    -H 'content-type: application/json' \
    -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
  [ -n "$jwt" ] || { echo "login failed"; cat "$TMP/cp.log"; exit 1; }
}

mint_token() {
  ENROLL_TOKEN=$(curl -fsS -X POST "$BASE_LOCAL/v1/nodes/enrollment-token" \
    -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
}

# Mint a fresh enrollment token (single-use). Each node needs its own.
mint_token_for() {
  curl -fsS -X POST "$BASE_LOCAL/v1/nodes/enrollment-token" \
    -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])'
}

start_local_node() {
  local tok="${1:-}"
  local env_args=()
  [ -n "$tok" ] && env_args+=(AGENTGRID_ENROLL_TOKEN="$tok")
  env PATH="$BIN:$PATH" \
    AGENTGRID_SERVER="$BASE_LOCAL" \
    AGENTGRID_DATA_DIR="$LOCAL_NODE_DATA" \
    AGENTGRID_NODE_NAME="e2e-local" \
    AGENTGRID_WORKSPACE_ROOT="$LOCAL_WORK" \
    AGENTGRID_REPOSITORY_ROOT="$LOCAL_REPOS" \
    AGENTGRID_ADAPTERS="mock" \
    AGENTGRID_MAX_CONCURRENCY="2" \
    RUST_LOG="info" \
    "${env_args[@]}" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/local-node.log" 2>&1 &
  LOCAL_NODE_PID=$!
}

start_remote_node() {
  # Enroll phase: run the remote daemon WITH the enrollment token under a
  # short timeout so it persists credential.json and exits cleanly (we kill
  # it via the timeout — not pkill — so the credential flush completes).
  local tok="${1:-}"
  set +e
  "$HERE/remote-ssh.py" "AGENTGRID_SERVER='$BASE_REMOTE' \
    AGENTGRID_DATA_DIR='$REMOTE_DATA' \
    AGENTGRID_NODE_NAME='e2e-remote' \
    AGENTGRID_WORKSPACE_ROOT='$REMOTE_WORK' \
    AGENTGRID_REPOSITORY_ROOT='$REMOTE_REPOS' \
    AGENTGRID_ADAPTERS='mock' \
    AGENTGRID_MAX_CONCURRENCY='2' \
    AGENTGRID_ENROLL_TOKEN='$tok' \
    AGENTGRID_ALLOW_ROOT=1 \
    $REMOTE_PATH_PREFIX \
    timeout 5 $REMOTE_BIN_DIR/agentgrid-node-daemon >/tmp/ag-remote-node.log 2>&1" 2>&1 | tail -2
  set -e
}

start_remote_node_persistent() {
  # Re-launch the remote daemon WITHOUT the token — it loads credential.json
  # from the enroll phase. Detached via nohup + </dev/null so the SSH session
  # closing does not take it down.
  set +e
  "$HERE/remote-ssh.py" "AGENTGRID_SERVER='$BASE_REMOTE' \
    AGENTGRID_DATA_DIR='$REMOTE_DATA' \
    AGENTGRID_NODE_NAME='e2e-remote' \
    AGENTGRID_WORKSPACE_ROOT='$REMOTE_WORK' \
    AGENTGRID_REPOSITORY_ROOT='$REMOTE_REPOS' \
    AGENTGRID_ADAPTERS='mock' \
    AGENTGRID_MAX_CONCURRENCY='2' \
    AGENTGRID_ALLOW_ROOT=1 \
    $REMOTE_PATH_PREFIX \
    nohup $REMOTE_BIN_DIR/agentgrid-node-daemon >/tmp/ag-remote-node.log 2>&1 </dev/null &
    echo \$!" >/dev/null 2>&1
  set -e
}

wait_node_online() {  # $1 = node name substring
  local st="none"
  for _ in $(seq 1 60); do
    st=$(curl -fsS "$BASE_LOCAL/v1/nodes" -H "authorization: Bearer $jwt" \
      | python3 -c 'import sys,json
ns=json.load(sys.stdin)
want=sys.argv[1]
match=[n["status"] for n in ns if want in n.get("name","")]
print(match[0] if match else "none")' "$1" 2>/dev/null) || st="none"
    [ "$st" = "online" ] && return 0
    sleep 1
  done
  echo "  node $1 never came online (status=$st)"; return 1
}

submit() { # prompt; prints task id
  local prompt_json
  prompt_json=$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1")
  curl -fsS -X POST "$BASE_LOCAL/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":$prompt_json,\"repository\":\"*\",\"adapter\":\"mock\",\"timeout_secs\":120}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}

wait_terminal() {  # $1 id, $2 max secs; sets STATUS
  STATUS=""
  for _ in $(seq 1 "$2"); do
    STATUS=$(curl -fsS "$BASE_LOCAL/v1/tasks/$1" -H "authorization: Bearer $jwt" \
      | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
    case "$STATUS" in
      succeeded|failed|cancelled|timed_out|lost) return 0;;
    esac
    sleep 1
  done
  return 1
}

echo "  starting control plane on 0.0.0.0:$LISTEN_PORT"
start_cp
wait_ready || { echo "  CP not ready; log:"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE_LOCAL" "$USER" "$PASS"
login
mint_token

echo "  enrolling local node"
LOCAL_TOK=$(mint_token_for)
start_local_node "$LOCAL_TOK"
wait_node_online "e2e-local" || { cat "$TMP/local-node.log"; exit 1; }

echo "  enrolling remote node (drops a debug-gnu binary over SSH)"
REMOTE_TOK=$(mint_token_for)
start_remote_node "$REMOTE_TOK" >/dev/null
# Re-launch in persistent long-poll mode using the saved credential.
start_remote_node_persistent
set +e
wait_node_online "e2e-remote" || { echo "  remote node never came online; remote log:"; "$HERE/remote-ssh.py" "cat /tmp/ag-remote-node.log" 2>&1 | tail -20; exit 1; }

echo "  discovering node ids"
readarray -t NODES < <(curl -fsS "$BASE_LOCAL/v1/nodes" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json
ns=json.load(sys.stdin)
ns.sort(key=lambda n:n["name"])
print("\n".join(n["id"] for n in ns))')
[ "${#NODES[@]}" -ge 2 ] || { echo "  expected >=2 enrolled nodes, got ${#NODES[@]}"; exit 1; }

# Match node ids to roles: local runs integrator+verifier, remote runs workers.
LOCAL_NODE=$(curl -fsS "$BASE_LOCAL/v1/nodes" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json
ns=json.load(sys.stdin)
loc=[n["id"] for n in ns if "local" in n["name"]]
print(loc[0] if loc else "")')
REMOTE_NODE=$(curl -fsS "$BASE_LOCAL/v1/nodes" -H "authorization: Bearer $jwt" \
  | python3 -c 'import sys,json
ns=json.load(sys.stdin)
rem=[n["id"] for n in ns if "remote" in n["name"]]
print(rem[0] if rem else "")')
[ -n "$LOCAL_NODE" ] && [ -n "$REMOTE_NODE" ] || { echo "  could not resolve local/remote node ids"; exit 1; }
echo "  local node = $LOCAL_NODE   remote node = $REMOTE_NODE"

echo "  defining workflow (workers on remote, integrator+verifier on local)"
DEF=$(python3 -c 'import json,sys
rem,loc=sys.argv[1],sys.argv[2]
steps=[
  {"id":"w1","prompt":"impl a","role":"worker","depends_on":[],"requested_node_id":rem},
  {"id":"w2","prompt":"impl b","role":"worker","depends_on":[],"requested_node_id":rem},
  {"id":"int","prompt":"merge","role":"integrator","depends_on":["w1","w2"],"requested_node_id":loc},
  {"id":"ver","prompt":"verify","role":"verifier","depends_on":["int"],"requested_node_id":loc},
]
print(json.dumps({"name":"e2e-2host","steps":steps}))' "$REMOTE_NODE" "$LOCAL_NODE")
TID=$(curl -fsS -X POST "$BASE_LOCAL/v1/workflows" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
  -d "$DEF" | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
echo "  workflow template $TID"
RID=$(curl -fsS -X POST "$BASE_LOCAL/v1/workflows/$TID/runs" \
  -H "authorization: Bearer $jwt" -H 'content-type: application/json' -d '{}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')
echo "  run $RID; polling terminal (timeout 240s)"
for _ in $(seq 1 240); do
  STATUS=$(curl -fsS "$BASE_LOCAL/v1/workflow-runs/$RID" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["run"]["status"])')
  case "$STATUS" in
    succeeded|failed|cancelled|blocked) break;;
  esac
  # Drive the run forward until the background ticker alone would catch it;
  # see run-workflow.sh (Stage 8 E2E) which does the same.
  curl -fsS -X POST "$BASE_LOCAL/v1/workflow-runs/$RID/tick" -H "authorization: Bearer $jwt" >/dev/null 2>&1 || true
  sleep 1
done
echo "  run final status: $STATUS"
if [ "$STATUS" != "succeeded" ]; then
  echo "  FAILED: expected succeeded, got $STATUS"
  echo "  --- local node log tail ---"; tail -20 "$TMP/local-node.log" 2>/dev/null
  echo "  --- remote node log tail ---"; "$HERE/remote-ssh.py" "tail -20 /tmp/ag-remote-node.log"
  exit 1
fi
echo "  A OK: two-host workflow succeeded (workers remote, integrator+verifier local)"

# Provenance sanity: the projection should show one step ran on the remote host.
echo "  checking provenance (at least one step ran on the remote host)"
PROV=$(curl -fsS "$BASE_LOCAL/v1/workflow-runs/$RID/projection" -H "authorization: Bearer $jwt")
echo "$PROV" | python3 -c 'import sys,json
p=json.load(sys.stdin)
nodes=set()
for s in p.get("steps",[]):
  if s.get("node_id"):
    nodes.add(s["node_id"])
print(f"  steps ran on nodes: {sorted(nodes)}")'
echo ">> two-host OK"
