#!/usr/bin/env bash
# E2E (process-based, no Docker, no LLM): re-keying a node keeps its identity
# and its sealed secrets.
#
# ADR 0028. This is the recovery path for a leaked `credential.json`. Revoking
# and re-enrolling would mint a NEW node id, which drops every ciphertext in
# `adapter_env_sealed` and makes the operator re-enter every secret by hand. The
# re-key path exists to make that unnecessary, and "unnecessary" is exactly what
# a unit test cannot show: only a real node, with a real credential file and a
# real sealed blob, can prove the secret survived. So the assertions are:
#
#   1. a secret is sealed for the node and reaches the adapter (baseline — the
#      later check must not pass because the secret never worked);
#   2. the node is revoked and its old credential is refused by the CP;
#   3. `ag nodes rekey` hands out a token, the node spends it with
#      AGENTGRID_REKEY=1 and comes back with the SAME node id — one node, not
#      two, and not a replacement;
#   4. the adapter still receives the secret, so the ciphertext was neither
#      dropped nor re-sealed: the node kept its key;
#   5. the token is single-use.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="${AGENTGRID_BIN:-$ROOT/target/debug}"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7853}"
PORT="${AGENTGRID_PORT:-7853}"
USER="admin"
PASS="dev-only-password"
E2E_JWT_SECRET="e2e-rekey-secret"

SECRET="sk-ant-e2e-REKEY-7777"

TMP="$(mktemp -d -t ag-e2e-rekey-XXXXXX)"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos" "$TMP/cfg"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

export XDG_CONFIG_HOME="$TMP/cfg"
unset HTTP_PROXY HTTPS_PROXY http_proxy https_proxy ALL_PROXY all_proxy 2>/dev/null || true
export NO_PROXY="127.0.0.1,localhost"

# `e2e_check_bins` predates the CLI and only looks for `ag`, which is `ag.exe`
# under git bash.
AG_BIN=""
for cand in "$BIN/ag" "$BIN/ag.exe"; do
  if [ -x "$cand" ]; then AG_BIN="$cand"; break; fi
done

fail() { echo "FAIL: $*"; [ -f "$TMP/rk.log" ] && tail -20 "$TMP/rk.log"; exit 1; }

echo ">> rekey E2E: a revoked node comes back as itself, with its secrets"
e2e_check_bins
[ -n "$AG_BIN" ] || { echo "the ag CLI is missing from $BIN"; exit 1; }

e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token

e2e_start_node rk "e2e-rekey" "mock" "$TMP/work" "$TMP/repos" "$TMP/node"
e2e_wait_nodes_online 1 || fail "node never came online"

"$AG_BIN" --server "$BASE" login "$USER" "$PASS" >/dev/null

# --- 1. baseline: the secret is sealed and reaches the adapter --------------
echo ">> seal a secret and prove it reaches the adapter"
printf '%s' "$SECRET" | "$AG_BIN" --server "$BASE" adapter-env set '*' ANTHROPIC_API_KEY
LS="$("$AG_BIN" --server "$BASE" adapter-env ls)"
printf '%s\n' "$LS" | grep -q 'sealed=1 pending=0' \
  || fail "the entry was not sealed for the node (see ls above)"

masked_secret() {  # $1 = task id; prints the log lines the adapter produced
  e2e_task_events "$1" | python3 -c '
import sys, json
d = json.load(sys.stdin)
items = d.get("items", d) if isinstance(d, dict) else d
for e in items:
    t = (e.get("payload") or {}).get("text")
    if isinstance(t, str):
        print(t)
'
}

TID=$(e2e_submit "env:ANTHROPIC_API_KEY" mock)
e2e_wait_terminal 120 "$TID" || fail "baseline task never finished"
[ "$STATUS" = "succeeded" ] || fail "baseline task status=$STATUS"
masked_secret "$TID" | grep -q 'env ANTHROPIC_API_KEY=\*\*\*' \
  || fail "the secret never reached the adapter — the later check would prove nothing"

