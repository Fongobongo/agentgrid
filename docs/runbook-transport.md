# Transport Runbook — когда Poll, когда WebSocket

Статус: **v0.3** (2026-08-07). Транспорт = выбор канала для доставки назначений от Control Plane к node-daemon.

## Кратко

| Параметр | `poll` | `ws` | `auto` (по умолчанию) |
|---|---|---|---|
| Задержка назначения | ~1–2 с (каденс опроса) | < 200 мс (push) | пробуем WS → fallback на poll |
| Нагрузка на CP | больше HTTP запросов | меньше трафика | гибридно |
| Устойчивость | максимальная | зависит от сети | WS+fallback |
| Рекомендация | old nodes, консервативно | современный кластер | default |

## Выбор через переменные окружения

```bash
AGENTGRID_TRANSPORT=poll   # только long-polling
AGENTGRID_TRANSPORT=ws     # только WebSocket
AGENTGRID_TRANSPORT=auto   # пробовать WS, fallback на poll после 3 неудач
```

В `docker-compose.yml` передаётся каждому узлу:
```yaml
environment:
  AGENTGRID_TRANSPORT: ${AGENTGRID_TRANSPORT:-auto}
```

## Когда использовать каждый режим

### `poll` — долгостой и устойчивость

- Легаси-клиенты без поддержки WS
- Сети с нестабильным TCP/HTTP proxy-проходом
- Консервативный деплой: проверено временем, минимум сюрпризов
- Low-memory nodes (меньше соединений держать)

Недостатки:
- Задержка назначения ≈ интервалу опроса (по умолчанию 1 с)
- Больше холостых запросов при низком потоке задач

### `ws` — современное производство

- Требование низкой задержки (< 500 мс)
- Кластеры > 10 узлов (меньше HTTP-шума)
- Выделенные каналы к Control Plane (без HTTP-прокси)
- Высоконагруженные системы: батч назначение через WS пушит до N задач за раз

Недостатки:
- Зависит от качества TLS/TCP стека
- Требует support от network infrastructure (upgradeable connections)

### `auto` — золотая середина (рекомендуется)

Попробуйте WS; если коннект не удаётся 3 раза подряд, падает на poll на duration backoff (1–60 с экспонента), потом retry WS. Идеально для:
- Миграции с poll → WS без простоя
- Mix legacy + modern узлов
- Production with fallback guarantee

## Мониторинг метрик

На `/metrics` CP доступны:

```
agentgrid_node_transport_connections{transport="ws"}  # количество WS-подключений
agentgrid_poll_requests_total                         # общее кол-во poll-запросов
agentgrid_ws_assignment_pushes_total                  # всего push через WS
```

Здоровье транспорта:
- WS mode: gauge ≥ 1 при наличии узлов
- Poll mode: poll_requests_total растёт steadily

Пример проверки:
```bash
curl -fsS http://cp:7800/metrics | grep agentgrid_node_transport_connections
```

## Тестирование в локальном стенде

### Docker Compose (2 узла)

```bash
AGENTGRID_TRANSPORT=ws bash tests/e2e/run.sh   # оба узла на WS
AGENTGRID_TRANSPORT=poll bash tests/e2e/run.sh # оба на poll
```

### Two-host E2E (локальный + удалённый хост)

```bash
AGENTGRID_TRANSPORT=ws bash tests/e2e/run-two-host.sh
AGENTGRID_TRANSPORT=poll bash tests/e2e/run-two-host.sh
```

Оба режима зелёные, workflow succeeds, все события доезжают.

## Load baseline (измерения 2026-08-07)

100 узлов, 1000 задач, debug binary:

| Transport | Wall time | p50 assign | p99 assign | write_lock_failures |
|---|---|---|---|---|
| auto | 30.4s | 21.3s | 29.5s | 0 |
| poll | 30.0s | 22.7s | 29.5s | 0 |

Разница в пределах шума — fallback корректен. Для снижения p99 нужна пакетная доставка (stage 1.2 batched assignment) или push по WS.

## Связь через промежуточный хост (релей на узле)

Когда узел не видит Control Plane напрямую (firewall, NAT без hairpin, не
открытый peering), но видит другой узел, который CP видит, — второй узел может
пронести трафик первого. Отдельный бинарь ставить не нужно: релей живёт в
`agentgrid-node-daemon` и включается одной переменной.

