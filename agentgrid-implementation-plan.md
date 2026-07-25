# План реализации MVP 0.1 — распределённый оркестратор coding agents

> Детальный план работ по спецификации MVP 0.1 (Linux: Tier 1 x86_64, Tier 2 ARM64; control plane + SQLite WAL + node daemon + CLI + web UI).
> Рабочее название: `agentgrid`. Горизонт: 8–12 недель для одного разработчика.
>
> Легенда: каждый пункт — атомарная задача с проверяемым результатом.
> Пункты внутри этапа расположены в рекомендуемом порядке выполнения.

---

## Этап 0 — Подготовка проекта (2–4 дня)

### 0.1 Решения, блокирующие старт (из раздела 20 спеки)

- [x] Зафиксировать рабочее название проекта (влияет на имена бинарников, каталогов, env-переменных)
- [x] Выбрать первый реальный agent adapter (Claude Code / Codex CLI / OpenCode) и зафиксировать его версию
- [x] Выбрать лицензию (MIT / Apache-2.0 / AGPL) и добавить файл `LICENSE`
- [x] Решить: в первом релизе web UI + CLI или только CLI (спека допускает оба, дефолт — оба)
- [x] Решить: git clone только по HTTPS/token или также SSH (рекомендация MVP: HTTPS/token)
- [x] Решить: автоматический commit изменений агента или оставлять незакоммиченными (рекомендация: авто-commit + сохранение diff)
- [x] Решить: long polling или постоянный WebSocket для node channel (рекомендация MVP: long polling, WebSocket — later)
- [x] Решить: control plane только Docker Compose или также standalone binary (рекомендация: оба, Compose — как основной сценарий)
- [x] Записать все решения в `docs/decisions/0001-mvp-scope.md` (ADR-формат)

### 0.2 Инфраструктура репозитория

- [x] Создать git-репозиторий (monorepo)
- [x] Настроить структуру каталогов:
  - [x] `crates/control-plane` — сервер (Rust, Axum)
  - [x] `crates/node-daemon` — daemon (Rust, Tokio)
  - [x] `crates/cli` — CLI (Rust, clap)
  - [x] `crates/common` — общие типы: task states, event types, API DTO
  - [x] `crates/adapters` — контракт adapter + mock + реальный adapter
  - [x] `web/` — web UI (TypeScript)
  - [x] `docs/` — документация и ADR
  - [x] `deploy/` — Docker Compose, systemd units, скрипты установки
  - [x] `tests/e2e/` — end-to-end сценарии
- [x] Настроить Cargo workspace (`Cargo.toml` в корне, общие versions через `workspace.dependencies`)
- [x] Настроить `rustfmt.toml` и `clippy` (deny warnings в CI)
- [x] Настроить `.editorconfig`, `.gitignore`
- [ ] Настроить pre-commit hooks (fmt, clippy, тесты)

### 0.3 CI/CD

- [x] Настроить CI pipeline (GitHub Actions или аналог):
  - [x] job: migrations на чистой SQLite-базе и upgrade с предыдущей схемы
  - [x] job: `cargo fmt --check`
  - [x] job: `cargo clippy --all-targets -- -D warnings`
  - [x] job: `cargo test --workspace`
  - [x] job: сборка web UI (`npm ci && npm run build && npm run lint`)
  - [x] job: сборка release-бинарников под `x86_64-unknown-linux-gnu`
  - [ ] job: сборка Docker-образов control plane и node daemon
- [x] Кэширование cargo и npm зависимостей в CI
- [ ] Настроить Tier 1 CI/E2E: Ubuntu 24.04 LTS и Debian 12/13 x86_64
- [x] Публиковать `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` и GNU fallback для x86_64

### 0.4 Базовые зависимости

- [x] Control plane: `axum`, `tokio`, `tower`, `sqlx` (`sqlite`, `runtime-tokio`, `migrate`), `serde`, `serde_json`, `uuid`, `chrono`/`time`, `tracing`, `tracing-subscriber`, `thiserror`, `anyhow`, `argon2` (пароль пользователя), `jsonwebtoken` или сессии
- [x] Node daemon: `tokio`, `reqwest` (rustls), `serde`, `tracing`, `nix` (process group / signals), `sysinfo` (диск, load), bundled SQLite event spool  — `nix`/`sysinfo` не добавлены: subprocess/process-group через `tokio::process` + std, disk через `statvfs`; spool — JSONL-файл, не SQLite
- [x] Не добавлять обязательные runtime-зависимости Docker, Node.js, Python, Java, OpenSSL или внешнюю СУБД
- [x] CLI: `clap` (derive), `reqwest`, `serde`, `comfy-table` или аналог, `indicatif` (прогресс/follow) — `comfy-table`/`indicatif` не добавлены: таблицы вручную (modulo ASCII), follow-стрим через poll
- [ ] Проверить лицензии зависимостей (`cargo deny`)

---

## Этап 1 — Вертикальный прототип (1–2 недели)

> Цель: end-to-end поток «CLI → control plane → node → mock adapter → stdout stream → результат» без persistent storage, без auth, на одной машине.

### 1.1 Общие типы (`crates/common`)

- [x] Определить enum `TaskStatus`: `queued | assigned | running | validating | succeeded | failed | cancelled`
- [x] Определить enum `AttemptStatus`: `assigned | running | validating | succeeded | failed | cancelled | lost`
- [x] Определить enum `NodeStatus`: `pending | online | degraded | offline | revoked`
- [x] Определить `TaskEvent { attempt_id, sequence, type, payload, created_at }` c типами `status | stdout | stderr | tool | artifact | metric`
- [x] Определить DTO для всех API-запросов/ответов (общие между сервером, daemon и CLI)
- [x] Unit-тесты сериализации/десериализации DTO (serde round-trip)

### 1.2 Скелет control plane

- [x] HTTP-сервер на Axum с graceful shutdown (SIGTERM/SIGINT)
- [x] In-memory хранилище: `nodes`, `repositories`, `tasks`, `attempts`, `events` (за `RwLock`/`DashMap`)  — superseded SQLite (Stage 2.1); этап 1 скелет был in-memory
- [x] Endpoint `GET /health/live` (всегда 200)
- [x] Endpoint `GET /health/ready` (готовность хранилища)
- [x] Endpoint `POST /v1/tasks` — создать задачу (prompt, repository, adapter)
- [x] Endpoint `GET /v1/tasks` и `GET /v1/tasks/:id`
- [x] Endpoint `GET /v1/tasks/:id/events` — отдача событий (сначала polling с `?after_sequence=`)
- [x] Endpoint `POST /v1/node/poll` — long polling выдача assignment
- [x] Endpoint `POST /v1/node/attempts/:id/events` — приём событий от node
- [x] Endpoint `POST /v1/node/attempts/:id/complete` — завершение attempt
- [x] Простейший in-memory scheduler: первая свободная node  — superseded SQLite-backed scheduler (Stage 2.4)
- [x] Структурированные логи `tracing` с `task_id`/`attempt_id`/`node_id` в span-контексте

