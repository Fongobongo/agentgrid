# План реализации MVP 0.1 — распределённый оркестратор coding agents

> Детальный план работ по спецификации MVP 0.1 (Linux: Tier 1 x86_64, Tier 2 ARM64; control plane + SQLite WAL + node daemon + CLI + web UI).
> Рабочее название: `agentgrid`. Горизонт: 8–12 недель для одного разработчика.
>
> Легенда: каждый пункт — атомарная задача с проверяемым результатом.
> Пункты внутри этапа расположены в рекомендуемом порядке выполнения.

---

## Этап 0 — Подготовка проекта (2–4 дня)

### 0.1 Решения, блокирующие старт (из раздела 20 спеки)

- [ ] Зафиксировать рабочее название проекта (влияет на имена бинарников, каталогов, env-переменных)
- [ ] Выбрать первый реальный agent adapter (Claude Code / Codex CLI / OpenCode) и зафиксировать его версию
- [ ] Выбрать лицензию (MIT / Apache-2.0 / AGPL) и добавить файл `LICENSE`
- [ ] Решить: в первом релизе web UI + CLI или только CLI (спека допускает оба, дефолт — оба)
- [ ] Решить: git clone только по HTTPS/token или также SSH (рекомендация MVP: HTTPS/token)
- [ ] Решить: автоматический commit изменений агента или оставлять незакоммиченными (рекомендация: авто-commit + сохранение diff)
- [ ] Решить: long polling или постоянный WebSocket для node channel (рекомендация MVP: long polling, WebSocket — later)
- [ ] Решить: control plane только Docker Compose или также standalone binary (рекомендация: оба, Compose — как основной сценарий)
- [ ] Записать все решения в `docs/decisions/0001-mvp-scope.md` (ADR-формат)

### 0.2 Инфраструктура репозитория

- [ ] Создать git-репозиторий (monorepo)
- [ ] Настроить структуру каталогов:
  - [ ] `crates/control-plane` — сервер (Rust, Axum)
  - [ ] `crates/node-daemon` — daemon (Rust, Tokio)
  - [ ] `crates/cli` — CLI (Rust, clap)
  - [ ] `crates/common` — общие типы: task states, event types, API DTO
  - [ ] `crates/adapters` — контракт adapter + mock + реальный adapter
  - [ ] `web/` — web UI (TypeScript)
  - [ ] `docs/` — документация и ADR
  - [ ] `deploy/` — Docker Compose, systemd units, скрипты установки
  - [ ] `tests/e2e/` — end-to-end сценарии
- [ ] Настроить Cargo workspace (`Cargo.toml` в корне, общие versions через `workspace.dependencies`)
- [ ] Настроить `rustfmt.toml` и `clippy` (deny warnings в CI)
- [ ] Настроить `.editorconfig`, `.gitignore`
- [ ] Настроить pre-commit hooks (fmt, clippy, тесты)

### 0.3 CI/CD

- [ ] Настроить CI pipeline (GitHub Actions или аналог):
  - [ ] job: migrations на чистой SQLite-базе и upgrade с предыдущей схемы
  - [ ] job: `cargo fmt --check`
  - [ ] job: `cargo clippy --all-targets -- -D warnings`
  - [ ] job: `cargo test --workspace`
  - [ ] job: сборка web UI (`npm ci && npm run build && npm run lint`)
  - [ ] job: сборка release-бинарников под `x86_64-unknown-linux-gnu`
  - [ ] job: сборка Docker-образов control plane и node daemon
- [ ] Кэширование cargo и npm зависимостей в CI
- [ ] Настроить Tier 1 CI/E2E: Ubuntu 24.04 LTS и Debian 12/13 x86_64
- [ ] Публиковать `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` и GNU fallback для x86_64

### 0.4 Базовые зависимости

- [ ] Control plane: `axum`, `tokio`, `tower`, `sqlx` (`sqlite`, `runtime-tokio`, `migrate`), `serde`, `serde_json`, `uuid`, `chrono`/`time`, `tracing`, `tracing-subscriber`, `thiserror`, `anyhow`, `argon2` (пароль пользователя), `jsonwebtoken` или сессии
- [ ] Node daemon: `tokio`, `reqwest` (rustls), `serde`, `tracing`, `nix` (process group / signals), `sysinfo` (диск, load), bundled SQLite event spool
- [ ] Не добавлять обязательные runtime-зависимости Docker, Node.js, Python, Java, OpenSSL или внешнюю СУБД
- [ ] CLI: `clap` (derive), `reqwest`, `serde`, `comfy-table` или аналог, `indicatif` (прогресс/follow)
- [ ] Проверить лицензии зависимостей (`cargo deny`)

---

## Этап 1 — Вертикальный прототип (1–2 недели)

> Цель: end-to-end поток «CLI → control plane → node → mock adapter → stdout stream → результат» без persistent storage, без auth, на одной машине.

### 1.1 Общие типы (`crates/common`)

- [ ] Определить enum `TaskStatus`: `queued | assigned | running | validating | succeeded | failed | cancelled`
- [ ] Определить enum `AttemptStatus`: `assigned | running | validating | succeeded | failed | cancelled | lost`
- [ ] Определить enum `NodeStatus`: `pending | online | degraded | offline | revoked`
- [ ] Определить `TaskEvent { attempt_id, sequence, type, payload, created_at }` c типами `status | stdout | stderr | tool | artifact | metric`
- [ ] Определить DTO для всех API-запросов/ответов (общие между сервером, daemon и CLI)
- [ ] Unit-тесты сериализации/десериализации DTO (serde round-trip)

