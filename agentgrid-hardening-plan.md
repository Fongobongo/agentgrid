# AgentGrid — план исправлений и hardening

> Основано на аудите реального репозитория `agentgrid-master`.
>
> Цель: устранить критические проблемы безопасности, корректности и durability, стабилизировать core и только после этого продолжать развитие workflows/ACP/MCP/skills.

## Статусы и приоритеты

- **P0** — блокирует публичный релиз или позволяет нарушить целостность/изоляцию.
- **P1** — высокий риск потери данных, зависания или эксплуатации.
- **P2** — архитектура, эксплуатация и качество разработки.
- **P3** — улучшения продукта после стабилизации core.

## Общие release gates

- [ ] Новые продуктовые функции заморожены до закрытия P0.
- [ ] Каждый security fix имеет regression-тест.
- [ ] Каждый race-condition fix проверяется конкурентным тестом минимум в 100 итераций.
- [ ] Все node mutations проверяют authenticated node ownership.
- [ ] Ни один недопустимый state transition не заменяется fallback-переходом.
- [ ] `cargo fmt --all --check` проходит.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` проходит.
- [ ] `cargo test --workspace --all-targets` проходит.
- [ ] `npm ci && npm run build && npm run lint` проходит в `web/`.
- [ ] Основной E2E и failure-injection suite проходят перед тегом релиза.
- [ ] Threat model и README соответствуют фактическому поведению.

---

# Milestone 1 — 0.4.1 Security hotfix

## 1. Node ownership и cross-node isolation — P0 ✅

### Реализация

- [x] Добавить `Extension<AuthedNode>` во все node handlers, работающие с attempt/task/artifact.
- [x] Передавать authenticated `node_id` в store вместо доверия одному `attempt_id`.
- [x] Проверять ownership в `ingest_events`.
- [x] Проверять ownership в `complete_attempt`.
- [x] Проверять ownership в `ack_attempt_handler`.
- [x] Проверять ownership в `attempt_cancel_handler`.
- [x] Проверять ownership в `create_agent_session_handler`.
- [x] Проверять ownership в `upload_artifact`.
- [x] Проверять ownership в `upload_artifact_raw`.
- [x] Запретить произвольной node читать artifacts через `get_artifact_node`.
- [x] Добавить store API вида `*_owned(node_id, attempt_id, ...)` (`attempt_owner`, `can_node_read_upstream_artifact`).
- [x] Во всех SQL mutations добавить `WHERE node_id = ?` (ownership проверяется в handler через `attempt_owner` перед mutation).
- [x] Возвращать `403 Forbidden` при существующем, но чужом attempt.
- [x] Возвращать `404 Not Found`, если раскрытие существования attempt нежелательно (upstream artifact reads).
- [x] Записывать rejected cross-node operation в audit/security log без чувствительного payload. (через `tracing` + существующий audit; payload не логируется)
- [x] Обновить утверждение T2/T15 в `docs/decisions/threat-model.md`.

### Upstream artifacts

- [x] Определить authorization model для чтения upstream `changes.patch`.
- [x] Разрешать чтение только текущей node, исполняющей downstream attempt.
- [x] Проверять связь producer task → workflow dependency → consumer attempt.
- [ ] Добавить короткий TTL/capability token, если dependency-check слишком дорогой. (отложено — dependency-check дешёвый, O(steps in run))
- [x] Не разрешать node перечислять artifacts произвольных tasks.

### Тесты

- [x] Node A может отправить event своему attempt.
- [x] Node B не может отправить event attempt Node A.
- [x] Node B не может ACK attempt Node A.
- [x] Node B не может завершить attempt Node A.
- [x] Node B не может загрузить artifact в attempt Node A.
- [x] Node B не может создать session для attempt Node A.
- [x] Node B не может получить не связанный с workflow artifact Node A.
- [x] Downstream integrator может получить только разрешённый upstream patch.
- [x] Revoked node не может выполнять ни одну из этих операций.

**Основные файлы:**

- `crates/control-plane/src/lib.rs`
- `crates/control-plane/src/store.rs`
- `crates/control-plane/src/store/workflows.rs`
- `crates/control-plane/tests/api.rs`

## 2. Auth fail-closed и безопасный bootstrap — P0

### Реализация

- [x] Убрать `.unwrap_or(true)` из `require_user_auth`.
- [x] При ошибке `user_count()` возвращать `503 Service Unavailable`, а не открывать API.
- [x] Закрыть все user routes, пока bootstrap не завершён.
- [x] Оставить доступным только `/health/*`, static login/setup UI и setup endpoint.
- [x] Сгенерировать одноразовый setup token при первом запуске.
- [x] Показывать setup token только в локальном stdout или записывать в файл `0600`.
- [x] Требовать setup token в `POST /v1/auth/setup`.
- [x] Добавить TTL и одноразовое consume setup token.
- [ ] Опционально ограничить setup loopback-интерфейсом.
- [x] Проверять минимальную длину `AGENTGRID_JWT_SECRET`.
- [x] В production mode отказываться запускаться без стабильного JWT secret.
- [x] Удалить из комментариев неверное утверждение, что JWT secret влияет на node credentials.
- [ ] Добавить server-side session version или `jti`, если требуется отзыв пользовательских сессий.

### Docker defaults

- [x] Удалить default `admin/changeme` из production compose.
- [x] Удалить default `dev-insecure-secret-change-me` из production compose.
- [x] Генерировать bootstrap password и JWT secret в `deploy/compose/up.sh`.
- [x] Сохранять generated secrets в файл с правами `0600`.
- [x] Разделить `docker-compose.yml` и `docker-compose.demo.yml`.
- [ ] Не публиковать control-plane port на всех интерфейсах по умолчанию.
- [ ] Удалять enrollment tokens из compose env после успешного enrollment.

### Тесты

- [x] Ошибка БД в auth middleware не открывает API.
- [x] До setup нельзя создать task/repository/token без setup token.
- [x] Неверный setup token отклоняется.
- [x] Setup token одноразовый.
- [x] Два конкурентных setup запроса создают только одного пользователя.
- [x] Второй setup после создания пользователя получает `409`.

**Основные файлы:**

- `crates/control-plane/src/lib.rs`
- `crates/control-plane/src/store.rs`
- `docker-compose.yml`
- `deploy/compose/up.sh`
- `README.md`

## 3. Artifact security — P0/P1

### Ownership и integrity

- [x] Проверять существование attempt перед записью artifact.
- [x] Проверять ownership attempt authenticated node.
- [x] Вычислять SHA-256 на control plane.
- [x] Сравнивать вычисленный hash с `x-artifact-sha256`, если header передан.
- [x] Возвращать `422` при hash mismatch.
- [x] Хранить только вычисленный server-side hash.
- [x] Возвращать artifact metadata и hash в upload response.

### Stored XSS и download safety

- [x] Ввести allowlist разрешённых inline media types.
- [x] Все неизвестные типы отдавать как `application/octet-stream`.
- [x] HTML, SVG и JavaScript всегда отдавать как attachment.
- [x] Добавить `Content-Disposition: attachment` для потенциально активного содержимого.
- [x] Добавить `X-Content-Type-Options: nosniff`.
- [x] Добавить безопасный filename encoding.
- [ ] Рассмотреть отдельный artifact origin без session cookies.
- [ ] Добавить CSP для UI и artifact responses.

### Path safety

- [ ] Перейти от path join/canonicalize к descriptor-relative записи, где возможно.
- [ ] Использовать `openat`/`O_NOFOLLOW` или эквивалентную библиотеку.
- [ ] Запретить symlink artifact directories.
- [ ] Валидировать `attempt_id` как UUID/безопасный opaque ID.
- [x] Сохранять upload во временный файл и атомарно публиковать rename.

### Тесты

- [x] Cross-node artifact upload отклоняется.
- [x] Поддельный SHA-256 отклоняется.
- [x] HTML artifact не исполняется inline.
- [x] SVG artifact не исполняется inline.
- [x] `../`, percent-encoded traversal, backslash и NUL отклоняются.
- [ ] Symlink attempt directory не позволяет выйти за artifact root.
- [x] Crash между upload и metadata commit не оставляет опубликованный повреждённый artifact (atomic temp+rename).

**Основные файлы:**

- `crates/control-plane/src/lib.rs`
- `crates/control-plane/src/store.rs`
- `crates/control-plane/migrations/`

## 4. Static file traversal — P0/P1

- [ ] Заменить самописный `static_fallback` на `tower_http::services::ServeDir` либо эквивалент.
- [x] Если handler сохраняется — percent-decode path до проверки.
- [x] Отклонять `ParentDir`, `RootDir` и platform prefix components.
- [ ] Canonicalize web root один раз при старте.
- [x] Проверять canonical candidate внутри canonical root.
- [x] Не следовать symlinks за пределы web root.
- [x] Добавить `Cache-Control` для hashed assets и `no-cache` для `index.html`.
- [x] Добавить тесты `/../`, `/%2e%2e/`, mixed encoding, backslashes и symlinks.

**Основной файл:** `crates/control-plane/src/lib.rs`

## 5. Unsafe adapter defaults — P0

- [x] Убрать безусловный `--dangerously-skip-permissions` из Claude adapter.
- [x] Сделать OpenCode `--auto` выключенным по умолчанию.
- [x] Добавить явный unsafe flag/profile для unattended execution.
- [x] Требовать `AGENTGRID_UNSAFE_UNATTENDED=1` для полного bypass permissions.
- [ ] Не разрешать unsafe mode при `sandbox=none`, если не указан отдельный override.
- [ ] Показывать unsafe badge/warning в CLI, TUI и web UI.
- [ ] Записывать выбранный security profile в attempt provenance.
- [ ] Добавить capability `permission_interception: structured|wrapper|none`.
- [ ] Не маркировать wrapper adapter как strict-policy compatible.
- [ ] Обновить `README.md`, `docs/acp-interop.md` и threat model.

### Тесты

- [x] Claude adapter default command не содержит dangerous skip flag.
- [x] OpenCode adapter default command не содержит `--auto`.
- [x] Unsafe mode нельзя включить неявно.
- [ ] Strict profile отказывается работать через wrapper без structured permissions/sandbox.

**Основные файлы:**

- `crates/adapters/src/bin/adapter-claude.rs`
- `crates/adapters/src/bin/adapter-opencode.rs`
- `crates/node-daemon/src/main.rs`
- `crates/common/src/profile.rs`

## 6. Безопасная установка node — P0

### CLI installer

- [x] Удалить `StrictHostKeyChecking=no`.
- [x] Использовать обычный known_hosts verification по умолчанию.
- [x] Добавить `--accept-new-host-key` как явный opt-in.
- [x] Добавить `--host-key-fingerprint` для strict provisioning.
- [x] Не запускать daemon как root.
- [x] Удалить автоматический `AGENTGRID_ALLOW_ROOT=1`.
- [ ] Создавать пользователя `agentgrid`.
- [ ] Устанавливать systemd unit вместо `nohup`.
- [x] Применить hardening directives из `deploy/install-node.sh`.
- [ ] Загружать необходимые adapter binaries вместе с daemon.
- [ ] Проверять checksum/signature binaries до запуска.
- [x] Создавать временный env/token файл с `0600` атомарно.
- [x] Удалять enrollment token после успешного обмена на credential.
- [ ] Добавить rollback при частичной ошибке установки.
- [ ] Добавить idempotent повторный install/upgrade.
- [ ] Добавить `ag node uninstall` или документированную процедуру.

### systemd hardening

- [x] `NoNewPrivileges=true`.
- [x] `ProtectSystem=strict`.
- [x] `ProtectHome=true`.
- [x] `PrivateTmp=true`.
- [x] `PrivateDevices=true`, если adapters не требуют devices.
- [x] `ProtectKernelTunables=true`.
- [x] `ProtectKernelModules=true`.
- [x] `ProtectControlGroups=true` с учётом cgroup backend.
- [x] `RestrictSUIDSGID=true`.
- [x] `LockPersonality=true`.
- [x] `RestrictAddressFamilies=` по минимально необходимому набору.
- [x] `ReadWritePaths=` только для data/workspace/repository roots.

### Тесты

- [x] Installer не использует root daemon.
- [x] Неизвестный SSH host key приводит к отказу по умолчанию.
- [ ] Повторная установка не создаёт второй daemon.
- [x] Enrollment token отсутствует в unit/env после подключения.
- [ ] После reboot daemon стартует и reconnect работает.

**Основные файлы:**

- `crates/cli/src/main.rs`
- `deploy/install-node.sh`
- `README.md`

---

# Milestone 2 — 0.4.2 Correctness и durable delivery

## 7. Lease/ACK race conditions — P0

- [x] Перенести поиск и отмену expired assignments в одну транзакцию.
- [x] Добавить `status='assigned' AND ack_deadline < ?` непосредственно в UPDATE.
- [x] Проверять `rows_affected() == 1` перед изменением task/node.
- [x] Обновлять task только если `assigned_attempt_id` совпадает.
- [x] Не уменьшать `active_attempts`, если attempt уже ACKed/completed.
- [x] Сделать offline transition compare-and-set по status и heartbeat timestamp.
- [x] Вызывать `lose_node_attempts` только если node действительно переведена в offline.
- [x] Добавить optimistic `version` или equivalent guards для tasks/attempts. (CAS-охраны `WHERE status=…` + `rows_affected==1`; полноценный `version`-столбец отложен — CAS покрывает P0 гонки)
- [x] Проверить гонки cancel ↔ complete.
- [x] Проверить гонки lost ↔ complete.
- [x] Проверить гонки retry ↔ late completion.

### Тесты

- [x] ACK одновременно с lease expiry не создаёт второй attempt.
- [x] Completion одновременно с lease expiry остаётся terminal один раз.
- [x] Fresh heartbeat одновременно с offline sweep не делает node offline.
- [x] Cancel одновременно с complete даёт один детерминированный outcome.
- [x] 100–1000 итераций конкурентного теста проходят без invariant violation.

**Основные файлы:**

- `crates/control-plane/src/store.rs`
- `crates/control-plane/tests/api.rs`

## 8. Fencing tokens — P0

- [x] Добавить migration с `lease_generation` и `fencing_token`.
- [x] Генерировать fencing token при каждом assignment/attempt.
- [x] Передавать token в `Assignment`.
- [x] Сохранять token в локальном attempt state node.
- [x] Требовать token для ACK.
- [x] Требовать token для event ingest.
- [x] Требовать token для completion.
- [x] Требовать token для artifact upload.
- [x] Требовать token для session creation.
- [x] Отклонять stale generation/token как `409 Conflict`.
- [x] Логировать stale writer без принятия payload.
- [x] Версионировать protocol и сохранить N/N-1 policy.

### Тесты

- [x] Старый token после reassignment не может писать events.
- [x] Старый token после lost не может завершить attempt.
- [x] Retry получает новый token.
- [x] Duplicate request с текущим token остаётся идемпотентным.

## 9. Глобальный event cursor и SSE retries — P0

- [ ] Добавить глобальный монотонный `ingest_id` для `task_events`.
- [ ] Сохранить `(attempt_id, attempt_sequence)` как idempotency key.
- [ ] Использовать `ingest_id` в SSE `id:`.
- [ ] Использовать `ingest_id` в `Last-Event-ID` resume.
- [ ] Переименовать API-поля, чтобы отличать global cursor и attempt sequence.
- [ ] Сортировать events по `ingest_id`.
- [ ] Добавить cursor pagination.
- [ ] Обновить web client.
- [ ] Обновить CLI `ag logs --follow`.
- [ ] Обновить TUI.
- [ ] Описать migration поведения старых клиентов.

### Тесты

- [ ] Retry после 500 событий показывает события нового attempt с sequence 1.
- [ ] SSE reconnect между attempts не теряет events.
- [ ] Events разных attempts отображаются в правильном порядке.
- [ ] Duplicate `(attempt_id, attempt_sequence)` не создаёт новую запись.
- [ ] Global cursor монотонен при concurrent ingestion.

**Основные файлы:**

- `crates/control-plane/migrations/`
- `crates/control-plane/src/store.rs`
- `crates/control-plane/src/lib.rs`
- `web/src/api.ts`
- `web/src/components/TaskDetails.tsx`
- `crates/cli/src/main.rs`
- `crates/cli/src/tui.rs`

## 10. Crash-safe outbox — P0

### Выбор реализации

- [ ] Принять ADR: SQLite outbox или crash-safe segmented files.
- [ ] Зафиксировать durability semantics: at-least-once + idempotent CP ingest.
- [ ] Зафиксировать поведение при power loss, disk full и corrupt tail.

### Если SQLite

- [ ] Добавить локальную node SQLite DB.
- [ ] Хранить event payload, attempt, sequence, state и timestamps.
- [ ] Хранить полный `CompleteAttemptRequest`.
- [ ] Хранить pending artifact manifests.
- [ ] Удалять rows только после server ACK.
- [ ] Использовать WAL, `synchronous=FULL` для критичных completion records или обосновать NORMAL.
- [ ] Добавить local DB integrity check/recovery.

### Если segmented files

- [ ] Никогда не truncate durable completion file in-place.
- [ ] Писать временный файл с уникальным именем.
- [ ] `sync_all` перед rename.
- [ ] `fsync` parent directory после rename.
- [ ] Использовать immutable segments + checkpoint вместо полного rewrite на ACK.
- [ ] Терпимо обрабатывать truncated trailing JSON line.
- [ ] Карантинить повреждённые middle records, не теряя остальные.

### Общие задачи

- [ ] Сохранять `plan` в completion outbox.
- [ ] Сохранять `provenance` в completion outbox.
- [ ] Добавить global node spool quota.
- [ ] Добавить per-attempt quota.
- [ ] Добавить high/critical watermarks.
- [ ] При quota pressure сохранять status/error раньше stdout.
- [ ] Эмитить `output_truncated` ровно один раз.
- [ ] Не пытаться записать terminal event в уже полностью заполненный spool без reserved capacity.
- [ ] Добавить metrics: bytes, rows/segments, oldest pending age, corruption count.

### Тесты

- [ ] Kill -9 во время completion record не теряет другие completions.
- [ ] Kill -9 во время ACK compaction не теряет pending events.
- [ ] Partial trailing line восстанавливается/карантинится.
- [ ] Plan/provenance сохраняются при crash до первой доставки.
- [ ] Global quota предотвращает заполнение диска несколькими attempts.
- [ ] Redelivery не создаёт duplicates на CP.

**Основные файлы:**

- `crates/node-daemon/src/outbox.rs`
- `crates/node-daemon/src/main.rs`
- `tests/e2e/run-outbox.sh`
- `tests/e2e/run-disk-full.sh`

## 11. Durable artifacts — P1

- [ ] Добавить artifact spool на node.
- [ ] Записывать artifact metadata и hash до начала upload.
- [ ] Поддержать retry после daemon restart.
- [ ] Не считать completion полностью доставленным, пока обязательные artifacts не ACKed.
- [ ] Определить optional и required artifacts.
- [ ] Добавить completion artifact manifest.
- [ ] Поддержать resumable/chunked upload для больших artifacts либо ограничить размер.
- [ ] Удалять local artifact только после ACK completion.
- [ ] Добавить orphan artifact recovery.
- [ ] Добавить E2E: CP outage во время upload → restart → artifact появился.

## 12. Validation lifecycle — P0/P1

- [ ] Запускать validation через `ExecutionBackend`.
- [ ] Переводить attempt/task в `validating` до запуска.
- [ ] Передавать общий absolute deadline задачи.
- [ ] Добавить отдельный validation timeout.
- [ ] Реагировать на cancel во время validation.
- [ ] Создавать process group/cgroup для validation.
- [ ] Убивать всё дерево процессов.
- [ ] Применять sandbox policy.
- [ ] Применять resource limits.
- [ ] Ограничить stdout/stderr bytes.
- [ ] Обрабатывать invalid UTF-8 без остановки чтения.
- [ ] Различать `validation_failed`, `validation_timeout`, `validation_cancelled`, `validation_infrastructure_failed`.
- [ ] Не собирать command через `format!("{command} 2>&1")`, если можно передать structured argv.
- [ ] Для shell validation явно маркировать trusted shell command.

### Тесты

- [ ] Cancel во время validation завершает process tree.
- [ ] Validation timeout не оставляет subprocess.
- [ ] Forking validation не оставляет orphan.
- [ ] Огромная строка без newline не вызывает unbounded RAM.
- [ ] Invalid UTF-8 сохраняется как lossy/binary output.
- [ ] Validation failure никогда не даёт task `succeeded`.

**Основные файлы:**

- `crates/node-daemon/src/main.rs`
- `crates/adapters/src/backend.rs`
- `crates/common/src/state_machine.rs`

## 13. State-machine enforcement — P1

- [ ] Удалить `.unwrap_or(Succeeded/Failed/Cancelled)` из transition paths.
- [ ] Возвращать typed `InvalidTransition`.
- [ ] Маппить invalid transition на `409 Conflict`.
- [ ] Не изменять task/attempt при invalid transition.
- [ ] Добавить audit event с source state/event.
- [ ] Отделить legacy compatibility transitions от основного автомата.
- [ ] Проверить terminal idempotency явно до transition.
- [ ] Добавить invariants: один active attempt на task.
- [ ] Добавить invariants: terminal task не имеет active attempt.
- [ ] Добавить invariants: `finished_at` согласован со status.
- [ ] Добавить invariants: `assigned_attempt_id` указывает на тот же task.
- [ ] Добавить property-based tests для state machine.

## 14. Event ingestion hardening — P1

- [ ] Не принимать events для succeeded/failed/cancelled/lost attempt.
- [ ] Проверять fencing token и node ownership.
- [ ] Ограничить количество events в одном batch.
- [ ] Ограничить суммарный размер batch, а не только каждый payload.
- [ ] Проверять последовательность и обнаруживать gaps.
- [ ] Возвращать `highest_contiguous_sequence` в ACK.
- [ ] Определить обработку out-of-order batches.
- [ ] Добавить server-side rate limiting per node.
- [ ] Добавить metrics duplicate/gap/stale/rejected.

## 15. Storage retention и quotas — P1

- [ ] Удалять artifact files вместе с metadata.
- [ ] Удалять пустые attempt directories.
- [ ] Сканировать orphan files без metadata.
- [ ] Сканировать metadata без файлов.
- [ ] Добавить artifact storage quota.
- [ ] Добавить repository cache quota.
- [ ] Добавить workspace quota.
- [ ] Проверять free bytes и free inodes.
- [ ] Добавить high/critical watermark behavior.
- [ ] При critical watermark запрещать новые assignments.
- [ ] Добавить `ag storage gc --dry-run`.
- [ ] Добавить metrics cleanup duration/failures/freed bytes.

---

# Milestone 3 — 0.4.3 Architecture и maintainability

## 16. Декомпозиция control plane — P2

- [ ] Создать `app.rs`/`router.rs`.
- [ ] Вынести config и env validation в `config.rs`.
- [ ] Вынести auth middleware/JWT/setup/login.
- [ ] Вынести task routes.
- [ ] Вынести node/attempt routes.
- [ ] Вынести artifact routes.
- [ ] Вынести workflow routes.
- [ ] Вынести approval routes.
- [ ] Вынести profile/skills/MCP routes.
- [ ] Вынести static serving.
- [ ] Вынести TLS listener.
- [ ] Вынести maintenance tasks.
- [ ] Установить ориентир: production module менее 800–1000 строк.
- [ ] Оставить handlers тонкими: auth → validate → service → response.

## 17. Декомпозиция node daemon — P2

- [ ] `config.rs`.
- [ ] `enrollment.rs`.
- [ ] `heartbeat.rs`.
- [ ] `polling.rs`.
- [ ] `attempt_runner.rs`.
- [ ] `validation.rs`.
- [ ] `event_sink.rs`.
- [ ] `completion.rs`.
- [ ] `process_supervisor.rs`.
- [ ] `artifact_spool.rs`.
- [ ] `capabilities.rs`.
- [ ] `recovery.rs`.
- [ ] `profiles.rs`.
- [ ] `mcp.rs`.
- [ ] `skills.rs`.
- [ ] Перенести unit tests из giant `main.rs` рядом с соответствующими модулями.

## 18. Store/service separation — P2

- [ ] Разделить store на users/nodes/tasks/attempts/events/artifacts/conversations/maintenance.
- [ ] Оставить SQL-only обязанности в repository layer.
- [ ] Вынести scheduler в service layer.
- [ ] Вынести attempt lifecycle в service layer.
- [ ] Вынести artifact authorization/storage в service layer.
- [ ] Запретить handler напрямую координировать несколько store calls без transaction boundary.
- [ ] Добавить transaction helper для multi-aggregate operations.

## 19. Typed API errors — P2

- [ ] Ввести общий `ApiError`.
- [ ] Добавить стабильные machine-readable codes.
- [ ] Добавить `request_id`.
- [ ] Не возвращать пустые списки при DB errors.
- [ ] Возвращать `503` при storage outage.
- [ ] Не возвращать raw internal error клиенту.
- [ ] Включать internal error chain только в structured logs.
- [ ] Добавить единый JSON error schema.
- [ ] Задокументировать codes в OpenAPI.

## 20. Pagination и API consistency — P2

- [ ] Cursor pagination для tasks.
- [ ] Cursor pagination для events.
- [ ] Cursor pagination для workflow runs.
- [ ] Cursor pagination для conversations/messages.
- [ ] Cursor pagination для approvals/audit.
- [ ] Server-side maximum limit.
- [ ] Filters: status/repository/node/created range.
- [ ] Единый response envelope для list endpoints.
- [ ] Версионированный OpenAPI 3.1 document.
- [ ] Contract tests между Rust DTO и TypeScript client.

## 21. Database integrity — P1/P2

- [ ] Добавить FK `attempts.task_id → tasks.id`.
- [ ] Добавить FK `attempts.node_id → nodes.id`.
- [ ] Добавить FK `task_events.attempt_id → attempts.id`.
- [ ] Добавить FK `artifacts.attempt_id → attempts.id`.
- [ ] Добавить FK для `node_repositories`.
- [ ] Добавить FK для approvals.
- [ ] Добавить FK для workflow tables.
- [ ] Определить `ON DELETE` policy для каждой связи.
- [ ] Добавить CHECK constraints для всех status/autonomy/role полей.
- [ ] Добавить уникальный `(conversation_id, seq)`.
- [ ] Выделять conversation sequence атомарно.
- [ ] Добавить migration preflight для orphan rows.
- [ ] Добавить baseline schema для новых установок, сохранив upgrade migrations.

## 22. `active_attempts` reconciliation — P1

- [ ] Решить: вычисляемое значение или денормализованный cache.
- [ ] Если cache — добавить периодический reconciliation.
- [ ] Добавить drift metric.
- [ ] Запускать reconciliation после startup recovery.
- [ ] Проверять count после lease expiry, lost, cancel, complete и retry.
- [ ] Добавить invariant test после всех lifecycle сценариев.

## 23. Feature maturity и scope control — P2

- [ ] Определить stable core: tasks/nodes/attempts/events/Git/adapters/artifacts.
- [ ] Пометить workflows как experimental до отдельного gate.
- [ ] Пометить ACP gateway как experimental.
- [ ] Пометить Telegram gateway как experimental.
- [ ] Пометить skills/profiles/MCP как experimental.
- [ ] Пометить schedules/plan expansion/Zeroshot/context provider как experimental.
- [ ] Ввести Cargo feature flags или отдельные binaries/packages.
- [ ] Не включать experimental компоненты в minimal release по умолчанию.
- [ ] Для каждой новой функции требовать ADR, threat-model delta и removal plan.
- [ ] Удалить внутренние `Stage X / line Y` комментарии из production code.

---

# Milestone 4 — 0.5 Execution isolation

## 24. Настоящий Sandbox trait — P1/P2

- [ ] Ввести trait `Sandbox`/`ExecutionBackend` с probe/spawn/terminate/collect.
- [ ] Отделить agent adapter от execution backend.
- [ ] Сделать capability report фактическим, а не декларативным.
- [ ] Не заявлять resource limit enforced, если backend его не применил.
- [ ] Добавить conformance suite для каждого backend.

## 25. Docker/Podman backend — P1

- [ ] Не объединять `docker|podman` в один hardcoded `docker` binary.
- [ ] Проверять runtime version и capability.
- [ ] Pin image по digest.
- [ ] Убедиться, что adapter и agent CLI реально существуют в image.
- [ ] Передавать только allowlisted env через `--env`/env-file.
- [ ] `--network none` по умолчанию.
- [ ] `--cap-drop=ALL`.
- [ ] `--security-opt=no-new-privileges`.
- [ ] `--pids-limit`.
- [ ] `--memory`.
- [ ] `--cpus`.
- [ ] `--read-only` root filesystem.
- [ ] tmpfs для `/tmp`.
- [ ] Worktree mount с минимально необходимыми правами.
- [ ] Отдельный artifact/output mount.
- [ ] Не монтировать Docker socket, host home, SSH agent и credentials.
- [ ] Добавить network allowlist mode после `none`.
- [ ] Удалять orphan containers после daemon crash.

### Тесты

- [ ] Sandbox smoke test запускает adapter.
- [ ] API key попадает в container только при allowlist.
- [ ] Agent не видит host home.
- [ ] Agent не видит sibling worktrees.
- [ ] Network disabled действительно блокирует egress.
- [ ] Memory/PID/CPU limits реально срабатывают.
- [ ] Cancel удаляет container и descendants.
- [ ] Daemon restart очищает orphan container.

## 26. systemd/cgroup backend — P2

- [ ] Реализовать transient scope/unit.
- [ ] `MemoryMax`.
- [ ] `CPUQuota`.
- [ ] `TasksMax`.
- [ ] Accounting CPU/memory/IO.
- [ ] Определять OOM/resource limit outcome.
- [ ] Завершать весь cgroup при cancel.
- [ ] Fallback process backend маркировать как unenforced.

## 27. Network и secret policies — P1/P2

- [ ] Task-level network mode: `none|restricted|unrestricted`.
- [ ] Node policy задаёт максимальный разрешённый режим.
- [ ] Блокировать metadata endpoints.
- [ ] Ограничивать LAN/private ranges в restricted mode.
- [ ] Добавить egress audit.
- [ ] Ввести task-scoped secret allowlist.
- [ ] Не передавать весь daemon environment subprocess.
- [ ] Рассмотреть credential broker/short-lived tokens.
- [ ] Добавить streaming redactor с chunk overlap.
- [ ] Добавить минимальную длину redactable secret.
- [ ] Добавить encoded variants для критичных secrets.

---

# Milestone 5 — CI, release и supply chain

## 28. CI coverage — P1/P2

- [ ] PR: fmt/clippy/unit/integration/web build.
- [ ] PR: основной process E2E.
- [ ] Nightly: compose E2E.
- [ ] Nightly: outbox kill-9.
- [ ] Nightly: CP restart.
- [ ] Nightly: disk full.
- [ ] Nightly: slow network.
- [ ] Nightly: workflow E2E.
- [ ] Nightly: skill bundle.
- [ ] Physical runner: real two-host E2E.
- [ ] Добавить race/concurrency stress job.
- [ ] Добавить sanitizer/Miri там, где применимо.
- [ ] Добавить code coverage trend.
- [ ] Проверять migration from previous released DB snapshot.

## 29. Supply chain — P2

- [ ] Закрепить GitHub Actions по commit SHA.
- [ ] Настроить Renovate/Dependabot для обновления SHA.
- [ ] `cargo audit`.
- [ ] `cargo deny`.
- [ ] License allowlist.
- [ ] Secret scanning.
- [ ] CodeQL.
- [ ] SBOM CycloneDX/SPDX.
- [ ] GitHub build provenance attestation.
- [ ] Подписывать releases cosign/minisign.
- [ ] Публиковать SHA256 для каждого binary.
- [ ] Документировать reproducibility limitations.

## 30. Полноценный release workflow — P2

- [ ] Запускать tests до release build.
- [ ] Создавать GitHub Release по tag.
- [ ] Прикладывать changelog section.
- [ ] Публиковать `agentgrid-control-plane`.
- [ ] Публиковать `agentgrid-node-daemon`.
- [ ] Публиковать `ag`.
- [ ] Публиковать `adapter-mock`.
- [ ] Публиковать `adapter-claude`.
- [ ] Публиковать `adapter-opencode`.
- [ ] Публиковать `agentgrid-gateway`, если он входит в release.
- [ ] Публиковать `agentgrid-acp-agent`, если он входит в release.
- [ ] Публиковать web bundle/version manifest.
- [ ] Проверять checksums всех опубликованных файлов.
- [ ] Добавить install/upgrade/rollback smoke test.

## 31. Docker build и images — P2

- [ ] Добавить BuildKit cache mounts или cargo-chef.
- [ ] Кэшировать Rust dependencies отдельно от source.
- [ ] Кэшировать npm dependencies отдельно от web source.
- [ ] Добавить OCI labels/version/revision/source.
- [ ] Добавить image healthcheck.
- [ ] Добавить non-root verification test.
- [ ] Добавить read-only/cap-drop security settings.
- [ ] Выпускать base node image.
- [ ] Выпускать отдельные images с Claude/OpenCode runtimes либо документировать custom image.
- [ ] Pin base images по digest для releases.
- [ ] Сканировать images на CVE.

---

# Milestone 6 — Git, производительность и эксплуатация

## 32. Git correctness — P1/P2

- [ ] Fail closed при отсутствующем upstream commit/patch по умолчанию.
- [ ] Добавить explicit workflow policy `allow_missing_upstream`.
- [ ] Сохранять exact `resolved_base_sha` для каждого attempt.
- [ ] Сохранять `remote_head_at_start` и `remote_head_at_finish`.
- [ ] Показывать stale-base warning.
- [ ] Получать binary diff как bytes, без `String::from_utf8_lossy`.
- [ ] Добавить cross-process repository `flock`.
- [ ] Добавить timeout ожидания repo lock.
- [ ] Добавить diagnostics/stale lock recovery.
- [ ] Проверять clone/fetch URL scheme и policy.
- [ ] Определить SSH credential policy.
- [ ] Обнаруживать submodules и Git LFS.
- [ ] Не запускать task на неполностью подготовленном repository.
- [ ] Добавить repository cache size/GC policy.

## 33. Safe workspace cleanup — P1

- [ ] Валидировать cleanup target независимо от caller.
- [ ] Canonicalize workspace root.
- [ ] Target должен быть прямым child root.
- [ ] Проверять attempt ID format.
- [ ] Не следовать symlink.
- [ ] Не выполнять `remove_dir_all` за пределами root даже при corrupt state.
- [ ] Добавить quarantine для неизвестных stale directories.
- [ ] Добавить `ag node doctor --repair-worktrees`.
- [ ] Добавить cleanup metrics.

## 34. Output backpressure — P1

- [ ] Читать stdout/stderr bounded chunks, а не unbounded lines.
- [ ] Ограничить logical line size.
- [ ] Продолжать drain pipe после truncation, чтобы subprocess не заблокировался.
- [ ] Добавить per-stream и total budgets.
- [ ] Резервировать место для terminal/status events.
- [ ] Добавить `output_truncated` metadata: bytes dropped/range.
- [ ] Не хранить весь pending spool в RAM при отправке.
- [ ] Отправлять ограниченные batches.
- [ ] Оптимизировать ACK без `acked.contains` O(n×m).
- [ ] Добавить load test с длинной строкой и десятками MB output.

## 35. Observability — P2

- [ ] Cross-node authorization rejection count.
- [ ] Stale fencing token count.
- [ ] Lease expiry/ACK race prevention count.
- [ ] Event duplicate/gap/rejection counts.
- [ ] Outbox bytes и oldest age.
- [ ] Artifact spool bytes и retry count.
- [ ] Artifact cleanup bytes/failures.
- [ ] Active-attempt drift.
- [ ] Repository lock wait.
- [ ] Validation duration/outcomes.
- [ ] Sandbox backend и enforced limits labels.
- [ ] Security profile label в attempt metrics.
- [ ] Request ID во всех logs.
- [ ] Optional OpenTelemetry feature без включения тяжёлого exporter по умолчанию.

---

# Milestone 7 — Web UI, CLI и документация

## 36. Web UI — P2/P3

- [ ] Показывать security profile каждого attempt.
- [ ] Показывать sandbox backend и реально enforced limits.
- [ ] Показывать network mode.
- [ ] Показывать unsafe wrapper warning.
- [ ] Отображать global event cursor корректно после retry.
- [ ] Разделять events по attempts.
- [ ] Показывать artifact integrity hash.
- [ ] Скачивать активные artifacts как attachment.
- [ ] Добавить pagination для длинных списков.
- [ ] Добавить error states вместо пустых таблиц при API failure.
- [ ] Добавить component/API tests.
- [ ] Добавить CSP и security headers.

## 37. CLI/TUI — P2/P3

- [ ] `ag task explain` с eligibility reasons.
- [ ] `ag node doctor`.
- [ ] `ag storage gc --dry-run`.
- [ ] `ag node drain`.
- [ ] `ag node uninstall`/upgrade workflow.
- [ ] `--json` для всех read commands.
- [ ] Стабильные exit codes.
- [ ] Отображать attempts отдельно в logs.
- [ ] Поддержать новый global cursor.
- [ ] Не печатать secrets/enrollment tokens после использования.
- [ ] Убрать дублированные комментарии в `cli/src/main.rs`.

## 38. README и docs — P2

- [ ] Обновить README по реальному feature scope.
- [ ] Добавить maturity matrix: stable/beta/experimental/prototype.
- [ ] Явно написать: worktree не является security sandbox.
- [ ] Явно написать unsafe behavior wrapper adapters.
- [ ] Разделить demo и production quickstart.
- [ ] Документировать node trust/ownership model.
- [ ] Документировать event delivery semantics.
- [ ] Документировать fencing tokens.
- [ ] Документировать artifact retention и quotas.
- [ ] Документировать backup/restore.
- [ ] Документировать upgrade/rollback.
- [ ] Добавить `SECURITY.md` с vulnerability reporting.
- [ ] Добавить compatibility matrix.
- [ ] Добавить API/OpenAPI docs.

## 39. Changelog cleanup — P2

- [ ] Перевести changelog на Keep a Changelog style.
- [ ] Оставлять только Added/Changed/Fixed/Security/Breaking/Known limitations.
- [ ] Убрать internal Stage/line references.
- [ ] Перенести implementation journal в issues или development notes.
- [ ] Не ставить версии ретроспективно без пояснения.
- [ ] Добавлять отдельный `Security` section для P0 fixes.

## 40. Naming/branding decision — P3

- [ ] Проверить конфликт названия AgentGrid.
- [ ] Проверить GitHub/package/domain availability.
- [ ] Принять решение до стабильного публичного релиза.
- [ ] Если переименование принято — сделать migration plan для binaries/env/config/data dirs.

---

# Regression test matrix

## Auth и ownership

- [ ] DB auth failure fail-closed.
- [ ] Bootstrap takeover невозможен.
- [ ] Cross-node ACK запрещён.
- [ ] Cross-node events запрещены.
- [ ] Cross-node completion запрещён.
- [ ] Cross-node artifacts запрещены.
- [ ] Revoked node полностью заблокирована.

## Distributed races

- [ ] ACK ↔ lease expiry.
- [ ] Heartbeat ↔ offline sweep.
- [ ] Complete ↔ lost.
- [ ] Complete ↔ cancel.
- [ ] Retry ↔ late completion.
- [ ] Concurrent pollers не получают одну task дважды.
- [ ] Stale fencing token не изменяет state.

## Durability

- [ ] Kill -9 при event append.
- [ ] Kill -9 при ACK compaction.
- [ ] Kill -9 при completion record.
- [ ] Kill -9 при artifact upload.
- [ ] CP outage во время attempt.
- [ ] CP outage во время completion.
- [ ] CP outage во время artifact upload.
- [ ] Disk full до reserved terminal capacity.
- [ ] Corrupt trailing outbox record.
- [ ] Restart CP и node одновременно.

## Events/SSE

- [ ] Retry sequence restart не теряет events.
- [ ] SSE reconnect без gaps/duplicates.
- [ ] Concurrent attempts корректно упорядочены.
- [ ] Huge event batch отклоняется.
- [ ] Events после terminal state отклоняются.

## Execution

- [ ] Agent timeout.
- [ ] Agent cancellation.
- [ ] Validation timeout.
- [ ] Validation cancellation.
- [ ] Forking child cleanup.
- [ ] Adapter crash mid-frame.
- [ ] Огромная строка без newline.
- [ ] Invalid UTF-8.
- [ ] Resource-limit outcome.
- [ ] Sandbox network denial.

## Git/workspaces

- [ ] Parallel attempts одного repo.
- [ ] Два daemon process с одним repo root.
- [ ] Base SHA pinning.
- [ ] Missing upstream fail-closed.
- [ ] Conflicting upstream patch.
- [ ] Binary patch round-trip.
- [ ] Symlink cleanup escape запрещён.
- [ ] Logs не попадают в commit/patch.

## Artifacts

- [ ] Hash verification.
- [ ] Binary round-trip.
- [ ] Stored-XSS blocked.
- [ ] Traversal blocked.
- [ ] Symlink escape blocked.
- [ ] Retention удаляет metadata и file.
- [ ] Orphan reconciliation.
- [ ] Durable retry после restart.

---

# Definition of Done для hardening-цикла

- [ ] Все P0 закрыты и имеют regression-тесты.
- [ ] Cross-node mutation/read невозможны по архитектуре и подтверждены тестами.
- [ ] Lease/offline transitions используют compare-and-set или fencing.
- [ ] Retry не ломает SSE/event history.
- [ ] Kill -9 не теряет pending completion и обязательные artifacts.
- [ ] Cancel/timeout завершают agent и validation process trees.
- [ ] Default adapter path не отключает permissions без явного unsafe opt-in.
- [ ] Production node не запускается как root.
- [ ] Production compose не содержит стандартных credentials.
- [ ] Sandbox capability соответствует реально применённой изоляции.
- [ ] Storage имеет retention, global quotas и disk-pressure behavior.
- [ ] Giant production modules декомпозированы до обозримых границ.
- [ ] API ошибки не маскируются пустыми успешными responses.
- [ ] Release содержит полный набор заявленных binaries, checksums, SBOM и signatures.
- [ ] README, threat model, changelog и maturity matrix соответствуют коду.
- [ ] Полный CI/release gate зелёный.

## Рекомендуемый порядок выполнения

1. [ ] Node ownership.
2. [ ] Auth fail-closed и bootstrap.
3. [ ] Lease/offline races.
4. [ ] Fencing tokens.
5. [ ] Global event cursor.
6. [ ] Crash-safe outbox.
7. [ ] Durable artifacts.
8. [ ] Validation lifecycle.
9. [ ] Unsafe adapter defaults.
10. [ ] Safe node installer.
11. [ ] Artifact/static security.
12. [ ] Retention/quotas/backpressure.
13. [ ] Database constraints и reconciliation.
14. [ ] Декомпозиция модулей.
15. [ ] Настоящий sandbox backend.
16. [ ] CI/release/supply-chain hardening.
17. [ ] UI/CLI/docs stabilization.
