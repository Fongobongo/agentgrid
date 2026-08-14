#!/usr/bin/env bash
# Measure idle RSS budgets for control-plane + node-daemon (debug binaries).
# Runs entirely inside one bash call: launches CP with timeout, bootstraps
# a user, enrolls a node pinging with long-poll, snapshots VmRSS, tears down.

set -euo pipefail
CP_PORT=7801
CP_DB=/tmp/ag-rss-cp.db
CP_LOG=/tmp/ag-rss-cp.log
NODE_LOG=/tmp/ag-rss-node.log
rm -f "$CP_DB"* "$CP_LOG" "$NODE_LOG"
pkill -u "$(id -un)" -f agentgrid-control-plane 2>/dev/null || true
pkill -u "$(id -un)" -f agentgrid-node-daemon 2>/dev/null || true
sleep 2

# Launch CP in foreground w/ timeout. Background the whole bash script from the
# caller; this script self-terminates within TIMEOUT_CP.
TOUCH=/tmp/ag-rss-alive; rm -f $TOUCH

# Use setsid to detach; output goes to logfile; pipe stdin from /dev/null.
AGENTGRID_LISTEN=127.0.0.1:$CP_PORT AGENTGRID_DB=$CP_DB setsid -f ./target/debug/agentgrid-control-plane >"$CP_LOG" 2>&1 < /dev/null
# setsid -f returns immediately; child runs.
sleep 5

for _ in $(seq 1 30); do
    if curl -sS -o /dev/null "http://127.0.0.1:$CP_PORT/v1/auth/setup" 2>/dev/null; then
        break
    fi
    sleep 1
done

SETUP=$(grep -oE '[0-9a-f]{32}' "$CP_LOG" | head -1)
echo "SETUP=$SETUP"
[ -n "$SETUP" ] || { echo "no setup token"; tail -5 "$CP_LOG"; exit 1; }
curl -sS -X POST "http://127.0.0.1:$CP_PORT/v1/auth/setup" -H "content-type: application/json" \
    -d "{\"username\":\"admin\",\"password\":\"adminpw\",\"setup_token\":\"$SETUP\"}" -o /tmp/x.json -w "setup -> %{http_code}\n"
JWT=$(curl -sS -X POST "http://127.0.0.1:$CP_PORT/v1/auth/login" -H "content-type: application/json" \
    -d '{"username":"admin","password":"adminpw"}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
echo "JWT=${JWT:0:20}..."
ETOK=$(curl -sS -X POST "http://127.0.0.1:$CP_PORT/v1/nodes/enrollment-token" -H "authorization: Bearer $JWT" -d '{}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')
echo "ETOK=${ETOK:0:16}..."

# Launch node-daemon in bg with timeout; measure idle RSS while it's polling.
AGENTGRID_SERVER=http://127.0.0.1:$CP_PORT \
AGENTGRID_NODE_NAME=rss-probe-1 \
AGENTGRID_DATA_DIR=/tmp/ag-rss-node \
AGENTGRID_ENROLL_TOKEN=$ETOK \
AGENTGRID_ADAPTERS=mock \
AGENTGRID_TRANSPORT=poll \
setsid -f ./target/debug/agentgrid-node-daemon >"$NODE_LOG" 2>&1 < /dev/null
sleep 6

# Confirm node enrolled (heartbeat heartbeat).
curl -sS "http://127.0.0.1:$CP_PORT/v1/nodes" -H "authorization: Bearer $JWT" -o /tmp/nodes.json -w "nodes -> %{http_code}\n"
python3 -c 'import json; d=json.load(open("/tmp/nodes.json")); items=d.get("items",d);
for n in items:
    nid = n["id"]; st = n["status"]
    print(f"  node {nid[:24]} status={st}")'

# Snapshot VmRSS.
echo "=== VmRSS snapshot ==="
total_cp=0
for pid in $(pgrep -f "agentgrid-control-plane"); do
    rss=$(awk '/^VmRSS/{print $2}' /proc/$pid/status 2>/dev/null)
    [ -n "$rss" ] && { total_cp=$((total_cp + rss)); echo "  CP pid $pid: $rss KB"; }
done
total_nd=0
for pid in $(pgrep -f "agentgrid-node-daemon"); do
    rss=$(awk '/^VmRSS/{print $2}' /proc/$pid/status 2>/dev/null)
    [ -n "$rss" ] && { total_nd=$((total_nd + rss)); echo "  ND pid $pid: $rss KB"; }
done
echo "=== totals (debug binary) ==="
echo "  CP total: ${total_cp} KB (~$((total_cp / 1024)) MB)   budget: 64 MB"
echo "  ND total: ${total_nd} KB (~$((total_nd / 1024)) MB)   budget: 25 MB"

# Cleanup.
pkill -u "$(id -un)" -f agentgrid-control-plane 2>/dev/null || true
pkill -u "$(id -un)" -f agentgrid-node-daemon 2>/dev/null || true
sleep 2
echo "=== torn down ==="
pgrep -af agentgrid-control-plane | grep -v 1182424 | head || echo "no CP"
pgrep -af agentgrid-node-daemon | head || echo "no ND"
