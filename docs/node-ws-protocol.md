# Протокол узлового WebSocket-канала (`/v1/node/ws`)

План 0.3, пункт 2.1. Решение: ADR 0009. Канал несёт только управляющие
сообщения (назначения, отмены, liveness); события, завершение попыток,
артефакты и ACP-сессии идут существующими HTTP POST-эндпоинтами без изменений.

Все сообщения — текстовые фреймы, JSON-объекты:

```json
{ "type": "<тип>", ...поля }
```

## Подключение и авторизация

- `GET /v1/node/ws` с заголовками handshake:
  - `Authorization: Bearer <node-credential>` — та же credential, что на
    `/v1/node/*` (`node_id_for_credential`); невалидная/отозванная →
    HTTP 401, апгрейда нет.
- После апгрейда узел ОБЯЗАН первым отправить `hello`; сервер до `hello_ok`
  ничего не пушит.
- Один узел = одно активное подключение: новое подключение того же узла
  закрывает прежнее (код `4003`).

## Таблица сообщений

| Направление | type | Поля | Семантика |
|---|---|---|---|
| узел → CP | `hello` | `node_id`, `name`, `adapters`, `repositories`, `max_concurrency`, `protocol_version`, `agent_version` | Регистрация в сессии. Аналог полей `PollRequest`; несовместимый major `protocol_version` → close `4002`. |
| CP → узел | `hello_ok` | `server_time` (unix ms) | Сессия принята, узел в реестре. |
| CP → узел | `assignment` | `assignments`: массив `Assignment` (те же поля, что в `PollResponse.assignments`, включая `fencing_token`, `timeout_secs`, `attempt_id`) | Пуш назначения после commit планировщика. Батч ≤ свободных слотов узла. |
| узел → CP | `ack` | `attempt_ids`: массив, `fencing_tokens`: массив (параллельно `attempt_ids`, legacy-узлы опускают), `ok`: bool, `error?` | Подтверждение получения назначения. Нет ack за 25 с → attempt гасится reaper'ом, задача возвращается в очередь (как потерянный poll-ответ). `ok=false` → попытка сразу failed с ошибкой. |
| CP → узел | `cancel` | `attempt_id` | Отмена попытки (по `POST /v1/tasks/:id/cancel`). |
| узел → CP | `cancel_ack` | `attempt_id` | Подтверждение получения отмены. Отмена действительна по статусу attempt'а в store даже без ack — пуш только ускоряет доставку. |
| CP → узел | `ping` | — (WS ping-фрейм, раз в 15 с) | Liveness. Ответ — стандартный WS pong. Нет pong 45 с → разрыв. |
| узел → CP | `heartbeat` | `free_slots`: u32 | Узел сообщает о свободившихся слотах; CP может сразу пушить следующее назначение. Без heartbeat CP пушит только после ack/cancel_ack. |

## Коды закрытия соединения

| Код | Причина | Действие узла |
|---|---|---|
| 1000 | штатное закрытие (drain/stop CP) | reconnect по backoff |
| 4001 | credential отозвана/невалидна после апгрейда | только poll; требуется повторный enroll |
| 4002 | несовместимая `protocol_version` | только poll, узел degraded (`incompatible_protocol`) |
| 4003 | открыто новое подключение этого же узла | не переподключаться (это мы и есть дубль/старая сессия) |

## Семантика vs poll

WS и poll дают одинаковое поведение:

- назначение создаёт запись `attempts` в store до пуша; потеря пуша/ack =
  таймаут reaper'а, как потерянный HTTP-ответ поллинга;
- батч ограничивается `min(max_batch?, свободные слоты)` — в WS батч
  всегда оптимален по слотам (заголовок `x-agentgrid-max-batch` не нужен);
- `fencing_token` генерируется на назначении и проверяется на HTTP
  data-plane вызовах (ack/события/завершение) и на WS `ack`
  (`check_fencing_token`, рассинхрон → отклонение, как 409) — одинаково
  для обоих транспортов;
- узел на poll не видит назначений, отданных WS-узлу, и наоборот:
  планировщик один, очередь задач одна (`queued` → `assigned` атомарно);
- отмена идемпотентна по статусу attempt'а.

## Выбор транспорта на узле

`AGENTGRID_TRANSPORT`:

- `auto` (по умолчанию) — WS; после серии неудач handshake/обрывов
  переключение на poll, повторные попытки WS по экспоненциальному backoff
  (1 с → 60 с). Переключение логируется и видно в метрике.
- `ws` — только WS; при недоступности канала узел остаётся без назначений
  (reconnect), на poll не деградирует.
- `poll` — классический long polling (режим N-1).

Метрика CP: `agentgrid_node_transport_connections{transport="ws"}` (gauge);
на узле — `agentgrid_node_transport` (gauge, 1=ws/0=poll) плюс счётчик
переключений.

## Пример сессии

```
узел → CP:  {"type":"hello","node_id":"n-1","name":"w1","adapters":["mock"],
             "repositories":["repo"],"max_concurrency":2,
             "protocol_version":"1","agent_version":"0.3.0"}
CP → узел:  {"type":"hello_ok","server_time":1786350000000}
CP → узел:  {"type":"assignment","assignments":[{...attempt a1...},{...attempt a2...}]}
узел → CP:  {"type":"ack","attempt_ids":["a1","a2"],"fencing_tokens":["t1","t2"],"ok":true}
... работа, события идут по HTTP POST /v1/attempts/:id/events ...
CP → узел:  {"type":"cancel","attempt_id":"a2"}
узел → CP:  {"type":"cancel_ack","attempt_id":"a2"}
узел → CP:  {"type":"heartbeat","free_slots":1}
```