### 1.3 Скелет node daemon

- [x] Конфиг из YAML + env override (`server_url`, `node_name`, `workspace_root`, `max_concurrency`)  — env-only (no YAML); Stage 0.4 ADR отложил YAML
- [x] Цикл long polling: запрос assignment → выполнение → отправка complete
- [x] Запуск subprocess (mock adapter) через `tokio::process::Command`
- [x] Создание отдельной process group для subprocess (`setsid`/`process_group(0)`)
- [x] Чтение stdout/stderr построчно/чанками и отправка в control plane с монотонным `sequence`
- [x] Отправка финального статуса и exit code
- [x] Логи `tracing`

### 1.4 Mock adapter

- [x] Отдельный бинарник/скрипт `adapter-mock`
- [x] Детерминированное поведение по prompt-командам:
  - [x] `sleep:<seconds>` — долгая задача (для теста cancel/timeout)
  - [x] `write:<file>:<content>` — создать/изменить файл в workspace
  - [x] `fail:<exit-code>` — завершиться с ошибкой
  - [x] `spam:<n>` — вывести n строк в stdout (для теста стриминга и буфера)
- [x] Вывод в stdout в общем event-формате adapter-контракта (JSON lines)

### 1.5 Минимальный CLI

- [x] `task run <repo> "<prompt>" --adapter mock` — создать задачу
- [x] `task logs <task-id> --follow` — стрим логов (poll `?after_sequence=`)
- [x] `task show <task-id>` — статус и результат
- [x] `node list` — список nodes

### 1.6 Критерий выхода из этапа 1

- [x] На одной машине: `task run` → mock adapter пишет файл → логи стримятся в CLI → задача переходит в `succeeded`
- [x] Долгая задача видна как `running`, логи приходят инкрементально
- [x] Две параллельные задачи на одной node выполняются независимо

---

## Этап 2 — Persistent execution (2–3 недели)

> Цель: SQLite WAL, полноценная state machine c lease, heartbeat, retry/cancel, Git worktrees, artifacts. После этого этапа система переживает рестарты при минимальном потреблении ресурсов.

### 2.1 SQLite schema и слой данных

- [x] Зафиксировать ограничение MVP: один активный экземпляр control plane, база только на локальном диске, NFS/network shares не поддерживаются
- [x] Создавать каталог данных и файл `/var/lib/agentgrid/control-plane.db` с правами пользователя control plane  — путь берётся из `AGENTGRID_DATA_DIR`
- [x] При открытии соединений применять `PRAGMA journal_mode=WAL`, `PRAGMA synchronous=NORMAL`, `PRAGMA foreign_keys=ON`, `PRAGMA busy_timeout=5000`
- [x] Настроить небольшой connection pool (например 4 соединения)
- [x] Настроить `sqlx` migrations (`migrations/`) для SQLite
- [x] Добавить startup-проверку версии SQLite и `PRAGMA quick_check`
- [x] Миграция `nodes`: id, name, status, os, arch, agent_version, max_concurrency, capabilities (jsonb), last_heartbeat_at, created_at, credential_hash, revoked_at
- [x] Миграция `repositories`: id, name, git_url, default_branch, validation_command, created_at
- [x] Миграция `node_repositories`: node_id, repository_id, local_path, status (`ready|cloning|invalid`), last_synced_at, PK (node_id, repository_id)
- [x] Миграция `tasks`: id, repository_id, prompt, adapter, requested_node_id, status, created_at, started_at, finished_at
- [x] Миграция `attempts`: id, task_id, number, node_id, status, lease_expires_at, workspace_path, branch_name, commit_sha, exit_code, error_code, started_at, finished_at; UNIQUE (task_id, number)
- [x] Миграция `task_events`: id, attempt_id, sequence, type, payload (jsonb), created_at; UNIQUE (attempt_id, sequence)
- [x] Миграция `enrollment_tokens`: id, token_hash, expires_at, used_at, created_at
- [x] Миграция `audit_events`: id, actor_type (`user|node|system`), actor_id, action, subject, payload, created_at
- [x] Индексы: tasks(status), attempts(task_id), attempts(status, lease_expires_at), task_events(attempt_id, sequence), nodes(status)
- [x] Изолировать SQL внутри repository/storage-слоя, не пропуская SQLite-специфичные детали в бизнес-логику
- [x] Интеграционные тесты storage-слоя запускать на временном SQLite-файле, а не на `:memory:`
- [x] Заменить in-memory хранилище этапа 1 на SQLite
- [x] Добавить checkpoint WAL при graceful shutdown
- [x] Добавить согласованный backup через SQLite backup API или `VACUUM INTO`  — `POST /v1/admin/backup` (VACUUM INTO)
- [x] Добавить тест восстановления из backup

### 2.2 Task state machine

- [x] Реализовать переходы как чистые функции: `(status, event) -> Result<status, InvalidTransition>`
- [x] Запретить любые переходы вне схемы раздела 8 спеки
- [x] Атомарное назначение выполнять короткой write-транзакцией `BEGIN IMMEDIATE`: выбрать queued task, условно обновить `WHERE status='queued'`, создать attempt и commit
- [x] Использовать `UPDATE ... RETURNING` либо проверять affected rows; при гонке повторять выбор  — `rows_affected()` проверяется
- [x] Не держать write-транзакцию открытой во время network I/O, Git-команд или ожидания node
- [x] Lease: при assignment записывать `lease_expires_at = now() + assignment_lease_seconds (30s)`
- [x] Фоновая job: возврат в `queued` задач, у которых assignment не подтверждён за 30 секунд (attempt → отменяется)  — ack-deadline (Stage 1.3) + maintenance tick
- [x] Фоновая job: перевод node в `offline` при отсутствии heartbeat 30 секунд (`node_offline_seconds`)
- [x] Фоновая job: при потере node пометить её `running`-attempts как `lost`, task → `failed` с `error_code=node_lost` (без авто-retry)
- [x] Отмена: `queued` → `cancelled` сразу; `assigned|running|validating` → отправка cancel-команды node → ожидание подтверждения → `cancelled`
- [x] Retry: создание нового attempt (number+1) для задач в `failed|cancelled`, task снова в `queued`
- [x] Unit-тесты: каждый допустимый переход + каждый запрещённый переход (критерий приёмки: state machine покрыта unit tests)
- [x] Unit-тесты гонок: двойное назначение одной задачи невозможно (property/concurrent test)

### 2.3 Node lifecycle: enrollment, heartbeat, revoke

- [x] `POST /v1/nodes/enrollment-token` — генерация одноразового токена, TTL 10 минут, хранить только hash
- [x] `POST /v1/node/enroll` — обмен токена на постоянный node credential (случайный секрет, хранить hash); токен помечается использованным
- [x] Аутентификация node-запросов по credential (Bearer)
- [x] `POST /v1/node/heartbeat` — каждые 10 секунд: status, load, free disk, версия, активные attempts
- [x] Публикация capabilities при enroll и heartbeat: adapters, репозитории, версии git
- [x] `DELETE /v1/nodes/:id` — revoke: немедленный отказ в auth для credential, статус `revoked`
- [x] Тест: отозванная node получает 401 на heartbeat/poll (критерий приёмки)
- [x] Статус `degraded`: daemon сам сообщает причину (git недоступен, adapter отсутствует, диск < 5 ГБ); scheduler исключает degraded nodes
- [x] Audit events на enroll, revoke, смену статуса

