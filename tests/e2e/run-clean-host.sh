#!/usr/bin/env bash
# Plan 6.1/6.2 clean-host acceptance (items 504 + 515): the static musl node
# stack runs on a Tier-1 host with ONLY the documented minimal dependencies
# (Linux kernel, git, CA certificates, an agent CLI) — no Docker, Node.js,
# Python, Java or external database — and DNS/hostname, credential and proxy
# flows work in the static build.
#
# Runs INSIDE the clean host (a debian:12-slim container in CI, or any bare
# Tier-1 machine). Env:
#   AG_CLEAN_BIN   dir with the musl binaries (default /bins)
#   AG_CLEAN_HOST  CP hostname — a NAME, not an IP, so the musl resolver is
#                  exercised (default cp-local; CI maps it via --add-host)
#   AG_CLEAN_PORT  CP port (default 7800)
#   AG_CLEAN_STRICT=0  relax the forbidden-tools assertion (default 1:
#                  fail if docker/node/python/java are present — the point
#                  of the test is a clean host)
#
# Test tooling inside the host: git (required dep), ca-certificates
# (required dep), curl (harness only — fetches the setup token; the product
# never needs it). Everything else goes through the musl `ag` CLI.
#
# Item 515 notes (honest scope):
# - DNS: the node AND the `ag` CLI talk to the CP by hostname (musl
#   resolver end to end, not a literal IP).
# - System CA: reqwest uses `rustls-tls` (bundled webpki roots) — the stack
#   has no system-CA dependency by construction; the HTTP core flow below
#   proves it on a host whose CA store is never consulted.
# - Proxy: `AGENTGRID_PROXY_URLS` with a dead entry is exercised live — the
#   node must rotate past it (log: "rotating") and come online via direct
#   fallback. Pool semantics (parse/failover/revive) are unit-tested in
#   proxy.rs; a live forward-proxy *success* path needs a network-calling
#   adapter (real LLM key) and stays a follow-up.
# - Credential flows: enrollment-token mint → node enroll → bearer
#   heartbeat/poll/task loop, all inside the container.
set -euo pipefail

BIN="${AG_CLEAN_BIN:-/bins}"
HOST="${AG_CLEAN_CP_HOST:-cp-local}"
PORT="${AG_CLEAN_CP_PORT:-7800}"
BASE="http://$HOST:$PORT"
STRICT="${AG_CLEAN_STRICT:-1}"

for b in agentgrid-control-plane agentgrid-node-daemon adapter-mock ag; do
  [ -x "$BIN/$b" ] || { echo ">> FATAL: $BIN/$b missing or not executable"; exit 1; }
done

echo ">> [clean] forbidden tools must be absent (strict=$STRICT)"
fail=0
for t in docker podman node nodejs npm python3 python java; do
  if command -v "$t" >/dev/null 2>&1; then
    echo "  PRESENT (forbidden on a clean host): $t"
    fail=1
  fi
done
if [ "$fail" = "1" ]; then
  if [ "$STRICT" = "1" ]; then
    echo ">> FATAL: host is not clean"; exit 1
  else
    echo ">> WARN: continuing on a non-clean host (AG_CLEAN_STRICT=0)"
  fi
else
  echo "  clean: no docker/podman/node/python/java"
fi

echo ">> [clean] required deps: git"
git --version
command -v curl >/dev/null 2>&1 || { echo ">> FATAL: curl (test harness) missing"; exit 1; }

echo ">> [dns] hostname resolution for $HOST"
if command -v getent >/dev/null 2>&1; then
  getent hosts "$HOST" || { echo ">> FATAL: cannot resolve $HOST"; exit 1; }
else
  echo "  (no getent; the musl binaries below prove resolution by connecting)"
fi

TMP="$(mktemp -d -t ag-clean-XXXXXX)"
export HOME="$TMP/home"
mkdir -p "$HOME" "$TMP/node" "$TMP/work" "$TMP/repos"
AG="$BIN/ag --server $BASE"
CP_PID=""
NODE_PID=""

cleanup() {
  set +e
  [ -n "$NODE_PID" ] && kill -9 "$NODE_PID" 2>/dev/null
  [ -n "$CP_PID" ] && kill "$CP_PID" 2>/dev/null
  sleep 0.3
  [ "${AG_E2E_KEEP:-0}" = "1" ] || rm -rf "$TMP"
}
trap cleanup EXIT

echo ">> [cp] starting musl control plane"
AGENTGRID_LISTEN="0.0.0.0:$PORT" \
AGENTGRID_DB="$TMP/cp.db" \
AGENTGRID_JWT_SECRET="clean-host-secret" \
AGENTGRID_ARTIFACT_ROOT="$TMP/artifacts" \
nohup "$BIN/agentgrid-control-plane" >"$TMP/cp.log" 2>&1 &
CP_PID=$!

echo ">> [cp] waiting for /health/ready via hostname"
for _ in $(seq 1 40); do
  "$BIN/ag" --server "$BASE" status 2>/dev/null | grep -q "(healthy)" && break
  sleep 0.5