### 1.2 Скелет control plane

- [ ] HTTP-сервер на Axum с graceful shutdown (SIGTERM/SIGINT)
- [ ] In-memory хранилище: `nodes`, `repositories`, `tasks`, `attempts`, `events` (за `RwLock`/`DashMap`)
- [ ] Endpoint `GET /health/live` (всегда 200)
- [ ] Endpoint `GET /health/ready` (готовность хранилища)
- [ ] Endpoint `POST /v1/tasks` — создать задачу (prompt, repository, adapter)
- [ ] Endpoint `GET /v1/tasks` и `GET /v1/tasks/:id`
- [ ] Endpoint `GET /v1/tasks/:id/events` — отдача событий (сначала polling с `?after_sequence=`)
- [ ] Endpoint `POST /v1/node/poll` — long polling выдача assignment
- [ ] Endpoint `POST /v1/node/attempts/:id/events` — приём событий от node
- [ ] Endpoint `POST /v1/node/attempts/:id/complete` — завершение attempt
- [ ] Простейший in-memory scheduler: первая свободная node
- [ ] Структурированные логи `tracing` с `task_id`/`attempt_id`/`node_id` в span-контексте

### 1.3 Скелет node daemon

- [ ] Конфиг из YAML + env override (`server_url`, `node_name`, `workspace_root`, `max_concurrency`)
- [ ] Цикл long polling: запрос assignment → выполнение → отправка complete
- [ ] Запуск subprocess (mock adapter) через `tokio::process::Command`
- [ ] Создание отдельной process group для subprocess (`setsid`/`process_group(0)`)
- [ ] Чтение stdout/stderr построчно/чанками и отправка в control plane с монотонным `sequence`
- [ ] Отправка финального статуса и exit code
- [ ] Логи `tracing`

### 1.4 Mock adapter

- [ ] Отдельный бинарник/скрипт `adapter-mock`
- [ ] Детерминированное поведение по命 prompt-командам:
  - [ ] `sleep:<seconds>` — долгая задача (для теста cancel/timeout)
  - [ ] `write:<file>:<content>` — создать/изменить файл в workspace
  - [ ] `fail:<exit-code>` — завершиться с ошибкой
  - [ ] `spam:<n>` — вывести n строк в stdout (для теста стриминга и буфера)
- [ ] Вывод в stdout в общем event-формате adapter-контракта (JSON lines)

### 1.5 Минимальный CLI

- [ ] `task run <repo> "<prompt>" --adapter mock` — создать задачу
- [ ] `task logs <task-id> --follow` — стрим логов (poll `?after_sequence=`)
- [ ] `task show <task-id>` — статус и результат
- [ ] `node list` — список nodes

### 1.6 Критерий выхода из этапа 1

- [ ] На одной машине: `task run` → mock adapter пишет файл → логи стримятся в CLI → задача переходит в `succeeded`
- [ ] Долгая задача видна как `running`, логи приходят инкрементально
- [ ] Две параллельные задачи на одной node выполняются независимо

---

## Этап 2 — Persistent execution (2–3 недели)

> Цель: SQLite WAL, полноценная state machine c lease, heartbeat, retry/cancel, Git worktrees, artifacts. После этого этапа система переживает рестарты при минимальном потреблении ресурсов.

### 2.1 SQLite schema и слой данных

- [ ] Зафиксировать ограничение MVP: один активный экземпляр control plane, база только на локальном диске, NFS/network shares не поддерживаются
- [ ] Создавать каталог данных и файл `/var/lib/agentgrid/control-plane.db` с правами пользователя control plane
- [ ] При открытии соединений применять `PRAGMA journal_mode=WAL`, `PRAGMA synchronous=NORMAL`, `PRAGMA foreign_keys=ON`, `PRAGMA busy_timeout=5000`
- [ ] Настроить небольшой connection pool (например 4 соединения)
- [ ] Настроить `sqlx` migrations (`migrations/`) для SQLite
- [ ] Добавить startup-проверку версии SQLite и `PRAGMA quick_check`
- [ ] Миграция `nodes`: id, name, status, os, arch, agent_version, max_concurrency, capabilities (jsonb), last_heartbeat_at, created_at, credential_hash, revoked_at
- [ ] Миграция `repositories`: id, name, git_url, default_branch, validation_command, created_at
- [ ] Миграция `node_repositories`: node_id, repository_id, local_path, status (`ready|cloning|invalid`), last_synced_at, PK (node_id, repository_id)
- [ ] Миграция `tasks`: id, repository_id, prompt, adapter, requested_node_id, status, created_at, started_at, finished_at
- [ ] Миграция `attempts`: id, task_id, number, node_id, status, lease_expires_at, workspace_path, branch_name, commit_sha, exit_code, error_code, started_at, finished_at; UNIQUE (task_id, number)
- [ ] Миграция `task_events`: id, attempt_id, sequence, type, payload (jsonb), created_at; UNIQUE (attempt_id, sequence)
- [ ] Миграция `enrollment_tokens`: id, token_hash, expires_at, used_at, created_at
- [ ] Миграция `audit_events`: id, actor_type (`user|node|system`), actor_id, action, subject, payload, created_at
- [ ] Индексы: tasks(status), attempts(task_id), attempts(status, lease_expires_at), task_events(attempt_id, sequence), nodes(status)
- [ ] Изолировать SQL внутри repository/storage-слоя, не пропуская SQLite-специфичные детали в бизнес-логику
- [ ] Интеграционные тесты storage-слоя запускать на временном SQLite-файле, а не на `:memory:`
- [ ] Заменить in-memory хранилище этапа 1 на SQLite
- [ ] Добавить checkpoint WAL при graceful shutdown
- [ ] Добавить согласованный backup через SQLite backup API или `VACUUM INTO`
- [ ] Добавить тест восстановления из backup

