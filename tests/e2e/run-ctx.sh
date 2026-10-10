#!/usr/bin/env bash
# E2E (process-based, no Docker): the CTX exit criterion (Stage 11) — a repeat
# attempt on the same (repo, base) does NOT re-spawn the indexer.
#
# The indexer is `tests/e2e/ctx-stub.sh` (contract-conformant; synthetic
# digest, real invocation counting + JSON shape). The worker is
# `adapter-fake-acp` (test ACP agent, built with the workspace) driven over
# the ACP path — the path that composes the context block. A real-indexer run
# is one env override away:
#   AGENTGRID_CTX_BIN=/usr/local/bin/ctx bash tests/e2e/run-ctx.sh
#
# Asserts: task 1 succeeds with a `context_pack` event (cache_hit=false,
# provider=external-ctx) and the digest text inside the agent's received
# prompt (via AG_FAKE_RECORD_PROMPT); task 2 succeeds with cache_hit=true
# while the stub ran exactly once total.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

[ -x "$BIN/adapter-fake-acp" ] || { echo "build debug binaries first (adapter-fake-acp missing)"; exit 1; }

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7816}"
PORT="${AGENTGRID_PORT:-7816}"
USER="admin"
PASS="changeme"
source "$ROOT/tests/e2e/lib-bootstrap.sh"

TMP="$(mktemp -d -t ag-e2e-ctx-XXXXXX)"
CP_DB="$TMP/cp.db"
NODE_DATA="$TMP/node"
WORK="$TMP/work"
REPOS="$TMP/repos"
CTX_CACHE="$TMP/ctx-cache"
STUB_LOG="$TMP/ctx-index.log"
PROMPT_REC="$TMP/prompt1.txt"
mkdir -p "$NODE_DATA" "$WORK" "$REPOS" "$CTX_CACHE"

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
  AGENTGRID_JWT_SECRET="e2e-ctx-secret" \
  AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
  nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
  CP_PID=$!
}

start_node() {
  env PATH="$BIN:$PATH" \
    AGENTGRID_SERVER="$BASE" \
    AGENTGRID_DATA_DIR="$NODE_DATA" \
    AGENTGRID_NODE_NAME="e2e-ctx" \
    AGENTGRID_WORKSPACE_ROOT="$WORK" \
    AGENTGRID_REPOSITORY_ROOT="$REPOS" \
    AGENTGRID_ADAPTERS="fake-acp:acp" \
    AGENTGRID_CTX_BIN="${AGENTGRID_CTX_BIN:-$ROOT/tests/e2e/ctx-stub.sh}" \
    AGENTGRID_CTX_CACHE_DIR="$CTX_CACHE" \
    CTX_STUB_LOG="$STUB_LOG" \
    AGENTGRID_ADAPTER_ENV="AG_FAKE_RECORD_PROMPT=$PROMPT_REC" \
    AGENTGRID_MAX_CONCURRENCY="1" \
    RUST_LOG="info" \
    AGENTGRID_ENROLL_TOKEN="$ENROLL_TOKEN" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/node.log" 2>&1 &
  NODE_PID=$!
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
    -d "{\"prompt\":$prompt_json,\"repository\":\"*\",\"adapter\":\"fake-acp\",\"timeout_secs\":120}" \
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

cache_hit_of() {  # $1 = task id; prints the context_pack cache_hit (or empty)
  curl -fsS "$BASE/v1/tasks/$1/events?after_sequence=0" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;evs=json.load(sys.stdin);hits=[e["payload"].get("cache_hit") for e in evs if e.get("payload",{}).get("kind")=="context_pack"];print(str(hits[0]).lower() if hits else "")'
}

provider_of() {  # $1 = task id; prints the context_pack provider (or empty)
  curl -fsS "$BASE/v1/tasks/$1/events?after_sequence=0" -H "authorization: Bearer $jwt" \
    | python3 -c 'import sys,json;evs=json.load(sys.stdin);ps=[e["payload"].get("provider") for e in evs if e.get("payload",{}).get("kind")=="context_pack"];print(ps[0] if ps else "")'
}

echo ">> ctx E2E: fake-acp worker + stub indexer"
start_cp
wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
login
mint_token
start_node
wait_node_online || exit 1

echo ">> task 1 (cold cache): index runs, digest reaches the prompt"
T1=$(submit "ctx e2e first")
wait_terminal 120 "$T1" || { echo "  task 1 never terminal: $STATUS"; cat "$TMP/node.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: task 1 expected succeeded, got $STATUS"; cat "$TMP/node.log"; exit 1; }
[ "$(provider_of "$T1")" = "external-ctx" ] || { echo "  FAILED: no external-ctx pack event on task 1"; exit 1; }
[ "$(cache_hit_of "$T1")" = "false" ] || { echo "  FAILED: task 1 must be a cache miss"; exit 1; }
echo "  task 1: provider=external-ctx cache_hit=false"
grep -q "ctx-digest" "$PROMPT_REC" || { echo "  FAILED: digest missing from the agent prompt"; cat "$PROMPT_REC"; exit 1; }
echo "  digest present in the agent prompt"

echo ">> task 2 (warm cache): no re-index"
T2=$(submit "ctx e2e second")
wait_terminal 120 "$T2" || { echo "  task 2 never terminal: $STATUS"; cat "$TMP/node.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: task 2 expected succeeded, got $STATUS"; exit 1; }
[ "$(cache_hit_of "$T2")" = "true" ] || { echo "  FAILED: task 2 must be a cache hit"; exit 1; }
echo "  task 2: cache_hit=true"
[ "$(wc -l <"$STUB_LOG")" = "1" ] || { echo "  FAILED: indexer ran more than once:"; cat "$STUB_LOG"; exit 1; }
echo "  indexer ran exactly once across both attempts"

echo ">> ctx E2E OK (index once, warm repeat, digest in prompt)"
