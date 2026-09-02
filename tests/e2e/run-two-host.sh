#!/usr/bin/env bash
# Two-host E2E: control plane runs on THIS machine, a single remote node
# daemon runs on AG_REMOTE_HOST over SSH, talks back over the network.
#
# Env (read from env or .env in repo root):
#   AG_REMOTE_HOST  remote box ip/name (required)
#   AG_REMOTE_PORT  ssh port (default 22)
#   AG_REMOTE_USER  ssh user (default root)
#   AG_REMOTE_KEY   path to the private key (default ~/.ssh/id_ed25519_agentgrid_remote)
#
# The node binary is built for the runner's own arch; both hosts are
# x86_64 linux per the deploy matrix. A self-signed TLS error is not the
# point here; use plain http (VPN/protected channel assumption, see
# docs/decisions/0009). Exit 0 = round trip worked.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
[ -f "$ROOT/.env" ] && { set -a; source "$ROOT/.env"; set +a; }

: "${AG_REMOTE_HOST:?set AG_REMOTE_HOST}"
AG_REMOTE_PORT="${AG_REMOTE_PORT:-22}"
AG_REMOTE_USER="${AG_REMOTE_USER:-root}"
AG_REMOTE_KEY="${AG_REMOTE_KEY:-$HOME/.ssh/id_ed25519_agentgrid_remote}"
# Trust-on-first-use; CI runners have an empty known_hosts and must not block.
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o GlobalKnownHostsFile=/dev/null)

PORT=7820
TMP="$(mktemp -d -t ag-e2e-twohost-XXXXXX)"
RWORK=/tmp/ag-e2e-twohost-$$

cleanup() {
  set +e
  [ -n "${CPPID:-}" ] && kill "$CPPID" 2>/dev/null
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null; done
  ssh "${SSH_OPTS[@]}" -i "$AG_REMOTE_KEY" -p "$AG_REMOTE_PORT" "$AG_REMOTE_USER@$AG_REMOTE_HOST" \
      "pkill -f agentgrid-node-daemon; rm -rf $RWORK" 2>/dev/null
  rm -rf "$TMP"
}
trap cleanup EXIT

BIN="$ROOT/target/debug"
[ -x "$BIN/agentgrid-control-plane" ] || { echo "build debug binaries first"; exit 1; }

echo ">> cp on :$PORT, remote node on $AG_REMOTE_HOST"
AGENTGRID_LISTEN="0.0.0.0:$PORT" AGENTGRID_DB="$TMP/cp.db" \
  AGENTGRID_JWT_SECRET="e2e-twohost-secret" AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
  nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
CPPID=$!

source "$ROOT/tests/e2e/lib-bootstrap.sh"
bootstrap_first_user "$TMP/cp.log" "http://127.0.0.1:$PORT" "admin" "changeme"
for _ in $(seq 1 20); do
  JWT=$(curl -fsS -H 'content-type: application/json' \
    -d '{"username":"admin","password":"changeme"}' \
    "http://127.0.0.1:$PORT/v1/auth/login" 2>/dev/null \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])' 2>/dev/null) && break
  sleep 0.5
done
[ -n "${JWT:-}" ] || { echo "login failed"; exit 1; }