### 2.2 Task state machine

- [ ] Реализовать переходы как чистые функции: `(status, event) -> Result<status, InvalidTransition>`
- [ ] Запретить любые переходы вне схемы раздела 8 спеки
- [ ] Атомарное назначение выполнять короткой write-транзакцией `BEGIN IMMEDIATE`: выбрать queued task, условно обновить `WHERE status='queued'`, создать attempt и commit
- [ ] Использовать `UPDATE ... RETURNING` либо проверять affected rows; при гонке повторять выбор
- [ ] Не держать write-транзакцию открытой во время network I/O, Git-команд или ожидания node
- [ ] Lease: при assignment записывать `lease_expires_at = now() + assignment_lease_seconds (30s)`
- [ ] Фоновая job: возврат в `queued` задач, у которых assignment не подтверждён за 30 секунд (attempt → отменяется)
- [ ] Фоновая job: перевод node в `offline` при отсутствии heartbeat 30 секунд (`node_offline_seconds`)
- [ ] Фоновая job: при потере node пометить её `running`-attempts как `lost`, task → `failed` с `error_code=node_lost` (без авто-retry)
- [ ] Отмена: `queued` → `cancelled` сразу; `assigned|running|validating` → отправка cancel-команды node → ожидание подтверждения → `cancelled`
- [ ] Retry: создание нового attempt (number+1) для задач в `failed|cancelled`, task снова в `queued`
- [ ] Unit-тесты: каждый допустимый переход + каждый запрещённый переход (критерий приёмки: state machine покрыта unit tests)
- [ ] Unit-тесты гонок: двойное назначение одной задачи невозможно (property/concurrent test)

### 2.3 Node lifecycle: enrollment, heartbeat, revoke

- [ ] `POST /v1/nodes/enrollment-token` — генерация одноразового токена, TTL 10 минут, хранить только hash
- [ ] `POST /v1/node/enroll` — обмен токена на постоянный node credential (случайный секрет, хранить hash); токен помечается использованным
- [ ] Аутентификация node-запросов по credential (Bearer)
- [ ] `POST /v1/node/heartbeat` — каждые 10 секунд: status, load, free disk, версия, активные attempts
- [ ] Публикация capabilities при enroll и heartbeat: adapters, репозитории, версии git
- [ ] `DELETE /v1/nodes/:id` — revoke: немедленный отказ в auth для credential, статус `revoked`
- [ ] Тест: отозванная node получает 401 на heartbeat/poll (критерий приёмки)
- [ ] Статус `degraded`: daemon сам сообщает причину (git недоступен, adapter отсутствует, диск < 5 ГБ); scheduler исключает degraded nodes
- [ ] Audit events на enroll, revoke, смену статуса

### 2.4 Scheduler по спецификации

- [ ] Фильтр: только `online` nodes
- [ ] Фильтр: node имеет нужный репозиторий в статусе `ready` и нужный adapter
- [ ] Фильтр: активные attempts < max_concurrency
- [ ] Выбор: минимум активных задач; tie-break — самое раннее время последнего назначения (хранить `last_assigned_at` на node)
- [ ] Поддержка явного `requested_node_id` (scheduler не выбирает другую машину; если node недоступна — зад��ча остаётся `queued` с понятной причиной)
- [ ] Если подходящих nodes нет — задача остаётся `queued`, причина видна в API (`no_eligible_nodes: [reasons]`)
- [ ] Unit-тесты: каждый фильтр, tie-break, requested_node, пустой пул
- [ ] Метрика scheduler latency (queued → assigned)

### 2.5 Репозитории и Git worktrees на node

- [ ] `POST /v1/repositories` — регистрация: name, git_url, default_branch, validation_command
- [ ] Команда/поток attach: node клонирует репозиторий в `repository_root/<repo-name>` (bare или полный clone — зафиксировать решение)
- [ ] Поддержка существующего локального пути как источника (валидация: это git-репозиторий, ветка существует)
- [ ] Статусы node_repository: `cloning → ready | invalid` c описанием ошибки
- [ ] Для каждого attempt:
  - [ ] `git fetch` до актуального `default_branch`
  - [ ] создание ветки `agent/<task-id>/<attempt-number>` от default_branch
  - [ ] `git worktree add <workspace_root>/<attempt-id> <branch>`
  - [ ] запрет второго attempt в том же worktree (проверка существования каталога + lock-файл)
- [ ] По завершении работы агента:
  - [ ] `git add -A && git commit` (если есть изменения; авторство `agentgrid <noreply@...>`, в message — task id и prompt-сниппет)
  - [ ] сохранить `git diff --binary` базовой ветки → артефакт `changes.patch`
  - [ ] сохранить commit SHA в attempt
  - [ ] при ошибке/отмене: сохранить незакоммиченные изменения (diff рабочего дерева) как артефакт
- [ ] Retention: удаление worktree через 24 часа после завершения (фоновая job) + ручная очистка
- [ ] Гарантия: исходная рабочая копия пользователя и base clone не изменяются (тест)
- [ ] Тесты: создание/удаление worktree, повторный attempt, конфликт имён веток, репозиторий с submodules (минимум — понятная ошибка)

### 2.6 События, стриминг и идемпотентность

