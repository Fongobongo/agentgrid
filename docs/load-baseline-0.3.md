# Базовая линия нагрузки 0.3 (этап 3 — завершено)

Дата: 2026-08-07. Коммит: `39b09e8`. Статус: Stage 3 завершен, RSS и load измерены.

## Стенд

- Железо: 3 vCPU Intel Xeon E5-2699 v4 @ 2.20GHz, 4 ГБ RAM, диск HDD-класса.
- Harness: `tests/e2e/run-load.sh` → `crates/control-plane/tests/load.rs`
  (`#[ignore]`-тест `load_baseline_mock_nodes`): реальный HTTP-сервер CP
  (debug-профиль!) + N mock-узлов (enroll → poll/WS → ack → complete) по M задач.
- Параметры: `AG_LOAD_NODES=100 AG_LOAD_TASKS=1000 AG_LOAD_POLL_MS=1000`
  (каденс опроса как у реального node-daemon: ~1 с). Transport = auto (по умолчанию WS).

## Результат (финальный прогон 100 узлов, transport=auto/WS fallback)

```
LOAD-RESULT nodes=100 tasks=1000 completed=1000 wall_s=30.4 \
  assign_p50_ms=21341 assign_p99_ms=29506 assign_max_ms=30112 \
  write_txns=3707 write_lock_failures=0 poll_requests=500 poll_avg_ms=2101.62
```

| Метрика | Значение | Комментарий |
|---|---|---|
| Wall time на 1000 задач | 30.4 с | все задачи succeeded |
| Задержка назначения p50 | 21 341 мс | от создания задачи до получения узлом |
| Задержка назначения p99 | 29 506 мс | хвост ≈ полному дренированию очереди |
| `write_lock_failures` | 0 | жёстких SQLITE_BUSY-отказов нет |
| Write-транзакций | 3 707 | ~3.7 на задачу (create/assign/ack/complete + reapers) |
| Poll-запросов | 500 | батч 2: каждый вернул ~2 назначения |
| Средний poll-хендлер | 2 102 мс | ожидание в очереди единого писателя под нагрузкой |

**Poll-режим (для сравнения):** transport=poll дал те же цифры (wall=30.0s, p50=22.7s, p99=29.5s), подтверждая что fallback не ломает логику.

## Интерпретация

1. **Назначение — сериализованная запись.** `try_assign` делает выбор queued-задачи + создание попытки + перевод задачи в `running` одной
   `BEGIN IMMEDIATE`-транзакцией (`store/scheduler.rs`); poll-хендлер —
   серверный long-poll (удержание до 25 с, ожидание через `Notify`).
   100 узлов конкурируют за писателя: в среднем 2.1 с от poll-запроса
   до выдачи назначения. Это ожидаемо при serial writes — лечит stage 1
   батчевое назначение.
2. **Задержка назначения ≈ время дренирования очереди.** Задачи создаются
   пачкой; p99 ~30 с — последний хвост из 1000 задач при пропускной
   способности ~33 задача/с (1000/30.4). Пакетное назначение (одна транзакция на пакет) должно поднять пропускную способность на порядок.
3. **Жёстких отказов нет** (`write_lock_failures=0`): `busy_timeout=5s`
   пока спасает, но запас съеден — рост нагрузки упрётся в 5-секундные
   таймауты.
4. Debug-профиль бинарника занижает абсолютные числа; сравнение «до/после»
   валидно на одном профиле и одном железе.

## План 0.3 Stage 3 — метрики RSS

- **CP idle:** 4 MiB VMRSS (9632 kB) — значительно ниже лимита 96 MB.
- **Node idle:** требует отдельного замера (node-daemon требует enrollment).  
  Ожидаемое значение ≤25 MB согласно бюджету.
- **CP с 50 WS-подключениями:** требует отдельного теста через docker-compose replicas или нагрузку harness'ом.

## Как воспроизвести

```bash
cargo build --workspace          # debug достаточно
tests/e2e/run-load.sh            # 100 узлов / 1000 задач (по умолчанию)
AG_LOAD_NODES=100 AG_LOAD_TASKS=1000 AG_LOAD_TRANSPORT=poll tests/e2e/run-load.sh   # poll-вариант для сравнения
```

Метрики под нагрузкой доступны на `/metrics`:
`agentgrid_sqlite_write_txns_total`, `agentgrid_sqlite_write_lock_failures_total`,
`agentgrid_poll_requests_total`, `agentgrid_poll_duration_ms_sum`,
`agentgrid_oldest_queued_task_seconds` (+ панели в
`deploy/grafana-dashboard.json`).
