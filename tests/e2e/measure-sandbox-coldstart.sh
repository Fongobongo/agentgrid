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

# ADR 0022 added a fourth backend after this benchmark was written, so the
# number that mattered most — what bwrap costs per attempt, and whether it is
# in the same class as a container — had never been measured. The argv below
# is `sandbox::bwrap_head` for a default task (network_mode=none, writable
# worktree, no run-as, no egress proxy), kept in the same order on purpose: a
# benchmark of a different argv is not a benchmark of this backend.
#
# The first invocation also pays for bwrap's own namespace setup; running it
# N times is what the daemon does (one bwrap per attempt), so there is no warm
# path to amortise.
WD="$(mktemp -d -t ag-bwrap-bench-XXXXXX)"
trap 'rm -rf "$WD"' EXIT
bwrap_head() { # <share-net?>
    printf '%s\n' --die-with-parent --new-session --unshare-all
    if [ "$1" = "share-net" ]; then
        printf '%s\n' --share-net
    fi
    printf '%s\n' --ro-bind / / --dev /dev --proc /proc --tmpfs /tmp \
        --bind "$WD" "$WD" --chdir "$WD" --
}

if command -v bwrap >/dev/null 2>&1 && bwrap --version >/dev/null 2>&1; then
    echo "# $(bwrap --version)"
    # Fail loudly rather than printing a fast row that measured nothing: a
    # host with user namespaces disabled would otherwise look like the
    # cheapest backend available.
    mapfile -t HEAD_NONE < <(bwrap_head none)
    if ! bwrap "${HEAD_NONE[@]}" /bin/true >/dev/null 2>&1; then
        echo "bwrap: present but cannot run (user namespaces disabled?), skipped"
    else
        bench "bwrap (net=none)" bwrap "${HEAD_NONE[@]}" /bin/true
        mapfile -t HEAD_NET < <(bwrap_head share-net)
        bench "bwrap (share-net)" bwrap "${HEAD_NET[@]}" /bin/true
    fi
else
    echo "bwrap: unavailable, skipped"
fi