- [ ] Идемпотентный ingest: `INSERT ... ON CONFLICT (attempt_id, sequence) DO NOTHING`
- [ ] Объединять stdout/stderr в chunks по 16–64 КБ или за интервалы 100–250 мс
- [ ] Записывать batches событий одной короткой транзакцией
- [ ] После завершения attempt переносить полный raw log в файловый artifact; в SQLite оставлять metadata, status events и ограниченный индекс log chunks
- [ ] `idempotency_key` для всех mutation node-запросов (`enroll`, `ack`, `complete`): таблица обработанных ключей либо естественные ключи
- [ ] Локальный буфер событий на node: очередь на диске (append-only файл или встроенный SQLite) на случай потери сети
- [ ] Лимит буфера 100 МБ на attempt; при превышении — сворачивание старых stdout/stderr chunks (метка `truncated`), status events не удаляются
- [ ] Повторная отправка после восстановления сети по sequence number (resume с последнего подтверждённого)
- [ ] SSE endpoint `GET /v1/tasks/:id/events?stream=true` для web UI (или WebSocket — по решению 0.1)
- [ ] Тест: обрыв сети в середине задачи → события доехали без дублей и пропусков после восстановления

### 2.7 Cancellation и timeout

- [ ] Cancel из API доставляется node через poll-канал (или отдельный канал команд)
- [ ] Daemon: `SIGTERM` всей process group → 10 секунд ожидания → `SIGKILL` всей process group
- [ ] Проверка отсутствия осиротевших дочерних процессов после kill (тест с mock adapter, порождающим детей)
- [ ] Timeout задачи: default 60 минут, настраивается per-task; по истечении — тот же механизм, что cancel, но статус `failed` c `error_code=timeout`
- [ ] Частичный diff сохраняется при cancel и timeout (критерий приёмки)

### 2.8 Artifacts

- [ ] Хранилище артефактов на control plane: локальная ФС `artifact_root/<attempt-id>/<name>` (в SQLite только metadata)
- [ ] Загрузка артефактов с node на complete: `changes.patch`, `validation.log`, `agent-raw-output.log`
- [ ] `GET /v1/tasks/:id/artifacts/:name` — отдача с корректным Content-Type и лимитом размера
- [ ] Retention артефактов: `artifact_retention_hours` (168h default), фоновая очистка

### 2.9 Критерий выхода из этапа 2

- [ ] Рестарт control plane: queued-задачи не теряются, running-attempts корректно восстанавливают стриминг
- [ ] Аварийное завершение во время записи не повреждает SQLite; после старта проходит `quick_check`
- [ ] Рост WAL ограничен checkpoints; длительный reader не приводит к неконтролируемому росту диска
- [ ] Рестарт daemon: незавершённые attempts обнаружены и зарепорчены (`lost` или продолжение, по спеке — сообщить)
- [ ] Kill -9 daemon в середине задачи → attempt = `lost` после истечения heartbeat window
- [ ] Все события идемпотентны, дублей в UI/CLI нет

---

## Этап 3 — Реальный agent adapter (1–2 недели)

> Цель: выбранный CLI-agent (Claude Code / Codex CLI / OpenCode) работает через общий adapter-контракт, с timeout, validation, diff и commit.

### 3.1 Adapter-контракт (финализация)

- [ ] Зафиксировать контракт в коде и документации: `prepare(task, workspace, config)`, `start(prompt)`, `stream_events()`, `cancel()`, `collect_result()`
- [ ] Определить общий формат событий adapter → daemon (JSON lines в stdout): `log`, `tool_call`, `file_change`, `progress`, `result`, `error`
- [ ] Определить конфиг adapter в node YAML: путь к бинарнику, env-переменные (API key), дополнительные аргументы
- [ ] Capability discovery: daemon проверяет наличие и версию бинарника adapter при старте и в heartbeat
- [ ] Сохранение raw output агента как артефакт (защита от смены формата CLI — риск №1 спеки)

### 3.2 Реализация выбранного adapter

- [ ] Изучить headless/non-interactive режим выбранного CLI (флаги, формат вывода, exit codes, поведение при отсутствии TTY)
- [ ] Зафиксировать поддерживаемую версию CLI (pin + проверка версии при prepare, warning при несовпадении)
- [ ] `prepare`: проверка бинарника, проверка API-ключа, подготовка рабочего каталога
- [ ] `start`: запуск в workspace с prompt; передача секретов только через env процесса
- [ ] Парсинг stream-вывода CLI → общие события (с fallback: нераспознанные строки → `log`)
- [ ] Обработка ошибок: rate limit, невалидный ключ, сетевая ошибка LLM — различимые `error_code`
- [ ] `cancel`: корректное завершение через механизм этапа 2.7
- [ ] `collect_result`: сводка изменений агента (файлы, краткое описание если CLI его даёт)
- [ ] Интеграционный тест на реальном мини-репозитории (можно пометить `#[ignore]` для CI без ключей)

### 3.3 Validation-команда

- [ ] Запуск validation после успешного завершения агента (статус `validating`)
- [ ] Validation выполняется в том же worktree через `sh -c "<validation_command>"`
- [ ] Отдельный timeout для validation (настраиваемый, default 15 минут)
- [ ] stdout/stderr validation стримятся как события и сохраняются в `validation.log`
- [ ] Ошибка validation даёт `failed` с `error_code=validation_failed` — отличимо от `agent_failed` (критерий приёмки)
- [ ] Diff и commit создаются до validation, чтобы результат агента сохранялся даже при падении тестов

### 3.4 Маскирование секретов

- [ ] Реестр известных секретов задачи (env-значения, переданные adapter)
- [ ] Фильтр в pipeline событий: замена вхождений секретов на `***` в stdout/stderr до отправки с node
- [ ] Тест: секрет из env не появляется ни в events, ни в артефактах, ни в логах daemon (критерий приёмки)

