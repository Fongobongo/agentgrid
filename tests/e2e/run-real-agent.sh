#!/usr/bin/env bash
# Plan 1.5: real-agent E2E behind an API key. Brings up a control plane and
# TWO local nodes running a REAL adapter (claude by default), submits the
# trivial greeting task twice, and asserts both succeed — with concurrency 1
# per node the scheduler places one attempt on each node, so one real task
# shape runs across two nodes.
#
# Gate: exits 77 (skip) when the API key or agent CLI is absent, so the
# nightly CI job stays green without secrets. The per-adapter contract side
# of this gate lives in `#[ignore]`d tests in crates/adapters.
#
# Env:
#   AG_REAL_ADAPTER   claude (default) | opencode
#   AG_REAL_KEY       API key (claude: falls back to ANTHROPIC_API_KEY)
#   AG_REAL_KEY_NAME  opencode: provider env name (default OPENCODEZEN_API_KEY)
#   AG_REAL_PORT      control-plane port (default 7813)
#   AG_REAL_TIMEOUT   per-task timeout seconds (default 600)
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"
HERE="$ROOT/tests/e2e"

ADAPTER="${AG_REAL_ADAPTER:-claude}"
PORT="${AG_REAL_PORT:-7813}"
BASE="http://127.0.0.1:$PORT"
TIMEOUT="${AG_REAL_TIMEOUT:-600}"
USER="admin"
PASS="changeme"

skip() { echo "SKIP (real-agent e2e): $1"; exit 77; }

case "$ADAPTER" in
  claude)
    AGENT_BIN="claude"
    KEY="${AG_REAL_KEY:-${ANTHROPIC_API_KEY:-}}"
    [ -n "$KEY" ] || skip "no key: set AG_REAL_KEY or ANTHROPIC_API_KEY"
    ADAPTER_ENV="ANTHROPIC_API_KEY=$KEY"
    ;;
  opencode)
    AGENT_BIN="opencode"
    KEY_NAME="${AG_REAL_KEY_NAME:-OPENCODEZEN_API_KEY}"
    KEY="${AG_REAL_KEY:-${!KEY_NAME:-}}"
    # opencode may also auth via `opencode auth login` (auth.json); only skip
    # when there is neither a key nor the auth file.
    if [ -z "$KEY" ] && [ ! -f "$HOME/.local/share/opencode/auth.json" ]; then
      skip "no key ($KEY_NAME/AG_REAL_KEY) and no opencode auth.json"
    fi
    ADAPTER_ENV="${KEY_NAME}=${KEY}"
    ;;
  *) echo "unknown AG_REAL_ADAPTER=$ADAPTER (claude|opencode)"; exit 1 ;;
esac
# Optional extra env for the adapter child (e.g. provider keys that opencode
# resolves via its own env like NVIDIA_API_KEY, or AGENTGRID_OPENCODE_MODEL).
# Space-separated K=V pairs, appended verbatim.
ADAPTER_ENV="${ADAPTER_ENV} ${AG_REAL_EXTRA_ENV:-}"

command -v "$AGENT_BIN" >/dev/null 2>&1 || skip "$AGENT_BIN CLI not on PATH"
[ -x "$BIN/agentgrid-control-plane" ] || { echo "build debug binaries first"; exit 1; }
[ -x "$BIN/agentgrid-node-daemon" ] || { echo "agentgrid-node-daemon missing"; exit 1; }
[ -x "$BIN/adapter-$ADAPTER" ] || { echo "adapter-$ADAPTER missing"; exit 1; }

source "$HERE/lib-bootstrap.sh"

TMP="$(mktemp -d -t ag-e2e-real-XXXXXX)"
CP_PID=""; NODE_A_PID=""; NODE_B_PID=""
cleanup() {
  set +e
  [ -n "$NODE_A_PID" ] && kill -9 "$NODE_A_PID" 2>/dev/null
  [ -n "$NODE_B_PID" ] && kill -9 "$NODE_B_PID" 2>/dev/null
  [ -n "$CP_PID" ] && kill "$CP_PID" 2>/dev/null
  if [ "${AG_E2E_KEEP:-0}" = "1" ]; then echo "kept sandbox: $TMP"; else rm -rf "$TMP"; fi
}
trap cleanup EXIT

echo ">> real-agent E2E: adapter=$ADAPTER on :$PORT (two local nodes)"

AGENTGRID_LISTEN="127.0.0.1:$PORT" \
AGENTGRID_DB="$TMP/cp.db" \
AGENTGRID_JWT_SECRET="e2e-real-secret" \
AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
CP_PID=$!

for _ in $(seq 1 40); do
  curl -fsS "$BASE/health/ready" >/dev/null 2>&1 && break
  sleep 0.5
