#!/usr/bin/env bash
# Release-artifact smoke test (hardening follow-up to the v0.3.7 musl
# HEALTHCHECK bug): actually BOOT the built container images and verify
# liveness end to end — not just that they compile.
#
#  1. Dockerfile.control-plane-musl (FROM scratch, no shell → deliberately no
#     HEALTHCHECK; liveness is the HTTP listener): docker run + curl
#     /health/ready from outside, then a task-API round trip (login +
#     task list) to prove the DB and WAL work on a fresh volume.
#  2. Dockerfile.control-plane + Dockerfile.node-daemon (glibc, wget/shell
#     present): assert `docker inspect` reports Health.Status == healthy
#     after the stack is up (the original bug class: HEALTHCHECK CMD that
#     could never succeed in its own image).
#
# Requires: docker, curl, python3. Runs the same images e2e/run.sh builds;
# safe to re-run (tears everything down on exit).
set -euo pipefail
cd "$(dirname "$0")/../.."

cleanup() {
  docker rm -f ag-musl-smoke >/dev/null 2>&1 || true
  bash deploy/compose/down.sh --purge >/dev/null 2>&1 || true
}
trap cleanup EXIT

echo ">> building images (cp-musl, cp, node)"
docker build -q -t ag-cp-musl:smoke -f Dockerfile.control-plane-musl . >/dev/null
docker build -q -t ag-cp:test -f Dockerfile.control-plane . >/dev/null
docker build -q -t ag-node:test -f Dockerfile.node-daemon . >/dev/null

echo ">> 1/2 musl control-plane (FROM scratch): boot + /health/ready + API round trip"
docker run -d --name ag-musl-smoke -p 127.0.0.1:7811:7800 ag-cp-musl:smoke >/dev/null
for _ in $(seq 1 30); do
  curl -fsS http://127.0.0.1:7811/health/ready >/dev/null 2>&1 && break
  sleep 1
done
curl -fsS http://127.0.0.1:7811/health/ready >/dev/null \
  || { echo "SMOKE FAILED: musl control-plane never became ready"; exit 1; }
echo "musl cp: /health/ready OK"

# Fresh DB bootstraps an admin via the log's setup token; assert login works
# end to end (proves SQLite WAL + migrations on the musl binary inside the
# scratch image, on a real volume).
SETUP=$(docker logs ag-musl-smoke 2>&1 | grep -oE '[0-9a-f]{32}' | head -1 || true)
[ -n "$SETUP" ] || { echo "SMOKE FAILED: no setup token in musl cp logs"; exit 1; }
curl -fsS -X POST http://127.0.0.1:7811/v1/auth/setup \
  -H 'content-type: application/json' \
  -d "{\"username\":\"smoke\",\"password\":\"smokepw\",\"setup_token\":\"$SETUP\"}" >/dev/null
JWT=$(curl -fsS -X POST http://127.0.0.1:7811/v1/auth/login \
  -H 'content-type: application/json' \
  -d '{"username":"smoke","password":"smokepw"}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
[ -n "$JWT" ] || { echo "SMOKE FAILED: login on musl cp"; exit 1; }
curl -fsS "http://127.0.0.1:7811/v1/tasks?limit=1" -H "authorization: Bearer $JWT" >/dev/null \
  || { echo "SMOKE FAILED: task list on musl cp"; exit 1; }
echo "musl cp: setup + login + task list OK"
docker rm -f ag-musl-smoke >/dev/null

echo ">> 2/2 glibc images: HEALTHCHECK status must reach 'healthy'"
export AGENTGRID_ADMIN_USER=smoke
export AGENTGRID_ADMIN_PASSWORD=smokepw
bash deploy/compose/up.sh >/dev/null
# docker-compose.yml uses `:?` interpolation guards; compose re-interpolates
# the whole file even for read-only `ps`/`inspect`. After bootstrap up.sh
# strips the one-time node tokens from deploy/compose/.env, so the guards
# only pass with dummy exports (same pattern as deploy/compose/down.sh).
export AGENTGRID_JWT_SECRET="${AGENTGRID_JWT_SECRET:-smoke}"
export NODE1_TOKEN="${NODE1_TOKEN:-smoke}" NODE2_TOKEN="${NODE2_TOKEN:-smoke}"
# The compose project name is fixed by the directory (ag); ps -q takes the
# service name regardless of the container_name compose derives.
CP_CID="$(docker compose -f docker-compose.yml ps -q control-plane)"
ND_CID="$(docker compose -f docker-compose.yml ps -q node-1)"
# HEALTHCHECK: interval 30s start-period 10s → first probe can take ~40s.
for _ in $(seq 1 24); do
  CP_H=$(docker inspect --format='{{.State.Health.Status}}' "$CP_CID" 2>/dev/null || echo starting)
  ND_H=$(docker inspect --format='{{.State.Health.Status}}' "$ND_CID" 2>/dev/null || echo starting)
  echo "health: cp=$CP_H node=$ND_H"
  [ "$CP_H" = "healthy" ] && [ "$ND_H" = "healthy" ] && break
  sleep 5
done
[ "$CP_H" = "healthy" ] || { echo "SMOKE FAILED: control-plane Health=$CP_H (not healthy)"; exit 1; }
[ "$ND_H" = "healthy" ] || { echo "SMOKE FAILED: node-daemon Health=$ND_H (not healthy)"; exit 1; }
echo "glibc cp + node HEALTHCHECK: healthy OK"

echo "SMOKE OK"