### 3.5 Критерий выхода из этапа 3

- [ ] Реальная задача (например «добавь healthcheck endpoint») выполняется выбранным агентом на удалённой node
- [ ] По завершении доступны: diff, commit SHA, validation result, полные логи
- [ ] Ошибки агента, validation и инфраструктуры различимы по `error_code`

---

## Этап 4 — Интерфейсы (2 недели)

### 4.1 Аутентификация пользователя

- [ ] Локальный пользователь: создание при первом запуске (setup-команда или env)
- [ ] `POST /v1/auth/login` — пароль (argon2) → сессионный токен/JWT
- [ ] Auth middleware для всех `/v1/*` пользовательских endpoint (кроме health)
- [ ] Хранение токена CLI в `~/.config/agentgrid/credentials` с правами 0600
- [ ] Rate limit на login

### 4.2 CLI (полный набор команд спеки)

- [ ] `server start` — запуск control plane (standalone-режим, если выбран в 0.1)
- [ ] `token create` — выдача enrollment token
- [ ] `node install --server <url> --token <token>` — установка daemon: создание пользователя, каталогов, systemd unit, enroll
- [ ] `node list` — таблица: имя, статус, адаптеры, репозитории, загрузка, last heartbeat
- [ ] `repo add <git-url> --name <name>` и `repo attach <name> --node <node>`
- [ ] `task run <repo> "<prompt>" --adapter <a> --validate "<cmd>" [--node <node>] [--timeout <min>]`
- [ ] `task logs <id> --follow` — live-стрим с reconnect и resume по sequence
- [ ] `task cancel <id>`, `task retry <id>`, `task show <id>` (статус, node, время, diff-сводка, путь к артефактам)
- [ ] Человекочитаемые ошибки + `--json` для машиночитаемого вывода
- [ ] Exit codes: 0 — успех, ненулевые — категории ошибок (для скриптов)

### 4.3 Web UI

- [ ] Настроить проект (Vite + React/Svelte + TypeScript), прокси к API в dev
- [ ] Экран логина
- [ ] **Dashboard**: счётчики nodes online/offline, задачи running/queued, последние 10 завершённых задач со статусами
- [ ] **Nodes**: таблица со статусом, capabilities, загрузкой, adapters, repositories; кнопка revoke с подтверждением
- [ ] **New task**: форма — репозиторий, prompt, adapter, auto/manual node, validation command; валидация формы
- [ ] **Task details**:
  - [ ] timeline смены статусов с временем
  - [ ] live stdout/stderr через SSE с автоскроллом и паузой
  - [ ] информация о node и attempt (с историей attempts)
  - [ ] просмотр diff (подсветка синтаксиса patch)
  - [ ] commit SHA и validation result (с логом)
  - [ ] кнопки cancel / retry согласно текущему статусу
- [ ] Обработка обрыва SSE: reconnect + дозагрузка пропущенных событий по sequence
- [ ] Сборка UI в статику, раздача из control plane
- [ ] Проверка: логи в UI появляются ≤ 2 секунд после получения control plane (критерий приёмки)

### 4.4 Критерий выхода из этапа 4

- [ ] Весь сценарий 5.3 спеки проходим и через CLI, и через web UI
- [ ] Отмена и retry работают из обоих интерфейсов

---

## Этап 5 — Hardening и релиз (2–3 недели)

### 5.1 Безопасность (раздел 13 спеки)

- [ ] HTTPS: документированная установка за reverse proxy (Caddy/nginx) + поддержка собственного TLS в бинарнике (rustls) — выбрать и зафиксировать
- [ ] Проверить: enrollment token одноразовый, TTL ≤ 10 минут, хранится только hash
- [ ] Проверить: у каждой node уникальный credential, revoke действует немедленно
- [ ] Daemon отказывается стартовать под root без явного `--allow-root`
- [ ] systemd unit: отдельный пользователь `agentgrid`, `ProtectSystem=strict`, `ReadWritePaths` только workspace/repository roots, `NoNewPrivileges=true`
- [ ] Лимиты размеров: prompt (например 64 KB), event (1 MB), artifact (например 50 MB) — конфигурируемы, возвращают 413
- [ ] Audit events на все действия пользователя и nodes (login, task create/cancel/retry, enroll, revoke, repo add)
- [ ] Предупреждение в UI и документации: agent имеет права пользователя daemon, sandbox отсутствует в MVP
- [ ] Базовый threat-review: пройтись по каждому endpoint — auth, валидация входа, лимиты

### 5.2 Наблюдаемость (раздел 15 спеки)

- [ ] Единый формат структурированных логов (JSON): timestamp, level, component, node_id, task_id, attempt_id, message
- [ ] `GET /metrics` в формате Prometheus:
  - [ ] nodes по статусам, queued/running tasks
  - [ ] task duration (histogram), success/failure/cancel rate
  - [ ] scheduler latency, heartbeat latency
  - [ ] размер event buffer и свободный диск по nodes (из heartbeat)
- [ ] `GET /health/ready` проверяет чтение SQLite и возможность записи в каталог данных
- [ ] Метрики SQLite: размер main DB/WAL, время ожидания write lock, число `SQLITE_BUSY`, длительность checkpoint
- [ ] Документация по подключению Prometheus/Grafana (опционально — готовый dashboard JSON)

### 5.3 Пакетирование и установка