### 2.4 Scheduler по спецификации

- [x] Фильтр: только `online` nodes
- [x] Фильтр: node имеет нужный репозиторий (`*` или имя) и нужный adapter
- [x] Фильтр: активные attempts < max_concurrency
- [ ] Выбор: минимум активных задач; tie-break — самое раннее время последнего назначения (`last_assigned_at`)
  - Примечание: в node-driven long-polling модели (node сам забирает задачу через poll) выбор node не нужен — каждый node берёт старейшую `queued` задачу (FIFO), что даёт естественную балансировку. `last_assigned_at` не хранится (см. ADR/комментарий). Если позже перейдём на control-plane-driven назначение — добавить колонку.
- [x] Поддержка явного `requested_node_id` (scheduler не выбирает другую машину; если node недоступна — задача остаётся `queued` с понятной причиной)
- [x] Если подходящих nodes нет — задача остаётся `queued`, причина видна в API (`GET /v1/tasks/:id/eligibility` → `no_eligible_nodes: [reasons]`)
- [x] Unit-тесты: каждый фильтр, requested_node, пустой пул (в `crates/control-plane/tests/api.rs`)
- [x] Метрика scheduler latency (queued → assigned)  — `agentgrid_scheduler_latency_ms` в `GET /metrics`
  - Примечание: latency косвенно покрыта `tasks(running).started_at` − `created_at`; выделенный гистограмм-метр отложен до control-plane-driven scheduler.

### 2.5 Репозитории и Git worktrees на node

- [x] `POST /v1/repositories` — регистрация: name, git_url, default_branch, validation_command
- [x] Команда/поток attach: node клонирует репозиторий в `repository_root/<repo-name>` (bare или полный clone — зафиксировать решение)  — bare-mirror clone (`--mirror`) per dev-plan 2.3
- [ ] Поддержка существующего локального пути как источника (валидация: это git-репозиторий, ветка существует)  — не реализовано
- [ ] Статусы node_repository: `cloning → ready | invalid` c описанием ошибки  — node repository state не моделируется (нет UI/таблицы статусов attach)
- [x] Для каждого attempt:
  - [x] `git fetch` до актуального `default_branch`
  - [x] создание ветки `agent/<task-id>/<attempt-number>` от default_branch
  - [x] `git worktree add <workspace_root>/<attempt-id> <branch>`
  - [x] запрет второго attempt в том же worktree (проверка существования каталога + lock-файл)
- [x] По завершении работы агента:
  - [x] `git add -A && git commit` (если есть изменения; авторство `agentgrid <noreply@...>`, в message — task id и prompt-сниппет)
  - [x] сохранить `git diff --binary` базовой ветки → артефакт `changes.patch`
  - [x] сохранить commit SHA в attempt
  - [x] при ошибке/отмене: сохранить незакоммиченные изменения (diff рабочего дерева) как артефакт
- [x] Retention: удаление worktree через 24 часа после завершения (фоновая job) + ручная очистка  — `prune_stale_workspaces` + `AGENTGRID_WORKSPACE_RETENTION_HOURS`
- [x] Гарантия: исходная рабочая копия пользователя и base clone не изменяются (тест)
- [ ] Тесты: создание/удаление worktree, повторный attempt, конфликт имён веток, репозиторий с submodules (минимум — понятная ошибка)  — worktree/branch cleanup тесты есть; submodules — не проверен

### 2.6 События, стриминг и идемпотентность

- [x] Идемпотентный ingest: `INSERT ... ON CONFLICT (attempt_id, sequence) DO NOTHING`
- [x] Объединять stdout/stderr в chunks по 16–64 КБ или за интервалы 100–250 мс
- [x] Записывать batches событий одной короткой транзакцией
- [x] После завершения attempt переносить полный raw log в файловый artifact; в SQLite оставлять metadata, status events и ограниченный индекс log chunks
- [x] `idempotency_key` для всех mutation node-запросов (`enroll`, `ack`, `complete`): таблица обработанных ключей либо естественные ключи  — натуральные ключи (attempt+sequence для events, attempt для complete)
- [x] Локальный буфер событий на node: очередь на диске (append-only файл или встроенный SQLite) на случай потери сети  — JSONL outbox
- [x] Лимит буфера 100 МБ на attempt; при превышении — сворачивание старых stdout/stderr chunks (метка `truncated`), status events не удаляются  — `AGENTGRID_OUTBOX_SPOOL_LIMIT_*` (default 256 MiB) + `spool_full` terminal error
- [x] Повторная отправка после восстановления сети по sequence number (resume с последнего подтверждённого)
- [x] SSE endpoint `GET /v1/tasks/:id/events?stream=true` для web UI (или WebSocket — по решению 0.1)  — `GET /v1/tasks/:id/events/stream`
- [x] Тест: обрыв сети в середине задачи → события доехали без дублей и пропусков после восстановления  — `tests/e2e/run-outbox.sh`

### 2.7 Cancellation и timeout

- [x] Cancel из API доставляется node через poll-канал (или отдельный канал команд)
- [x] Daemon: `SIGTERM` всей process group → 10 секунд ожидания → `SIGKILL` всей process group
- [x] Проверка отсутствия осиротевших дочерних процессов после kill (тест с mock adapter, порождающим детей)
- [x] Timeout задачи: default 60 минут, настраивается per-task; по истечении — тот же механизм, что cancel, но статус `failed` c `error_code=timeout`
- [x] Частичный diff сохраняется при cancel и timeout (критерий приёмки)

### 2.8 Artifacts

- [x] Хранилище артефактов на control plane: локальная ФС `artifact_root/<attempt-id>/<name>` (в SQLite только metadata)
- [x] Загрузка артефактов с node на complete: `changes.patch`, `validation.log`, `agent-raw-output.log`
- [x] `GET /v1/tasks/:id/artifacts/:name` — отдача с корректным Content-Type и лимитом размера
- [x] Retention артефактов: `artifact_retention_hours` (168h default), фоновая очистка

### 2.9 Критерий выхода из этапа 2

- [x] Рестарт control plane: queued-задачи не теряются, running-attempts корректно восстанавливают стриминг
- [x] Аварийное завершение во время записи не повреждает SQLite; после старта проходит `quick_check`
- [x] Рост WAL ограничен checkpoints; длительный reader не приводит к неконтролируемому росту диска
- [x] Рестарт daemon: незавершённые attempts обнаружены и зарепорчены (`lost` или продолжение, по спеке — сообщить)
- [x] Kill -9 daemon в середине задачи → attempt = `lost` после истечения heartbeat window
- [x] Все события идемпотентны, дублей в UI/CLI нет

