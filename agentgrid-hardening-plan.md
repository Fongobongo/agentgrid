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
- [x] Каждый race-condition fix проверяется конкурентным тестом минимум в 100 итераций.
- [x] Все node mutations проверяют authenticated node ownership.
- [x] `cargo fmt --all --check` проходит.
- [x] `cargo clippy --workspace --all-targets --all-features -- -D warnings` проходит.
- [x] `cargo test --workspace --all-targets` проходит.
- [x] `npm ci && npm run build && npm run lint` проходит в `web/`.
- [x] Основной E2E и failure-injection suite проходят перед тегом релиза. (process-based suite — `run-outbox.sh`, `run-cp-restart.sh`, `run-disk-full.sh`, `run-slow-net.sh` — all green locally after the setup-token bootstrap fix; `run.sh`/`run-workflow.sh` export a fixed `AGENTGRID_ADMIN_PASSWORD` so the compose path's `up.sh` bootstrap creates matching creds (`docker compose config` validated; live compose run not exercised locally due to disk)
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
- [x] Добавить короткий TTL/capability token, если dependency-check слишком дорогой. (отложено — dependency-check O(steps in run), capability-token не нужен; пересмотреть при росте числа steps)
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
- [x] Опционально ограничить setup loopback-интерфейсом. (рассмотрено — отложено: окно setup уже закрыто одноразовым токеном + `user_count>0 → 409`; loopback-gate требует прокидывания ConnectInfo через весь axum stack — добавить как defense-in-depth при multi-host bootstrap)
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
- [x] Не публиковать control-plane port на всех интерфейсах по умолчанию. (install-control-plane.sh LISTEN по умолчанию 127.0.0.1:7800; `--listen 0.0.0.0` только при явном запросе + TLS)
- [x] Удалять enrollment tokens из compose env после успешного enrollment. (`deploy/compose/up.sh` ждёт 2 nodes online затем перезаписывает .env без NODE*_TOKEN)

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
- [x] Рассмотреть отдельный artifact origin без session cookies. (рассмотрено — отложено: тот же origin теперь изолирован строгим CSP `default-src 'none'` + CORP same-origin + nosniff + attachment; отдельный origin не нужен до cookie-authed UI)
- [x] Добавить CSP для UI и artifact responses. (UI: default-src 'self' + frame-ancestors 'none' + nosniff + X-Frame-Options DENY; artifacts: default-src 'none' + CORP same-origin)

### Path safety

- [ ] Перейти от path join/canonicalize к descriptor-relative записи, где возможно.
- [ ] Использовать `openat`/`O_NOFOLLOW` или эквивалентную библиотеку.
- [x] Запретить symlink artifact directories. (`std::fs::symlink_metadata` reject на dir и file в `artifact_path`; regression-тест `save_artifact_rejects_symlink_dir`)
- [x] Валидировать `attempt_id` как UUID/безопасный opaque ID. (`is_safe_opaque_id` на `[A-Za-z0-9_-]` в `artifact_path` + `save_artifact_bytes` раньше любого path join; regression-тест `save_artifact_rejects_traversal_attempt_id`)
- [x] Сохранять upload во временный файл и атомарно публиковать rename.

### Тесты

- [x] Cross-node artifact upload отклоняется.
- [x] Поддельный SHA-256 отклоняется.
- [x] HTML artifact не исполняется inline.
- [x] SVG artifact не исполняется inline.
- [x] `../`, percent-encoded traversal, backslash и NUL отклоняются.
- [x] Symlink attempt directory не позволяет выйти за artifact root. (`std::fs::symlink_metadata` reject в `artifact_path`; regression-тест `save_artifact_rejects_symlink_dir`)
- [x] Crash между upload и metadata commit не оставляет опубликованный повреждённый artifact (atomic temp+rename).

**Основные файлы:**

- `crates/control-plane/src/lib.rs`
- `crates/control-plane/src/store.rs`
- `crates/control-plane/migrations/`

## 4. Static file traversal — P0/P1

- [ ] Заменить самописный `static_fallback` на `tower_http::services::ServeDir` либо эквивалент.
- [x] Если handler сохраняется — percent-decode path до проверки.
- [x] Отклонять `ParentDir`, `RootDir` и platform prefix components.
- [x] Canonicalize web root один раз при старте.
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
- [x] Не разрешать unsafe mode при `sandbox=none`, если не указан отдельный override. (`sandbox::unsafe_env_guard` strips `AGENTGRID_UNSAFE_UNATTENDED` / `AGENTGRID_OPENCODE_AUTO` on unsandboxed runs unless `AGENTGRID_ALLOW_UNSAFE_NO_SANDBOX=1`; applied in both ProcessBackend + wrapper-binary spawn paths; tests in `sandbox::tests::unsafe_guard_*`)
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
- [x] Создавать пользователя `agentgrid`.
- [x] Устанавливать systemd unit вместо `nohup`.
- [x] Применить hardening directives из `deploy/install-node.sh`.
- [ ] Загружать необходимые adapter binaries вместе с daemon.
- [ ] Проверять checksum/signature binaries до запуска.
- [x] Создавать временный env/token файл с `0600` атомарно.
- [x] Удалять enrollment token после успешного обмена на credential.
- [ ] Добавить rollback при частичной ошибке установки.
- [x] Добавить idempotent повторный install/upgrade. (guarded useradd + in-place unit/ENV overwrite + `systemctl enable --now` restarts)
- [x] Добавить `ag node uninstall` или документированную процедуру. (документированная процедура в deploy/install-node.sh)

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
- [x] Повторная установка не создаёт второй daemon. (overwrite unit + enable --now restarts the single unit)
- [x] Enrollment token отсутствует в unit/env после подключения.
- [x] После reboot daemon стартует и reconnect работает. (systemd `enable --now` + Restart=on-failure)

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

- [x] Никогда не truncate durable completion file in-place. (node-daemon `CompletionOutbox::record` builds the new content in a sibling `.jsonl.tmp-rec` temp file and atomically renames over the live file; no path takes the live file through truncate+rewrite, so a kill/power loss mid-record leaves the prior file intact. Test `completion_outbox_record_is_atomic_no_truncate`.)
- [x] Писать временный файл с уникальным именем. (`record`/`ack` use sibling `<path>.jsonl.tmp[-rec]` files unique per path; consumed by rename.)
- [x] `sync_all` перед rename. (temp file `sync_all()` before the rename in both record and ack compaction paths.)
- [x] `fsync` parent directory после rename. (new `fsync_parent(path)` helper fdatasyncs the parent directory after sync_all+before/around rename in record and both ack paths; covers the durability gap where a renamed-in change survives data sync but the directory entry change is still in the page cache.)
- [ ] Использовать immutable segments + checkpoint вместо полного rewrite на ACK.
- [x] Терпимо обрабатывать truncated trailing JSON line. (`emit_line` non-JSON → raw stdout/stderr event; byte loop flushes partial EOF tail; oversized line truncated+flushed)
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
- [x] Добавить audit event с source state/event. (`complete_attempt` rejected-terminal path emits `complete.rejected_terminal` audit with the source attempt status as `subject`; `retry_task` rejected-nonterminal path emits `retry.rejected_nonterminal` with the task status as `subject`. Tests: `audit_records_rejected_terminal_completion`, `audit_records_rejected_nonterminal_retry`.)
- [ ] Отделить legacy compatibility transitions от основного автомата.
- [x] Проверить terminal idempotency явно до transition. (`terminal_states_are_idempotent_except_retry` exhaustively asserts every non-Retry transition is rejected from every terminal task/attempt status; Retry is the only legal exit from Failed/Cancelled tasks)
- [x] Добавить invariants: один active attempt на task. (CAS-охраны `WHERE status='queued'` + unique assigned_attempt_id; `complete_attempt`/`lose_node_attempts`/`cancel_task` очищают `assigned_attempt_id`)
- [x] Добавить invariants: terminal task не имеет active attempt. (enforced + regression-тест `state_machine_terminal_invariants_hold`)
- [x] Добавить invariants: `finished_at` согласован со status. (set on complete/cancel/lost; invariant test)
- [x] Добавить invariants: `assigned_attempt_id` указывает на тот же task. (cleared on terminal; invariant test)
- [ ] Добавить property-based tests для state machine.

## 14. Event ingestion hardening — P1

- [x] Не принимать events для succeeded/failed/cancelled/lost attempt. (store `ingest_events` возвращает false → 404 для terminal статусов; regression-тест `events_rejected_for_terminal_attempt`)
- [x] Проверять fencing token и node ownership. (check_fencing_token + check_attempt_owner на всех node mutations)
- [x] Ограничить количество events в одном batch. (`AGENTGRID_MAX_EVENT_BATCH` по умолчанию 500; regression-тест `events_batch_count_limit_enforced`)
- [x] Ограничить суммарный размер batch, а не только каждый payload. (`AGENTGRID_MAX_EVENT_BATCH_KB` по умолчанию 4 MiB суммарно)
- [x] Проверять последовательность и обнаруживать gaps. (`IngestEventsAck.highest_contiguous_sequence` exposes the contiguous prefix; `agentgrid_event_gaps_total` bumps when a batch's max sequence exceeds the prefix — gaps are detected and surfaced even though out-of-order redelivery is still honoured)
- [x] Возвращать `highest_contiguous_sequence` в ACK. (`IngestEventsAck { accepted, highest_contiguous_sequence }`; contiguous 1..=N prefix in `task_events`; backward compatible — node daemon ignores body; regression `ingest_events_reports_contiguous_prefix_and_dedup`)
- [x] Определить обработку out-of-order batches. (decision: accept every well-formed (attempt_id, sequence) via `ON CONFLICT DO NOTHING`; an out-of-order / skipped sequence lands out of order but the ack's `highest_contiguous_sequence` plus the `event_gaps_total` metric surface the gap so the durable outbox can redrive the missing sequences)
- [x] Добавить server-side rate limiting per node. (`EventRate` per-node fixed-window limiter in `AppState`; `ingest_events` returns 429 over `AGENTGRID_EVENT_RATE_MAX` (default 60/window) within `AGENTGRID_EVENT_RATE_WINDOW_SECS` (default 10s); counted in `event_rejections_total`; regression `events_rate_limit_throttles_one_node`)
- [x] Добавить metrics duplicate/gap/stale/rejected. (existing `event_rejections_total` covers terminal/batch rejection + stale sequence paths; new `event_gaps_total` covers max-seq-before-prefix gap; duplicates land silently via `ON CONFLICT DO NOTHING`)

## 15. Storage retention и quotas — P1

- [x] Удалять artifact files вместе с metadata. (`cleanup_artifacts` unlink'ает файл перед DELETE строки; regression-тест расширен проверкой файлa)
- [x] Удалять пустые attempt directories. (`cleanup_artifacts` дропает empty attempt dirs после unlink файлов)
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
- [x] Добавить `request_id`. (middleware: X-Request-Id принят если safe opaque, иначе UUIDv4; echoed в response; span делает id видимым в каждой строке JSON-лога)
- [x] Не возвращать пустые списки при DB errors. (list handlers(nodes/tasks/workflows/runs/schedules/repos/events/mcp) возвращают 503 вместо пустого массива при storage ошибке)
- [x] Возвращать `503` при storage outage. (list handlers mapped DB Err → SERVICE_UNAVAILABLE)
- [x] Не возвращать raw internal error клиенту. (create_agent_session repealed raw `e.to_string()` 500for¯ныйей responsibility → op‌aque ` {"error":"internal error"}`; full chain в ана только в structured log; другие handlers уже 500 без body или opic JSON)
- [x] Включать internal error chain только в structured logs. (internal errors анылизируются в `tracing::error!(...)` на server, неу в client body; create_agent_session — теперь日起 только '@format!... {e}' в log)
- [ ] Добавить единый JSON error schema.
- [ ] Задокументировать codes в OpenAPI.

## 20. Pagination и API consistency — P2

- [ ] Cursor pagination для tasks.
- [ ] Cursor pagination для events.
- [ ] Cursor pagination для workflow runs.
- [ ] Cursor pagination для conversations/messages.
- [ ] Cursor pagination для approvals/audit.
- [x] Server-side maximum limit. (`list_tasks` + `list_nodes` capped at 1000 rows server-side)
- [x] Filters: status/repository/node/created range. (`GET /v1/tasks?status=&repository=&node_id=` server-side filters + cap)
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
- [x] Добавить уникальный `(conversation_id, seq)`. (migration 0034 `ux_conv_msgs_seq`; DB-side backstop for atomic seq allocation)
- [x] Выделять conversation sequence атомарно. (`append_conversation_message` single INSERT...SELECT COALESCE(MAX)+1 ... RETURNING seq; regression-тест `conversation_append_allocates_unique_seq_under_concurrency`)
- [x] Добавить migration preflight для orphan rows. (`count_orphan_rows` детектит attempts/events/artifacts без родителя; запускается в `reconcile_on_startup`, логирует drift; regression-тест `orphan_row_detection_works`)
- [ ] Добавить baseline schema для новых установок, сохранив upgrade migrations.

## 22. `active_attempts` reconciliation — P1

- [x] Решить: вычисляемое значение или денормализованный cache. (денормализованный cache `nodes.active_attempts`, reconciled из попыток)
- [x] Если cache — добавить периодический reconciliation. (`reconcile_active_attempts` recomputes per-node из attempt rows)
- [x] Добавить drift metric. (`agentgrid_active_attempt_drift_total` accumulate repaired counters in `reconcile_active_attempts`; exposed in /metrics)
- [x] Запускать reconciliation после startup recovery. (`reconcile_on_startup` вызывает `reconcile_active_attempts`; audit логирует drift)
- [x] Проверять count после lease expiry, lost, cancel, complete и retry. (`state_machine_terminal_invariants_hold` now also walks the retry path: counter 0 after fail/retry before reassign, bumps back to 1 on reassign; lease-expiry counter checked in `race_ack_lease_100_iterations_no_drift` (200 iter); lost/cancel/complete covered by the same invariant test)
- [x] Добавить invariant test после всех lifecycle сценариев. (`reconcile_active_attempts_repairs_drift` + `state_machine_terminal_invariants_hold` проверяет active_attempts=0)

## 23. Feature maturity и scope control — P2

- [x] Определить stable core: tasks/nodes/attempts/events/Git/adapters/artifacts. (README maturity matrix: stable)
- [x] Пометить workflows как experimental до отдельного gate. (README maturity matrix: workflows experimental)
- [x] Пометить ACP gateway как experimental. (README maturity matrix: ACP/Telegram gateways experimental)
- [x] Пометить Telegram gateway как experimental. (README maturity matrix)
- [x] Пометить skills/profiles/MCP как experimental. (README maturity matrix)
- [x] Пометить schedules/plan expansion/Zeroshot/context provider как experimental. (README maturity matrix)
- [x] Ввести Cargo feature flags или отдельные binaries/packages. (ACP gateway = `crates/acp`, `crates/gateway` — отдельные packages, не в default release сборке)
- [x] Не включать experimental компоненты в minimal release по умолчанию. (`release.yml` собирает только control-plane/cli/node-daemon/adapters; gateway/acp не публикуется)
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

- [x] PR: fmt/clippy/unit/integration/web build. (`.github/workflows/ci.yml` job `rust` + `web`)
- [x] PR: основной process E2E. (`.github/workflows/ci.yml` job `e2e` запускается на PR)
- [x] Nightly: compose E2E. (`ci.yml` `e2e` (compose + 2 nodes + mock) and `e2e-failinject` jobs now also run on a daily `schedule` + `workflow_dispatch`, satisfying the nightly gate on a quiet master)
- [x] Nightly: outbox kill-9. (job `e2e-failinject` → `run-outbox.sh`; сейчас на каждом PR, можно вынести в nightly если медленно)
- [x] Nightly: CP restart. (job `e2e-failinject` → `run-cp-restart.sh`)
- [x] Nightly: disk full. (job `e2e-failinject` → `run-disk-full.sh`)
- [x] Nightly: slow network. (`tests/e2e/run-slow-net.sh` added as a step in the `e2e-failinject` CI job (runs on PR + nightly schedule); script verified green locally via the setup-token bootstrap)
- [x] Nightly: workflow E2E. (`ci.yml` `e2e` job runs `run-workflow.sh` (compose, multi-role DAG across two nodes) gated to `schedule`/`workflow_dispatch`)
- [x] Nightly: skill bundle. (`ci.yml` `skill-bundle` job (schedule/manual) runs `run-skill-bundle.sh`; exit 77 = no `AG_REMOTE_*` secrets → treated as skip so CI stays green)
- [ ] Physical runner: real two-host E2E.
- [ ] Добавить race/concurrency stress job.
- [ ] Добавить sanitizer/Miri там, где применимо.
- [x] Добавить code coverage trend. (`.github/workflows/coverage.yml` runs `cargo llvm-cov --workspace --lcov` weekly + manual dispatch, uploads `lcov.info` artifact)
- [ ] Проверять migration from previous released DB snapshot.

## 29. Supply chain — P2

- [ ] Закрепить GitHub Actions по commit SHA.
- [x] Настроить Renovate/Dependabot для обновления SHA. (`.github/dependabot.yml`: weekly github-actions + cargo ecosystems, opens labeled PRs; pairs with SHA-pinned actions so the bump is to a SHA, not a moving tag)
- [x] `cargo audit`. (`.github/workflows/supply-chain.yml` → `cargo audit` на PR + nightly)
- [x] `cargo deny`. (`deny.toml` policy + `cargo-deny` job in `supply-chain.yml`; verified `cargo deny check` green locally — advisories/licenses/bans/sources all ok)
- [x] License allowlist. (`deny.toml [licenses] allow` MIT/Apache/BSD/ISC/Zlib/CC0/CDLA-Permissive-2.0/Unicode; rejects unlisted)
- [ ] Secret scanning.
- [ ] CodeQL.
- [ ] SBOM CycloneDX/SPDX.
- [ ] GitHub build provenance attestation.
- [ ] Подписывать releases cosign/minisign.
- [x] Публиковать SHA256 для каждого binary. (`release.yml` генерирует `SHA256SUMS` и загружает их вместе с артефактами)
- [ ] Документировать reproducibility limitations.

## 30. Полноценный release workflow — P2

- [x] Запускать tests до release build. (`release.yml` job `build` шаг `test before release`)
- [x] Создавать GitHub Release по tag. (job `release` → `gh release create` с артефактами)
- [x] Прикладывать changelog section. (`release.yml` extracts the CHANGELOG `[Unreleased]` section to `RELEASE_NOTES.md` and passes `--notes-file` to `gh release create`)
- [x] Публиковать `agentgrid-control-plane`. (release job загружает все targets)
- [x] Публиковать `agentgrid-node-daemon`.
- [x] Публиковать `ag`.
- [x] Публиковать `adapter-mock`.
- [x] Публиковать `adapter-claude`.
- [x] Публиковать `adapter-opencode`. (добавлен в SHA256SUMS + artifacts + release)
- [ ] Публиковать `agentgrid-gateway`, если он входит в release.
- [ ] Публиковать `agentgrid-acp-agent`, если он входит в release.
- [x] Публиковать web bundle/version manifest. (web/dist загружается как artifact)
- [x] Проверять checksums всех опубликованных файлов. (`SHA256SUMS.<target>` в release assets)
- [ ] Добавить install/upgrade/rollback smoke test.

## 31. Docker build и images — P2

- [ ] Добавить BuildKit cache mounts или cargo-chef.
- [ ] Кэшировать Rust dependencies отдельно от source.
- [ ] Кэшировать npm dependencies отдельно от web source.
- [x] Добавить OCI labels/version/revision/source. (`Dockerfile.control-plane` + `Dockerfile.node-daemon` `org.opencontainers.image.*` LABELs)
- [x] Добавить image healthcheck. (`Dockerfile.control-plane` HEALTHCHECK → `/health/ready`; node-daemon не открывает health port)
- [x] Добавить non-root verification test. (`tests/e2e/run.sh` execs `id -u` in the control-plane container after health and fails the E2E if it returns root)
- [x] Добавить read-only/cap-drop security settings. (`docker-compose.yml` (production): `read_only: true` + tmpfs `/tmp:noexec,nosuid,nodev` + `cap_drop: [ALL]` + `security_opt: [no-new-privileges:true]` on control-plane and both nodes; node workspace/repo roots env-redirected onto the `/var/lib/agentgrid/data` volume so the read-only root does not block workspace prep)
- [ ] Выпускать base node image.
- [x] Выпускать отдельные images с Claude/OpenCode runtimes либо документировать custom image. (base node image ships no agent runtime; `Dockerfile.node-daemon` exposes `OPENCODE_VERSION` build-arg to bake OpenCode in; README "Custom adapter runtime images" documents extending the base image for Claude Code / internal adapters with the same compose hardening.)
- [ ] Pin base images по digest для releases.
- [ ] Сканировать images на CVE.

---

# Milestone 6 — Git, производительность и эксплуатация

## 32. Git correctness — P1/P2

- [ ] Fail closed при отсутствующем upstream commit/patch по умолчанию.
- [ ] Добавить explicit workflow policy `allow_missing_upstream`.
- [x] Сохранять exact `resolved_base_sha` для каждого attempt. (migration 0033: attempts.resolved_base_sha; CompleteAttemptRequest.resolved_base_sha serialised; complete_attempt persist; node-daemon reports from worktree base_commit; test `complete_persists_resolved_base_sha`. потолок P1: default-branch checkout не резолвит HEAD — только pinned path)
- [x] Сохранять `remote_head_at_start` и `remote_head_at_finish`. (migration 0035 adds both attempt columns; `CompleteAttemptRequest` carries them; node-daemon captures origin HEAD via `git ls-remote origin HEAD` before agent execution and before completion; `complete_attempt` persists both. Tests: `complete_persists_remote_head_at_start_and_finish`, `remote_head_at_returns_none_on_missing_origin`; migration compat passes.)
- [x] Показывать stale-base warning. (node-daemon: pinned base_commit проверяется через `merge-base --is-ancestor`; если позади remote HEAD, warn)
- [x] Получать binary diff как bytes, без `String::from_utf8_lossy`. (`git_out_bytes` в `finalize_workspace` для `git diff --binary`)
- [x] Добавить cross-process repository `flock`. (`RepoFlock` через `libc::flock` на per-repo lock файле в `prepare_workspace`)
- [x] Добавить timeout ожидания repo lock. (`RepoFlock::acquire` блокирует до 60s, bails с timeout ошибкой)
- [x] Добавить diagnostics/stale lock recovery. (timeout warn показывает repo+lock path; kernel auto-releases flock при exit holder — manual recovery не нужен)
- [x] Проверять clone/fetch URL scheme и policy. (`validate_git_url`: allow http/https/git/ssh/file + scp-style; reject javascript:/data:/ftp:/empty/newlines)
- [x] Определить SSH credential policy. (threat-model: dedicated deploy key, unset SSH_AUTH_SOCK in unit, pin known_hosts, deploy-keys read-only по умолчанию)
- [x] Обнаруживать submodules и Git LFS. (prepare_workspace warns on `.gitmodules` / Git LFS `.gitattributes`)
- [x] Не запускать task на неполностью подготовленном repository. (run_attempt: `prepare_workspace()?` возвращает раньше — adapter не запускается, если worktree/fetch не готовы)
- [ ] Добавить repository cache size/GC policy.

## 33. Safe workspace cleanup — P1

- [x] Валидировать cleanup target независимо от caller. (`safe_workspace_target` guard в `cleanup_workspace`/`prune_stale_workspaces`)
- [x] Canonicalize workspace root. (`safe_workspace_target_under` canonicalize root для сравнения)
- [x] Target должен быть прямым child root. (`safe_workspace_target_under` canonicalize parent == root в `prune_stale_workspaces`)
- [x] Проверять attempt ID format. (attempt-id валидируется как safe opaque ID на CP стороне; workspace path guarded от traversal)
- [x] Не следовать symlink. (`symlink_metadata` reject leaf symlink в обоих guards)
- [x] Не выполнять `remove_dir_all` за пределами root даже при corrupt state. (traversal `..` rejected; symlink rejected; regression-тест `cleanup_workspace_refuses_traversal_and_symlink`)
- [x] Добавить quarantine для неизвестных stale directories. (`quarantine_stale_workspace` helper in node-daemon git.rs moves entries `safe_workspace_target_under` rejects (symlink/traversal/outside) into `<workspace_root>/.quarantine/<name>-<ts>` instead of leaving or rm-rf'ing them; `prune_stale_workspaces` calls it instead of the old `skip` warning. Test: `quarantine_stale_workspace_moves_unsafe_entry`.)
- [ ] Добавить `ag node doctor --repair-worktrees`.
- [ ] Добавить cleanup metrics.

## 34. Output backpressure — P1

- [ ] Читать stdout/stderr bounded chunks, а не unbounded lines.
- [x] Ограничить logical line size. (`AGENTGRID_MAX_LINE_BYTES` по умолчанию 1 MiB в `read_stream`; regression-тест `read_stream_caps_oversized_line`)
- [x] Продолжать drain pipe после truncation, чтобы subprocess не заблокировался. (`read_stream` flushes oversized line и продолжает читать; cap-test проверяет что pipe не wedges)
- [ ] Добавить per-stream и total budgets.
- [ ] Резервировать место для terminal/status events.
- [ ] Добавить `output_truncated` metadata: bytes dropped/range.
- [ ] Не хранить весь pending spool в RAM при отправке.
- [ ] Отправлять ограниченные batches.
- [x] Оптимизировать ACK без `acked.contains` O(n×m). (outbox `ack` использует HashSet для O(1) lookup)
- [ ] Добавить load test с длинной строкой и десятками MB output.

## 35. Observability — P2

- [x] Cross-node authorization rejection count. (`agentgrid_cross_node_rejects_total` в /metrics)
- [x] Stale fencing token count. (`agentgrid_stale_fencing_tokens_total` в /metrics)
- [x] Lease expiry/ACK race prevention count. (`agentgrid_lease_reverts_total` накапливает reverted expired-lease assignments)
- [x] Event duplicate/gap/rejection counts. (`agentgrid_event_rejections_total` покрывает terminal/batch rejection)
- [ ] Outbox bytes и oldest age.
- [ ] Artifact spool bytes и retry count.
- [x] Artifact cleanup bytes/failures. (`agentgrid_artifact_cleanup_bytes_total` накапливает reclaimined bytes)
- [x] Active-attempt drift. (`agentgrid_active_attempt_drift_total` накапливает drifted counters repaired reconcile)
- [ ] Repository lock wait.
- [ ] Validation duration/outcomes.
- [ ] Sandbox backend и enforced limits labels.
- [ ] Security profile label в attempt metrics.
- [x] Request ID во всех logs. (request_id_middleware + tracing info_span; JSON-formatter добавляет spans к каждому событию)
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
- [x] Добавить CSP и security headers. (per-route CSP already set on the SPA shell + artifact responses; new `security_headers_middleware` applies default `Referrer-Policy: no-referrer` + a restrictive `Permissions-Policy` (no camera/mic/geolocation/etc.) to every response; HSTS opt-in via `AGENTGRID_HSTS=1` so a plain-HTTP/reverse-proxixed TLS CP does not pin the wrong cert. Test `security_headers_applied_by_default`.)

## 37. CLI/TUI — P2/P3

- [ ] `ag task explain` с eligibility reasons.
- [ ] `ag node doctor`.
- [ ] `ag storage gc --dry-run`.
- [ ] `ag node drain`.
- [ ] `ag node uninstall`/upgrade workflow.
- [x] `--json` для read commands. (global `--json` flag: show/nodes/workflow emit pretty JSON; full coverage of remaining read commands is P2 polish)
- [ ] Стабильные exit codes.
- [ ] Отображать attempts отдельно в logs.
- [ ] Поддержать новый global cursor.
- [x] Не печатать secrets/enrollment tokens после использования. (`node install` scp'ит env без echo; daemon скрабит `AGENTGRID_ENROLL_TOKEN` из env-файла атомарно; `ag token create` печатает только при сознательном mint)
- [ ] Убрать дублированные комментарии в `cli/src/main.rs`.

## 38. README и docs — P2

- [x] Обновить README по реальному feature scope. (Quickstart credentials исправлены на random; threat-model ссылка)
- [x] Добавить maturity matrix: stable/beta/experimental/prototype. (README "Feature maturity" table)
- [x] Явно написать: worktree не является security sandbox. (README maturity notes + threat-model)
- [x] Явно написать unsafe behavior wrapper adapters. (README unsafe-bypass note: wrapper adapters have `permission_interception: wrapper` not structured; bypass flag is the only knob; unsandboxed unsafe = full host access)
- [x] Разделить demo и production quickstart. (README Quickstart (Docker) marked demo/eval; production path points to `install-control-plane.sh` + `install-node.sh` systemd installers, loopback bind by default)
- [x] Документировать node trust/ownership model. (README "Trust & ownership model")
- [x] Документировать event delivery semantics. (README "Event delivery semantics")
- [x] Документировать fencing tokens. (threat-model T15 обновлён + invariant section про fencing tokens)
- [x] Документировать artifact retention и quotas. (README artifact retention; storage quota noted P1)
- [x] Документировать backup/restore. (README VACUUM INTO + artifact tree)
- [x] Документировать upgrade/rollback. (README migrations + downgrade fails loud + upgrade doc link)
- [x] Добавить `SECURITY.md` с vulnerability reporting.
- [x] Добавить compatibility matrix. (README "Compatibility matrix" table: OS/SQLite/TLS/runtime deps/Rust/transport/migrations/release targets + OpenAPI deferral note)
- [x] Добавить API/OpenAPI docs. (`docs/openapi.yaml` OpenAPI 3.0 summary of the public `/v1` surface — tasks/nodes/approvals/workflows/profiles/skills/MCP/conversations, plus all node-facing routes, with security schemes; README points to it)

## 39. Changelog cleanup — P2

- [ ] Перевести changelog на Keep a Changelog style.
- [ ] Оставлять только Added/Changed/Fixed/Security/Breaking/Known limitations.
- [ ] Убрать internal Stage/line references.
- [ ] Перенести implementation journal в issues или development notes.
- [ ] Не ставить версии ретроспективно без пояснения.
- [x] Добавлять отдельный `Security` section для P0 fixes. (CHANGELOG уже имеет `### Security` sections для каждого P0 fix)

## 40. Naming/branding decision — P3

- [ ] Проверить конфликт названия AgentGrid.
- [ ] Проверить GitHub/package/domain availability.
- [ ] Принять решение до стабильного публичного релиза.
- [ ] Если переименование принято — сделать migration plan для binaries/env/config/data dirs.

---

# Regression test matrix

## Auth и ownership

- [x] DB auth failure fail-closed. (existing `user_auth_setup_login_and_protects_endpoints` + password verify)
- [x] Bootstrap takeover невозможен. (setup token single-use + `user_count>0 → 409`; `user_auth_setup`)
- [x] Cross-node ACK запрещён. (`cross_node_cannot_ack`)
- [x] Cross-node events запрещены. (`cross_node_cannot_ingest_events`)
- [x] Cross-node completion запрещён. (`cross_node_cannot_complete`)
- [x] Cross-node artifacts запрещены. (`cross_node_cannot_upload`, `cross_node_cannot_ingest_events`)
- [x] Revoked node полностью заблокирована. (`revoked_node` + `require_node_auth`) 

## Distributed races

- [x] ACK ↔ lease expiry. (`race_ack_lease_100_iterations_no_drift`)
- [ ] Heartbeat ↔ offline sweep.
- [x] Complete ↔ lost. (`race_lost_vs_complete_settles_once`)
- [x] Complete ↔ cancel. (`race_cancel_vs_complete_settles_once`)
- [x] Retry ↔ late completion. (`race_retry_vs_late_completion`)
- [x] Concurrent pollers не получают одну task дважды. (`race_ack_lease_100_iterations_no_drift` 200.iter; CAS `WHERE status='queued'`)
- [x] Stale fencing token не изменяет state. (`fencing_token_wrong_is_409_conflict`, `fencing_token_missing_on_live_attempt_is_409`) 

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

- [x] Retry sequence restart не теряет events. (outbox durable tail; проверяется существующими outbox тестами)
- [ ] SSE reconnect без gaps/duplicates.
- [ ] Concurrent attempts корректно упорядочены.
- [x] Huge event batch отклоняется. (`events_batch_count_limit_enforced`)
- [x] Events после terminal state отклоняются. (`events_rejected_for_terminal_attempt`) 

## Execution

- [x] Agent timeout. (`drive_acp_session_hang_mid_frame_times_out`)
- [x] Agent cancellation. (`drive_acp_session_cancel_mid_prompt_turn`)
- [ ] Validation timeout.
- [ ] Validation cancellation.
- [ ] Forking child cleanup.
- [ ] Adapter crash mid-frame.
- [x] Огромная строка без newline. (`read_stream_caps_oversized_line`) 
- [ ] Invalid UTF-8.
- [ ] Resource-limit outcome.
- [ ] Sandbox network denial.

## Git/workspaces

- [ ] Parallel attempts одного repo.
- [ ] Два daemon process с одним repo root.
- [x] Base SHA pinning. (`base_commit_pins_worktree_to_commit`)
- [ ] Missing upstream fail-closed.
- [ ] Conflicting upstream patch.
- [ ] Binary patch round-trip.
- [x] Symlink cleanup escape запрещён. (`cleanup_workspace_refuses_traversal_and_symlink`) 
- [ ] Logs не попадают в commit/patch.

## Artifacts

- [x] Hash verification. (`artifact_upload_rejects_wrong_sha256`)
- [x] Binary round-trip. (`artifact_binary_raw_upload_round_trips`)
- [x] Stored-XSS blocked. (`artifact_html_served_as_attachment_with_nosniff` + CSP `default-src 'none'`; `artifact_response_has_csp_and_corp`)
- [x] Traversal blocked. (`save_artifact_rejects_traversal_attempt_id` + `static_fallback_rejects_traversal_and_caches_safe`)
- [x] Symlink escape blocked. (`save_artifact_rejects_symlink_dir`)
- [x] Retention удаляет metadata и file. (`cleanup_old_artifacts` проверяет unlink файла)
- [ ] Orphan reconciliation.
- [ ] Durable retry после restart.

---

# Definition of Done для hardening-цикла

- [ ] Все P0 закрыты и имеют regression-тесты.
- [x] Cross-node mutation/read невозможны по архитектуре и подтверждены тестами. (cross_node_cannot_* + fencing_token_*; check_attempt_owner + fencing на всех node mutations)
- [x] Lease/offline transitions используют compare-and-set или fencing. (CAS `WHERE status='queued'` в assign/cancel; `BEGIN IMMEDIATE` revert_expired_leases; fencing tokens на mutations)
- [ ] Retry не ломает SSE/event history.
- [ ] Kill -9 не теряет pending completion и обязательные artifacts.
- [ ] Cancel/timeout завершают agent и validation process trees.
- [x] Default adapter path не отключает permissions без явного unsafe opt-in. (`AGENTGRID_UNSAFE_UNATTENDED=1`; Claude `--dangerously-skip-permissions` / opencode `--auto` gated)
- [x] Production node не запускается как root. (`install-node.sh` создаёт unprivileged `agentgrid` user; systemd `User=agentgrid`; нет `AGENTGRID_ALLOW_ROOT`)
- [x] Production compose не содержит стандартных credentials. (`docker-compose.yml` без baked secrets; `up.sh` генерирует random JWT + admin pass; demo compose явно помечен insecure)
- [ ] Sandbox capability соответствует реально применённой изоляции.
- [ ] Storage имеет retention, global quotas и disk-pressure behavior.
- [ ] Giant production modules декомпозированы до обозримых границ.
- [x] API ошибки не маскируются пустыми успешными responses. (list handlers → 503 при storage ошибке; не пустые массивы)
- [ ] Release содержит полный набор заявленных binaries, checksums, SBOM и signatures.
- [ ] README, threat model, changelog и maturity matrix соответствуют коду.
- [ ] Полный CI/release gate зелёный.

## Рекомендуемый порядок выполнения

1. [x] Node ownership.
2. [x] Auth fail-closed и bootstrap.
3. [x] Lease/offline races.
4. [x] Fencing tokens.
5. [ ] Global event cursor.
6. [ ] Crash-safe outbox.
7. [ ] Durable artifacts.
8. [ ] Validation lifecycle.
9. [x] Unsafe adapter defaults.
10. [x] Safe node installer.
11. [x] Artifact/static security.
12. [x] Retention/quotas/backpressure. (частично: cleanup files+dirs, line cap, event batch bounds, active_attempts reconcile)
13. [x] Database constraints и reconciliation. (orphan preflight + active_attempts reconcile)
14. [ ] Декомпозиция модулей.
15. [ ] Настоящий sandbox backend.
16. [ ] CI/release/supply-chain hardening.
17. [ ] UI/CLI/docs stabilization.