- [ ] Dockerfile control plane (multi-stage, distroless/slim)
- [ ] Dockerfile node daemon (для тестов и опционального запуска в контейнере)
- [ ] `docker-compose.yml`: один control plane с persistent volume для SQLite и artifacts; миграции при старте; запуск одной командой
- [ ] Проверить корректный SIGTERM, WAL checkpoint и сохранность volume после пересоздания контейнера
- [ ] Скрипт/команда установки node: создание пользователя, каталогов `/var/lib/agentgrid/...`, systemd unit, enroll — целевое время подключения < 10 минут (критерий приёмки)
- [ ] Release-бинарники для Ubuntu LTS и Debian stable (+ проверка glibc-совместимости или musl static build)
- [ ] Версионирование: semver, `--version` у всех бинарников, проверка совместимости версий daemon ↔ control plane (warning при расхождении)

### 5.4 End-to-end тесты

- [ ] E2E-стенд: docker-compose c control plane на SQLite и двумя node-контейнерами
- [ ] E2E: конкурентное назначение не вызывает двойных assignments или необработанного `database is locked`
- [ ] E2E: online backup SQLite успешно восстанавливается
- [ ] Сценарий: happy path с mock adapter (run → logs → succeeded → diff/commit)
- [ ] Сценарий: параллельные задачи на двух nodes, проверка распределения scheduler
- [ ] Сценарий: cancel долгой задачи (процессы убиты, частичный diff сохранён)
- [ ] Сценарий: timeout задачи
- [ ] Сценарий: убийство node-контейнера во время running → attempt `lost`
- [ ] Сценарий: рестарт control plane с queued-задачами → задачи выполняются после рестарта
- [ ] Сценарий: обрыв сети node (`docker network disconnect`) → буферизация и досылка без дублей
- [ ] Сценарий: revoke node → отказ в доступе
- [ ] Сценарий: validation failure ≠ agent failure (разные error_code)
- [ ] Прогон E2E в CI на каждый PR (mock adapter; реальный agent — nightly/manual)

### 5.5 Документация

- [ ] README: что это, архитектурная схема, quick start
- [ ] Getting started: запуск control plane + подключение двух nodes + первая задача (пошагово)
- [ ] Справочник конфигурации (все ключи YAML/env с default-значениями)
- [ ] Документация SQLite: WAL, локальный диск, backup/restore, `SQLITE_BUSY`, безопасное копирование базы
- [ ] Справочник API (`/v1`, все endpoint, коды ошибок)
- [ ] Справочник CLI
- [ ] Гайд по написанию своего adapter (контракт + пример mock)
- [ ] Раздел о безопасности: модель угроз, ограничения MVP (нет sandbox), рекомендации
- [ ] Troubleshooting: типовые ошибки enroll, clone, adapter, TLS

### 5.6 Финальная проверка критериев приёмки (раздел 17 спеки)

**Подключение**

- [ ] Control plane запускается одной командой как standalone binary и через Docker Compose
- [ ] Для запуска не требуется отдельный сервер БД; state хранится в одном локальном SQLite-файле
- [ ] Документированы backup/restore и ограничение «один активный control plane»
- [ ] Чистая Linux-машина подключается как node < 10 минут
- [ ] Node появляется online ≤ 15 секунд
- [ ] Отозванная node не может отправлять heartbeat / получать задачи

**Выполнение**

- [ ] Задача из CLI выполняется на другой Linux-машине
- [ ] Scheduler не назначает offline/перегруженные nodes
- [ ] Отдельный worktree и ветка на задачу; исходный working tree не изменяется
- [ ] Логи в UI ≤ 2 секунды
- [ ] После успеха доступны diff, commit SHA, validation result
- [ ] Mock adapter покрывает весь pipeline без LLM API

**Ошибки**

- [ ] Cancel убивает всю process group
- [ ] Рестарт control plane не теряет queued tasks
- [ ] Дублированные events не отображаются дважды
- [ ] Потеря node → attempt `lost`
- [ ] validation failure ≠ agent failure
- [ ] Частичный diff сохраняется при failure/cancel

**Качество**

- [ ] State machine покрыта unit tests
- [ ] E2E-тест с двумя node-контейнерами и mock adapter проходит в CI
- [ ] Бинарники собираются для актуальных Ubuntu и Debian
- [ ] Все публичные API имеют префикс `/v1`
- [ ] В логах нет значений тестовых secrets

### 5.7 Definition of Done (раздел 21 спеки)

- [ ] Два независимых физических/виртуальных Linux-host подключены к одному control plane с SQLite WAL
- [ ] Control plane в простое укладывается в ресурсный бюджет (цель: RSS ≤ 64 МБ при типовой конфигурации)
- [ ] С первого host отправлена задача, которая на втором host:
  - [ ] получает Git-репозиторий
  - [ ] создаёт отдельный worktree
  - [ ] запускает реальный coding agent
  - [ ] транслирует логи
  - [ ] выполняет validation
  - [ ] сохраняет diff и commit
  - [ ] корректно отображает success / failure / cancellation
  - [ ] не теряет историю после перезапуска control plane
- [ ] Тег релиза `v0.1.0`, changelog, опубликованные артефакты сборки

---

## Сквозные практики (на протяжении всего проекта)

- [ ] Каждая фича — через PR с зелёным CI (даже в соло-режиме)
- [ ] Не добавлять функциональность вне scope MVP (раздел 4 спеки) — записывать идеи в backlog 0.2
- [ ] Обновлять ADR при каждом архитектурном решении
- [ ] Раз в неделю — ручной прогон happy path на двух реальных машинах (не только в контейнерах)
- [ ] Вести `CHANGELOG.md`

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