done
curl -fsS "$BASE/health/ready" >/dev/null || { echo "CP never became ready"; cat "$TMP/cp.log"; exit 1; }

bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"

jwt=$(curl -fsS -X POST "$BASE/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "{\"username\":\"$USER\",\"password\":\"$PASS\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
[ -n "$jwt" ] || { echo "login failed"; cat "$TMP/cp.log"; exit 1; }

mint_token() {
  curl -fsS -X POST "$BASE/v1/nodes/enrollment-token" \
    -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])'
}

# Real agents need the unattended double-ack; sandbox stays none (CI has no
# Docker-in-Docker here), so re-allow unsafe explicitly.
start_node() {
  local name="$1" tok="$2"
  env PATH="$BIN:$PATH" \
    AGENTGRID_SERVER="$BASE" \
    AGENTGRID_DATA_DIR="$TMP/$name" \
    AGENTGRID_NODE_NAME="$name" \
    AGENTGRID_WORKSPACE_ROOT="$TMP/$name-work" \
    AGENTGRID_REPOSITORY_ROOT="$TMP/$name-repos" \
    AGENTGRID_ADAPTERS="$ADAPTER" \
    AGENTGRID_ADAPTER_ENV="$ADAPTER_ENV" \
    AGENTGRID_MAX_CONCURRENCY="1" \
    AGENTGRID_ENROLL_TOKEN="$tok" \
    AGENTGRID_UNSAFE_UNATTENDED="1" \
    AGENTGRID_I_UNDERSTAND_UNSAFE="1" \
    AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX="1" \
    RUST_LOG="info" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/$name.log" 2>&1 &
}

echo ">> enrolling two nodes with adapter=$ADAPTER"
mkdir -p "$TMP/artifacts"
start_node "node-a" "$(mint_token)"; NODE_A_PID=$!
start_node "node-b" "$(mint_token)"; NODE_B_PID=$!

# Wait for both nodes to register.
for _ in $(seq 1 40); do
  n=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(len(json.load(sys.stdin)["items"]))')
  [ "$n" = "2" ] && break
  sleep 1
done
[ "$n" = "2" ] || { echo "expected 2 nodes, got $n"; tail -5 "$TMP"/node-*.log; exit 1; }
echo ">> two nodes enrolled"

submit() {
  curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":\"Create a file named greeting.txt containing exactly: hello from agentgrid. Do nothing else.\",\"repository\":\"*\",\"adapter\":\"$ADAPTER\",\"timeout_secs\":$TIMEOUT}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}

echo ">> submitting the trivial real task twice (one lands per node)"
T1=$(submit)
T2=$(submit)
echo "   tasks: $T1 $T2"

status_of() {
  curl -fsS "$BASE/v1/tasks/$1" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])'
}

echo ">> waiting for terminal status (timeout ${TIMEOUT}s)"
deadline=$((SECONDS + TIMEOUT + 60))
s1=""; s2=""
while [ $SECONDS -lt $deadline ]; do
  s1=$(status_of "$T1"); s2=$(status_of "$T2")
  case "$s1" in queued|running) ;; *) case "$s2" in queued|running) ;; *) break;; esac;; esac
  sleep 3
done
echo "   $T1 -> $s1 / $T2 -> $s2"
[ "$s1" = "succeeded" ] || {
  echo "E2E FAILED: $T1 -> $s1"
  tail -20 "$TMP"/node-*.log "$TMP/cp.log"
  for f in "$TMP"/artifacts/*/agent-raw-output.log "$TMP"/artifacts/*/agent.jsonl; do
    [ -f "$f" ] && { echo "== $f"; tail -c 1500 "$f"; }
  done
  exit 1
}
[ "$s2" = "succeeded" ] || {
  echo "E2E FAILED: $T2 -> $s2"
  tail -20 "$TMP"/node-*.log "$TMP/cp.log"
  for f in "$TMP"/artifacts/*/agent-raw-output.log "$TMP"/artifacts/*/agent.jsonl; do
    [ -f "$f" ] && { echo "== $f"; tail -c 1500 "$f"; }
  done
  exit 1
}

# Placement: with concurrency=1 the two attempts must spread across nodes.
a=$(grep -c "starting attempt" "$TMP/node-a.log" || true)
b=$(grep -c "starting attempt" "$TMP/node-b.log" || true)
echo "   attempts: node-a=$a node-b=$b"
if [ "$a" -lt 1 ] || [ "$b" -lt 1 ]; then
  echo "E2E FAILED: both nodes should have run an attempt (a=$a b=$b)"
  exit 1
fi

echo "REAL-AGENT E2E OK ($ADAPTER, two nodes)"