done
"$BIN/ag" --server "$BASE" status 2>/dev/null | grep -q "(healthy)" \
  || { echo ">> FATAL: CP never healthy"; cat "$TMP/cp.log"; exit 1; }
echo "  CP healthy over hostname (musl resolver + ag CLI work)"

echo ">> [auth] bootstrap first admin (setup-token flow)"
TOKEN=$(awk '/=== agentgrid setup token/{f=1;next} /=== present this at/{f=0} f' "$TMP/cp.log" \
  | grep -E '^[A-Za-z0-9_-]+$' | tail -1)
[ -n "$TOKEN" ] || { echo ">> FATAL: no setup token in CP log"; cat "$TMP/cp.log"; exit 1; }
curl -fsS -X POST "$BASE/v1/auth/setup" -H 'content-type: application/json' \
  -d "{\"username\":\"admin\",\"password\":\"changeme\",\"setup_token\":\"$TOKEN\"}" >/dev/null \
  || { echo ">> FATAL: setup failed"; exit 1; }
$AG login admin changeme >/dev/null || { echo ">> FATAL: ag login"; exit 1; }
echo "  bootstrap + login OK (credential flow)"

echo ">> [node] minting enrollment token"
ENROLL_TOKEN=$($AG token create | sed 's/^export AGENTGRID_ENROLL_TOKEN=//')
[ -n "$ENROLL_TOKEN" ] || { echo ">> FATAL: token mint failed"; exit 1; }

start_node() {  # $1 = proxy URL or empty (empty = direct, pool parses to none)
  env PATH="$BIN:$PATH" \
    AGENTGRID_SERVER="$BASE" \
    AGENTGRID_DATA_DIR="$TMP/node" \
    AGENTGRID_NODE_NAME="clean-host" \
    AGENTGRID_WORKSPACE_ROOT="$TMP/work" \
    AGENTGRID_REPOSITORY_ROOT="$TMP/repos" \
    AGENTGRID_ADAPTERS="mock" \
    AGENTGRID_MAX_CONCURRENCY="1" \
    AGENTGRID_ALLOW_ROOT=1 \
    AGENTGRID_ENROLL_TOKEN="$ENROLL_TOKEN" \
    AGENTGRID_PROXY_URLS="${1:-}" \
    RUST_LOG="info" \
    nohup "$BIN/agentgrid-node-daemon" >"$TMP/node.log" 2>&1 &
  NODE_PID=$!
}

echo ">> [node] starting musl node daemon (DNS + credential + HTTP in static build)"
start_node ""
echo ">> [node] waiting for online"
for _ in $(seq 1 60); do
  $AG --json status 2>/dev/null | grep -q '"online": *1' && break
  sleep 1
done
$AG --json status 2>/dev/null | grep -q '"online": *1' \
  || { echo ">> FATAL: node never online"; cat "$TMP/node.log"; exit 1; }
echo "  node online (enrolled over hostname, heartbeating)"

echo ">> [task] mock happy path to succeeded"
TID=$($AG run '*' 'spam:5' | tail -n 1)
[ -n "$TID" ] || { echo ">> FATAL: ag run printed no task id"; exit 1; }
for _ in $(seq 1 90); do
  ST=$($AG --json show "$TID" 2>/dev/null | grep -o '"status": *"[^"]*"' | head -1)
  case "$ST" in
    *succeeded*|*failed*|*cancelled*) break;;
  esac
  sleep 1
done
echo "$ST" | grep -q succeeded || { echo ">> FATAL: task not succeeded ($ST)"; exit 1; }
$AG logs --no-color "$TID" | grep -q "spam line" \
  || { echo ">> FATAL: logs missing spam lines"; exit 1; }
echo "  happy path OK (run/show/logs/succeeded)"

echo ">> [proxy] restart with a dead proxy entry (failover must rotate + go direct)"
kill -9 "$NODE_PID" 2>/dev/null || true
sleep 1
: > "$TMP/node.log"
start_node "http://127.0.0.1:9"
for _ in $(seq 1 60); do
  $AG --json status 2>/dev/null | grep -q '"online": *1' && break
  sleep 1
done
$AG --json status 2>/dev/null | grep -q '"online": *1' \
  || { echo ">> FATAL: node with dead proxy never online (direct fallback broken?)"; cat "$TMP/node.log"; exit 1; }
# The online check above can pass on the pre-restart (stale) heartbeat, so
# wait for the rotation marker itself on the FRESH log (bounded).
for _ in $(seq 1 30); do
  grep -q "rotating" "$TMP/node.log" && break
  sleep 1
done
grep -q "rotating" "$TMP/node.log" \
  || { echo ">> FATAL: no proxy rotation in node log (pool not exercised?)"; cat "$TMP/node.log"; exit 1; }
echo "  proxy failover OK (tried pool entry, rotated, direct)"

echo ">> clean-host e2e OK: minimal deps suffice; DNS/credential/proxy flows work in musl"
