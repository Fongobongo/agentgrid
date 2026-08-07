# Troubleshooting Guide — AgentGrid v0.3.1

Решение распространённых проблем в production среде.

## Transport & Connection Issues

### Node не подключается к Control Plane

**Симптомы:** Узел постоянно переподключается, heartbeat never reaches CP.

```bash
# Проверить логи узла
journalctl -u agentgrid-node-daemon -f | tail -50

# Искать ключевые строки
grep "error.*connect\|reconnect\|enroll" /var/log/agentgrid-node-daemon.log
```

**Возможные причины и решения:**

1. **Firewall блокирует порт 7800**
   ```bash
   # Проверить доступность порта
   nc -zv cp-host 7800
   ss -tlnp | grep 7800
   
   # Решить: открыть port 7800 в firewall
   sudo ufw allow 7800/tcp
   ```

2. **WebSocket upgrade заблокирован HTTP proxy**
   ```bash
   # Проверить поддержку WebSocket
   curl -v -N -H 'Connection: Upgrade' \
        -H 'Upgrade: websocket' \
        http://cp-host:7800/v1/node/ws
   
   # Решить: использовать polling mode
   export AGENTGRID_TRANSPORT=poll
   ```

3. **TLS certificate issues (HTTPS)**
   ```bash
   # Проверить cert validity
   openssl s_client -connect cp-host:443 -showcerts < /dev/null
   
   # Решить: добавить CA cert в trust store или disable TLS verification (dev only)
   export RUST_LOG=tls::debug
   ```

### Fencing token rejection (409)

**Симптомы:** WS ack rejected с fencing token mismatch, task stuck in Assigned state.

```bash
# Проверить fencing tokens в логCP
grep "ws ack rejected: fencing token" /var/log/control-plane.log

# Проверить attempt status
ag task show TASK_ID | grep -E "status|fencing_token"
```

**Решение:** Это нормально при параллельных попытках выполнения одной задачи. Система автоматически откатит старуюAttempt на новую. Если проблема persistent — проверить clock skew между узлами.

## Resource Issues

### High `write_lock_failures` counter

**Симптомы:** Metric `agentgrid_sqlite_write_lock_failures_total` растёт.

```bash
# Проверить текущий counter
curl -fsS http://cp:7800/metrics | grep write_lock_failures

# Проверить SQLite logs
journalctl -u agentgrid-control-plane | grep SQLITE_BUSY
```

**Причины:**
- Конкуренция за запись при high load
- Длительные транзакции в миграциях

**Решения:**
```bash
# Увеличить busy timeout
export AGENTGRID_SQLITE_BUSY_TIMEOUT=10000

# Включить batched assignment (coming soon)
# Или масштаироваться горизонтально (read replicas)
```

### High memory usage (RSS > budget)

**Симптомы:** `docker stats` показывает RSS >96MB для CP или >25MB для node idle.

```bash
# Проверяем RSS
docker stats --no-stream ag-control-plane-1
cat /proc/$(pgrep -f agentgrid-control-plane)/status | grep VmRSS

# Находим утечки
journalctl -u agentgrid-control-plane | grep -i "memory\|heap\|allocation"
```

**Диагностика:**
1. Проверить количество активных WS соединений
   ```bash
   curl -fsS http://cp:7800/metrics | grep ws_connections
   ```
2. Проверить размер базы данных
   ```bash
   du -sh /var/lib/agentgrid/control-plane.db*
   sqlite3 /var/lib/agentgrid/control-plane.db "SELECT count(*) FROM tasks;"
   ```

**Решения:**
- Очистить старые артефакты: `ag storage gc`
- Перезапустить узлы: `sudo systemctl restart agentgrid-node-daemon`
- Свести нагрузку до приемлемого уровня (scale down tasks/sec rate)

## Task Execution Issues

### Task stuck в "running" forever

**Симптомы:** Статус задачи не меняется из Running после назначения.

```bash
# Получить детальную инфу
ag task show TASK_ID --full

# Проверить лог адаптера
ag logs TASK_ID --adapter-log
```

**Возможные причины:**

1. **Адаптер crashed** — проверить exit code
   ```bash
   journalctl -u adapter-process-id 2>/dev/null || true
   
   # Resolved: cancel and retry
   ag task cancel TASK_ID
   ag run repo "prompt..." --adapter mock
   ```

2. **Adapter hung** — deadlock in tool call
   ```bash
   # Check adapter process
   ps aux | grep adapter-mock
   
   # Timeout kill
   kill -9 $(pgrep -f adapter-mock)
   
   # Retake with timeout
   ag task retry TASK_ID --timeout 120
   ```

3. **Event stream corrupted** — lost connection between node and CP
   ```bash
   # Verify event ingestion
   curl -fsS "http://cp:7800/v1/tasks/$TASK_ID/events?after_ingest=0" \
     | jq '.[].sequence' | sort -n | uniq -c
   
   # Solution: node reconnects automatically, events replay from outbox
   ```

### Adapter fails to start

**Симптомы:** Node reports "adapter not ready", no tasks execute.

```bash
# Check adapter availability
which adapter-mock
chmod +x /usr/local/bin/adapter-mock

# Check environment
AGENTGRID_ADAPTERS="mock" agentgrid-node-daemon

# Verify binary execution
adapter-mock --help 2>&1 | head -5
```

**Решения:**
```bash
# Rebuild binaries
cargo build --release -p agentgrid-adapters
sudo cp target/release/adapter-{mock,claude,opencode} /usr/local/bin/

# Or use docker image
docker pull ag-node:test
```

## Storage & Disk Issues

### Disk full warning