- [ ] Использовать `rustls`; не требовать системный OpenSSL
- [ ] Собирать SQLite внутрь бинарника (`bundled`), не требовать системную SQLite library
- [ ] Проверить запуск node daemon на чистой Tier 1 машине, где отсутствуют Docker, Node.js, Python, Java и внешняя СУБД
- [ ] Обязательные зависимости node ограничить Linux kernel ≥ 5.10, Git ≥ 2.30, CA certificates и выбранным CLI-agent/runtime
- [ ] Отделить требования daemon от требований adapter и проекта: отсутствие Node.js не мешает работе daemon и adapters, которым Node.js не нужен
- [ ] Сделать Docker/Podman опциональным executor; default executor — `process`
- [ ] Реализовать fallback ручного запуска на системах без systemd

### 6.2 Release targets и размер бинарников

- [ ] Публиковать `x86_64-unknown-linux-musl` как основной Tier 1 artifact
- [ ] Публиковать `aarch64-unknown-linux-musl` как Tier 2 artifact
- [ ] Публиковать `x86_64-unknown-linux-gnu` как fallback для корпоративных Linux-систем
- [ ] Проверить DNS, системные CA, proxy и credential flows в musl-сборке
- [ ] Настроить release profile:
  - [ ] `opt-level = "s"`
  - [ ] `lto = "thin"`
  - [ ] `codegen-units = 1`
  - [ ] `panic = "abort"`
  - [ ] `strip = "symbols"`
- [ ] Отключить ненужные default features зависимостей
- [ ] Не включать одновременно несколько TLS backends
- [ ] Не включать тяжёлые telemetry exporters по умолчанию
- [ ] Зафиксировать размеры release-бинарников в CI и выводить регрессию размера в build summary

### 6.3 Минимальные ресурсы и бюджеты

- [ ] Зафиксировать минимальную машину daemon: 1 CPU, 128 МБ RAM, 100–300 МБ диска сверх workspaces
- [ ] Зафиксировать целевой RSS node daemon: 8–25 МБ idle, ≤ 60 МБ streaming без agent subprocess
- [ ] Зафиксировать целевой RSS control plane: ≤ 64 МБ idle при типовой конфигурации
- [ ] Добавить benchmark/smoke test RSS в CI или release pipeline
- [ ] Документировать, что реальные требования задачи зависят от проекта: 512 МБ–1 ГБ для простого editing, 2–4 ГБ для Node/Python tests, 4–8+ ГБ для Rust/Java/C++
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

- [ ] Heartbeat при running: каждые 5–10 секунд
- [ ] Heartbeat в idle: каждые 20–30 секунд
- [ ] Добавить jitter ±10–20%, чтобы nodes не синхронизировали запросы после рестарта
- [ ] Long polling timeout установить 25–60 секунд
- [ ] Не допускать polling каждую секунду
- [ ] Сохранить быстрый переход offline: учитывать режим heartbeat и grace window
- [ ] Нагрузочный тест heartbeat/poll для 100 idle nodes на одном control plane

### 6.6 Batching, bounded queues и backpressure

- [ ] Все async channels сделать bounded; запретить `unbounded_channel` в execution/event pipeline
- [ ] Ограничить live memory buffer на attempt до 1–4 МБ
- [ ] Ограничить disk spool до 100 МБ на attempt
- [ ] При заполнении memory buffer сбрасывать события в disk spool, а не накапливать RAM
- [ ] При достижении disk limit агрегировать старые stdout/stderr и добавлять `output_truncated`
- [ ] Никогда не удалять status/error/result events при truncation
- [ ] Формировать log batches по 16–64 КБ или каждые 100–250 мс
- [ ] Отправлять status/error/result немедленно, не ожидая batch timeout
- [ ] Выполнять batch insert событий одной короткой SQLite-транзакцией
- [ ] Включать gzip/zstd HTTP compression только выше порога 8–16 КБ
- [ ] Проверить backpressure mock-сценарием `spam` с объёмом больше RAM/disk limits
- [ ] Проверить, что медленный или недоступный control plane не вызывает роста RSS node

### 6.7 Политика хранения логов

- [ ] Во время выполнения хранить в SQLite status events и ограниченный live tail
- [ ] Полный stdout/stderr писать последовательно в append-only файл attempt
- [ ] После завершения закрывать и сжимать raw log в `.zst`
- [ ] Удалять bulk log chunks из SQLite после формирования artifact
- [ ] Оставлять в SQLite последние 500–2000 строк для быстрого Task details
- [ ] Полный лог отдавать через artifacts API с Range/streaming, не загружая файл целиком в RAM
- [ ] Adapter парсит только стабильные события `status/stdout/stderr/tool/result/error/artifact`
- [ ] Неизвестные записи CLI сохранять как raw log, а не завершать adapter ошибкой

### 6.8 Git cache и worktree performance

- [ ] Хранить один repository mirror/cache на пару node+repository
- [ ] Не выполнять полный clone для каждого attempt
- [ ] Создавать worktrees через общую Git object database
- [ ] Сериализовать `git fetch` mutex/file lock-ом на repository
- [ ] Объединять fetch для группы одновременно стартующих задач
- [ ] Не запускать `git gc`/maintenance во время активных attempts репозитория
- [ ] Запускать Git maintenance только в idle window
- [ ] Удалять старые worktrees и ветки пакетно
- [ ] Обнаруживать и публиковать Git LFS как capability
- [ ] Обнаруживать и публиковать submodules support как capability
- [ ] Не включать partial clone по умолчанию в MVP; оставить опцией после проверки offline-поведения
- [ ] Тестировать отсутствие изменений исходной рабочей копии и повторное использование object database

