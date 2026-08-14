# RSS Baseline — Idle (debug binary)

План 0.3 Этап 3.2 — замеры RSS бюджетов.

> Статус: **idle baseline (debug build)**. Полный нагрузочный замер (100 mock-узлов
> × 1000 задач, Этап 3.1) отложен — ему нужен либо docker-compose harness, либо
> удалённая two-host box; отчёт «после» против этой отметки пишется когда
> запустится 3.1.

## Метод

-   `deploy/dev-bench/measure-rss-baseline.sh` поднимает `agentgrid-control-plane` debug
    binary на `127.0.0.1:7801` с временной SQLite (fresh WAL), бутстрапит первого
    пользователя, минтит enrollment token, загружает `agentgrid-node-daemon`
    (mock adapter, long-poll), ждёт буфер heartbeat, сэмплирует `VmRSS` из
    `/proc/<pid>/status` по каждому процессу и складывает.

-   Подacht — **debug** экземпляр (не musl release); release реально меньше (нужно
    масштабировать вниз, грубый ориентир — debug на 30–60% тяжелее release). Точка
    отсчёта, не Бюджет.

-   Условия измерения: idle, 0 задач, 0 active attempts, 1 long-poll коннект.
    Нагрузка из шапки 0.3 (50 WS-conнектов на CP + tasks) здесь **не** повторена.

## Замеры (debug build)

## Date: 2026-08-14

| binary                       | VmRSS sum | budget (plan AGENTS) | статус  |
|------------------------------|-----------|----------------------|---------|
| agentgrid-control-plane idle | ~52 МБ    | 64 МБ                 | below ✓ |
| agentgrid-node-daemon idle   | ~17 МБ    | 25 МБ                 | below ✓ |

(node зарегистрирован в статус `degraded` — mock adapter binary отсутствует в
PATH скрипта. Это не влияет на RSS замер — degraded node всё ещё держит
polling loop; pre-load halo.)

## Интерпретация

-   Оба idle замера под бюджетом на **debug** бинаре. Release вероятно
    значительно ниже; бюджет остаётся консервативным.
-   Рост RSS под нагрузкой (план 0.3 Этап 3.1) — **не** оценён. До тех пор
    бег по 50 WS-коннектов не подтверждён. Бюджет `≤96 МБ CP с WS` (напр. из
    `runbook-transport.md`) оставлен как целевой, не подтверждённый замером.

## Future work

-   Повторить этот отчёт на **release**/musl бинаре.
-   Нагрузочный замер по Этап 3.1 — 100 mock-узлов × 1000 задач, смешанный
    транспорт. Нужен либо локальный docker-compose (100 контейнеров на одной
    машине — м.б. нереалистично на dev box), либо two-host runner на
    `191.96.11.161`. До тех пор **CP ≤ 96 МБ** — целевое число, не замер.
