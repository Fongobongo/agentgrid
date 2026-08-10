#!/usr/bin/env bash
# Plan 2.3 (#9): sandbox cold-start benchmark.
#
# Measures the submit→running latency the control plane assigns a task and a
# node daemon turns around for `sandbox=none` (legacy) vs `sandbox=docker`
# (sandbox_command prefix), per AGENTGRID_SANDBOX mode. Needs a running
# control plane + one node daemon already configured; AGENTGRID_CONTROL
# + AG_AGENT_API_TOKEN must be in env (or flags).
#
# Usage:
#   AGENTGRID_CONTROL=http://127.0.0.1:7800 \
#   AG_AGENT_API_TOKEN=<token> \
#   ./deploy/sandbox-benchmark.sh [iterations]
#
# Output: per-iteration Microsoft-format rows `sandbox=<m> iter=<n> submit_ms=<t>`,
# plus a final summary line per mode. Podman gets the same row because
# `SandboxKind::Docker` covers both (`AGENTGRID_SANDBOX=podman` → docker).
#
# Honest limitation: this measures the full task round-trip (CP create+
# assign + node poll + spawn + `TaskAssigned→Running` update), not the raw
# `docker run` container-start wall time. The dominant contributions are
# CP tick cadence (≈1 s default) and the node's scheduler poll; the actual
# sandbox cold-start is the *delta* between the two modes (see
# docs/deploy/sandbox-benchmark.md for how to isolate it with a tracing span).

set -euo pipefail

CONTROL=${AGENTGRID_CONTROL:-http://127.0.0.1:7800}
TOKEN=${AG_AGENT_API_TOKEN:-}
ITER=${1:-5}
ADAPTER=${ADAPTER:-mock}
PROMPT=${PROMPT:-"sandbox cold-start benchmark"}
REPO=${REPO:-"https://example.invalid/x"}

if [[ -z "$TOKEN" ]]; then
    echo "AG_AGENT_API_TOKEN is required" >&2; exit 1
fi

bench() {
    local sandbox=$1 iter=$2
    local t0 ms id attempts_running
    t0=$(date +%s%3N)
    id=$(curl -fsS -X POST "$CONTROL/v1/tasks" \
        -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' \
        -d "{\"prompt\":\"$PROMPT\",\"repository\":\"$REPO\",\"adapter\":\"$ADAPTER\",\"security_profile\":\"$sandbox\"}" \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
    # Poll until the task is Running (or Done/failed).
    for _ in $(seq 1 120); do
        sleep 0.25
        local st
        st=$(curl -fsS "$CONTROL/v1/tasks/$id" \
            -H "Authorization: Bearer $TOKEN" \
            | python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["task"]["status"])')
        case $st in
            running|done|failed)
                break;;
        esac
    done
    ms=$(( $(date +%s%3N) - t0 ))
    echo "sandbox=$sandbox iter=$iter submit_ms=$ms status=$st"
}

summary=()
for sandbox in none docker; do
    echo "== sandbox=$sandbox (set AGENTGRID_SANDBOX=$sandbox on the node first) ==" >&2
    read -r -p "Press Enter to run $ITER iterations with sandbox=$sandbox… " _
    total=0
    for i in $(seq 1 "$ITER"); do
        out=$(bench "$sandbox" "$i")
        echo "$out"
        ms=$(awk '{for(i=1;i<=NF;i++) if($i~/^submit_ms=/){split($i,a,"="); print a[2]}}' <<<"$out")
        total=$((total + ms))
    done
    avg=$((total / ITER))
    summary+=("$sandbox avg_ms=$avg over $ITER iters")
done

echo "--- summary ---"
for s in "${summary[@]}"; do echo "$s"; done