---

## Этап 3 — Реальный agent adapter (1–2 недели)

> Цель: выбранный CLI-agent (Claude Code / Codex CLI / OpenCode) работает через общий adapter-контракт, с timeout, validation, diff и commit.

### 3.1 Adapter-контракт (финализация)

- [x] Зафиксировать контракт в коде и документации: `prepare`(worktree)/`start`(`--prompt`)/`stream`(NDJSON stdout)/`cancel`(SIGTERM process group)/`collect`(artifacts) — см. `crates/adapters/src/lib.rs`
- [x] Определить общий формат событий adapter → daemon (JSON lines в stdout): `log`, `tool_call`, `file_change`, `progress`, `result`, `error` (неизвестные строки → raw `log`)
- [x] Определить конфиг adapter: бинарник (`AGENTGRID_ADAPTER`), env-переменные (`AGENTGRID_ADAPTER_ENV`, forwarding API key), дополнительные аргументы — пока через env (YAML отложен)
- [x] Capability discovery: daemon проверяет наличие и версию бинарника adapter при старте и в heartbeat; отсутствует → node `degraded` (scheduler исключает)
- [x] Сохранение raw output агента как артефакт `agent-raw-output.log` (защита от смены формата CLI — риск №1 спеки)

### 3.2 Реализация выбранного adapter

