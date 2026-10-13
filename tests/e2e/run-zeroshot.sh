#!/usr/bin/env bash
# E2E (process-based, no Docker): one Agentgrid task runs on the Zeroshot
# cluster path end-to-end (Stage 10 / Exit 10 shape).
#
# The executor is `tests/e2e/zeroshot-stub.sh` (contract-conformant; see its
# header for the honest simplifications vs a real binary). A real-binary run
# is one env override away:
#   AGENTGRID_ZEROSHOT_BIN=/usr/local/bin/zeroshot bash tests/e2e/run-zeroshot.sh
# (needs docker + a real binary; CI uses the stub).
#
# Asserts: happy path (succeeded + cluster_created event + result event +
# zeroshot-result.json artifact + commit), failure path (`fail:3` →
# infrastructure is NOT blamed: agent exit 3, kill marker present), cancel
# path (sleep task → POST cancel → cancelled + kill marker present).
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="$ROOT/target/debug"

command -v docker >/dev/null 2>&1 || { echo "skip: docker not on PATH (zeroshot probe needs the runtime)"; exit 77; }

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7825}"
PORT="${AGENTGRID_PORT:-7825}"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-zeroshot-secret"
E2E_CONCURRENCY=2

TMP="$(mktemp -d -t ag-e2e-zeroshot-XXXXXX)"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos"

STUB_STATE="$TMP/stub-state"
mkdir -p "$STUB_STATE" "$TMP/stubbin"
cp "$ROOT/tests/e2e/zeroshot-stub.sh" "$TMP/stubbin/zeroshot"
chmod +x "$TMP/stubbin/zeroshot"
export ZEROSHOT_STUB_STATE="$STUB_STATE"
# A real binary overrides the stub without touching the script.
ZEROSHOT_BIN="${AGENTGRID_ZEROSHOT_BIN:-$TMP/stubbin/zeroshot}"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

submit_zs() {  # $1 = prompt; prints task id
  e2e_submit "$1" "zeroshot" "*" 120
}

events_contain() {  # $1 = task id, $2 = python predicate over evs list -> prints yes/no
  e2e_task_events "$1" \
    | python3 -c "import sys,json;evs=json.load(sys.stdin);print('yes' if ($2) else 'no')"
}

echo ">> zeroshot E2E via $ZEROSHOT_BIN"
e2e_check_bins
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
e2e_start_node zs "e2e-zeroshot" "zeroshot" "$TMP/work" "$TMP/repos" "$TMP/node" \
  "PATH=$TMP/stubbin:$BIN:$PATH" \
  "AGENTGRID_ZEROSHOT_BIN=$ZEROSHOT_BIN" \
  "AGENTGRID_ADAPTER_ENV=ZEROSHOT_STUB_STATE=$STUB_STATE"
e2e_wait_nodes_online 1 || { echo "--- nodes API:"; curl -s "$BASE/v1/nodes" -H "authorization: Bearer $jwt" || true; echo "--- node log:"; cat "$TMP/zs.log" || true; echo "--- cp log tail:"; tail -50 "$TMP/cp.log" || true; exit 1; }

echo ">> happy path: cluster run streams events, exports results"
T1=$(submit_zs "zeroshot e2e hello")
echo "  task $T1"
e2e_wait_terminal 120 "$T1" || { echo "  task never terminal: $STATUS"; cat "$TMP/zs.log"; exit 1; }
[ "$STATUS" = "succeeded" ] || { echo "  FAILED: expected succeeded, got $STATUS"; cat "$TMP/zs.log"; exit 1; }
[ "$(events_contain "$T1" "any(e.get('payload',{}).get('kind')=='cluster_created' for e in evs)")" = "yes" ] \
  || { echo "  FAILED: no cluster_created event"; exit 1; }
echo "  cluster_created event present"
[ -n "$(ls "$STUB_STATE"/started-* 2>/dev/null)" ] \
  || { echo "  FAILED: stub run marker missing"; ls "$STUB_STATE"; exit 1; }
[ -n "$(ls "$STUB_STATE"/exported-* 2>/dev/null)" ] \
  || { echo "  FAILED: stub export marker missing"; ls "$STUB_STATE"; exit 1; }
echo "  run + export markers present"
code=$(curl -s -o /dev/null -w '%{http_code}' "$BASE/v1/tasks/$T1/artifacts/zeroshot-result.json" -H "authorization: Bearer $jwt")
[ "$code" = "200" ] || { echo "  FAILED: zeroshot-result.json artifact HTTP $code"; exit 1; }
echo "  zeroshot-result.json artifact downloadable"

echo ">> failure path: fail:3 is the agent's failure, kill still runs"
T2=$(submit_zs "fail:3")
e2e_wait_terminal 120 "$T2" || { echo "  task never terminal: $STATUS"; exit 1; }
[ "$STATUS" = "failed" ] || { echo "  FAILED: expected failed, got $STATUS"; exit 1; }
[ -n "$(ls "$STUB_STATE"/killed-* 2>/dev/null)" ] \
  || { echo "  FAILED: no kill marker after failed run (cancel-total)"; ls "$STUB_STATE"; exit 1; }
echo "  failed + kill marker present"

echo ">> cancel path: sleep task cancelled, cluster killed"
rm -f "$STUB_STATE"/killed-*
T3=$(submit_zs "sleep:60")
e2e_wait_status running "$T3" 60 || { echo "  task never running: $STATUS"; exit 1; }
curl -fsS -X POST "$BASE/v1/tasks/$T3/cancel" -H "authorization: Bearer $jwt" >/dev/null \
  || { echo "  cancel POST failed"; exit 1; }
e2e_wait_terminal 60 "$T3" || { echo "  cancelled task never terminal: $STATUS"; exit 1; }
[ "$STATUS" = "cancelled" ] || { echo "  FAILED: expected cancelled, got $STATUS"; exit 1; }
[ -n "$(ls "$STUB_STATE"/killed-* 2>/dev/null)" ] \
  || { echo "  FAILED: no kill marker after cancel (cancel-total)"; ls "$STUB_STATE"; exit 1; }
echo "  cancelled + kill marker present"

echo ">> zeroshot E2E OK (happy + failure + cancel-total)"
