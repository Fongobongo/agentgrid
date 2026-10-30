#!/usr/bin/env bash
# E2E (process-based, no Docker, no LLM): the eval harness measures a fixture.
#
# Proves the whole measurement chain without an LLM: fixture JSON -> POST
# /v1/tasks per case -> node + mock adapter -> terminal status -> usage summed
# from `metric` events -> score -> baseline diff. The mock adapter makes the
# numbers exact (`usage:1000:5` => 1000 tokens, 5 cents), so a wrong sum fails
# loudly instead of drifting by a few percent.
#
# Three assertions, in the order a user would hit them:
#   1. a fixture whose cases match their `expect` exits 0 and scores 2/2;
#   2. the same run against itself as a baseline reports no regression;
#   3. a case that used to pass and now fails exits 1 and says so.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT="$PWD"
BIN="${AGENTGRID_BIN:-$ROOT/target/debug}"

BASE="${AGENTGRID_BASE:-http://127.0.0.1:7822}"
PORT="${AGENTGRID_PORT:-7822}"
USER="admin"
PASS="changeme"
E2E_JWT_SECRET="e2e-eval-secret"
E2E_CONCURRENCY=2

TMP="$(mktemp -d -t ag-e2e-eval-XXXXXX)"
mkdir -p "$TMP/node" "$TMP/work" "$TMP/repos" "$TMP/cfg"

source "$ROOT/tests/e2e/lib-e2e.sh"
trap e2e_cleanup EXIT

# The CLI keeps its session under XDG_CONFIG_HOME; point it at the scratch dir
# so a developer's real `ag` session is neither used nor overwritten.
export XDG_CONFIG_HOME="$TMP/cfg"
AG="$BIN/ag"

echo ">> eval E2E: a fixture must score its cases and diff against a baseline"
e2e_check_bins
[ -x "$AG" ] || { echo "build the CLI first ($AG missing)"; exit 1; }
e2e_start_cp
e2e_wait_ready || { echo "CP not ready"; cat "$TMP/cp.log"; exit 1; }
bootstrap_first_user "$TMP/cp.log" "$BASE" "$USER" "$PASS"
e2e_login
e2e_mint_token
e2e_start_node eval "e2e-eval" "mock" "$TMP/work" "$TMP/repos" "$TMP/node"
e2e_wait_nodes_online 1 || exit 1

mkdir -p "$TMP/cfg/agentgrid"
printf '{"token":"%s"}' "$jwt" >"$TMP/cfg/agentgrid/credentials"

# `*` = any repository: the mock adapter never checks out a tree, and an eval
# fixture should not have to name a real repo just to measure the plumbing.
write_fixture() {  # $1 = path, $2 = prompt for the case expected to pass
  cat >"$1" <<EOF
{
  "adapter": "mock",
  "cases": [
    { "name": "counts-usage", "repository": "*", "prompt": "$2", "expect": "pass" },
    { "name": "known-limitation", "repository": "*", "prompt": "fail:3", "expect": "fail" }
  ]
}
EOF
}

echo ">> run 1: both cases meet their expectation"
write_fixture "$TMP/good.json" "usage:1000:5"
AGENTGRID_SERVER="$BASE" "$AG" eval "$TMP/good.json" --json >"$TMP/score1.json"
grep -q '"ok": true' "$TMP/score1.json" || { echo "no passing case"; cat "$TMP/score1.json"; exit 1; }

python3 - "$TMP/score1.json" <<'PY'
import json, sys
# json.load also proves stdout was one document: `--json` used to print the
# per-case progress lines first, which made the file useless as a baseline.
score = json.load(open(sys.argv[1]))
cases = {c["name"]: c for c in score["cases"]}
assert len(cases) == 2, cases
assert all(c["ok"] for c in cases.values()), cases
assert (score["passed"], score["total"]) == (2, 2), score
assert score["diffs"] == [], score
# Exact numbers: the mock adapter emitted `usage:1000:5`.
got = cases["counts-usage"]
assert (got["tokens"], got["cost_cents"]) == (1000, 5), got
assert (score["tokens"], score["cost_cents"]) == (1000, 5), score
# A failing case still bills nothing — it never reached the adapter's usage.
assert cases["known-limitation"]["status"] != "succeeded", cases["known-limitation"]
print("   score:", {k: (v["status"], v["tokens"], v["cost_cents"]) for k, v in cases.items()})
PY

echo ">> run 2: the baseline is this very run, so nothing regressed"
AGENTGRID_SERVER="$BASE" "$AG" eval "$TMP/good.json" --baseline "$TMP/score1.json" >"$TMP/run2.log" 2>&1 \
  || { echo "a self-baseline reported a regression:"; cat "$TMP/run2.log"; exit 1; }
grep -q "no regressions" "$TMP/run2.log" \
  || { echo "expected 'no regressions':"; cat "$TMP/run2.log"; exit 1; }

echo ">> run 3: a case that passed before now fails => exit 1, named"
write_fixture "$TMP/worse.json" "fail:1"
set +e
AGENTGRID_SERVER="$BASE" "$AG" eval "$TMP/worse.json" --baseline "$TMP/score1.json" >"$TMP/run3.log" 2>&1
rc=$?
set -e
[ "$rc" -ne 0 ] || { echo "a lost pass exited 0"; cat "$TMP/run3.log"; exit 1; }
grep -q "counts-usage: passed before" "$TMP/run3.log" \
  || { echo "regression not named:"; cat "$TMP/run3.log"; exit 1; }

echo "PASS: eval harness scored 2/2, matched its baseline, and flagged a lost pass"
