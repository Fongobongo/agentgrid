#!/bin/bash
set -euo pipefail
cd /opt/ag
export AGENTGRID_JWT_SECRET="rss_test_secret_123456789012345678901234"
export AGENTGRID_LISTEN="0.0.0.0:7812"
nohup target/debug/agentgrid-control-plane > /tmp/rss-cp.log 2>&1 &
PID=$!
echo "Starting CP PID $PID..."
for i in $(seq 1 15); do
  if curl -fsS http://127.0.0.1:7812/health/ready >/dev/null 2>&1; then
    echo "CP ready after ${i}s"
    break
  fi
  sleep 1
done
RSS=$(awk '/VmRSS/{print $2/1024}' /proc/$PID/status 2>/dev/null || echo "N/A")
echo "CP RSS idle: $RSS MB"
curl -fsS http://127.0.0.1:7812/metrics | awk '/agentgrid_node_transport_connections{transport="ws"}/{print "WS connections:",$NF}'
kill $PID 2>/dev/null || true
