#!/usr/bin/env bash
# The egress topology is part of the guarantee, and the daemon cannot check it:
# it only knows a network name. This asserts the guarantee from the outside.
#
#   bash tests/e2e/run-egress-isolation.sh
#
# Needs docker (runs in the compose e2e job). Three assertions, each of which
# has been wrong in this repo at least once:
#   1. the egress network is `internal`, so a sandbox has no route of its own;
#   2. without the proxy there is no way out — the old single-network compose
#      passed this test only because nothing tried;
#   3. with the proxy, a non-allowlisted domain is refused AND an allowlisted
#      one passes. The second half is the one that catches a squid config that
#      denies everything (the `acl localnet src` + `deny localnet` bug: every
#      sandbox is a 172.16/12 *client*, so the rule refused all traffic).
set -euo pipefail

cd "$(dirname "$0")/../.."
COMPOSE=(docker compose -f deploy/egress-proxy/docker-compose.yml)
NET=agentgrid-egress
CURL_IMAGE="${AG_EGRESS_CURL_IMAGE:-curlimages/curl:8.11.1}"
PROXY=http://egress-proxy:3128

fail() { echo "FAIL: $*"; exit 1; }

cleanup() { "${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo ">> bringing the egress stack up"
"${COMPOSE[@]}" up -d >/dev/null
docker pull -q "$CURL_IMAGE" >/dev/null

internal=$(docker network inspect "$NET" --format '{{.Internal}}' 2>/dev/null || echo missing)
[ "$internal" = "true" ] || fail "$NET is '$internal', not internal — a sandbox on it gets NAT and the allowlist is advisory"

echo ">> squid enforces the allowlist"
code=""
for _ in $(seq 1 60); do
  code=$(docker run --rm --network "$NET" -e https_proxy="$PROXY" "$CURL_IMAGE" \
    -s -o /dev/null -w '%{http_code}' --max-time 5 https://example.com/ 2>/dev/null || true)
  [ "$code" = "403" ] && break
  sleep 2
done
[ "$code" = "403" ] || fail "a non-allowlisted domain returned '$code' through the proxy, expected 403 (squid unreachable, or not enforcing)"

code=$(docker run --rm --network "$NET" -e https_proxy="$PROXY" "$CURL_IMAGE" \
  -s -o /dev/null -w '%{http_code}' --max-time 10 https://api.github.com/ 2>/dev/null || true)
[ "$code" = "200" ] || fail "an allowlisted domain returned '$code' through the proxy, expected 200 — the allowlist denies everything (check the src/dst acls)"

echo ">> and nothing gets out around it"
if docker run --rm --network "$NET" "$CURL_IMAGE" -sS --max-time 5 https://api.github.com/ >/dev/null 2>&1; then
  fail "a container on $NET reached the internet without the proxy — the network is not internal"
fi

echo "EGRESS ISOLATION OK"
