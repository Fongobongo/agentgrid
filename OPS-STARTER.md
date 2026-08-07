# Operator Starter Guide — AgentGrid v0.3.1

Быстрый старт для развёртывания и поддержки кластера AgentGrid.

## Предварительные требования

- Linux (x86_64 или aarch64) с ядром ≥ 5.10
- Git, Docker или systemd
- SQLite WAL support (уже в бинарниках)

## Быстрое развёртывание (Docker Compose)

### Шаг 1: Поднять control plane + 2 узла

```bash
cd /opt/ag
export AGENTGRID_JWT_SECRET="$(head -c 48 /dev/urandom | base64)"
export NODE1_TOKEN="$(openssl rand -hex 32)"
export NODE2_TOKEN="$(openssl rand -hex 32)"

# Это создаст .env и поднимет сервисы
bash deploy/compose/up.sh
```

### Шаг 2: Проверить здоровье

```bash
curl -fsS http://127.0.0.1:7800/health/ready && echo "OK"
docker ps --format '{{.Names}} {{.Status}}'
```

Ожидаемое состояние: `ag-control-plane-1`, `ag-node-1-1`, `ag-node-2-1` все `Up`.

### Шаг 3: Запустить тестовую задачу

```bash
AGENTGRID_SERVER="http://127.0.0.1:7800"
PASSWORD=$(cat deploy/compose/.env | grep ADMIN_PASS | cut -d= -f2)

# Login
JWT=$(curl -fsS -X POST "$AGENTGRID_SERVER/v1/auth/login" \
  -H 'content-type: application/json' \
  -d "{\"username\":\"admin\",\"password\":\"$PASSWORD\"}" \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')

# Submit mock task
TASK_ID=$(curl -fsS -X POST "$AGENTGRID_SERVER/v1/tasks" \
  -H "authorization: Bearer $JWT" -H 'content-type: application/json' \
  -d '{"prompt":"test","adapter":"mock","repository":"*","timeout_secs":60}' \
  | python3 -c 'import sys,json;print(json.load(sys.stdin)["id"])')

echo "Task created: $TASK_ID"
```

### Шаг 4: Отследить выполнение

```bash
# Ждать завершения
for i in $(seq 1 60); do
  STATUS=$(curl -fsS "$AGENTGRID_SERVER/v1/tasks/$TASK_ID" -H "authorization: Bearer $JWT" \
    | python3 -c 'import sys,json;print(json.load(sys.stdin)["status"])')
  echo "Status: $STATUS"
  [ "$STATUS" = "succeeded" ] && break || sleep 1
done

# Показать лог событий
curl -fsS "$AGENTGRID_SERVER/v1/tasks/$TASK_ID/events" -H "authorization: Bearer $JWT" | jq '.[].type'
```

## Развёртывание systemd (production)

### Control Plane

```bash
# Установить control plane
sudo bash deploy/install-control-plane.sh --listen 127.0.0.1:7800

# Включить и запустить
sudo systemctl enable agentgrid-control-plane
sudo systemctl start agentgrid-control-plane

# Проверить статус
journalctl -u agentgrid-control-plane -f
```

### Node Daemon

```bash
# На хосте с агентом
export AGENTGRID_SERVER="https://cp.example.com:7800"
export AGENTGRID_ENROLL_TOKEN=$(curl -fsS https://cp.example.com:7800/v1/nodes/enrollment-token \
  -H "authorization: Bearer <bootstrap_token>")

sudo bash deploy/install-node.sh --adapters "mock,claude"
sudo systemctl enable agentgrid-node-daemon
sudo systemctl start agentgrid-node-daemon
```

## Мониторинг

### Health checks

```bash
# Control plane ready
curl -fsS http://cp:7800/health/ready && echo "CP OK"

# Control plane live
curl -fsS http://cp:7800/health/live && echo "CP ALIVE"
```

### Metrics endpoint

```bash
# All metrics
curl -fsS http://cp:7800/metrics

# Key counters
grep 'agentgrid_node_transport_connections{transport="ws"}' /metrics
grep 'agentgrid_poll_requests_total' /metrics
grep 'agentgrid_sqlite_write_lock_failures_total' /metrics
grep 'agentgrid_oldest_queued_task_seconds' /metrics
```

### Grafana dashboard

Импорт JSON:
```bash
curl -fsS https://raw.githubusercontent.com/earendil-works/agentgrid/main/deploy/grafana-dashboard.json > ag-cp.json
```

## Troubleshooting

### Узел не подключается по WebSocket

```bash
# Проверить логи
journalctl -u agentgrid-node-daemon -f | grep -i "error.*connect\|reconnect"

# Пробросить на polling
export AGENTGRID_TRANSPORT=poll
```

### Высокие `write_lock_failures`

Увеличить таймаут в config:
```bash
export AGENTGRID_SQLITE_BUSY_TIMEOUT=10000
```

Или включить batched assignment (coming soon).

### Задача зависла в `running`

```bash
# Проверить адаптер
ag task show TASK_ID
ag task logs TASK_ID

# Cancel если нужно
ag task cancel TASK_ID
```

## Обновление

### Docker Compose

```bash
docker compose down
docker rmi ag-cp:test ag-node:test
docker build -t ag-cp:test -f Dockerfile.control-plane .
docker build -t ag-node:test -f Dockerfile.node-daemon .
bash deploy/compose/up.sh
```

### systemd

```bash
# Control plane
sudo systemctl stop agentgrid-control-plane
sudo cp target/release/agentgrid-control-plane /usr/local/bin/
sudo systemctl start agentgrid-control-plane

# Nodes
sudo systemctl stop agentgrid-node-daemon
sudo cp target/release/agentgrid-node-daemon /usr/local/bin/
sudo systemctl start agentgrid-node-daemon
```

## Резервное копирование

```bash
# База данных
sqlite3 /var/lib/agentgrid/control-plane.db ".backup '/backup/cp-backup-$(date +%Y%m%d).db'"

# Артефакты
rsync -avz /var/lib/agentgrid/artifacts/ backup:/agentgrid-artifacts/

# Перезапуск CP после бэкапа
sudo systemctl reload agentgrid-control-plane
```

## Безопасность

- **Не публиковать порт 7800** без TLS + обратного прокси
- **Ротация JWT секретов** еженедельно (`AGENTGRID_JWT_SECRET`)
- **Read-only root filesystem** в Docker (уже настроен)
- **Drop all capabilities** (уже настроен)

## Ресурсы

- **CP idle RSS**: ~4 MiB
- **Node idle RSS**: ~15 MiB (оценка)
- **Лимиты**: CP ≤96MB, node ≤25MB idle

См. `docs/runbook-transport.md` для выбора транспорта и `CHANGELOG.md` для истории изменений.
