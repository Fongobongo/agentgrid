#!/usr/bin/env bash
# Roadmap #9: sandbox cold-start benchmark.
# Measures spawn latency of the `none` (bare worktree) backend vs the
# `docker` backend for a trivial command. The delta is the per-attempt
# cold-start penalty the sandbox adds — the number to compare against
# microVM/pool claims (e.g. CubeSandbox).
#
# Usage: measure-sandbox-coldstart.sh [iterations] [image]
# Requires: docker (skipped cleanly without it). No CI gate — manual probe.
set -euo pipefail

N="${1:-10}"
IMAGE="${2:-alpine:3.20}"

printf '%-24s %10s %10s %10s\n' backend min_ms avg_ms max_ms

bench() { # <label> <cmd...>
    local label="$1"; shift
    local min=999999 max=0 total=0 i t0 t1 dt
    for ((i = 0; i < N; i++)); do
        t0=$(date +%s%N)
        "$@" >/dev/null 2>&1 || { echo "$label: command failed"; return 1; }
        t1=$(date +%s%N)
        dt=$(( (t1 - t0) / 1000000 ))
        (( dt < min )) && min=$dt
        (( dt > max )) && max=$dt
        (( total += dt ))
    done
    printf '%-24s %10d %10d %10d\n' "$label" "$min" "$(( total / N ))" "$max"
}

bench "none (worktree)" true

if command -v docker >/dev/null && docker info >/dev/null 2>&1; then
    docker image inspect "$IMAGE" >/dev/null 2>&1 || docker pull -q "$IMAGE" >/dev/null
    # Same flags as sandbox::docker_run_head, minus workdir volume (irrelevant
    # to spawn latency).
    bench "docker" docker run --rm --network none --cap-drop=ALL \
        --pids-limit 256 --memory 1g "$IMAGE" true
else
    echo "docker: unavailable, skipped"
fi
