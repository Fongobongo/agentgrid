#!/usr/bin/env bash
# The egress topology is part of the guarantee, and the daemon cannot check it:
# it only knows a network name. This asserts the guarantee from the outside.
#
#   bash tests/e2e/run-egress-isolation.sh
#
# Needs docker (runs in the compose e2e job). Five assertions, each of which has
# been wrong in this repo at least once:
#   1. the egress network is `internal`, so a sandbox has no route of its own;
#   2. a non-allowlisted domain is refused over plain http — the proxy answers
#      that request itself, so the 403 is a real status code;
#   3. the same over https/CONNECT, which is what an agent actually does, and
#      where the refusal arrives as `%{http_connect}` rather than `%{http_code}`;
#   4. an allowlisted domain is allowed — asserted on the CONNECT reply, not on
#      the origin's status. That one catches a squid config that denies
#      everything (the `acl localnet src` + `deny localnet` bug: every sandbox is
#      a 172.16/12 *client*, so the rule refused all traffic);
#   5. with no proxy at all, nothing gets out — the old single-network compose
#      passed this only because nothing tried.
set -euo pipefail

cd "$(dirname "$0")/../.."
COMPOSE=(docker compose -f deploy/egress-proxy/docker-compose.yml)
NET=agentgrid-egress
CURL_IMAGE="${AG_EGRESS_CURL_IMAGE:-curlimages/curl:8.11.1}"
PROXY=http://egress-proxy:3128

fail() { echo "FAIL: $*"; exit 1; }

cleanup() { "${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true; }
trap cleanup EXIT

# A failure here has three very different causes — squid died, squid is up but
# not enforcing, or the network is not internal — and they look identical from
# curl (`000`, `403`, `200`). Dumping the sidecar's own state on the way out is
# the difference between one CI cycle and three.
dump_diagnostics() {
  echo "---- docker compose ps ----"
  "${COMPOSE[@]}" ps -a 2>&1 || true
  echo "---- egress-proxy logs (tail 60) ----"
  "${COMPOSE[@]}" logs --no-color --tail 60 2>&1 || true
  echo "---- networks ----"
  docker network inspect "$NET" --format '{{.Name}} internal={{.Internal}}' 2>&1 || true
}

echo ">> bringing the egress stack up"
"${COMPOSE[@]}" up -d >/dev/null
docker pull -q "$CURL_IMAGE" >/dev/null

internal=$(docker network inspect "$NET" --format '{{.Internal}}' 2>/dev/null || echo missing)
[ "$internal" = "true" ] || { dump_diagnostics; fail "$NET is '$internal', not internal — a sandbox on it gets NAT and the allowlist is advisory"; }

echo ">> squid enforces the allowlist"
# A denial on an `https://` URL reads as curl `000`, not `403`: curl opens a
# CONNECT tunnel, the proxy refuses it, and no response ever comes back *from
# the target* — so `%{http_code}` has nothing to report. Asserting 403 on an
# https URL is a test that can never pass, which is how this looked like a
# broken proxy for one CI cycle. `%{http_connect}` is the code of the CONNECT
# reply, which is the thing that was actually denied.
code=""
for _ in $(seq 1 20); do
  # Plain http: the proxy answers the request itself, so the 403 is a real
  # status code.
  code=$(docker run --rm --network "$NET" -e http_proxy="$PROXY" "$CURL_IMAGE" \
    -s -o /dev/null -w '%{http_code}' --max-time 3 http://example.com/ 2>/dev/null || true)
  [ "$code" = "403" ] && break
  sleep 2
done
[ "$code" = "403" ] || { dump_diagnostics; fail "a non-allowlisted domain returned '$code' through the proxy, expected 403 (000 = the proxy never answered: check the sidecar logs above; 200 = not enforcing)"; }

# And the same for the CONNECT path agents actually use.
connect=""
for _ in $(seq 1 10); do
  connect=$(docker run --rm --network "$NET" -e https_proxy="$PROXY" "$CURL_IMAGE" \
    -s -o /dev/null -w '%{http_connect}' --max-time 5 https://example.com/ 2>/dev/null || true)
  [ "$connect" = "403" ] && break
  sleep 2
done
[ "$connect" = "403" ] || { dump_diagnostics; fail "an https CONNECT to a non-allowlisted domain was answered '$connect', expected 403"; }

# The proxy's *decision*, not the origin's opinion. Asserting
# `%{http_code} = 200` made this test depend on GitHub's rate limiter: an
# unauthenticated request from a CI runner gets a 403 from api.github.com
# itself, while squid's own log says `TCP_TUNNEL/200` — the tunnel opened and
# the config is fine. `%{http_connect}` is the CONNECT reply, which is squid's
# answer and the only thing this test can speak for.
allow=""
for _ in $(seq 1 10); do
  allow=$(docker run --rm --network "$NET" -e https_proxy="$PROXY" "$CURL_IMAGE" \
    -s -o /dev/null -w '%{http_connect}' --max-time 10 https://api.github.com/ 2>/dev/null || true)
  [ "$allow" = "200" ] && break
  sleep 2
done
[ "$allow" = "200" ] || { dump_diagnostics; fail "an allowlisted domain was refused at the CONNECT ('$allow'), expected 200 — the allowlist denies everything (check the src/dst acls)"; }

echo ">> and nothing gets out around it"
if docker run --rm --network "$NET" "$CURL_IMAGE" -sS --max-time 5 https://api.github.com/ >/dev/null 2>&1; then
  fail "a container on $NET reached the internet without the proxy — the network is not internal"
fi

echo "EGRESS ISOLATION OK"