### 6.9 Toolchains и capability discovery

- [ ] Agentgrid не устанавливает автоматически Rust, Node.js, Python, Java, Go и package managers
- [ ] При старте обнаруживать версии `git`, adapters и распространённых runtimes/tools
- [ ] Не запускать `--version` перед каждой задачей; кэшировать capability snapshot
- [ ] Обновлять capabilities при старте, периодически, вручную и после `command not found`
- [ ] Представлять readiness каждого adapter отдельно: `ready`, `missing`, `incompatible`, `misconfigured`
- [ ] Repository requirements хранить структурированно: OS, arch, tools, versions, memory, disk
- [ ] Scheduler проверяет требования до assignment
- [ ] Поддержать semver/range comparison для совместимых tools
- [ ] Показывать пользователю точную причину `no_eligible_nodes`

### 6.10 Resource reservations и pressure hysteresis

- [ ] Добавить конфиг `reserved_memory_mb`
- [ ] Добавить `min_free_disk_mb` (default 5120)
- [ ] Добавить `max_load_average_per_cpu`
- [ ] Сохранить `max_concurrency` как жёсткий верхний предел
- [ ] Heartbeat передаёт free RAM, free disk, load average и active attempts
- [ ] Scheduler не назначает задачу при нарушении resource reservations
- [ ] Переводить node в `degraded(resource_pressure)` после трёх последовательных плохих измерений
- [ ] Возвращать node в `online` после пяти нормальных измерений
- [ ] Не менять status из-за одного кратковременного load spike
- [ ] Тестировать две тяжёлые задачи при `max_concurrency=2`, но недостаточной памяти для второй

### 6.11 cgroups v2 и subprocess limits

- [ ] Default executor запускает subprocess под отдельным Unix user
- [ ] При наличии systemd/cgroups v2 создавать transient scope на attempt
- [ ] Поддержать `MemoryMax`
- [ ] Поддержать `CPUQuota`
- [ ] Поддержать `TasksMax`
- [ ] Завершать весь cgroup при cancel/timeout
- [ ] Fallback при отсутствии systemd scope — process group + SIGTERM/SIGKILL
- [ ] Публиковать поддержку cgroups как capability
- [ ] Тестировать превышение memory limit и корректный `error_code=resource_limit`
- [ ] Тестировать fork-heavy mock adapter и `TasksMax`

### 6.12 Protocol и version compatibility

- [ ] Передавать `node_version`
- [ ] Передавать `protocol_version`
- [ ] Передавать `capabilities_schema_version`
- [ ] Передавать `supported_event_versions`
- [ ] Делать новые JSON-поля optional
- [ ] Игнорировать неизвестные поля и сохранять unknown event как raw payload
- [ ] Control plane поддерживает текущую и предыдущую minor-версию node
- [ ] Несовместимую node переводить в `degraded(incompatible_protocol)`, а не завершать процесс
- [ ] Добавить contract tests для N и N-1 node/control-plane versions
- [ ] Автоматические migrations поддерживают upgrade; downgrade базы явно не гарантировать

### 6.13 Матрица ОС и файловых систем

- [ ] Tier 1: Ubuntu 24.04 LTS x86_64, полный CI/E2E
- [ ] Tier 1: Debian 12/13 x86_64, полный CI/E2E
- [ ] Tier 1 filesystem: ext4 и xfs
- [ ] Tier 1: systemd и Git 2.39+
- [ ] Tier 2: ARM64 Ubuntu/Debian — публиковать бинарник и выполнять smoke test
- [ ] Tier 2: Fedora, Rocky/Alma, Arch — документировать limited testing
- [ ] Tier 3/best effort: Alpine, WSL2, NixOS, системы без systemd, NAS, read-only root
- [ ] Не поддерживать kernel < 5.10, 32-bit и big-endian в MVP
- [ ] Не поддерживать SQLite и workspaces на NFS/network filesystem
- [ ] Для WSL2 предупреждать против `/mnt/c`; рекомендовать Linux filesystem

### 6.14 Performance acceptance tests

- [ ] Idle node daemon RSS ≤ 25 МБ на Tier 1 машине
- [ ] Idle control plane RSS ≤ 64 МБ с SQLite и web UI
- [ ] Streaming node RSS ≤ 60 МБ без учёта child process
- [ ] 1 ГБ mock stdout не приводит к линейному росту RAM
- [ ] 100 idle nodes с long polling/heartbeat не создают постоянную высокую CPU load
- [ ] Повторный attempt существующего repository не выполняет полный clone
- [ ] Две параллельные задачи одного repository не запускают два fetch одновременно
- [ ] Node без нужного runtime не получает несовместимую задачу
- [ ] Resource pressure блокирует assignment до запуска subprocess
- [ ] ARM64 musl binary стартует и проходит mock happy path
- [ ] Tier 1 установка daemon проходит без Docker, Node.js, Python и внешней СУБД

### 6.15 Осознанно не делать в MVP

- [ ] Не заменять JSON на Protobuf только ради микросекундной оптимизации
- [ ] Не создавать собственный binary wire protocol
- [ ] Не внедрять lock-free очереди без подтверждённого bottleneck
- [ ] Не писать собственный embedded KV-store вместо SQLite
- [ ] Не внедрять P2P routing и consensus
- [ ] Не делать Docker обязательным способом execution
- [ ] Не обещать поддержку любого Linux без compatibility tiers
- [ ] Профилировать до добавления низкоуровневых оптимизаций; приоритет — Git cache, batching, bounded queues и быстрый старт adapter