TOK=$(curl -fsS -X POST -H "authorization: Bearer $JWT" \
  "http://127.0.0.1:$PORT/v1/nodes/enrollment-token" | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

# The CP box is not publicly reachable on $PORT (firewall). Bridge via an
# ssh reverse tunnel: remote 127.0.0.1:$RPORT -> local 127.0.0.1:$PORT.
# Random port: a SIGKILLed earlier run can leave the remote sshd holding a
# stale listener for minutes; a fixed port collides on immediate retries.
RPORT=""
for attempt in 1 2 3 4 5; do
  RPORT=$((10000 + RANDOM % 50000))
  if ssh "${SSH_OPTS[@]}" -f -N -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 \
      -o ServerAliveCountMax=2 \
      -i "$AG_REMOTE_KEY" -p "$AG_REMOTE_PORT" \
      -R "$RPORT:127.0.0.1:$PORT" "$AG_REMOTE_USER@$AG_REMOTE_HOST"; then
    PIDS+=($!)
    break
  fi
  RPORT=""
done
[ -n "$RPORT" ] || { echo "reverse tunnel failed after 5 tries"; exit 1; }
echo "   cp reachable for the node through ssh -R :$RPORT"

echo ">> staging node daemon on $AG_REMOTE_HOST"
scp -q "${SSH_OPTS[@]}" -i "$AG_REMOTE_KEY" -P "$AG_REMOTE_PORT" \
  "$BIN/agentgrid-node-daemon" "$ROOT/target/debug/adapter-mock" \
  "$AG_REMOTE_USER@$AG_REMOTE_HOST:$RWORK/" \
  || { mkdir -p "" 2>/dev/null; ssh "${SSH_OPTS[@]}" -i "$AG_REMOTE_KEY" -p "$AG_REMOTE_PORT" \
      "$AG_REMOTE_USER@$AG_REMOTE_HOST" "mkdir -p $RWORK" \
      && scp -q "${SSH_OPTS[@]}" -i "$AG_REMOTE_KEY" -P "$AG_REMOTE_PORT" \
        "$BIN/agentgrid-node-daemon" "$ROOT/target/debug/adapter-mock" \
        "$AG_REMOTE_USER@$AG_REMOTE_HOST:$RWORK/"; }

ssh "${SSH_OPTS[@]}" -n -f -i "$AG_REMOTE_KEY" -p "$AG_REMOTE_PORT" "$AG_REMOTE_USER@$AG_REMOTE_HOST" "
  cd $RWORK && chmod +x agentgrid-node-daemon adapter-mock &&
  PATH=\"$RWORK:\$PATH\" \
  AGENTGRID_SERVER='http://127.0.0.1:$RPORT' \
  AGENTGRID_DATA_DIR=$RWORK/data AGENTGRID_NODE_NAME=remote-two \
  AGENTGRID_WORKSPACE_ROOT=$RWORK/work AGENTGRID_REPOSITORY_ROOT=$RWORK/repos \
  AGENTGRID_ADAPTERS=mock AGENTGRID_TRANSPORT=poll \
  AGENTGRID_ENROLL_TOKEN=$TOK AGENTGRID_ALLOW_ROOT=1 RUST_LOG=info \
  nohup ./agentgrid-node-daemon >node.log 2>&1 </dev/null &
  sleep 2; pgrep -f agentgrid-node-daemon >/dev/null && echo remote-node-up"

echo ">> waiting for the remote node to come online"
ok=0
for _ in $(seq 1 60); do
  J=$(curl -fsS -H "authorization: Bearer $JWT" "http://127.0.0.1:$PORT/v1/nodes" 2>/dev/null) || continue
  S=$(echo "$J" | python3 -c 'import sys,json
n=[x for x in json.load(sys.stdin)["items"] if x["name"]=="remote-two"]
print(n[0]["status"] if n else "")' 2>/dev/null)
  [ "$S" = "online" ] && { ok=1; break; }
  sleep 2
done
[ "$ok" = "1" ] || { echo "remote node never came online"; cat "$TMP/cp.log" | tail -10; exit 1; }
echo "   online"

echo ">> running a mock task on the remote node"
TASK=$(curl -fsS -X POST -H "authorization: Bearer $JWT" -H 'content-type: application/json' \
  -d '{"prompt":"write ok.txt","repository":"*","adapter":"mock","requested_node_id":null}' \
  "http://127.0.0.1:$PORT/v1/tasks" | python3 -c 'import sys,json;d=json.load(sys.stdin);print(d.get("id") or d.get("task_id"))')
for _ in $(seq 1 90); do
  ST=$(curl -fsS -H "authorization: Bearer $JWT" "http://127.0.0.1:$PORT/v1/tasks/$TASK" 2>/dev/null \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])' 2>/dev/null)
  case "$ST" in
    succeeded) echo "   task succeeded across the wire"; break;;
    failed) echo "task failed"; exit 1;;
  esac
  sleep 2
done
[ "${ST:-}" = "succeeded" ] || { echo "timeout: status=$ST"; exit 1; }

echo "TWO-HOST E2E OK"
