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
E2E_JWT_SECRET="e2e-ctx-secret"
E2E_CONCURRENCY=1

TMP="$(mktemp -d -t ag-e2e-ctx-XXXXXX)"
CTX_CACHE="$TMP/ctx-cache"
STUB_LOG="$TMP/ctx-index.log"
PROMPT_REC="$TMP/prompt1.txt"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos" "$CTX_CACHE"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

submit_acp() {  # $1 = prompt; prints task id
  local prompt_json
  prompt_json=$(python3 -c 'import json,sys;print(json.dumps(sys.argv[1]))' "$1")
  curl -fsS -X POST "$BASE/v1/tasks" \
    -H "authorization: Bearer $jwt" -H 'content-type: application/json' \
    -d "{\"prompt\":$prompt_json,\"repository\":\"*\",\"adapter\":\"fake-acp\",\"timeout_secs\":120}" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])'
}

cache_hit_of() {  # $1 = task id; prints the context_pack cache_hit (or empty)
  e2e_task_events "$1" \
    | python3 -c 'import sys,json;evs=json.load(sys.stdin);hits=[e["payload"].get("cache_hit") for e in evs if e.get("payload",{}).get("kind")=="context_pack"];print(str(hits[0]).lower() if hits else "")'
}

provider_of() {  # $1 = task id; prints the context_pack provider (or empty)
  e2e_task_events "$1" \
    | python3 -c 'import sys,json;evs=json.load(sys.stdin);ps=[e["payload"].get("provider") for e in evs if e.get("payload",{}).get("kind")=="context_pack"];print(ps[0] if ps else "")'
}

echo ">> ctx E2E: fake-acp worker + stub indexer"
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
e2e_start_node ctx "e2e-ctx" "fake-acp:acp" "$TMP/work" "$TMP/repos" "$TMP/node" \
  "AGENTGRID_CTX_BIN=${AGENTGRID_CTX_BIN:-$ROOT/tests/e2e/ctx-stub.sh}" \
  "AGENTGRID_CTX_CACHE_DIR=$CTX_CACHE" \
  "CTX_STUB_LOG=$STUB_LOG" \
  "AGENTGRID_ADAPTER_ENV=AG_FAKE_RECORD_PROMPT=$PROMPT_REC"
e2e_wait_nodes_online 1 || exit 1

echo ">> task 1 (cold cache): index runs, digest reaches the prompt"
T1=$(submit_acp "ctx e2e first")
e2e_wait_terminal 120 "$T1" || { echo "  task 1 never terminal: $STATUS"; cat "$TMP/ctx.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: task 1 expected succeeded, got $STATUS"; cat "$TMP/ctx.log"; exit 1; }
[ "$(provider_of "$T1")" = "external-ctx" ] || { echo "  FAILED: no external-ctx pack event on task 1"; exit 1; }
[ "$(cache_hit_of "$T1")" = "false" ] || { echo "  FAILED: task 1 must be a cache miss"; exit 1; }
echo "  task 1: provider=external-ctx cache_hit=false"
grep -q "ctx-digest" "$PROMPT_REC" || { echo "  FAILED: digest missing from the agent prompt"; cat "$PROMPT_REC"; exit 1; }
echo "  digest present in the agent prompt"

echo ">> task 2 (warm cache): no re-index"
T2=$(submit_acp "ctx e2e second")
e2e_wait_terminal 120 "$T2" || { echo "  task 2 never terminal: $STATUS"; cat "$TMP/ctx.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: task 2 expected succeeded, got $STATUS"; exit 1; }
[ "$(cache_hit_of "$T2")" = "true" ] || { echo "  FAILED: task 2 must be a cache hit"; exit 1; }
echo "  task 2: cache_hit=true"
[ "$(wc -l <"$STUB_LOG")" = "1" ] || { echo "  FAILED: indexer ran more than once:"; cat "$STUB_LOG"; exit 1; }
echo "  indexer ran exactly once across both attempts"

echo ">> ctx E2E OK (index once, warm repeat, digest in prompt)"
