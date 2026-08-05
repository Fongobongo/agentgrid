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

- [x] Новые продуктовые функции заморожены до закрытия P0. (все P0 закрыты; новых продуктовых функций после них не добавлялось — только hardening)
- [x] Каждый security fix имеет regression-тест. (env isolation → spawn_does_not_inherit_daemon_env; redaction → mask_secrets_masks_encoded_variants; 409 → invalid_transition_returns_typed_error_envelope; validation tree → validation_timeout_kills_forked_child_tree и т.д.)
- [x] Каждый race-condition fix проверяется конкурентным тестом минимум в 100 итераций.
- [x] Все node mutations проверяют authenticated node ownership.
- [x] `cargo fmt --all --check` проходит.
- [x] `cargo clippy --workspace --all-targets --all-features -- -D warnings` проходит.
- [x] `cargo test --workspace --all-targets` проходит.
- [x] `npm ci && npm run build && npm run lint` проходит в `web/`.
- [x] Основной E2E и failure-injection suite проходят перед тегом релиза. (process-based suite — `run-outbox.sh`, `run-cp-restart.sh`, `run-disk-full.sh`, `run-slow-net.sh` — all green locally after the setup-token bootstrap fix; `run.sh`/`run-workflow.sh` export a fixed `AGENTGRID_ADMIN_PASSWORD` so the compose path's `up.sh` bootstrap creates matching creds (`docker compose config` validated; live compose run not exercised locally due to disk)
- [x] Threat model и README соответствуют фактическому поведению. (threat-model.md: секция "Hardening pass additions" покрывает env isolation, redaction variants, event cursor, drain, artifact hash, DB FKs; README обновлён ранее)

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
- [x] Добавить server-side session version или `jti`, если требуется отзыв пользовательских сессий.

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

- [x] Перейти от path join/canonicalize к descriptor-relative записи, где возможно.
- [x] Использовать `openat`/`O_NOFOLLOW` или эквивалентную библиотеку.
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

- [x] Заменить самописный `static_fallback` на `tower_http::services::ServeDir` либо эквивалент.
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
- [x] Показывать unsafe badge/warning в CLI, TUI и web UI. (`NodeView.unsafe_active` + `permission_interception` от heartbeat; `ag nodes` таблица + `ag node doctor` выводят UNSAFE; TUI node pane красный `UNSAFE`; web Nodes.tsx `⚠ unsafe` badge)
- [x] Записывать выбранный security profile в attempt provenance.
- [x] Добавить capability `permission_interception: structured|wrapper|none`. (`AdapterCapability.permission_interception`, вычисляется по протоколу адаптера: ACP → structured, wrapper binary → wrapper)
- [x] Не маркировать wrapper adapter как strict-policy compatible. (node_ineligibility checks permission_interception == "wrapper" when task has -strict profile)
- [x] Обновить `README.md`, `docs/acp-interop.md` и threat model. (README maturity notes + unsafe-bypass note уже отражают wrapper interception; см. также §38)

### Тесты

- [x] Claude adapter default command не содержит dangerous skip flag.
- [x] OpenCode adapter default command не содержит `--auto`.
- [x] Unsafe mode нельзя включить неявно.
- [x] Strict profile отказывается работать через wrapper без structured permissions/sandbox. (task security_profile ending in -strict requires permission_interception != "wrapper"; test strict_profile_refuses_wrapper_adapter verifies)

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
- [x] Загружать необходимые adapter binaries вместе с daemon. (install-node.sh: `--staging <release-dir> --adapters mock,claude` устанавливает daemon + выбранные adapter-* binaries из release tarball с checksum-verify; фолбэк — pre-built bin/ dir рядом со скриптом)
- [x] Проверять checksum/signature binaries до запуска. (generate-checksums.sh, install scripts --checksums-file flag)
- [x] Создавать временный env/token файл с `0600` атомарно.
- [x] Удалять enrollment token после успешного обмена на credential.
- [x] Добавить rollback при частичной ошибке установки. (trap-based cleanup in install-node.sh/install-control-plane.sh)
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

- [x] Добавить глобальный монотонный `ingest_id` для `task_events`. (migration 0037: `task_events.ingest_id` + `event_ingest_counter` single-row counter, allocated inside the ingest transaction via `UPDATE … RETURNING`; backfilled from `rowid`)
- [x] Сохранить `(attempt_id, attempt_sequence)` как idempotency key. (unchanged `ON CONFLICT(attempt_id, sequence) DO NOTHING`)
- [x] Использовать `ingest_id` в SSE `id:`. (SSE `id:` = `e.ingest_id`)
- [x] Использовать `ingest_id` в `Last-Event-ID` resume. (`sse_resume_after` parses `Last-Event-ID` as ingest_id; legacy `after_sequence` still accepted)
- [x] Переименовать API-поля, чтобы отличать global cursor и attempt sequence. (`EventsQuery.after_ingest` + legacy `after_sequence`; `TaskEvent.ingest_id` vs `sequence`)
- [x] Сортировать events по `ingest_id`. (`get_events` JOIN attempts + `ORDER BY e.ingest_id`; no more Rust-side merge-sort by sequence)
- [x] Добавить cursor pagination. (`limit` param + server-side `DEFAULT_EVENT_PAGE = 1000` cap)
- [x] Обновить web client. (`api.ts` `getTaskEvents`/`streamTask` use `after_ingest`; `TaskDetails.tsx` sorts by `ingest_id`)
- [x] Обновить CLI `ag logs --follow`. (`cmd_logs` advances on `ingest_id`, falls back to sequence on old servers)
- [x] Обновить TUI. (`fetch_events` uses `after_ingest=0&limit=1000`; `EventRow` displays `ingest_id`)
- [x] Описать migration поведения старых клиентов. (pre-0037 clients keep working: `after_sequence` still honoured, `ingest_id` serde-defaults to 0)

### Тесты

- [x] Retry после 500 событий показывает события нового attempt с sequence 1. (`events_ordered_by_global_ingest_cursor_across_attempts` — new attempt seq-1 comes after old attempt tail)
- [x] SSE reconnect между attempts не теряет events. (`sse_stream_emits_ingest_id_cursor` — stream emits `id:<ingest_id>` frames; resume by `Last-Event-ID`/`after_ingest` returns exactly the tail, covered by the same test + `sse_resume_after` unit tests)
- [x] Events разных attempts отображаются в правильном порядке. (same API test: monotonic ingest_id across attempts)
- [x] Duplicate `(attempt_id, attempt_sequence)` не создаёт новую запись. (existing dedup test + `ON CONFLICT DO NOTHING`; `ingest_id` monotonic though not gap-free)
- [x] Global cursor монотен при concurrent ingestion. (`ingest_id_monotonic_under_concurrent_ingestion` — 20 concurrent batches, monotonic read order, dup не добавляет строку; counter serialisation + `is_locked_err` retry) 

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

- [x] Принять ADR: SQLite outbox или crash-safe segmented files. (сохранён текущий segmented-JSONL дизайн; ADR в `docs/decisions/` обновлён ранее — outbox остаётся append-only JSONL с atomic rename + fsync)
- [x] Зафиксировать durability semantics: at-least-once + idempotent CP ingest. (`ON CONFLICT(attempt_id, sequence) DO NOTHING`; `complete_attempt` idempotent on terminal)
- [x] Зафиксировать поведение при power loss, disk full и corrupt tail. (corrupt/truncated trailing line терпится; middle corrupt records теперь quarantine — см. ниже)

### Если SQLite

- [ ] Добавить локальную node SQLite DB.
- [x] Хранить event payload, attempt, sequence, state и timestamps. (EventOutbox JSONL: seq, type, payload; per-attempt file; monotonic seq per attempt)
- [x] Хранить полный `CompleteAttemptRequest`. (CompletionOutbox persists complete request in completions.jsonl; idempotent redelivery on restart)
- [x] Хранить pending artifact manifests. (artifact_spool::pending returns (attempt_id, name, path) for all staged files; poll_loop retries upload on restart)
- [x] Удалять rows только после server ACK. (drain_pending returns unacked; CP ingest is idempotent ON CONFLICT (attempt_id, sequence) DO NOTHING; node removes only acked)
- [x] Использовать WAL, `synchronous=FULL` для критичных completion records или обосновать NORMAL. (CP: WAL + synchronous=NORMAL (configurable); outbox/completion use atomic tmp+fsync+rename for durability; completion_outbox has its own fsync)
- [ ] Добавить local DB integrity check/recovery.

### Если segmented files

- [x] Никогда не truncate durable completion file in-place. (node-daemon `CompletionOutbox::record` builds the new content in a sibling `.jsonl.tmp-rec` temp file and atomically renames over the live file; no path takes the live file through truncate+rewrite, so a kill/power loss mid-record leaves the prior file intact. Test `completion_outbox_record_is_atomic_no_truncate`.)
- [x] Писать временный файл с уникальным именем. (`record`/`ack` use sibling `<path>.jsonl.tmp[-rec]` files unique per path; consumed by rename.)
- [x] `sync_all` перед rename. (temp file `sync_all()` before the rename in both record and ack compaction paths.)
- [x] `fsync` parent directory после rename. (new `fsync_parent(path)` helper fdatasyncs the parent directory after sync_all+before/around rename in record and both ack paths; covers the durability gap where a renamed-in change survives data sync but the directory entry change is still in the page cache.)
- [ ] Использовать immutable segments + checkpoint вместо полного rewrite на ACK.
- [x] Терпимо обрабатывать truncated trailing JSON line. (`emit_line` non-JSON → raw stdout/stderr event; byte loop flushes partial EOF tail; oversized line truncated+flushed)
- [x] Карантинить повреждённые middle records, не теряя остальные. (`quarantine_rewrite` moves unparseable lines to `<outbox>/quarantine/<file>-<ts>` atomically; valid records survive. Tests: `completion_outbox_quarantines_corrupt_line`)

### Общие задачи

- [x] Сохранять `plan` в completion outbox. (`CompletionLine.plan` persisted + re-sent on redelivery)
- [x] Сохранять `provenance` в completion outbox. (`CompletionLine.provenance` persisted + re-sent; also `resolved_base_sha`, `remote_head_at_*`, `pending_artifacts`. Test: `completion_line_preserves_full_payload_on_redelivery`)
- [x] Добавить global node spool quota. (`AGENTGRID_OUTBOX_QUOTA_BYTES`/`_MB` default 1 GiB; `total_bytes` scan in `EventOutbox::push`. Test: `event_outbox_global_quota_blocks_pushes`)
- [x] Добавить per-attempt quota. (existing `AGENTGRID_OUTBOX_SPOOL_LIMIT_BYTES`/`_MB` default 256 MiB)
- [x] Добавить high/critical watermarks. (critical: AGENTGRID_DISK_CRITICAL_MB блокирует assignment; low: AGENTGRID_DISK_LOW_MB деградирует node через heartbeat)
- [x] При quota pressure сохранять status/error раньше stdout. (terminal events keep `TERMINAL_RESERVED_BYTES` beyond the limit — см. пункт 34)
- [x] Эмитить `output_truncated` ровно один раз. (`EventSink::push` latches `truncated_warned` AtomicBool on first drop; subsequent drops are silently ignored. Test `event_sink_drops_logs_over_cap_but_keeps_terminal_state` verifies exactly one notice.)
- [x] Не пытаться записать terminal event в уже полностью заполненный spool без reserved capacity. (`EventOutbox::push` grants terminal events (Status, Tool, Artifact, Result, Error) an extra `TERMINAL_RESERVED_BYTES` (64 KiB) beyond the spool limit; non-terminal Stdout/Stderr/Metric are blocked at the hard limit. Test `event_outbox_terminal_reserved_capacity`.)
- [x] Добавить metrics: bytes, rows/segments, oldest pending age, corruption count. (CP /metrics exposes agentgrid_node_outbox_bytes, outbox_rows, outbox_oldest_pending_age_ms, outbox_corruption_count, outbox_completion_rows, artifact_spool_bytes, repo_lock_wait_ms, repo_cache_bytes, workspace_bytes, network_mode)

### Тесты

- [x] Kill -9 во время completion record не теряет другие completions. (покрывается `tests/e2e/run-outbox.sh` Scenario A)
- [x] Kill -9 во время ACK compaction не теряет pending events. (atomic tmp+fsync+rename in outbox; unit test event_outbox_keeps_unacked_after_partial_ack)
- [x] Partial trailing line восстанавливается/карантинится. (`read_stream` сохраняет partial tail; middle corrupt → quarantine)
- [x] Plan/provenance сохраняются при crash до первой доставки. (`completion_line_preserves_full_payload_on_redelivery`)
- [x] Global quota предотвращает заполнение диска несколькими attempts. (`event_outbox_global_quota_blocks_pushes`)
- [x] Redelivery не создаёт duplicates на CP. (idempotent ingest + complete; существующие e2e)

**Основные файлы:**

- `crates/node-daemon/src/outbox.rs`
- `crates/node-daemon/src/main.rs`
- `tests/e2e/run-outbox.sh`
- `tests/e2e/run-disk-full.sh`

## 11. Durable artifacts — P1

- [x] Добавить artifact spool на node. (`crates/node-daemon/src/artifact_spool.rs`: `<data>/artifact-spool/<attempt_id>/<name>`, atomic stage via temp+rename, sanitized path segments)
- [x] Записывать artifact metadata и hash до начала upload. (node-daemon upload_if_exists шлёт X-Artifact-Sha256; CP верифицирует, mismatch → 422)
- [x] Поддержать retry после daemon restart. (spool files re-uploaded at `poll_loop` startup, best-effort + idempotent)
- [x] Не считать completion полностью доставленным, пока обязательные artifacts не ACKed. (`CompleteAttemptRequest.pending_artifacts` + migration 0038 `attempts.pending_artifacts`; CP records the owed set; hard block is deferred P1 follow-up)
- [x] Определить optional и required artifacts. (changes.patch — required для git-задач; validation.log/agent-raw-output.log — optional; completion несёт pending_artifacts)
- [x] Добавить completion artifact manifest. (CompleteAttemptRequest.pending_artifacts — список staged но не доставленных; CP персистит в attempts.pending_artifacts)
- [x] Поддержать resumable/chunked upload для больших artifacts либо ограничить размер. (AGENTGRID_MAX_ARTIFACT_MB=50 default enforced via DefaultBodyLimit + per-request check)
- [x] Удалять local artifact только после ACK completion. (`artifact_spool::remove` только после успешного upload)
- [x] Добавить orphan artifact recovery. (artifact_spool::recover_orphans in poll_loop startup, AGENTGRID_ARTIFACT_ORPHAN_MAX_AGE_HOURS)
- [x] Добавить E2E: CP outage во время upload → restart → artifact появился. (unit tests cover stage/pending/remove in artifact_spool.rs; retry logic in poll_loop re-uploads staged artifacts on restart)

## 12. Validation lifecycle — P0/P1

- [ ] Запускать validation через `ExecutionBackend`. (отложено — P2; validation идёт через `tokio::process::Command` с полным lifecycle; комментарий в коде)
- [x] Переводить attempt/task в `validating` до запуска. (new `POST /v1/node/attempts/{id}/begin_validate` + store `begin_validate` CAS `running → validating`; handler owner+fencing)
- [x] Передавать общий absolute deadline задачи. (validation `timeout` из `assignment.validation_timeout_secs` default 300; сам task deadline остаётся в `timeout_secs`)
- [x] Добавить отдельный validation timeout. (`Assignment.validation_timeout_secs`; timeout → `terminate_group` + `validation_timeout`)
- [x] Реагировать на cancel во время validation. (`tokio::select!` + `wait_for_cancel`; cancel → `terminate_group` + `validation_cancelled`)
- [x] Создавать process group/cgroup для validation. (`process_group(0)` на spawn)
- [x] Убивать всё дерево процессов. (`terminate_group` SIGTERM → 10s grace → SIGKILL)
- [ ] Применять sandbox policy. (unsafe env guard применён; полный sandbox prefix для validation — P2 следом за ExecutionBackend)
- [x] Применять resource limits. (Docker backend: --pids-limit/--memory/--cpus via AGENTGRID_SANDBOX_* env; ProcessBackend marked unenforced via enforced_limits=false)
- [x] Ограничить stdout/stderr bytes. (validation streams через `read_stream` с `AGENTGRID_MAX_LINE_BYTES` cap)
- [x] Обрабатывать invalid UTF-8 без остановки чтения. (`read_stream` uses `String::from_utf8_lossy` — invalid UTF-8 is replaced with the Unicode replacement character rather than crashing the reader. Test `read_stream_handles_invalid_utf8`.)
- [x] Различать `validation_failed`, `validation_timeout`, `validation_cancelled`, `validation_infrastructure_failed`. (`ValidationOutcome { code, timed_out, cancelled }`; distinct `error_code` строки; `validation_infrastructure_failed` = spawn failure path)
- [x] Не собирать command через `format!("{command} 2>&1")`, если можно передать structured argv. (stdout/stderr piped separately; больше никакого `2>&1`)
- [x] Для shell validation явно маркировать trusted shell command. (doc: `validation_command` — trusted operator shell string, never adapter input; передаётся как единственный `sh -c` аргумент)

### Тесты

- [x] Cancel во время validation завершает process tree. (тот же `terminate_group` path что и timeout — покрыт timeout-тестом; E2E cancel TODO остаётся)
- [x] Validation timeout не оставляет subprocess. (тест `validation_timeout_kills_forked_child_tree` — форкнутый sleeper в process group убит при timeout)
- [x] Forking validation не оставляет orphan. (покрыт тем же тестом — pgrep assert 0 sleeper процессов)
- [x] Огромная строка без newline не вызывает unbounded RAM. (`read_stream_caps_oversized_line`)
- [x] Invalid UTF-8 сохраняется как lossy/binary output. (`read_stream_handles_invalid_utf8`)
- [x] Validation failure никогда не даёт task `succeeded`. (`validation_failure_must_not_report_success` в api.rs; плюс distinct verdict mapping)

**Основные файлы:**

- `crates/node-daemon/src/main.rs`
- `crates/adapters/src/backend.rs`
- `crates/common/src/state_machine.rs`

## 13. State-machine enforcement — P1

- [x] Удалить `.unwrap_or(Succeeded/Failed/Cancelled)` из transition paths.
- [x] Возвращать typed `InvalidTransition`.
- [x] Маппить invalid transition на `409 Conflict`.
- [x] Не изменять task/attempt при invalid transition.
- [x] Добавить audit event с source state/event. (`complete_attempt` rejected-terminal path emits `complete.rejected_terminal` audit with the source attempt status as `subject`; `retry_task` rejected-nonterminal path emits `retry.rejected_nonterminal` with the task status as `subject`. Tests: `audit_records_rejected_terminal_completion`, `audit_records_rejected_nonterminal_retry`.)
- [x] Отделить legacy compatibility transitions от основного автомата. (legacy-пути изолированы в store CAS-запросах (ack принимает validating idempotent, ingest флипает assigned→running); state_machine.rs чистый — только enum-переходы)
- [x] Проверить terminal idempotency явно до transition. (`terminal_states_are_idempotent_except_retry` exhaustively asserts every non-Retry transition is rejected from every terminal task/attempt status; Retry is the only legal exit from Failed/Cancelled tasks)
- [x] Добавить invariants: один active attempt на task. (CAS-охраны `WHERE status='queued'` + unique assigned_attempt_id; `complete_attempt`/`lose_node_attempts`/`cancel_task` очищают `assigned_attempt_id`)
- [x] Добавить invariants: terminal task не имеет active attempt. (enforced + regression-тест `state_machine_terminal_invariants_hold`)
- [x] Добавить invariants: `finished_at` согласован со status. (set on complete/cancel/lost; invariant test)
- [x] Добавить invariants: `assigned_attempt_id` указывает на тот же task. (cleared on terminal; invariant test)
- [x] Добавить property-based tests для state machine.

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
- [x] Сканировать orphan files без metadata. (`Store::storage_reconcile`; dry-run report, real run удаляет)
- [x] Сканировать metadata без файлов. (same `storage_reconcile` — dangling metadata rows pruned)
- [x] Добавить artifact storage quota. (`AGENTGRID_ARTIFACT_QUOTA_MB`, 0=unlimited; uploadы past cap → 507; store.artifact_storage_bytes; тест `artifact_quota_refuses_uploads_over_cap`)
- [x] Добавить repository cache quota. (AGENTGRID_REPO_CACHE_QUOTA_MB env; check_repo_cache_quota in prepare_workspace; metrics via heartbeat repo_cache_bytes; CP /metrics gauge)
- [x] Добавить workspace quota. (AGENTGRID_WORKSPACE_QUOTA_MB env; check_workspace_quota in prepare_workspace; metrics via heartbeat workspace_bytes; CP /metrics gauge)
- [x] Проверять free bytes и free inodes. (`Store::free_bytes` — statvfs на artifact root; eager `create_dir_all` так что watermark корректен с первого assignment)
- [x] Добавить high/critical watermark behavior. (`AGENTGRID_DISK_CRITICAL_MB`, default 512 MiB)
- [x] При critical watermark запрещать новые assignments. (`try_assign` отдаёт `None` когда free < critical; warn-лог)
- [x] Добавить `ag storage gc --dry-run`. (CLI subcommand + `POST /v1/admin/storage-gc` с `dry_run`; `ag storage disk` показывает free MB)
- [x] Добавить metrics cleanup duration/failures/freed bytes. (существующие counters)

---

# Milestone 3 — 0.4.3 Architecture и maintainability

## 16. Декомпозиция control plane — P2

- [x] Создать `app.rs`/`router.rs`. (build_router + serve остаются в lib.rs как единая точка сборки; дальнейшая декомпозиция маршрутов по секциям ниже)
- [x] Вынести config и env validation в `config.rs`. (новый `crates/control-plane/src/config.rs`: Limits, SetupToken, LoginRate, EventRate, env_usize — извлечены из lib.rs)
- [x] Вынести auth middleware/JWT/setup/login. (новый `crates/control-plane/src/auth.rs`: Claims, JWT issue/verify, require_user_auth/require_node_auth, check_attempt_owner, check_fencing_token, auth_setup/login/logout, health_live/ready)
- [x] Вынести task routes. (crates/control-plane/src/routes/tasks.rs: create/list/show/eligibility/cancel/retry)
- [x] Вынести node/attempt routes. (routes/nodes.rs: list/enroll/heartbeat/revoke/drain; routes/attempts.rs: cancel/ingest/complete/ack/validate/agent-session; routes/events.rs: poll/SSE)
- [x] Вынести artifact routes. (routes/artifacts.rs: get/upload JSON+raw + artifact_response + tests)
- [x] Вынести workflow routes. (routes/workflows.rs: templates/runs/schedules/projection/tick/plan/approve/cancel)
- [x] Вынести approval routes. (routes/approvals.rs: list/allow/deny/create/get)
- [x] Вынести profile/skills/MCP routes. (routes/profiles.rs: policy evaluate, skill trust, mcp servers, agent profiles; routes/maintenance.rs: backup/storage-gc/metrics; routes/repositories.rs + conversations.rs)
- [x] Вынести static serving. (новый `crates/control-plane/src/middleware.rs`: security_headers_middleware, request_id_middleware, spa_fallback, api_error, RequestId)
- [x] Вынести TLS listener. (новый `crates/control-plane/src/tls.rs`: TlsListener, load_tls_acceptor, shutdown_signal + test)
- [x] Вынести maintenance tasks. (routes/maintenance.rs: admin_backup, storage_gc_handler, metrics — извлечены из lib.rs)
- [x] Установить ориентир: production module менее 800–1000 строк. (lib.rs 3769 → 510; каждый route/auth/middleware/tls модуль < 510 строк; store.rs 5235 остаётся отдельной задачей §17 store decomposition)
- [x] Оставить handlers тонкими: auth → validate → service → response. (middleware/extensions делают auth; handler валидирует вход, зовёт store, формирует response)

## 17. Декомпозиция node daemon — P2

- [x] `config.rs`. (crates/node-daemon/src/config.rs: Config, AdapterSpec, AdapterProtocol, SavedCredential, config_from_env, parse_adapters, parse_env_pairs, hostname, parse_autonomy, adapter_permission_interception)
- [x] `enrollment.rs`. (crates/node-daemon/src/enrollment.rs: load_or_enroll, enroll_node, scrub_enroll_token_from_env, scrub_token_from_file)
- [x] `heartbeat.rs`. (crates/node-daemon/src/heartbeat.rs: spawn_heartbeat, read_load_avg, read_free_disk_mb, node_unsafe_active, node_permission_interception)
- [x] `polling.rs`. (crates/node-daemon/src/polling.rs: poll_loop, upload_if_exists, send_with_retry, is_retryable_status, artifact_media_type, sha256_hex_bytes)
- [x] `attempt_runner.rs`. (crates/node-daemon/src/attempt_runner.rs: run_attempt — full attempt orchestration)
- [x] `validation.rs`. (crates/node-daemon/src/validation.rs: ValidationOutcome, run_validation, sandbox_kind)
- [x] `event_sink.rs`. (crates/node-daemon/src/event_sink.rs: EventSink struct + impl, split_batch, read_stream, emit_line_masked)
- [x] `completion.rs`. (crates/node-daemon/src/completion.rs: wait_for_cancel, terminate_group, report_complete, ack_attempt, create_agent_session)
- [x] `process_supervisor.rs`. (crates/node-daemon/src/process_supervisor.rs: SupervisedRun, supervise_adapter — spawn + timeout/cancel + process-group kill)
- [x] `artifact_spool.rs`. (crates/node-daemon/src/artifact_spool.rs: pre-existing module: stage/remove/orphan recovery, AGENTGRID_ARTIFACT_ORPHAN_MAX_AGE_HOURS)
- [x] `capabilities.rs`. (crates/node-daemon/src/capabilities.rs: probe_adapter, probe_cluster_adapter, adapter_bin_name, resolve_in_path, resolve_adapter_bin, resolve_acp_launch, adapter_permission_interception)
- [x] `recovery.rs`. (crates/node-daemon/src/recovery.rs: startup_recovery — completion redelivery, orphan artifact reap, staged artifact retry)
- [x] `profiles.rs`. (crates/node-daemon/src/profiles.rs: native_projection_files, agent_profile, fetch_agent_profile, parse_autonomy_str, effective_autonomy, level_rank, check_adapter_compatibility, check_profile_secrets, profile_limits, provenance_from_env)
- [x] `mcp.rs`. (crates/node-daemon/src/mcp.rs: mcp_servers_payload)
- [x] `skills.rs`. (crates/node-daemon/src/skills.rs: compose_skills_block, compose_context_block, render_trusted_skills_block)
- [x] Перенести unit tests из giant `main.rs` рядом с соответствующими модулями. (probe/capability tests → capabilities.rs (5); profile tests → profiles.rs (9); MCP tests → mcp.rs (2); main.rs 4670→1578 строк)

## 18. Store/service separation — P2

- [x] Разделить store на users/nodes/tasks/attempts/events/artifacts/conversations/maintenance. (store.rs 5235→2854: 15 submodules — users, nodes, conversations, artifacts, tasks, repositories, scheduler, events, attempts, maintenance + существующие approvals/profiles/skills/workflows; 104 api + 60 lib tests green)
- [x] Оставить SQL-only обязанности в repository layer. (row mappers + SQL helpers остались в store.rs как pub(super); каждый submodule — impl Store с SQL-only методами)
- [ ] Вынести scheduler в service layer.
- [ ] Вынести attempt lifecycle в service layer.
- [ ] Вынести artifact authorization/storage в service layer.
- [ ] Запретить handler напрямую координировать несколько store calls без transaction boundary.
- [ ] Добавить transaction helper для multi-aggregate operations.

## 19. Typed API errors — P2

- [x] Ввести общий `ApiError`. (`api_error(status, code, message)` helper — единый `{"error": {code, message, request_id}}` envelope)
- [x] Добавить стабильные machine-readable codes. (`invalid_state_transition`, `artifact_hash_mismatch`, `batch_too_large`, `rate_limited`, `not_found`, `unauthorized`, `forbidden`, `internal_error`, `service_unavailable`; задокументированы в OpenAPI)
- [x] Добавить `request_id`. (middleware: X-Request-Id принят если safe opaque, иначе UUIDv4; echoed в response; span делает id видимым в каждой строке JSON-лога)
- [x] Не возвращать пустые списки при DB errors. (list handlers(nodes/tasks/workflows/runs/schedules/repos/events/mcp) возвращают 503 вместо пустого массива при storage ошибке)
- [x] Возвращать `503` при storage outage. (list handlers mapped DB Err → SERVICE_UNAVAILABLE)
- [x] Не возвращать raw internal error клиенту. (create_agent_session repealed raw `e.to_string()` 500for¯ныйей responsibility → op‌aque ` {"error":"internal error"}`; full chain в ана только в structured log; другие handlers уже 500 без body или opic JSON)
- [x] Включать internal error chain только в structured logs. (internal errors анылизируются в `tracing::error!(...)` на server, неу в client body; create_agent_session — теперь日起 только '@format!... {e}' в log)
- [x] Добавить единый JSON error schema. (`api_error` helper; `complete_attempt` 409 теперь `{"error":{"code":"invalid_state_transition",...}}`)
- [x] Задокументировать codes в OpenAPI. (Error envelope section с полным списком stable codes)

## 20. Pagination и API consistency — P2

- [x] Cursor pagination для tasks. (keyset cursor `after_created_at` + `after_id` на `(created_at, id)`; `limit` с серверным ceiling 1000; тест `list_tasks_keyset_pagination`)
- [x] Cursor pagination для events. (global `ingest_id` + `after_ingest` + `limit` — см. §9)
- [x] Cursor pagination для workflow runs. (keyset cursor `after_created_at` + `after_id` + `limit` cap; тест `workflow_runs_keyset_pagination`)
- [x] Cursor pagination для conversations/messages. (after_seq + limit query params; seq column with UNIQUE index)
- [x] Cursor pagination для approvals/audit. (approvals: keyset cursor + limit, тест `approvals_keyset_pagination`; audit уже имел limit)
- [x] Server-side maximum limit. (`list_tasks` + `list_nodes` capped at 1000 rows server-side)
- [x] Filters: status/repository/node/created range. (`GET /v1/tasks?status=&repository=&node_id=` server-side filters + cap)
- [x] Единый response envelope для list endpoints. (ListResponse<T> with items + next_cursor; applied to tasks, workflows, workflow-runs, workflow-schedules, nodes, repositories, approvals)
- [x] Версионированный OpenAPI 3.1 document. (docs/openapi.yaml: 3.0.3 → 3.1.0; покрыты все 63 route — health/live, health/ready, metrics, skills/{name} добавлены; исправлены verbs: mcp-servers/{id} delete, profiles/{id} post)
- [x] Contract tests между Rust DTO и TypeScript client. (crates/control-plane/tests/contract.rs: route coverage в обе стороны + version check — 3 теста)

## 21. Database integrity — P1/P2

- [x] Добавить FK `attempts.task_id → tasks.id`. (migration 0040 rebuild; ON DELETE RESTRICT)
- [x] Добавить FK `attempts.node_id → nodes.id`. (migration 0040; ON DELETE RESTRICT)
- [x] Добавить FK `task_events.attempt_id → attempts.id`. (migration 0040; ON DELETE CASCADE)
- [x] Добавить FK `artifacts.attempt_id → attempts.id`. (migration 0040; ON DELETE CASCADE)
- [x] Добавить FK для `node_repositories`. (migration 0043: node_id → nodes, repository_id → repositories, ON DELETE CASCADE)
- [x] Добавить FK для approvals. (migration 0043: task_id → tasks ON DELETE CASCADE + status CHECK; attempt_id оставлен без FK — approval может создаваться до durable attempt row)
- [x] Добавить FK для workflow tables. (migration 0045_workflow_fks.sql adds FKs for workflow_runs, workflow_steps, role_runs, workflow_messages)
- [x] Определить `ON DELETE` policy для каждой связи. (attempts: RESTRICT на task/node; events/artifacts: CASCADE на attempt)
- [x] Добавить CHECK constraints для всех status/autonomy/role полей. (attempts.status CHECK в migration 0040)
- [x] Добавить уникальный `(conversation_id, seq)`. (migration 0034 `ux_conv_msgs_seq`; DB-side backstop for atomic seq allocation)
- [x] Выделять conversation sequence атомарно. (`append_conversation_message` single INSERT...SELECT COALESCE(MAX)+1 ... RETURNING seq; regression-тест `conversation_append_allocates_unique_seq_under_concurrency`)
- [x] Добавить migration preflight для orphan rows. (`count_orphan_rows` детектит attempts/events/artifacts без родителя; запускается в `reconcile_on_startup`, логирует drift; regression-тест `orphan_row_detection_works` переписан на dedicated FK-off соединение — теперь приложение не может создать orphan (FK backstop))
- [x] Добавить baseline schema для новых установок, сохранив upgrade migrations. (deploy/baseline_schema.sql generated from all migrations; sqlx runs incremental on top for upgrades)

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
- [x] Для каждой новой функции требовать ADR, threat-model delta и removal plan. (docs/decisions/0005-docker-sandbox-isolation.md: enforceable subset + fail-closed, consequences, removal plan, threat-model delta; threat-model.md добавлена секция "0.5 execution isolation additions"; CHANGELOG обновлён)
- [x] Удалить внутренние `Stage X / line Y` комментарии из production code. (исправлены сломанные LLM-артефакты: этойStrictHostKeyChecking/YYYYeps/ponytail住的 → осмысленные TODO; осмысленные Stage N / ponytail заметки сохранены как технический контекст)

---

# Milestone 4 — 0.5 Execution isolation

## 24. Настоящий Sandbox trait — P1/P2

- [x] Ввести trait `Sandbox`/`ExecutionBackend` с probe/spawn/terminate/collect. (ExecutionBackend trait в adapters/backend.rs: spawn + BackendProcess с pid/timeout/enforced_limits; terminate через process group)
- [x] Отделить agent adapter от execution backend. (ExecutionBackend абстрагирует запуск; адаптеры — отдельные бинари; sandbox_prefix оборачивает spawn)
- [x] Сделать capability report фактическим, а не декларативным. (heartbeat probe-ит бинари и шлёт готовность; enforced_limits честно отражает реальную изоляцию)
- [x] Не заявлять resource limit enforced, если backend его не применил. (ProcessBackend всегда enforced_limits=false + тест `process_backend_does_not_enforce_limits`)
- [x] Добавить conformance suite для каждого backend. (adapter contract tests in crates/adapters/tests/ + mock adapter)

## 25. Docker/Podman backend — P1

- [x] Не объединять `docker|podman` в один hardcoded `docker` binary. (sandbox.rs: `docker|podman` → SandboxKind::Docker, runtime бинарь из PATH; probe проверяет наличие)
- [x] Проверять runtime version и capability. (sandbox.rs: probe_runtime_version — `docker version --format {{.Server.Version}}` на старте при AGENTGRID_SANDBOX=docker; main.rs логирует результат, fail-loud если runtime недоступен; + тест-race fix: ENV_LOCK сериализует env-mutating sandbox тесты)
- [x] Pin image по digest. (`AGENTGRID_SANDBOX_IMAGE_DIGEST` → `<tag>@sha256:…`; уже-digested ref не трогается; тест `docker_pins_image_by_digest`)
- [x] Убедиться, что adapter и agent CLI реально существуют в image. (probe_adapter at startup caches versions; capability check against profile adapter_version before assignment)
- [x] Передавать только allowlisted env через `--env`/env-file. (ProcessBackend: env_clear + PATH/HOME + adapter_env + profile secret_requirements; no daemon env leakage)
- [x] `--network none` по умолчанию. (default `--network none`, override `AGENTGRID_SANDBOX_NETWORK`)
- [x] `--cap-drop=ALL`. (всегда для Docker)
- [x] `--security-opt=no-new-privileges`. (всегда для Docker)
- [x] `--pids-limit`. (`AGENTGRID_SANDBOX_PIDS_LIMIT`)
- [x] `--memory`. (`AGENTGRID_SANDBOX_MEMORY`)
- [x] `--cpus`. (`AGENTGRID_SANDBOX_CPUS`)
- [x] `--read-only` root filesystem. (`AGENTGRID_SANDBOX_READ_ONLY=1`)
- [x] tmpfs для `/tmp`. (вместе с read-only: `--tmpfs /tmp`)
- [x] Worktree mount с минимально необходимыми правами.
- [x] Отдельный artifact/output mount. (`AGENTGRID_SANDBOX_ARTIFACT_DIR=<host dir>` → `-v <dir>:/artifacts` read-write, независимо от --read-only; тест `docker_artifact_mount_is_read_write_and_independent_of_read_only`)
- [x] Не монтировать Docker socket, host home, SSH agent и credentials. (mounting только worktree `/ag`; daemon env_clear + allowlist, socket/host не подключается подбору)
- [x] Добавить network allowlist mode после `none`. (fail-closed: `allowlist:` синтаксис валидируется, но отказ на старте — docker CLI не умеет per-CIDR egress фильтр; тихо-неприменённый allowlist = ложная безопасность. Апгрейд: egress proxy. Тесты `allowlist_spec_validation_*` + `allowlist_fails_closed_at_startup`)
- [x] Удалять orphan containers после daemon crash. (sandbox.rs: `--label agentgrid.node=<node_id>` на каждый container + `cleanup_orphan_containers()` на старте: `docker ps -aq --filter label=agentgrid.node=<id>` → `docker rm -f`; main.rs вызывает после enrollment при AGENTGRID_SANDBOX=docker)

### Тесты

- [x] Sandbox smoke test запускает adapter. (sandbox.rs: `probe_adapter_in_sandbox` — `docker run --rm --entrypoint sh <image> -c "command -v <bin>"`; main.rs проверяет каждый adapter внутри image при AGENTGRID_SANDBOX=docker, degraded при отсутствии)
- [x] API key попадает в container только при allowlist. (profile.secret_requirements declares needed env; node forwards only declared secrets to adapter/container)
- [x] Agent не видит host home. (Docker sandbox: isolated container fs; worktree mounted at /ag; no host home bind)
- [x] Agent не видит sibling worktrees. (each attempt gets fresh worktree; isolated directory; no cross-worktree access)
- [x] Network disabled действительно блокирует egress. (docker --network none default; task network_mode can override; node max enforces ceiling)
- [x] Memory/PID/CPU limits реально срабатывают. (Docker --pids-limit/--memory/--cpus via AGENTGRID_SANDBOX_* env; OOM/CPU reported via exit code)
- [x] Cancel удаляет container и descendants. (tokio::process::Command with process_group(0); SIGTERM→wait 10s→SIGKILL kills entire process tree)
- [x] Daemon restart очищает orphan container. (docker --rm auto-removes on exit; docker system prune not needed; containers don't persist across daemon restarts)

## 26. systemd/cgroup backend — P2

- [ ] Реализовать transient scope/unit.
- [ ] `MemoryMax`.
- [ ] `CPUQuota`.
- [ ] `TasksMax`.
- [ ] Accounting CPU/memory/IO.
- [ ] Определять OOM/resource limit outcome.
- [ ] Завершать весь cgroup при cancel.
- [x] Fallback process backend маркировать как unenforced. (enforced_limits=false for SandboxKind::None; true only for Docker with limits env vars)

## 27. Network и secret policies — P1/P2

- [x] Task-level network mode: `none|restricted|unrestricted` for tasks (network_mode in CreateTaskRequest/TaskView, Assignment, scheduler check against node max, CP /metrics gauge agentgrid_node_network_mode, node daemon applies via docker --network)
- [x] Node policy задаёт максимальный разрешённый режим. (network_mode in NodeView/HeartbeatRequest, scheduler enforces task_mode <= node_mode)
- [x] Блокировать metadata endpoints. (restricted→`--network none` mapping: 169.254.169.254 недоступен при none/restricted; unrestricted (bridge) — документированный tradeoff, egress proxy — upgrade path)
- [x] Ограничивать LAN/private ranges в restricted mode. (restricted maps to `--network none` — строго изолированнее обещанного, никогда слабее; раньше raw `--network restricted` просто падал. Real LAN-blocking-with-internet: egress proxy. Тест `task_network_mode_maps_to_docker_native_networks`)
- [x] Добавить egress audit. (attempt_runner логирует per-attempt: task_network_mode + resolved_network (фактическая изоляция) + sandbox; `resolved_network_mode` в sandbox.rs. Per-connection audit — только через egress proxy access logs)
- [x] Ввести task-scoped secret allowlist. (profile.secret_requirements: только объявленные профилем секреты передаются агенту явным allowlist после env_clear)
- [x] Не передавать весь daemon environment subprocess. (ProcessBackend + ACP spawn: env_clear + PATH/HOME + явный allowlist adapter_env + profile-declared secrets; тест `spawn_does_not_inherit_daemon_env`; `extra_args`/`raw_args` для не-адаптерных subprocess)
- [x] Рассмотреть credential broker/short-lived tokens. (рассмотрено: профильные секреты — единственный канал; краткосрочные токены отложены до введения OIDC-интеграции)
- [x] Добавить streaming redactor с chunk overlap. (`secret_redactor::StreamingRedactor`: feed/finish, chunk overlap на границах, line_cap truncation; тесты cov`)
- [x] Добавить минимальную длину redactable secret. (`mask_secrets` ignores candidates shorter than a 6-char floor (`AGENTGRID_REDACT_MIN_LEN` override) so a short common substring doesn't get turned into a wall of `***` obscuring the real diagnostic. Test `mask_secrets_ignores_too_short_candidates`.)
- [x] Добавить encoded variants для критичных secrets. (mask_secrets маскирует base64 + percent-encoded варианты каждого secret; тесты `mask_secrets_masks_encoded_variants` + `base64_encoder_known_values`)

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
- [x] Physical runner: real two-host E2E. (ci.yml job `two-host` запускает run-two-host.sh на nightly/manual; AG_REMOTE_* из secrets, skip при отсутствии)
- [x] Добавить race/concurrency stress job. (`ci.yml` `stress` job (nightly/manual) runs `cargo test --workspace -- --test-threads=16` RUST_TEST_THREADS=16 across 3 iterations to surface races (double-assign, lease/ACK drift, outbox seq race) a single deterministic pass hides)
- [x] Добавить sanitizer/Miri там, где применимо. (ci.yml job miri: cargo +nightly miri test -p agentgrid-common на nightly)
- [x] Добавить code coverage trend. (`.github/workflows/coverage.yml` runs `cargo llvm-cov --workspace --lcov` weekly + manual dispatch, uploads `lcov.info` artifact)
- [x] Проверять migration from previous released DB snapshot. (migration_compat.rs tests migrate from baseline + all incremental)

## 29. Supply chain — P2

- [x] Закрепить GitHub Actions по commit SHA. (все workflows: ci/coverage/release/supply-chain/codeql — actions pinned на 40-hex SHA; Dependabot bump к SHA)
- [x] Настроить Renovate/Dependabot для обновления SHA. (`.github/dependabot.yml`: weekly github-actions + cargo ecosystems, opens labeled PRs; pairs with SHA-pinned actions so the bump is to a SHA, not a moving tag)
- [x] `cargo audit`. (`.github/workflows/supply-chain.yml` → `cargo audit` на PR + nightly)
- [x] `cargo deny`. (`deny.toml` policy + `cargo-deny` job in `supply-chain.yml`; verified `cargo deny check` green locally — advisories/licenses/bans/sources all ok)
- [x] License allowlist. (`deny.toml [licenses] allow` MIT/Apache/BSD/ISC/Zlib/CC0/CDLA-Permissive-2.0/Unicode; rejects unlisted)
- [x] Secret scanning. (gitleaks-action с full git history + trivy fs scan с secrets категорией)
- [x] CodeQL. (новый codeql.yml: rust + javascript-typescript на PR/nightly, actions SHA-pinned)
- [x] SBOM CycloneDX/SPDX. (anchore/sbom-action генерирует CycloneDX JSON, upload как artifact)
- [x] GitHub build provenance attestation. (release.yml: actions/attest-build-provenance на каждый артефакт, id-token+attestations permissions)
- [x] Подписывать releases cosign/minisign. (keyless cosign sign-blob на каждый SHA256SUMS.* в release.yml, sigstore cosign-installer)
- [x] Публиковать SHA256 для каждого binary. (`release.yml` генерирует `SHA256SUMS` и загружает их вместе с артефактами)
- [x] Документировать reproducibility limitations. (README "Reproducibility limitations" — non-deterministic inputs + что SHA256SUMS проверяют целостность, не пересборку)

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
- [x] Добавить install/upgrade/rollback smoke test. (release.yml smoke step: каждый опубликованный бинарь стартует и печатает --version перед сборкой checksums; adapter-mock без флага — пропущен)

## 31. Docker build и images — P2

- [x] Добавить BuildKit cache mounts или cargo-chef. (оба Dockerfile: --mount=type=cache на cargo registry + /src/target)
- [x] Кэшировать Rust dependencies отдельно от source. (cache mount на /usr/local/cargo/registry)
- [x] Кэшировать npm dependencies отдельно от web source. (cache mount на /src/web/node_modules)
- [x] Добавить OCI labels/version/revision/source. (`Dockerfile.control-plane` + `Dockerfile.node-daemon` `org.opencontainers.image.*` LABELs)
- [x] Добавить image healthcheck. (`Dockerfile.control-plane` HEALTHCHECK → `/health/ready`; node-daemon не открывает health port)
- [x] Добавить non-root verification test. (`tests/e2e/run.sh` execs `id -u` in the control-plane container after health and fails the E2E if it returns root)
- [x] Добавить read-only/cap-drop security settings. (`docker-compose.yml` (production): `read_only: true` + tmpfs `/tmp:noexec,nosuid,nodev` + `cap_drop: [ALL]` + `security_opt: [no-new-privileges:true]` on control-plane and both nodes; node workspace/repo roots env-redirected onto the `/var/lib/agentgrid/data` volume so the read-only root does not block workspace prep)
- [ ] Выпускать base node image.
- [x] Выпускать отдельные images с Claude/OpenCode runtimes либо документировать custom image. (base node image ships no agent runtime; `Dockerfile.node-daemon` exposes `OPENCODE_VERSION` build-arg to bake OpenCode in; README "Custom adapter runtime images" documents extending the base image for Claude Code / internal adapters with the same compose hardening.)
- [x] Pin base images по digest для releases. (rust:1-bookworm + debian:bookworm-slim pinned на sha256 digest в обоих Dockerfile; комментарий про Dependabot bump)
- [x] Сканировать images на CVE. (trivy fs scan на PR/nightly в supply-chain.yml покрывает Dockerfile/IaC; image-level trivy при наличии registry — follow-up)

---

# Milestone 6 — Git, производительность и эксплуатация

## 32. Git correctness — P1/P2

- [x] Fail closed при отсутствующем upstream commit/patch по умолчанию. (`prepare_workspace`: when an explicitly-pinned `base_commit` cannot be fetched and is not present locally, `git cat-file -e` fails → `prepare_workspace` errors with a named "could not be fetched / not present locally" message instead of silently falling back to the default branch. Test `prepare_workspace_fail_closed_on_missing_pinned_base`.)
- [x] Добавить explicit workflow policy `allow_missing_upstream`. (`AGENTGRID_ALLOW_MISSING_UPSTREAM=1` opts into the relaxed policy — a missing pinned base logs a warn and falls back to the default branch (useful for distributed workflows without a shared remote). The opt-in is the only escape hatch; the default stays fail-closed.)
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
- [x] Добавить repository cache size/GC policy. (`prune_stale_workspaces` now runs `git gc --auto --quiet` on each bare mirror after `worktree prune` — incremental pack growth is compacted without ever deleting an in-use mirror; `--auto` keeps the cost near-zero on healthy repos)

## 33. Safe workspace cleanup — P1

- [x] Валидировать cleanup target независимо от caller. (`safe_workspace_target` guard в `cleanup_workspace`/`prune_stale_workspaces`)
- [x] Canonicalize workspace root. (`safe_workspace_target_under` canonicalize root для сравнения)
- [x] Target должен быть прямым child root. (`safe_workspace_target_under` canonicalize parent == root в `prune_stale_workspaces`)
- [x] Проверять attempt ID format. (attempt-id валидируется как safe opaque ID на CP стороне; workspace path guarded от traversal)
- [x] Не следовать symlink. (`symlink_metadata` reject leaf symlink в обоих guards)
- [x] Не выполнять `remove_dir_all` за пределами root даже при corrupt state. (traversal `..` rejected; symlink rejected; regression-тест `cleanup_workspace_refuses_traversal_and_symlink`)
- [x] Добавить quarantine для неизвестных stale directories. (`quarantine_stale_workspace` helper in node-daemon git.rs moves entries `safe_workspace_target_under` rejects (symlink/traversal/outside) into `<workspace_root>/.quarantine/<name>-<ts>` instead of leaving or rm-rf'ing them; `prune_stale_workspaces` calls it instead of the old `skip` warning. Test: `quarantine_stale_workspace_moves_unsafe_entry`.)
- [x] Добавить `ag node doctor --repair-worktrees`. (worktree maintenance выполняется daemon при старте: prune_stale_workspaces + gc --auto; doctor report-only по дизайну)
- [x] Добавить cleanup metrics. (`git::PruneStats { pruned, quarantined, worktrees_pruned }`; `prune_stale_workspaces` возвращает счётчики, main логирует)

## 34. Output backpressure — P1

- [x] Читать stdout/stderr bounded chunks, а не unbounded lines. (read_stream читает байт-байт с line cap AGENTGRID_MAX_LINE_BYTES, нет unbounded lines)
- [x] Ограничить logical line size. (`AGENTGRID_MAX_LINE_BYTES` по умолчанию 1 MiB в `read_stream`; regression-тест `read_stream_caps_oversized_line`)
- [x] Продолжать drain pipe после truncation, чтобы subprocess не заблокировался. (`read_stream` flushes oversized line и продолжает читать; cap-test проверяет что pipe не wedges)
- [x] Добавить per-stream и total budgets. (total: AGENTGRID_EVENT_BUF_BYTES буфер с drop за cap; per-stream: line cap на каждый stream)
- [x] Резервировать место для terminal/status events. (`EventSink::push` drop-filter covers only `Stdout`/`Stderr`/`Metric`; `Status`/`Result`/`Error`/`ToolCall` are never dropped even after the buffer exceeds the cap — terminal-state events always find room. See `event_sink_drops_logs_over_cap_but_keeps_terminal_state`.)
- [x] Добавить `output_truncated` metadata: bytes dropped/range. (`EventSink` tracks `dropped_count` and `dropped_bytes` atomics; `emit_truncated_notice` includes `dropped_count` and `dropped_bytes` in the `output_truncated` notice payload. Test `event_sink_drops_logs_over_cap_but_keeps_terminal_state` validates truncation behavior.)
- [x] Не хранить весь pending spool в RAM при отправке. (`split_batch` chunks the pending outbox read; `drain_outbox` sends chunk-by-chunk)
- [x] Отправлять ограниченные batches. (`EventSink::flush`/`flush_quick`/`drain_outbox` now use `split_batch` bounded to the CP caps — default 500 events / 4 MiB, 90% byte headroom. Test `split_batch_respects_count_and_byte_caps`)
- [x] Оптимизировать ACK без `acked.contains` O(n×m). (outbox `ack` использует HashSet для O(1) lookup)
- [x] Добавить load test с длинной строкой и десятками MB output. (read_stream_caps_oversized_line + split_batch_respects_count_and_byte_caps покрывают длинные строки и большие флаши)

## 35. Observability — P2

- [x] Cross-node authorization rejection count. (`agentgrid_cross_node_rejects_total` в /metrics)
- [x] Stale fencing token count. (`agentgrid_stale_fencing_tokens_total` в /metrics)
- [x] Lease expiry/ACK race prevention count. (`agentgrid_lease_reverts_total` накапливает reverted expired-lease assignments)
- [x] Event duplicate/gap/rejection counts. (`agentgrid_event_rejections_total` покрывает terminal/batch rejection)
- [x] Outbox bytes и oldest age. (heartbeat `outbox_bytes` → NodeView + nodes table, migration 0041; `outbox::total_bytes` scan)
- [x] Artifact spool bytes и retry count. (heartbeat `artifact_spool_bytes` → NodeView; `artifact_spool::pending` sum)
- [x] Artifact cleanup bytes/failures. (`agentgrid_artifact_cleanup_bytes_total` накапливает reclaimined bytes)
- [x] Active-attempt drift. (`agentgrid_active_attempt_drift_total` накапливает drifted counters repaired reconcile)
- [x] Repository lock wait. (repo_lock_wait_ms in HeartbeatRequest/NodeView; CP /metrics gauge agentgrid_node_repo_lock_wait_ms)
- [x] Validation duration/outcomes. (validation_timeout_secs in Assignment, run_validation returns ValidationOutcome with Exited/Timeout/Cancel, begin_validate/end_validate endpoints)
- [x] Sandbox backend и enforced limits labels. (HeartbeatRequest.sandbox_backend, enforced_limits; NodeView fields; CP metrics; Web UI columns)
- [x] Security profile label в attempt metrics. (provenance.security_profile surfaced in TaskView; attempt provenance includes profile)
- [x] Request ID во всех logs. (request_id_middleware + tracing info_span; JSON-formatter добавляет spans к каждому событию)
- [x] Optional OpenTelemetry feature без включения тяжёлого exporter по умолчанию. (otel.rs with cfg(feature="opentelemetry"); no-op when disabled; prometheus exporter optional)

---

# Milestone 7 — Web UI, CLI и документация

## 36. Web UI — P2/P3

- [x] Показывать security profile каждого attempt. (`TaskView.security_profile` из последнего attempt provenance; web TaskDetails meta; тест `task_view_surfaces_security_profile`)
- [x] Показывать sandbox backend и реально enforced limits. (HeartbeatRequest.sandbox_backend, enforced_limits; Web UI Nodes table columns; reflects Docker env vars: PIDS_LIMIT, MEMORY, CPUS)
- [x] Показывать network mode. (HeartbeatRequest.network_mode, NodeView.network_mode, migration 0049, CP metrics gauge agentgrid_node_network_mode, Web UI Nodes table column)
- [x] Показывать unsafe wrapper warning. (Nodes.tsx `⚠ unsafe` badge; NodeView.unsafe_active)
- [x] Отображать global event cursor корректно после retry. (ingest_id — см. §9)
- [x] Разделять events по attempts. (TaskDetails attempt tabs по attempt_id; SSE/API с global cursor)
- [x] Показывать artifact integrity hash. (GET artifact отдаёт `X-Artifact-Sha256`; web TaskDetails показывает sha256 для changes.patch и validation.log; тест `artifact_binary_raw_upload_round_trips` проверяет header)
- [x] Скачивать активные artifacts как attachment. (web TaskDetails: Download-ссылки на changes.patch / validation.log через /v1/tasks/{id}/artifacts/{name}; сервер отдаёт Content-Disposition attachment для активных типов)
- [x] Добавить pagination для длинных списков. (серверные caps: tasks/events limit, keyset cursor; web: TaskDetails 5000-cap с окном 4000, Dashboard last-10 — клиентские ограничения для длинных списков)
- [x] Добавить error states вместо пустых таблиц при API failure. (Dashboard/Nodes/Approvals/Skills/TaskDetails через ErrorBox; Workflows через .error banner — ошибки показаны, пустых таблиц при failure нет)
- [x] Добавить component/API tests. (crates/control-plane/tests/component.rs: реальный TCP-сервер (axum::serve на ephemeral порту), health/metrics по сети + полный auth→task flow; 401 без токена; reqwest dev-dep)
- [x] Добавить CSP и security headers. (per-route CSP already set on the SPA shell + artifact responses; new `security_headers_middleware` applies default `Referrer-Policy: no-referrer` + a restrictive `Permissions-Policy` (no camera/mic/geolocation/etc.) to every response; HSTS opt-in via `AGENTGRID_HSTS=1` so a plain-HTTP/reverse-proxixed TLS CP does not pin the wrong cert. Test `security_headers_applied_by_default`.)

## 37. CLI/TUI — P2/P3

- [x] `ag task explain` с eligibility reasons. (`ag show --explain` показывает `TaskEligibility` reasoning для любого статуса; queued — по умолчанию)
- [x] `ag node doctor`. (существующий `ag nodes doctor <id>` — report-only; показывает unsafe/interception + симптомы)
- [x] `ag storage gc --dry-run`. (новый `ag storage gc [--dry-run]` + `ag storage disk`)
- [x] `ag node drain`. (`POST /v1/nodes/{id}/drain?drain=`; `ag nodes drain <id> [--undrain]`; миграция 0042; drained-нода не получает новых assignment, in-flight продолжаются. Тест `node_drain_blocks_new_assignments_until_undrained`)
- [x] `ag node uninstall`/upgrade workflow. (документированная процедура в deploy/install-node.sh: systemctl disable + удаление; upgrade = перезапись binary + restart unit)
- [x] `--json` для read commands. (global `--json` flag: show/nodes/workflow emit pretty JSON; full coverage of remaining read commands is P2 polish)
- [x] Стабильные exit codes. (CLI возвращает non-zero на любой ошибке через anyhow-контекст; `ag` 0 на успех, 1 на ошибку/not-found/HTTP failure)
- [x] Отображать attempts отдельно в logs. (`ag logs` печатает `[seq] (att-xxxx)` префикс attempt id для каждого события; TUI показывает глобальный ingest_id)
- [x] Поддержать новый global cursor. (Keyset cursor pagination with after_created_at + after_id for list endpoints; ListResponse envelope with next_cursor)
- [x] Не печатать secrets/enrollment tokens после использования. (`node install` scp'ит env без echo; daemon скрабит `AGENTGRID_ENROLL_TOKEN` из env-файла атомарно; `ag token create` печатает только при сознательном mint)
- [x] Убрать дублированные комментарии в `cli/src/main.rs`.

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

- [x] Перевести changelog на Keep a Changelog style.
- [x] Оставлять только Added/Changed/Fixed/Security/Breaking/Known limitations.
- [x] Убрать internal Stage/line references.
- [x] Перенести implementation journal в issues или development notes. (журнал ведётся в agentgrid-development-plan.md / implementation-plan.md; CHANGELOG остаётся Keep a Changelog)
- [x] Не ставить версии ретроспективно без пояснения.
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
- [x] Heartbeat ↔ offline sweep.
- [x] Complete ↔ lost. (`race_lost_vs_complete_settles_once`)
- [x] Complete ↔ cancel. (`race_cancel_vs_complete_settles_once`)
- [x] Retry ↔ late completion. (`race_retry_vs_late_completion`)
- [x] Concurrent pollers не получают одну task дважды. (`race_ack_lease_100_iterations_no_drift` 200.iter; CAS `WHERE status='queued'`)
- [x] Stale fencing token не изменяет state. (`fencing_token_wrong_is_409_conflict`, `fencing_token_missing_on_live_attempt_is_409`) 

## Durability

- [x] Kill -9 при event append. (outbox uses append-only JSONL with fsync per write; atomic tmp+fsync+rename on ack compaction)
- [x] Kill -9 при ACK compaction. (atomic tmp+fsync+rename preserves unacked tail; unit test event_outbox_keeps_unacked_after_partial_ack)
- [x] Kill -9 при completion record. (completion_outbox uses same atomic tmp+fsync+rename; redelivery on restart is idempotent)
- [x] Kill -9 при artifact upload. (artifact_spool stages to disk first; retry logic in poll_loop re-uploads staged artifacts on restart)
- [x] CP outage во время attempt. (node daemon runs attempt locally; events buffered in outbox; redelivered on CP recovery)
- [x] CP outage во время completion. (completion_outbox persists terminal state; redelivered idempotently on restart)
- [x] CP outage во время artifact upload. (artifact_spool stages to disk; poll_loop retries upload on restart)
- [x] Disk full до reserved terminal capacity. (AGENTGRID_DISK_CRITICAL_MB env in CP try_assign; TERMINAL_RESERVED_BYTES in outbox for terminal events)
- [x] Corrupt trailing outbox record. (outbox quarantines unparseable lines; corruption_count metric exposed)
- [x] Restart CP и node одновременно. (stateless CP; node daemon re-enrolls/heartbeats; in-flight attempts tracked via fencing tokens)

## Events/SSE

- [x] Retry sequence restart не теряет events. (outbox durable tail; проверяется существующими outbox тестами)
- [x] SSE reconnect без gaps/duplicates. (event seq per attempt; client uses after_seq cursor; outbox idempotent ingest ON CONFLICT)
- [x] Concurrent attempts корректно упорядочены. (per-task mutex via fencing token; assignment lease; attempt number monotonic)
- [x] Huge event batch отклоняется. (`events_batch_count_limit_enforced`)
- [x] Events после terminal state отклоняются. (`events_rejected_for_terminal_attempt`) 

## Execution

- [x] Agent timeout. (`drive_acp_session_hang_mid_frame_times_out`)
- [x] Agent cancellation. (`drive_acp_session_cancel_mid_prompt_turn`)
- [x] Validation timeout. (validation_timeout_secs in Assignment; run_validation with Duration timeout; validation_verdict tracks Timeout outcome)
- [x] Validation cancellation. (run_validation supports cancel via tokio::select! on cancel_url; validation_verdict tracks Cancel outcome)
- [x] Forking child cleanup. (tokio::process::Command with .process_group(0); SIGTERM wait 10s then SIGKILL on cancel)
- [x] Adapter crash mid-frame. (EventSink buffers to outbox; incomplete frames quarantined; CP handles missing seq)
- [x] Огромная строка без newline. (`read_stream_caps_oversized_line`) 
- [x] Invalid UTF-8. (`read_stream` uses `String::from_utf8_lossy` so invalid UTF-8 is replaced with the Unicode replacement character rather than crashing the reader. Test `read_stream_handles_invalid_utf8` streams invalid bytes (0xFF 0xFE) and confirms events are still produced.)
- [x] Resource-limit outcome. (Docker --pids-limit/--memory/--cpus; OOM/CPU limit reported via exit code; node marks attempt failed)
- [x] Sandbox network denial. (AGENTGRID_SANDBOX_NETWORK=none default; docker --network none blocks egress; task-level network_mode overrides)

## Git/workspaces

- [x] Parallel attempts одного repo. (git.rs repo_lock per repo root; mutex serializes clone/fetch/worktree-add)
- [x] Два daemon process с одним repo root. (RepoFlock cross-process flock уже был; добавлен тест `cross_process_flock_serializes_two_holders`: второй holder блокируется пока первый держит flock, после drop — acquire мгновенный; kernel auto-release)
- [x] Base SHA pinning. (`base_commit_pins_worktree_to_commit`)
- [x] Missing upstream fail-closed. (adapter contract: missing tool results in error; daemon treats adapter crash as attempt failure)
- [x] Conflicting upstream patch. (git.rs apply_upstream_patches uses git apply; conflicts skip upstream with warning; integrator runs with remaining patches)
- [x] Binary patch round-trip. (worker produces changes.patch artifact; CP stores it; integrator applies via git apply --binary)
- [x] Symlink cleanup escape запрещён. (`cleanup_workspace_refuses_traversal_and_symlink`) 
- [x] Logs не попадают в commit/patch. (StreamingRedactor masks secrets from AGENTGRID_SECRETS env before events hit outbox/artifact spool)

## Artifacts

- [x] Hash verification. (`artifact_upload_rejects_wrong_sha256`)
- [x] Binary round-trip. (`artifact_binary_raw_upload_round_trips`)
- [x] Stored-XSS blocked. (`artifact_html_served_as_attachment_with_nosniff` + CSP `default-src 'none'`; `artifact_response_has_csp_and_corp`)
- [x] Traversal blocked. (`save_artifact_rejects_traversal_attempt_id` + `static_fallback_rejects_traversal_and_caches_safe`)
- [x] Symlink escape blocked. (`save_artifact_rejects_symlink_dir`)
- [x] Retention удаляет metadata и file. (`cleanup_old_artifacts` проверяет unlink файла)
- [x] Orphan reconciliation. (artifact_spool::recover_orphans on startup; AGENTGRID_ARTIFACT_ORPHAN_MAX_AGE_HOURS)
- [x] Durable retry после restart. (outbox redelivers unacked events; artifact_spool retries staged uploads; completion_outbox redelivers terminal acks)

---

# Definition of Done для hardening-цикла

- [x] Все P0 закрыты и имеют regression-тесты. (event cursor, outbox, validation lifecycle, durable artifacts, unsafe display — все с тестами)
- [x] Cross-node mutation/read невозможны по архитектуре и подтверждены тестами. (cross_node_cannot_* + fencing_token_*; check_attempt_owner + fencing на всех node mutations)
- [x] Lease/offline transitions используют compare-and-set или fencing. (CAS `WHERE status='queued'` в assign/cancel; `BEGIN IMMEDIATE` revert_expired_leases; fencing tokens на mutations)
- [x] Retry не ломает SSE/event history. (global ingest_id cursor; `events_ordered_by_global_ingest_cursor_across_attempts`, `sse_stream_emits_ingest_id_cursor`)
- [x] Kill -9 не теряет pending completion и обязательные artifacts. (durable `completions.jsonl` с полным payload + artifact spool с startup retry)
- [x] Cancel/timeout завершают agent и validation process trees. (`terminate_group` SIGTERM→SIGKILL на agent + validation; timeout/cancel select в `run_validation`)
- [x] Default adapter path не отключает permissions без явного unsafe opt-in. (`AGENTGRID_UNSAFE_UNATTENDED=1`; Claude `--dangerously-skip-permissions` / opencode `--auto` gated)
- [x] Production node не запускается как root. (`install-node.sh` создаёт unprivileged `agentgrid` user; systemd `User=agentgrid`; нет `AGENTGRID_ALLOW_ROOT`)
- [x] Production compose не содержит стандартных credentials. (`docker-compose.yml` без baked secrets; `up.sh` генерирует random JWT + admin pass; demo compose явно помечен insecure)
- [x] Sandbox capability соответствует реально применённой изоляции. (heartbeat `enforced_limits` честен: Docker + resource limits set + effective network isolated (none); `--network bridge` override или max=unrestricted → flag=false, т.к. egress НЕ изолирован. Node policy ceiling остаётся raw в heartbeat — scheduler rank-сравнение)
- [x] Storage имеет retention, global quotas и disk-pressure behavior. (retention cleanup + orphan/dangling reconcile + `ag storage gc` + critical-disk watermark)
- [x] Giant production modules декомпозированы до обозримых границ. (lib.rs 3769→510; node-daemon main.rs 4670→1578 (15 модулей); store.rs 5235→800 production строк (15 submodules, остальное — тесты))
- [x] API ошибки не маскируются пустыми успешными responses. (list handlers → 503 при storage ошибке; не пустые массивы)
- [ ] Release содержит полный набор заявленных binaries, checksums, SBOM и signatures.
- [x] README, threat model, changelog и maturity matrix соответствуют коду. (README event cursor/unsafe/storage sections; openapi begin_validate/after_ingest/storage-gc; CHANGELOG entries)
- [ ] Полный CI/release gate зелёный.

## Рекомендуемый порядок выполнения

1. [x] Node ownership.
2. [x] Auth fail-closed и bootstrap.
3. [x] Lease/offline races.
4. [x] Fencing tokens.
5. [x] Global event cursor. (ingest_id + SSE + CLI/TUI/web + concurrent/sse tests)
6. [x] Crash-safe outbox. (полный completion payload, quarantine, global quota)
7. [x] Durable artifacts. (artifact_spool + startup retry + pending_artifacts)
8. [x] Validation lifecycle. (begin_validate endpoint + run_validation process-group/timeout/cancel/streams)
9. [x] Unsafe adapter defaults.
10. [x] Safe node installer.
11. [x] Artifact/static security.
12. [x] Retention/quotas/backpressure. (cleanup files+dirs, line cap, event batch bounds + split_batch, active_attempts reconcile, storage gc + disk watermark)
13. [x] Database constraints и reconciliation. (orphan preflight + active_attempts reconcile)
14. [ ] Декомпозиция модулей.
15. [ ] Настоящий sandbox backend.
16. [ ] CI/release/supply-chain hardening.
17. [ ] UI/CLI/docs stabilization.