**Симптомы:** CP refuses new tasks, `degraded` nodes reported.

```bash
# Check disk usage
df -h /var/lib/agentgrid

# Check artifact tree
du -sh /var/lib/agentgrid/artifacts/*

# GC old artifacts
ag storage gc --older-than 48h
```

**Emergency cleanup:**
```bash
# Hard delete old artifacts (use carefully!)
find /var/lib/agentgrid/artifacts -mtime +7 -delete
sqlite3 /var/lib/agentgrid/control-plane.db "DELETE FROM artifacts WHERE uploaded_at < datetime('now', '-7 days');"
```

### Database corruption

**Симптомы:** SQLite errors on startup, migrations fail.

```bash
# Check DB integrity
sqlite3 /var/lib/agentgrid/control-plane.db "PRAGMA integrity_check;"

# Backup before repair
cp /var/lib/agentgrid/control-plane.db /backup/cp-repair-backup.db

# Repair database
sqlite3 /var/lib/agentgrid/control-plane.db "VACUUM;"
```

**Preventive:** Regular backups via cron:
```bash
# Add to crontab
0 2 * * * sqlite3 /var/lib/agentgrid/control-plane.db ".backup '/backup/cp-\$(date +\%Y\%m\%d).db'"
```

## WebSocket-Specific Issues

### WS connections not registering

**Симптомы:** Metrics show 0 WS connections despite running nodes.

```bash
# Check if CP WS endpoint is enabled
curl -v -N -w "%{http_code}" -o /dev/null \
  -H "authorization: Bearer <node_credential>" \
  ws://cp:7800/v1/node/ws

# Enable WS debug logging
export RUST_LOG=agentgrid_control_plane::ws=debug
systemctl restart agentgrid-control-plane

# Check registration
grep "registered.*online" /var/log/control-plane.log
```

### Pong timeout or ping failure

**Симптомы:** Node reports "WS close received" or "pong timeout".

```bash
# Increase heartbeat interval
export AGENTGRID_WS_HEARTBEAT_INTERVAL_SECS=60

# Check network latency
ping -c 10 cp-host | grep rttr

# For high-latency networks, increase timeout
export AGENTGRID_WS_PONG_TIMEOUT_SECS=5
```

## Performance Issues

### Slow assignment latency (assign_p50 > 50ms)

**Symptoms:** Metrics show high assignment p50/p99.

```bash
# Current metrics
curl -fsS http://cp:7800/metrics | grep 'assign_p[59]'

# Check queue backlog
sqlite3 /var/lib/agentgrid/control-plane.db \
  "SELECT COUNT(*) FROM tasks WHERE status='queued';"
```

**Optimizations:**
1. Switch to WebSocket transport for instant pushes
   ```bash
   export AGENTGRID_TRANSPORT=ws
   ```
2. Enable batched assignments (feature coming)
3. Scale read replicas for query offload

### Poll average too high (>2s)

**Symptoms:** `poll_avg_ms` metric >2000ms with many concurrent polls.

```bash
# Monitor poll statistics
curl -fsS http://cp:7800/metrics | grep poll_avg
```

**Diagnosis:** This indicates serial write contention under load. The single-writer gate works correctly but batched assignment can improve throughput.

**Workaround:** Reduce poll cadence if traffic is low:
```bash
export AGENTGRID_POLL_CADENCE_MS=2000
```

## Security Issues

### Suspicious 403/404 responses

**Symptoms:** Unauthorized access attempts, fencing token collisions.

```bash
# Check auth failures
grep "403\|404\|unauthorized" /var/log/control-plane.log

# Audit node revocations
sqlite3 /var/lib/agentgrid/control-plane.db \
  "SELECT id, status, revoked_at FROM nodes ORDER BY revoked_at DESC LIMIT 10;"
```

**Response:** If compromised node detected, revoke immediately:
```bash
# Revoke suspicious node
curl -fsS -X POST "http://cp:7800/v1/nodes/<NODE_ID>/revoke" \
  -H "authorization: Bearer <admin_jwt>"

# Force node re-enrollment
sudo systemctl restart agentgrid-node-daemon
```

## Rollback & Recovery

### Rollback after bad deployment

**Steps:**
```bash
# 1. Stop all services
sudo systemctl stop agentgrid-control-plane
sudo systemctl stop agentgrid-node-daemon

# 2. Restore previous binary
sudo cp /backup/agentgrid-control-plane-v0.3.0 /usr/local/bin/

# 3. Restart
sudo systemctl start agentgrid-control-plane

# 4. Verify migration status
sqlite3 /var/lib/agentgrid/control-plane.db \
  "SELECT version FROM migration_info ORDER BY version DESC LIMIT 1;"
```

### Emergency disaster recovery

**Scenario:** Complete data loss of control plane DB + artifacts.

```bash
# From backup:
cp /backup/cp-backup-latest.db /var/lib/agentgrid/control-plane.db
chown agentgrid:agentgrid /var/lib/agentgrid/control-plane.db

# Restore artifacts
rsync -avz backup:/agentgrid-artifacts/ /var/lib/agentgrid/artifacts/

# Start fresh node enrollment
sudo systemctl restart agentgrid-control-plane
export ENROLL_TOKEN=$(curl -fsS http://cp:7800/v1/nodes/enrollment-token)
sudo systemctl restart agentgrid-node-daemon
```

## Additional Resources

- **Full documentation:** `README.md`, `docs/runbook-transport.md`
- **Architecture decisions:** `docs/decisions/`
- **API reference:** `docs/openapi.yaml`
- **Changelog:** `CHANGELOG.md`

Для дополнительных вопросов смотрите GitHub issues или создавайте ticket с меткой `support`.
