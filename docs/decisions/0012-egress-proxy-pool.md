# ADR 0012: CP-managed egress proxy pool с failover

Status: **Accepted** (v0.4.3).

## Контекст

Ноды подключаются наружу (control plane, LLM API адаптеров, GitHub
write-back, sandboxed-контейнеры). Операторам нужно гнать этот трафик через
свои прокси — и не одним, а пулом: один прокси падает, нода должна сама
свалиться на следующий, без ручной переконфигурации флота.

Варианты:

1. **Только env на ноде** (`HTTP_PROXY` и т.п.) — один прокси, без failover,
   без централизованного управления, per-process hacks в адаптерах.
2. **Прокси в docker-сети** (существующий egress-sidecar для `restricted`) —
   покрывает только sandboxed-контейнеры, не покрывает сам демон.
3. **CP-управляемый пул + failover в демоне** — один источник правды,
   ротация без перезапуска.

## Решение

Вариант 3:

- **CP — источник правды.** Таблица `proxies` (миграция 0080):
  `node_id NULL` = глобальный пул, иначе — запись для конкретной ноды.
  CRUD: `/v1/proxies` + `ag proxy ls/add/rm`. Нода получает эффективный
  список (глобал → node-scoped) в каждом `PollResponse.proxy_urls`.
- **Node — failover.** `ProxyPool` (node-daemon/src/proxy.rs): первый
  живой URL в порядке списка; connect/timeout помечает URL мёртвым на
  5 минут (TTL), затем он автоматически возвращается в ротацию. Все мертвы
  → прямой egress (fail-open для CP-трафика).
- **Поверхности применения:** poll/WS-fallback HTTP-клиент, GitHub API
  (PR/comment), env попытки (`HTTP(S)_PROXY`/`ALL_PROXY` для bare и
  контейнерных адаптеров).
- **Override:** `AGENTGRID_PROXY_URLS=url1,url2` на ноде полностью
  подавляет CP-список (наличие env = "админ ноды знает лучше").

## Последствия / ограничения

- WS-коннект проксировать нельзя (tokio-tungstenite без CONNECT); ноды в
  прокси-сети живут на `Transport::Poll`/`Auto`-fallback.
- Failover реактивный (по ошибке), не health-check'ами — до 1 failed
  запроса на смену прокси. Если это окажется больно — отдельный prober.
- Fail-open для CP-трафика осознанно: мёртвый пул не должен останавливать
  флот. Для sandbox restricted это не распространяется — там egress
  контролируется сетью.

## Triggers для пересмотра

- Нужен health-check/prober прокси (много жалоб на первый-failed-request).
- WS через CONNECT (когда будет реальный demand у операторов).