```
        ┌─────────────┐   нет маршрута   ┌──────────────┐
        │  узел B     │ ───────────────► │ Control Plane│
        │             │                  │              │
        │             │ ───► узел A ───► │              │
        └─────────────┘      релей       └──────────────┘
```

### На узле A (промежуточный хост)

```bash
AGENTGRID_RELAY_LISTEN=0.0.0.0:7900 \
AGENTGRID_RELAY_ALLOW=10.20.0.0/16 \
  agentgrid-node-daemon
```

- `AGENTGRID_RELAY_LISTEN` — адрес релея. Не задан = релея нет (по умолчанию).
- `AGENTGRID_RELAY_ALLOW` — CIDR-список источников, которым разрешено
  пользоваться релеем. Пусто = любой источник, и демон на старте пишет об этом
  предупреждение: релей расширяет круг хостов, которые достают до CP.
- Upstream не настраивается: релей всегда ходит на *текущий* CP этого узла
  (`base()`), поэтому при смене лидера он переключается на следующем запросе.
  Кластерный токен не нужен.

### На узле B (тот, который не видит CP)

```bash
AGENTGRID_SERVER=http://10.20.0.5:7900 \
AGENTGRID_TRANSPORT=ws \
  agentgrid-node-daemon
```

Всё. `poll`, `ws` и `auto` работают одинаково: релей проксирует HTTP, стримит
SSE и артефакты, и мостит `/v1/node/ws`. На стороне клиента ничего не меняется
— `AGENTGRID_SERVER` уже был точкой настройки.

### Что проверить, если не работает

| Симптом | Причина | Что делать |
|---|---|---|
| `403 peer not in the relay allowlist` | источник вне `AGENTGRID_RELAY_ALLOW` | добавить подсеть B; счётчик `agentgrid_edge_rejected_sources_total` растёт |
| демон A не стартует: `would forward to itself` | `AGENTGRID_SERVER` узла A указывает на его же релей | релей должен смотреть на CP, а не на себя |
| демон A не стартует: `bind the peer relay` | порт занят | другой порт или освободить |
| B регистрируется, но задачи не идут | задача не привязана к B | это не релей: назначение выбирает CP |
| `503 no control-plane leader` от релея | A сам потерял CP | смотреть транспорт A, не релей |

Счётчики релея — на его же `/metrics`:

```bash
curl -s http://10.20.0.5:7900/metrics | grep agentgrid_edge_
# agentgrid_edge_requests_total          — сколько запросов прошло
# agentgrid_edge_ws_bridges_total        — сколько WS-сессий проброшено
# agentgrid_edge_rejected_sources_total  — сколько хостов отбито allowlist'ом
# agentgrid_edge_upstream_errors_total   — сколько раз CP не ответил
```

### Границы

- **Один хоп.** Релей ходит на свой CP, а не на другой релей. Цепочка
  A → B → CP не поддерживается.
- **Это не обход авторизации.** `Authorization` передаётся как есть, CP
  по-прежнему аутентифицирует собственный креденшл узла B. Обходится только
  сетевая сегментация — ради чего всё и делается.
- **`/metrics` и `/health/*` релея перекрывают одноимённые пути CP** на этом
  порту (тот же компромисс, что у отдельного `agentgrid-cp-edge`).
- **HA-репликация CP** через релей не проверялась: теоретически можно указать
  peer URL на релей перед лидером, но это не покрыто тестами.

Полное обоснование — `docs/decisions/0026-relay-through-an-intermediate-host.md`.
E2E-сценарий — `tests/e2e/run-relay.sh` (в CI: «peer relay»).

## Troubleshooting

### Node не подключается по WS

1. Проверьте логи: `grep "error.*connect\|reconnect" /tmp/node.log`  
2. Убедитесь, что порт 7800 доступен и не заблокирован firewall  
3. В `auto` режиме fallback сработает автоматически — проверьте poll metrics

### Высокий `write_lock_failures`

Увеличите `busy_timeout` в config SQLite OR переходите на пакетное назначение (batch assignment). Это лечится stage 1.2.

### RSS превышен

CP idle = 4 MB при 50 WS-коннектах ≤ 96 MB budget. Если превышает — проверьте утечки в логах (`tracing::warn!`, heap allocation spikes).

## Future work

- **Stage 1.2:** packetized batch assignment — снизить `poll_avg_ms` < 50 мс
- **Stage 3.2:** полный замер RSS CP под нагрузкой (50 WS-коннектов + задачи)
- **Release v0.3.0:** musl binaries + final docs