NODE_ID=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt" | python3 -c '
import sys, json
d = json.load(sys.stdin)
ns = d.get("items", d) if isinstance(d, dict) else d
print(ns[0]["id"])
')
OLD_CRED=$(python3 -c 'import json,sys;print(json.load(open(sys.argv[1]))["credential"])' "$TMP/node/credential.json")
echo "   node $NODE_ID, credential ${OLD_CRED:0:8}…"

# --- 2. revoke: the leaked credential must stop working --------------------
echo ">> revoke the node (the leak scenario)"
curl -fsS -X DELETE "$BASE/v1/nodes/$NODE_ID" -H "authorization: Bearer $jwt" >/dev/null
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/node/heartbeat" \
  -H "authorization: Bearer $OLD_CRED" -H 'content-type: application/json' -d '{}')
[ "$CODE" = "401" ] || fail "a revoked credential still authenticates (HTTP $CODE)"
echo "   old credential -> HTTP $CODE"

e2e_kill_node

# --- 3. re-key --------------------------------------------------------------
echo ">> ag nodes rekey, spent with AGENTGRID_REKEY=1"
RK=$("$AG_BIN" --server "$BASE" --json nodes rekey "$NODE_ID" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
[ -n "$RK" ] || fail "no re-key token"

# The daemon reuses `credential.json`'s sealing key, so the same data dir is the
# whole point: a fresh one would be a new node with a new key.
ENROLL_TOKEN="$RK" e2e_start_node rk "e2e-rekey" "mock" "$TMP/work" "$TMP/repos" "$TMP/node" \
  AGENTGRID_REKEY=1

for _ in $(seq 1 60); do
  grep -q "re-keyed" "$TMP/rk.log" 2>/dev/null && break
  sleep 0.5
done
grep -q "re-keyed" "$TMP/rk.log" \
  || fail "the daemon never logged a successful re-key"
grep -q "$NODE_ID" "$TMP/rk.log" \
  || fail "the daemon re-keyed a different node id"

# One node, same id: a re-key that fell through to the create branch would show
# two rows here, and the old one would be a revoked ghost.
LIST=$(curl -fsS "$BASE/v1/nodes" -H "authorization: Bearer $jwt")
COUNT=$(printf '%s' "$LIST" | python3 -c '
import sys, json
d = json.load(sys.stdin)
ns = d.get("items", d) if isinstance(d, dict) else d
print(len(ns))
')
SAME=$(printf '%s' "$LIST" | python3 -c '
import sys, json
d = json.load(sys.stdin)
ns = d.get("items", d) if isinstance(d, dict) else d
print(sum(1 for n in ns if n.get("id") == sys.argv[1]))
' "$NODE_ID")
echo "   nodes=$COUNT, same id=$SAME (want $NODE_ID)"
[ "$COUNT" = "1" ] || fail "expected 1 node after the re-key, found $COUNT: $LIST"
[ "$SAME" = "1" ] || fail "the node id changed across the re-key: $LIST"

# --- 4. the secret survived the re-key -------------------------------------
echo ">> the adapter still receives the secret the CP sealed before the re-key"
TID2=$(e2e_submit "env:ANTHROPIC_API_KEY" mock)
e2e_wait_terminal 120 "$TID2" || fail "post-rekey task never finished"
[ "$STATUS" = "succeeded" ] || fail "post-rekey task status=$STATUS"
LOG2=$(masked_secret "$TID2")
printf '%s\n' "$LOG2" | sed 's/^/   | /'
printf '%s' "$LOG2" | grep -q 'env ANTHROPIC_API_KEY=\*\*\*' \
  || fail "the sealed secret did not survive the re-key"
if printf '%s' "$LOG2" | grep -q "REKEY-7777"; then
  fail "the raw secret reached the event log"
fi

# --- 5. single use ----------------------------------------------------------
echo ">> the spent token cannot mint a second credential"
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$BASE/v1/node/enroll" \
  -H 'content-type: application/json' \
  -d "{\"token\":\"$RK\",\"name\":\"x\",\"adapters\":[],\"repositories\":[]}")
[ "$CODE" = "400" ] || fail "a spent re-key token was accepted (HTTP $CODE)"
echo "   reuse -> HTTP $CODE"

echo "PASS: the revoked node came back as the same node, with the same sealed secret, and the token was single-use"
