# Базовая линия нагрузки 0.3 (этап 0 плана 0.3)

Дата: 2026-08-06. Коммит: см. git log (`docs/plans/0.3-websocket-and-scale.md`).
Цель — точка отсчёта «до» для этапов 1 (масштабируемость CP) и 2 (WebSocket).

## Стенд

- Железо: 3 vCPU Intel Xeon E5-2699 v4 @ 2.20GHz, 4 ГБ RAM, диск HDD-класса.
- Harness: `tests/e2e/run-load.sh` → `crates/control-plane/tests/load.rs`
  (`#[ignore]`-тест `load_baseline_mock_nodes`): реальный HTTP-сервер CP
  (debug-профиль!) + N mock-узлов (enroll → poll → ack → complete) по M задач.
- Параметры: `AG_LOAD_NODES=50 AG_LOAD_TASKS=500 AG_LOAD_POLL_MS=1000`
  (каденс опроса как у реального node-daemon: ~1 с).

## Результат (базовый прогон)

```
LOAD-RESULT nodes=50 tasks=500 completed=500 wall_s=15.9 \
  assign_p50_ms=10883 assign_p99_ms=15769 assign_max_ms=15872 \
  write_txns=2105 write_lock_failures=0 poll_requests=500 poll_avg_ms=585.04
```

| Метрика | Значение | Комментарий |
|---|---|---|
| Wall time на 500 задач | 15.9 с | все задачи succeeded |
| Задержка назначения p50 | 10 883 мс | от создания задачи до получения узлом |
| Задержка назначения p99 | 15 769 мс | хвост ≈ полному дренированию очереди |
| `write_lock_failures` | 0 | жёстких SQLITE_BUSY-отказов нет |
| Write-транзакций | 2105 | ~4 на задачу (create/assign/ack/complete) |
| Poll-запросов | 500 | каждый вернул назначение (1 запрос = 1 назначение) |
| Средний poll-хендлер | 585 мс | главный сигнал: сериализация записи |

Sanity-прогон (5 узлов / 20 задач / poll 100 мс): wall 0.4 с, p50 305 мс,
poll_avg 31 мс, failures 0 — деградация нелинейна по числу узлов.

## Интерпретация

1. **Назначение — сериализованная запись.** `try_assign` делает выбор
   queued-задачи + создание попытки + перевод задачи в `running` одной
   `BEGIN IMMEDIATE`-транзакцией (`store/scheduler.rs`); poll-хендлер —
   серверный long-poll (удержание до 25 с, ожидание через `Notify`).
   50 узлов конкурируют за писателя: в среднем 585 мс от poll-запроса
   до выдачи назначения (против 31 мс на 5 узлах). Это ровно то, что
   лечит этап 1: единый писатель (1.1) + пакетное назначение (1.2).
2. **Задержка назначения ≈ время дренирования очереди.** Задачи создаются
   пачкой; p99 ~16 с — последний хвост из 500 задач при пропускной
   способности ~31 назначение/с. Пакетное назначение (одна транзакция на
   пакет) должно поднять пропускную способность на порядок.
3. **Жёстких отказов нет** (`write_lock_failures=0`): `busy_timeout=5s`
   пока спасает, но запас съеден — рост нагрузки упрётся в 5-секундные
   таймауты.
4. Debug-профиль бинарника занижает абсолютные числа; сравнение «до/после»
   валидно на одном профиле и одном железе.

## Цели «после» (приёмка этапов 1–2 тем же harness'ом)

- `poll_avg_ms` < 50 при 50 узлах (этап 1).
- assign p99 < 200 мс при потоке 50 задач/с на свободный кластер —
  достижимо только с push-доставкой назначений (этап 2, WebSocket);
  на polling с каденсом 1 с математический предел ~1 с.
- `write_lock_failures` = 0 на всём диапазоне.

## Как воспроизвести

```bash
cargo build --workspace          # debug достаточно
tests/e2e/run-load.sh            # 50 узлов / 500 задач
AG_LOAD_NODES=100 AG_LOAD_TASKS=1000 tests/e2e/run-load.sh   # расширенный
```

Метрики под нагрузкой доступны на `/metrics`:
`agentgrid_sqlite_write_txns_total`, `agentgrid_sqlite_write_lock_failures_total`,
`agentgrid_poll_requests_total`, `agentgrid_poll_duration_ms_sum`,
`agentgrid_oldest_queued_task_seconds` (+ панели в
`deploy/grafana-dashboard.json`).