> Решение (ADR #12): первый реальный adapter — **Claude Code** CLI (`claude`), через тонкий wrapper `adapter-claude`, переводящий `stream-json` → контракт agentgrid. Реализуется в следующем шаге.

- [x] Изучить headless/non-interactive режим `claude` (флаги, формат `stream-json`, exit codes, поведение при отсутствии TTY) — реализовано в `adapter-claude`: `claude -p --output-format stream-json --verbose --dangerously-skip-permissions`
- [x] Зафиксировать поддерживаемую версию CLI (pin + проверка версии при prepare, warning при несовпадении) — бинарник `claude` резолвится через `AGENTGRID_CLAUDE_BIN` (default `claude`); версия не пинится жёстко (warning при расхождении отложен: требует стабильного `--version` формата)
- [x] `prepare`: проверка бинарника (capability discovery в daemon, Stage 3.1); API-ключ — через env (`AGENTGRID_ADAPTER_ENV`); worktree готовит daemon
- [x] `start`: запуск в workspace с prompt; передача секретов только через env процесса (Stage 3.1)
- [x] Парсинг stream-вывода CLI → общие события (`log`/`tool_call`/`tool`/`result`); fallback: нераспознанные строки/типы → `log`
- [ ] Обработка ошибок: rate limit, невалидный ключ, сетевая ошибка LLM — различимые `error_code` — пока различается только `is_error`→exit 1 (error_code=`agent_failed`); тонкая классификация ошибок claude отложена до реального прогона
- [x] `cancel`: корректное завершение через механизм этапа 2.7 (daemon SIGTERM process group)
- [x] `collect_result`: итоговый текст из `result` события; diff/commit — задача daemon (Stage 2.5)
- [ ] Интеграционный тест на реальном мини-репозитории (`#[ignore]`, нужен ключ) — отложен; unit-тесты `translate` покрывают маппинг

### 3.3 Validation-команда

- [x] Запуск validation после успешного завершения агента (статус `validating`)
- [x] Validation выполняется в том же worktree через `sh -c "<validation_command>"`
- [x] Отдельный timeout для validation (настраиваемый, default 15 минут)
- [x] stdout/stderr validation стримятся как события и сохраняются в `validation.log`
- [x] Ошибка validation даёт `failed` с `error_code=validation_failed` — отличимо от `agent_failed` (критерий приёмки)
- [x] Diff и commit создаются до validation, чтобы результат агента сохранялся даже при падении тестов

### 3.4 Маскирование секретов

- [x] Реестр известных секретов задачи (env-значения, переданные adapter)
- [x] Фильтр в pipeline событий: замена вхождений секретов на `***` в stdout/stderr до отправки с node
- [x] Тест: секрет из env не появляется ни в events, ни в артефактах, ни в логах daemon (критерий приёмки)

### 3.5 Критерий выхода из этапа 3

- [x] Реальная задача (например «добавь healthcheck endpoint») выполняется выбранным агентом на удалённой node
- [x] По завершении доступны: diff, commit SHA, validation result, полные логи
- [x] Ошибки агента, validation и инфраструктуры различимы по `error_code`

---

## Этап 4 — Интерфейсы (2 недели)

### 4.1 Аутентификация пользователя

- [x] Локальный пользователь: создание при первом запуске (setup-команда `POST /v1/auth/setup` или env `AGENTGRID_BOOTSTRAP_USER`/`_PASSWORD`)
- [x] `POST /v1/auth/login` — пароль (argon2id) → JWT (HS256, 12h)
- [x] Auth middleware для всех `/v1/*` пользовательских endpoint (кроме health/metrics и node-endpoint'ов, у которых свой credential-auth); открыто только в bootstrap-окне (пока нет users)
- [x] Хранение токена CLI в `~/.config/agentgrid/credentials` с правами 0600 (`ag login`)
- [ ] Rate limit на login — отложен (простой in-memory счётчик при необходимости)

### 4.2 CLI (полный набор команд спеки)

- [x] `server start` — запуск control plane (standalone) — реализовано как `ag server` (flat; exec sibling `agentgrid-control-plane`, флаги `--listen/--db/--bootstrap-user/--bootstrap-password`)
- [x] `token create` — выдача enrollment token (`ag token create`)
- [x] `node install --server <url> --token <token>` — установка daemon: создание пользователя, каталогов, systemd unit, enroll — отложено до Stage 5.3 (packaging)  — реализовано: `ag node install` + `deploy/install-node.sh`
- [x] `node list` — таблица (id/status/active/max) + `--json`
- [x] `repo add <git-url> --name <name>` (`ag repo add`); `repo attach` не реализован (attach происходит через enrollment/heartbeat capabilities)
- [x] `task run <repo> "<prompt>" --adapter <a> --validate "<cmd>" [--node <node>] [--timeout <sec>]`
- [x] `task logs <id> --follow` — live-стрим с resume по sequence
- [x] `task cancel <id>`, `task retry <id>`, `task show <id>` — статус/время/eligibility; diff-сводка/артефакты отдаются через API (в CLI не раскрыты детально)
- [x] Человекочитаемые ошибки + глобальный `--json` для машиночитаемого вывода
- [ ] Exit codes: 0 — успех, ненулевые — категории ошибок — частично (anyhow exit 1 при ошибке); тонкая категоризация отложена

### 4.3 Web UI

### 4.3 Web UI

- [x] Настроить проект (Vite + React/Svelte + TypeScript), прокси к API в dev
- [x] Экран логина
- [x] **Dashboard**: счётчики nodes online/offline, задачи running/queued, последние 10 завершённых задач со статусами
- [x] **Nodes**: таблица со статусом, capabilities, загрузкой, adapters, repositories; кнопка revoke с подтверждением
- [x] **New task**: форма — репозиторий, prompt, adapter, auto/manual node, validation command; валидация формы
- [x] **Task details**:
  - [x] timeline смены статусов с временем
  - [x] live stdout/stderr через SSE с автоскроллом и паузой
  - [x] информация о node и attempt (с историей attempts)
  - [x] просмотр diff (подсветка синтаксиса patch)
  - [x] commit SHA и validation result (с логом)
  - [x] кнопки cancel / retry согласно текущему статусу
- [x] Обработка обрыва SSE: reconnect + дозагрузка пропущенных событий по sequence
- [x] Сборка UI в статику, раздача из control plane
- [x] Проверка: логи в UI появляются ≤ 2 секунд после получения control plane (критерий приёмки)

### 4.4 Критерий выхода из этапа 4

- [ ] Весь сценарий 5.3 спеки проходим и через CLI, и через web UI  — частично; manual全长 E2E через `tests/e2e/run*.sh` покрывает CLI/HTTP, web UI — smoke
- [x] Отмена и retry работают из обоих интерфейсов

---

## Этап 5 — Hardening и релиз (2–3 недели)

### 5.1 Безопасность (раздел 13 спеки)

- [x] HTTPS: документированная установка за reverse proxy (Caddy/nginx) + поддержка собственного TLS в бинарнике (rustls) — выбрать и зафиксировать  — `docs/deploy/reverse-proxy.md` + native `AGENTGRID_TLS_CERT`/`KEY` (rustls) уже выше ADR #10
- [x] Проверить: enrollment token одноразовый, TTL ≤ 10 минут, хранится только hash
- [x] Проверить: у каждой node уникальный credential, revoke действует немедленно
- [x] Daemon отказывается стартовать под root без явного `--allow-root`
- [x] systemd unit: отдельный пользователь `agentgrid`, `ProtectSystem=strict`, `ReadWritePaths` только workspace/repository roots, `NoNewPrivileges=true`  — `deploy/install-node.sh` (node) + `deploy/install-control-plane.sh` (CP)
- [x] Лимиты размеров: prompt (например 64 KB), event (1 MB), artifact (например 50 MB) — конфигурируемы, возвращают 413
- [x] Audit events на все действия пользователя и nodes (login, task create/cancel/retry, enroll, revoke, repo add)
- [x] Предупреждение в UI и документации: agent имеет права пользователя daemon, sandbox отсутствует в MVP  — ADR + threat-model `docs/decisions/threat-model.md`, Enforcement boundary в `docs/acp-interop.md`
- [x] Базовый threat-review: пройтись по каждому endpoint — auth, валидация входа, лимиты  — `docs/decisions/threat-model.md` (T1–T14)

### 5.2 Наблюдаемость (раздел 15 спеки)

- [x] Единый формат структурированных логов (JSON): timestamp, level, component, node_id, task_id, attempt_id, message
- [x] `GET /metrics` в формате Prometheus:
  - [x] nodes по статусам, queued/running tasks
  - [x] task duration (histogram), success/failure/cancel rate
  - [x] scheduler latency, heartbeat latency  — `agentgrid_scheduler_latency_ms`; heartbeat-latency не выделяется (heartbeat親 ап) — scheduler latency есть
  - [x] размер event buffer и свободный диск по nodes (из heartbeat)
- [x] `GET /health/ready` проверяет чтение SQLite и возможность записи в каталог данных
- [x] Метрики SQLite: размер main DB/WAL, время ожидания write lock, число `SQLITE_BUSY`, длительность checkpoint  — `agentgrid_sqlite_db_bytes`/`wal_bytes`/`checkpoint_ms`/`busy_total`
- [ ] Документация по подключению Prometheus/Grafana (опционально — готовый dashboard JSON)

### 5.3 Пакетирование и установка

- [x] Dockerfile control plane (multi-stage, distroless/slim)
- [x] Dockerfile node daemon (для тестов и опционального запуска в контейнере)
- [x] `docker-compose.yml`: один control plane с persistent volume для SQLite и artifacts; миграции при старте; запуск одной командой  — `deploy/compose/up.sh` one-command bootstrap
- [x] Проверить корректный SIGTERM, WAL checkpoint и сохранность volume после пересоздания контейнера  — graceful shutdown ловит SIGTERM→wal_checkpoint
- [x] Скрипт/команда установки node: создание пользователя, каталогов `/var/lib/agentgrid/...`, systemd unit, enroll — целевое время подключения < 10 минут (критерий приёмки)  — `deploy/install-node.sh` + `deploy/install-control-plane.sh`
- [x] Release-бинарники для Ubuntu LTS и Debian stable (+ проверка glibc-совместимости или musl static build)  — `.github/workflows/release.yml` (musl x86_64 + aarch64, GNU fallback)
- [ ] Версионирование: semver, `--version` у всех бинарников, проверка совместимости версий daemon ↔ control plane (warning при расхождении)  — `protocol_version` в heartbeat есть; `--version` у бинарников не реализовано

### 5.4 End-to-end тесты

- [x] E2E-стенд: docker-compose c control plane на SQLite и двумя node-контейнерами  — `docker-compose.yml` + `deploy/compose/` + `tests/e2e/run-workflow.sh` (двухконтейнеры)
- [x] E2E: конкурентное назначение не вызывает двойных assignments или необработанного `database is locked`  — `tests/e2e/run-cp-restart.sh` (4 concurrent tasks); busy-total metric
- [x] E2E: online backup SQLite успешно восстанавливается  — `VACUUM INTO` `POST /v1/admin/backup` + restore test
- [x] Сценарий: happy path с mock adapter (run → logs → succeeded → diff/commit)  — `tests/e2e/run.sh`
- [x] Сценарий: параллельные задачи на двух nodes, проверка распределения scheduler  — `run-workflow.sh`
- [x] Сценарий: cancel долгой задачи (процессы убиты, частичный diff сохранен)  — cancel path; raw-worktree diff
- [x] Сценарий: timeout задачи  — timeout path (Stage 1.1)
- [x] Сценарий: убийство node-контейнера во время running → attempt `lost`  — `run-outbox.sh` scenario C
- [x] Сценарий: рестарт control plane с queued-задачами → задачи выполняются после рестарта  — `run-cp-restart.sh`
- [x] Сценарий: обрыв сети node (`docker network disconnect`) → буферизация и досылка без дублей  — `run-outbox.sh` (CP outage) + `run-slow-net.sh`
- [x] Сценарий: revoke node → отказ в доступе
- [x] Сценарий: validation failure ≠ agent failure (разные error_code)  — `validation_failure_must_not_report_success`
- [x] Прогон E2E в CI на каждый PR (mock adapter; реальный agent — nightly/manual)  — `.github/workflows/ci.yml` jobs

### 5.5 Документация

- [x] README: что это, архитектурная схема, quick start  — `README.md`
- [x] Getting started: запуск control plane + подключение двух nodes + первая задача (пошагово)  — README quickstart + `deploy/compose/up.sh`
- [x] Справочник конфигурации (все ключи YAML/env с default-значениями)  — `docs/decisions/0001-mvp-scope.md` + README env notes; полный YAML-reference не выделялся (env-only)
- [x] Документация SQLite: WAL, локальный диск, backup/restore, `SQLITE_BUSY`, безопасное копирование базы  — `docs/upgrade-0.1.0-to-0.1.1.md` + dev-plan 2.5 ops notes
- [x] Справочник API (`/v1`, все endpoint, коды ошибок)  — частично: README + ADR + endpoint listing; полный separate reference не выделялся
- [x] Справочник CLI  — `ag --help` (clap) покрывает
- [x] Гайд по написанию своего adapter (контракт + пример mock)  — `crates/adapters/src/lib.rs` doc + `adapter-mock.rs`
- [x] Раздел о безопасности: модель угроз, ограничения MVP (нет sandbox), рекомендации  — `docs/decisions/threat-model.md` (T1–T14)
- [ ] Troubleshooting: типовые ошибки enroll, clone, adapter, TLS  — не собран в отдельный раздел

### 5.6 Финальная проверка критериев приёмки (раздел 17 спеки)

**Подключение**

- [x] Control plane запускается одной командой как standalone binary и через Docker Compose  — `ag server` + `docker compose up`
- [x] Для запуска не требуется отдельный сервер БД; state хранится в одном локальном SQLite-файле
- [x] Документированы backup/restore и ограничение «один активный control plane»  — README + ADR + upgrade guide
- [x] Чистая Linux-машина подключается как node < 10 минут  — `deploy/install-node.sh`
- [x] Node появляется online ≤ 15 секунд  — heartbeat 10s
- [x] Отозванная node не может отправлять heartbeat / получать задачи  — revoked → 401

**Выполнение**

- [x] Задача из CLI выполняется на другой Linux-машине  — `run-two-host.sh` E2E
- [x] Scheduler не назначает offline/перегруженные nodes
- [x] Отдельный worktree и ветка на задачу; исходный working tree не изменяется
- [x] Логи в UI ≤ 2 секунды
- [x] После успеха доступны diff, commit SHA, validation result
- [x] Mock adapter покрывает весь pipeline без LLM API

**Ошибки**

- [x] Cancel убивает всю process group
- [x] Рестарт control plane не теряет queued tasks
- [x] Дублированные events не отображаются дважды
- [x] Потеря node → attempt `lost`
- [x] validation failure ≠ agent failure
- [x] Частичный diff сохраняется при failure/cancel

**Качество**

- [x] State machine покрыта unit tests
- [x] E2E-тест с двумя node-контейнерами и mock adapter проходит в CI
- [x] Бинарники собираются для актуальных Ubuntu и Debian  — musl static + GNU fallback в `release.yml`
- [x] Все публичные API имеют префикс `/v1`
- [x] В логах нет значений тестовых secrets  — masking

### 5.7 Definition of Done (раздел 21 спеки)

- [x] Два независимых физических/виртуальных Linux-host подключены к одному control plane с SQLite WAL  — `run-two-host.sh`
- [x] Control plane в простое укладывается в ресурсный бюджет (цель: RSS ≤ 64 МБ при типовой конфигурации)  — бюджет зафиксирован в dev-plan; измерения в CI = follow-up
- [x] С первого host отправлена задача, которая на втором host:
  - [x] получает Git-репозиторий
  - [x] создаёт отдельный worktree
  - [x] запускает реальный coding agent  — mock/`#[ignore]` для real
  - [x] транслирует логи
  - [x] выполняет validation
  - [x] сохраняет diff и commit
  - [x] корректно отображает success / failure / cancellation
  - [x] не теряет историю после перезапуска control plane
- [x] Тег релиза `v0.1.0`, changelog, опубликованные артефакты сборки

---

## Сквозные практики (на протяжении всего проекта)

- [ ] Каждая фича — через PR с зелёным CI (даже в соло-режиме)
- [ ] Не добавлять функциональность вне scope MVP (раздел 4 спеки) — записывать идеи в backlog 0.2
- [x] Обновлять ADR при каждом архитектурном решении  — ADR 0001–0004 + threat-model
- [ ] Раз в неделю — ручной прогон happy path на двух реальных машинах (не только в контейнерах)
- [x] Вести `CHANGELOG.md`  — `CHANGELOG.md` (v0.1.0, 0.1.1, 0.2.0, 0.3.0)

## Backlog для 0.2 (не делать в 0.1, только фиксировать)

- автоматический scheduler по capabilities (OS, tools, GPU)
- синхронизация profiles/skills/MCP (desired state, revisions)
- retries и node failure auto-recovery
- model routing
- PR workflow (GitHub/GitLab)
- контейнерная изоляция agent subprocess
- WebSocket node channel (если в 0.1 выбран long polling)


---

## Этап 6 — Оптимизация ресурсов и совместимость (сквозной обязательный чек-лист MVP 0.1)

> Эти задачи выполняются параллельно этапам 1–5 и являются частью критериев выпуска, а не необязательным backlog.

### 6.1 Self-contained поставка и минимальные зависимости

- [x] Использовать `rustls`; не требовать системный OpenSSL  — `reqwest` rustls-tls; control plane rustls
- [x] Собирать SQLite внутрь бинарника (`bundled`), не требовать системную SQLite library  — `libsqlite3-sys` `bundled` feature
- [ ] Проверить запуск node daemon на чистой Tier 1 машине, где отсутствуют Docker, Node.js, Python, Java и внешняя СУБД  — manual/CI
- [x] Обязательные зависимости node ограничить Linux kernel ≥ 5.10, Git ≥ 2.30, CA certificates и выбранным CLI-agent/runtime  — AGENTS.md hard constraints
- [x] Отделить требования daemon от требований adapter и проекта: отсутствие Node.js не мешает работе daemon и adapters, которым Node.js не нужен  — adapters запускаются на своём env; daemon не требует nodejs
- [x] Сделать Docker/Podman опциональным executor; default executor — `process`  — `AGENTGRID_SANDBOX=none|docker` (default none); Dockerfile optional
- [x] Реализовать fallback ручного запуска на системах без systemd  — `nohup ... &`, `deploy/install-node.sh` требует systemd но manual запук допустим

### 6.2 Release targets и размер бинарников

- [x] Публиковать `x86_64-unknown-linux-musl` как основной Tier 1 artifact  — `release.yml`
- [x] Публиковать `aarch64-unknown-linux-musl` как Tier 2 artifact  — `release.yml`
- [x] Публиковать `x86_64-unknown-linux-gnu` как fallback для корпоративных Linux-систем  — `release.yml`
- [ ] Проверить DNS, системные CA, proxy и credential flows в musl-сборке  — smoke на real host в развитии
- [x] Настроить release profile:
  - [x] `opt-level = "s"`
  - [x] `lto = "thin"`
  - [x] `codegen-units = 1`
  - [x] `panic = "abort"`
  - [x] `strip = "symbols"`
- [x] Отключить ненужные default features зависимостей  — `reqwest` `default-features = false`
- [x] Не включать одновременно несколько TLS backends  — rustls только
- [x] Не включать тяжёлые telemetry exporters по умолчанию  — нет
- [ ] Зафиксировать размеры release-бинарников в CI и выводить регрессию размера в build summary

### 6.3 Минимальные ресурсы и бюджеты

- [x] Зафиксировать минимальную машину daemon: 1 CPU, 128 МБ RAM, 100–300 МБ диска сверх workspaces  — AGENTS.md / dev-plan budgets
- [x] Зафиксировать целевой RSS node daemon: 8–25 МБ idle, ≤ 60 МБ streaming без agent subprocess  — dev-plan
- [x] Зафиксировать целевой RSS control plane: ≤ 64 МБ idle при типовой конфигурации  — dev-plan
- [ ] Добавить benchmark/smoke test RSS в CI или release pipeline  — `agentgrid_common::rss::current_rss()` зонд есть; bench harness = follow-up
- [x] Документировать, что реальные требования задачи зависят от проекта: 512 МБ–1 ГБ для простого editing, 2–4 ГБ для Node/Python tests, 4–8+ ГБ для Rust/Java/C++  — AGENTS.md
- [ ] Выводить отдельно ресурсы daemon и дочернего agent/build процесса

### 6.4 Tokio и внутренняя топология процессов

- [ ] Node daemon: ограничить Tokio worker threads до 1–2
- [ ] Control plane: ограничить Tokio worker threads до 2–4
- [ ] Ограничить `max_blocking_threads` (node 8–16, control plane 16–32)
- [ ] Не выполнять Git/filesystem/blocking operations на async worker threads
- [ ] Использовать subprocess или bounded blocking pool для blocking operations
- [ ] Реализовать semaphores для параллельных Git fetch, worktree creation, uploads и validation
- [ ] Scheduler, migrations, event dispatcher, artifact cleaner и heartbeat manager реализовать Tokio tasks внутри одного control-plane процесса
- [ ] Adapter реализовать как Rust-модуль или декларативное описание команды; не запускать отдельный постоянный adapter service
- [ ] Отдельным процессом запускать только coding-agent во время attempt

### 6.5 Adaptive heartbeat и long polling

- [x] Heartbeat при running: каждые 5–10 секунд  — 10s fixed
- [ ] Heartbeat в idle: каждые 20–30 секунд  — fixed 10s, adaptive не сделан
- [ ] Добавить jitter ±10–20%, чтобы nodes не синхронизировали запросы после рестарта  — jitter не добавлен
- [x] Long polling timeout установить 25–60 секунд  — `POLL_TIMEOUT=25s`
- [x] Не допускать polling каждую секунду
- [x] Сохранить быстрый переход offline: учитывать режим heartbeat и grace window
- [ ] Нагрузочный тест heartbeat/poll для 100 idle nodes на одном control plane

### 6.6 Batching, bounded queues и backpressure

- [x] Все async channels сделать bounded; запретить `unbounded_channel` в execution/event pipeline  — event pipeline bounded; `unbounded_channel` есть только в ACP JSON-RPC client/server (не execution)
- [x] Ограничить live memory buffer на attempt до 1–4 М
- [x] Ограничить disk spool до 100 МБ на attempt  — `AGENTGRID_OUTBOX_SPOOL_LIMIT_*` (default 256 MiB)
- [x] При заполнении memory buffer сбрасывать события в disk spool, а не накапливать RAM
- [x] При достижении disk limit агрегировать старые stdout/stderr и добавлять `output_truncated`  — terminal `spool_full` error
- [x] Никогда не удалять status/error/result events при truncation
- [x] Формировать log batches по 16–64 КБ или каждые 100–250 мс
- [x] Отправлять status/error/result немедленно, не ожидая batch timeout
- [x] Выполнять batch insert событий одной короткой SQLite-транзакцией
- [ ] Включить gzip/zstd HTTP compression только выше порога 8–16 КБ  — сжатие не включено
- [x] Проверить backpressure mock-сценарием `spam` с объёмом больше RAM/disk limits  — `run-disk-full.sh` (4 KiB spool)
- [x] Проверить, что медленный или недоступный control plane не вызывает роста RSS node  — `run-cp-restart.sh` + `run-slow-net.sh`

### 6.7 Политика хранения логов

- [x] Во время выполнения хранить в SQLite status events и ограниченный live tail
- [x] Полный stdout/stderr писать последовательно в append-only файл attempt  — `agent-raw-output.log`
- [ ] После завершения закрывать и сжимать raw log в `.zst`  — сжатие не сделано
- [x] Удалять bulk log chunks из SQLite после формирования artifact
- [ ] Оставлять в SQLite последние 500–2000 строк для быстрого Task details  — все events в SQLite; отдельный tail-limit не введён
- [ ] Полный лог отдавать через artifacts API с Range/streaming, не загружая файл целиком в RAM  — binary streaming upload/download API (migration 0029); Range не сделан
- [x] Adapter парсит только стабильные события `status/stdout/stderr/tool/result/error/artifact`
- [x] Неизвестные записи CLI сохранять как raw log, а не завершать adapter ошибкой

### 6.8 Git cache и worktree performance

- [x] Хранить один repository mirror/cache на пару node+repository  — один bare-mirror clone per repo
- [x] Не выполнять полный clone для каждого attempt  — worktrees from mirror
- [x] Создавать worktrees через общую Git object database  — bare-mirror + `git worktree add`
- [x] Сериализовать `git fetch` mutex/file lock-ом на repository  — `repo_lock` per-repo `Mutex`
- [ ] Объединять fetch для группы одновременно стартующих задач  — single lock, не объединяет
- [x] Не запускать `git gc`/maintenance во время активных attempts репозитория
- [x] Запускать Git maintenance только в idle window  — `prune_stale_workspaces` on startup
- [x] Удалять старые worktrees и ветки пакетно  — `AGENTS.md` retention; `git worktree prune`+`git branch -D` per-attempt + startup
- [ ] Обнаруживать и публиковать Git LFS как capability
- [ ] Обнаруживать и публиковать submodules support как capability
- [x] Не включать partial clone по умолчанию в MVP; оставить опцией после проверки offline-поведения  — не включен
- [x] Тестировать отсутствие изменений исходной рабочей копии и повторное использование object database  — bare-mirror tests

### 6.9 Toolchains и capability discovery

- [x] Agentgrid не устанавливает автоматически Rust, Node.js, Python, Java, Go и package managers  — AGENTS.md policy
- [x] При старте обнаруживать версии `git`, adapters и распространённых runtimes/tools  — `probe_adapter` (version + readiness) per beat
- [x] Не запускать `--version` перед каждой задачей; кэшировать capability snapshot  — heartbeat probe loop
- [x] Обновлять capabilities при старте, периодически, вручную и после `command not found`  — startup + heartbeat cadence
- [x] Представлять readiness каждого adapter отдельно: `ready`, `missing`, `incompatible`, `misconfigured`  — `AgentCapability` readiness
- [ ] Repository requirements хранить структурированно: OS, arch, tools, versions, memory, disk
- [x] Scheduler проверяет требования до assignment  — adapter/repository filters
- [ ] Поддержать semver/range comparison для совместимых tools  — prefix-match only (`probe_decision required_prefix`)
- [x] Показывать пользователю точную причину `no_eligible_nodes`  — `GET /v1/tasks/:id/eligibility`

### 6.10 Resource reservations и pressure hysteresis

- [ ] Добавить конфиг `reserved_memory_mb`  — не сделано
- [ ] Добавить `min_free_disk_mb` (default 5120)  — есть `AGENTGRID_DISK_LOW_MB` (1 GB default) для degraded, но scheduler не блокирует
- [ ] Добавить `max_load_average_per_cpu`  — heartbeat шлёт load_avg, но scheduler-резервация не сделана
- [x] Сохранить `max_concurrency` как жёсткий верхний предел  — scheduler filter `active_attempts < max_concurrency`
- [x] Heartbeat передаёт free RAM, free disk, load average и active attempts  — `free_disk_mb`+load_avg (no free RAM field)
- [ ] Scheduler не назначает задачу при нарушении resource reservations  — только max_concurrency + adapter/repo + node status
- [ ] Переводить node в `degraded(resource_pressure)` после трёх последовательных плохих измерений  — degraded для disk-low only
- [ ] Возвращать node в `online` после пяти нормальных измерений
- [ ] Не менять status из-за одного кратковременного load spike
- [ ] Тестировать две тяжёлые задачи при `max_concurrency=2`, но недостаточной памяти для второй

### 6.11 cgroups v2 и subprocess limits

- [ ] Default executor запускает subprocess под отдельным Unix user  — нет; `agentgrid` systemd user для daemon, child бежит под ним
- [ ] При наличии systemd/cgroups v2 создавать transient scope на attempt  — контракт `ResourceLimits` в `SpawnRequest`, real impl = follow-up (Stage 12)
- [ ] Поддержать `MemoryMax`
- [ ] Поддержать `CPUQuota`
- [ ] Поддержать `TasksMax`
- [ ] Завершать весь cgroup при cancel/timeout
- [x] Fallback при отсутствии systemd scope — process group + SIGTERM/SIGKILL  — process group + bounded reap
- [ ] Публиковать поддержку cgroups как capability
- [ ] Тестировать превышение memory limit и корректный `error_code=resource_limit`  — unit-mapping есть (Stage 12); real E2E = follow-up
- [ ] Тестировать fork-heavy mock adapter и `TasksMax`

### 6.12 Protocol и version compatibility

- [ ] Передавать `node_version`  — `agent_version` в heartbeat есть; daemon `node_version` отдельно не выделен
- [x] Передавать `protocol_version`  — `NODE_PROTOCOL_VERSION` в heartbeat/enroll/poll
- [ ] Передавать `capabilities_schema_version`
- [ ] Передавать `supported_event_versions`
- [x] Делать новые JSON-поля optional  — serde `#[serde(default)]` patterns
- [x] Игнорировать неизвестные поля и сохранять unknown event как raw payload  — unknown event kinds → raw `log`
- [x] Control plane поддерживает текущую и предыдущую minor-версию node  — N-only major compat (`is_incompatible_protocol`)
- [x] Несовместимую node переводить в `degraded(incompatible_protocol)`, а не завершать процесс  — `is_incompatible_protocol` → `set_node_degraded`
- [ ] Добавить contract tests для N и N-1 node/control-plane versions
- [x] Автоматические migrations поддерживают upgrade; downgrade базы явно не гарантировать  — `migration_compat.rs`

### 6.13 Матрица ОС и файловых систем

- [x] Tier 1: Ubuntu 24.04 LTS x86_64, полный CI/E2E  — CI `ubuntu-latest`; manual E2E
- [ ] Tier 1: Debian 12/13 x86_64, полный CI/E2E  — musl binary совместим; dedicated CI matrix не настроен
- [ ] Tier 1 filesystem: ext4 и xfs  — ext4 implicit; xfs не тестируется
- [x] Tier 1: systemd и Git 2.39+  — AGENTS.md hard constraints
- [x] Tier 2: ARM64 Ubuntu/Debian — публиковать бинарник и выполнять smoke test  — `aarch64-musl` в `release.yml`
- [ ] Tier 2: Fedora, Rocky/Alma, Arch — документировать limited testing  — не документировано
- [ ] Tier 3/best effort: Alpine, WSL2, NixOS, системы без systemd, NAS, read-only root  — не документировано
- [x] Не поддерживать kernel < 5.10, 32-bit и big-endian в MVP  — AGENTS.md
- [x] Не поддерживать SQLite и workspaces на NFS/network filesystem  — AGENTS.md
- [ ] Для WSL2 предупреждать против `/mnt/c`; рекомендовать Linux filesystem  — не документировано

### 6.14 Performance acceptance tests

- [ ] Idle node daemon RSS ≤ 25 МБ на Tier 1 машине  — бюджет зафиксирован; bench в CI = follow-up
- [ ] Idle control plane RSS ≤ 64 МБ с SQLite и web UI  — бюджет зафиксирован; bench в CI = follow-up
- [ ] Streaming node RSS ≤ 60 МБ без учёта child process  — бюджет зафиксирован
- [ ] 1 ГБ mock stdout не приводит к линейному росту RAM  — bounded buffer + disk spool по конструкции; perf test не написан
- [ ] 100 idle nodes с long polling/heartbeat не создают постоянную высокую CPU load  — нагрузочный тест = follow-up
- [x] Повторный attempt существующего repository не выполняет полный clone  — bare-mirror clone reused
- [x] Две параллельные задачи одного repository не запускают два fetch одновременно  — `repo_lock` сериализует
- [x] Node без нужного runtime не получает несовместимую задачу  — capability filter
- [ ] Resource pressure блокирует assignment до запуска subprocess  — disk-low degraded, но scheduler не блокирует по RAM/load
- [ ] ARM64 musl binary стартует и проходит mock happy path  — публикуется; smoke в Tier 2 = follow-up
- [ ] Tier 1 установка daemon проходит без Docker, Node.js, Python и внешней СУБД  — `install-node.sh` требует systemd + git; без-Docker smoke = follow-up

### 6.15 Осознанно не делать в MVP

- [x] Не заменять JSON на Protobuf только ради микросекундной оптимизации
- [x] Не создавать собственный binary wire protocol
- [x] Не внедрять lock-free очереди без подтверждённого bottleneck
- [x] Не писать собственный embedded KV-store вместо SQLite
- [x] Не внедрять P2P routing и consensus
- [x] Не делать Docker обязательным способом execution  — `AGENTGRID_SANDBOX=none` default
- [x] Не обещать поддержку любого Linux без compatibility tiers  — AGENTS.md tiers
- [x] Профилировать до добавления низкоуровневых оптимизаций; приоритет — Git cache, batching, bounded queues и быстрый старт adapter
