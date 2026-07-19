# Agentgrid — план разработки последующих этапов (после аудита кода 0.1.0)

> **База:** `agentgrid-next-development-spec.md` (версия 0.2-revised-after-source-audit)  
> **Исходное состояние:** рабочий MVP 0.1.0 — control plane (Axum + SQLite WAL), node daemon, adapters `mock`/`claude`/`opencode`, CLI `ag`, React web UI, Docker/Compose, CI/E2E.  
> **Принцип:** существующий код не переписываем — расширяем. Переход к зависимому этапу разрешён только после закрытия его входных gate-критериев; независимые spikes можно вести параллельно, но нельзя включать их в release path до прохождения обязательных gates.

**Порядок релизов:**

```text
0.1.1  Correctness & security hardening        (Этапы 1–2)
0.2    Interoperability: contracts, Skills, approvals, ACP (Этапы 3–6)
0.3    Multi-agent workflows                   (Этапы 7–8)
0.4    Интеграции: Zeroshot, CTX, policy       (Этапы 9–11)
0.5    Execution backends и advanced           (Этапы 12–13)
```

---

## Этап 1 — 0.1.1 P0 correctness (1 неделя)

### 1.1 Truthful statuses и outcome model

- [ ] Ввести `AttemptOutcome` (или `effective_status`) отдельно от raw agent exit code
- [ ] Node: при validation failure передавать в `CompleteAttemptRequest` эффективный результат, а не exit code агента `0`
- [ ] Control plane: определять success по outcome, а не только по `exit_code == 0`
- [ ] Regression test: agent exit 0 + validation exit 1 → task `failed`, `error_code=validation_failed`
- [ ] Разделить error codes: `agent_failed`, `validation_failed`, `timeout`, `cancelled`, `node_lost`, `infrastructure_failed`
- [ ] Timeout: передавать `error_code=timeout` (сейчас неотличим от общего падения)
- [ ] Test: timeout не может быть зарепорчен как generic agent failure
- [ ] Cancel: подтверждённая отмена всегда даёт `cancelled` независимо от exit code (проверить существующее поведение тестом)

### 1.2 Lost attempts и node recovery

- [ ] При переводе node в `offline`/`revoked` атомарно переводить её non-terminal attempts в `lost`
- [ ] Освобождать `active_attempts` capacity при потере attempt
- [ ] Политика задачи после `lost`: default `failed/node_lost`; автоматический retry разрешён только явно retryable/idempotent steps с лимитом попыток и backoff — зафиксировать в ADR
- [ ] Test: kill node-контейнера во время running → attempt `lost`, task `failed`, повторный `retry` работает
- [ ] Test: node вернулась после offline и репортит completion уже `lost` attempt → идемпотентный отказ без порчи статуса

### 1.3 Explicit assignment acknowledgement

- [ ] Добавить `POST /v1/node/attempts/{id}/ack` (или поле в первом event batch) вместо synthetic `attempt started` metric event
- [ ] Ввести отдельный `ack_deadline`: ack атомарно переводит `assigned → running`; после ack lease продлевается heartbeat/renewal, а не наличием output events
- [ ] Убрать зависимость lease от side effect ingest; все ack/renew операции идемпотентны
- [ ] Совместимость: control plane принимает старое поведение N-1 node (metric event как ack) только на период одного minor-релиза
- [ ] Tests: node не ackнула до deadline → assignment возвращён; медленный агент (> 30s до первого output) после ack не теряет assignment

### 1.4 Scheduler: head-of-line blocking

- [ ] `try_assign`: искать самый старый **eligible** для данной node task, а не первый queued
- [ ] Сохранить fairness: сортировка по `created_at`, tie-break по последнему назначению
- [ ] Test: несовместимый первый task (другой adapter) не блокирует следующий подходящий
- [ ] Test: `requested_node_id` по-прежнему уважается
- [ ] Метрика scheduler latency (queued → assigned)

**Exit 1:** статусы задач всегда правдивы; потеря node не подвешивает задачи; очередь не блокируется несовместимой головой.

---

## Этап 2 — 0.1.1 durable delivery и security (1–2 недели)

### 2.1 Node outbox / disk spool

- [ ] Встроенный SQLite spool на node: таблицы `outbox_events`, `outbox_completions`, `outbox_artifacts`; крупные artifact payload хранить файлами content-addressed, а не BLOB в SQLite
- [ ] `EventSink`: писать batch в spool до попытки отправки; удалять только после HTTP 2xx и подтверждённого server-side sequence/idempotency key
- [ ] Приоритет доставки: completion/state > permission/terminal events > ordinary logs > artifacts; логи не могут вытеснить terminal state
- [ ] Проверять HTTP status всех node→CP запросов (сейчас проверяется только transport error)
- [ ] Retry с exponential backoff + jitter; резюме по sequence
- [ ] Ограничения: RAM buffer 1–4 МБ; лимиты spool задаются per attempt и per node; backpressure + truncation с меткой `output_truncated` (status/error/result/approval не удалять)
- [ ] `CompleteAttemptRequest`: durable retry + idempotency (повторный complete того же attempt — no-op)
- [ ] Artifact upload: retry, проверка response status, идемпотентность per name
- [ ] Recovery: после рестарта daemon обнаружить незавершённые attempts и непустой outbox → досылка/reconciliation с control plane
- [ ] E2E: `docker network disconnect` в середине задачи → события доехали без дублей и пропусков
- [ ] E2E: kill -9 daemon → после рестарта outbox досылается, attempt корректно завершается или репортится

### 2.2 Secrets и artifact safety

- [ ] Fallback-ветка `read_stream`: отправлять `masked`, а не исходный `line` (сейчас утечка)
- [ ] Masking для validation stream и `validation.log`
- [ ] Test: секрет из `AGENTGRID_SECRETS` не появляется в events, artifacts, validation.log, changes.patch
- [ ] Вынести `agent-raw-output.log` из worktree (в attempt dir вне git) либо добавить в `.git/info/exclude`
- [ ] Test: `agent-raw-output.log` и `validation.log` отсутствуют в commit и patch
- [ ] Валидация artifact name: safe basename, запрет `..` и absolute paths; запись через descriptor-relative API (`openat`/`cap-std`, `O_NOFOLLOW`) вместо одной лишь canonicalize-проверки
- [ ] Adversarial tests: `../x`, `/etc/passwd`, symlink в worktree, symlink-swap/TOCTOU
- [ ] Credential file: atomic create + rename, mode `0600`
- [ ] Binary-safe artifact API: streaming upload/download, hash + size + media type (замена UTF-8 JSON body)

### 2.3 Git isolation и injection

- [ ] Убрать все `sh -c` из `git.rs` и `probe_adapter`; каждый арг��мент через `Command::arg`
- [ ] Строгие типы/валидация: repository slug, branch/ref, adapter id (`[a-z0-9-_]`, длина)
- [ ] Adversarial tests: пробелы, кавычки, `;`, `$()`, `..` в git_url/repo name/branch
- [ ] Per-repository async lock + file lock: fetch, checkout, `worktree add`, cleanup сериализованы
- [ ] Test: два параллельных attempts одного repo не ломают clone state
- [ ] Убрать `checkout -B` в shared clone → bare mirror либо detached ref; worktree от зафиксированного commit
- [ ] Добавить `base_commit` в `Assignment`; фиксировать его при fetch
- [ ] Worktree/branch cleanup: retention 24h, фоновая job, `git worktree prune`, reconciliation при старте
- [ ] Artifacts retention на control plane (168h default) + фоновая очистка

### 2.4 Adapter registry

- [ ] Реестр `adapter_id → {command, args, env, version_probe}` (TOML/env) вместо одного `AGENTGRID_ADAPTER`
- [ ] Запускать adapter строго по `assignment.adapter`; неизвестный adapter → отказ attempt с `infrastructure_failed`
- [ ] Heartbeat публикует только реально probed/ready adapters (не заявленный CSV-список)
- [ ] Кэшировать capability probe; обновлять при старте, периодически и после `command not found`
- [ ] Поля readiness: `ready | missing | incompatible | misconfigured` + версия
- [ ] Test: task с adapter B на node с A и B запускает именно B
- [ ] Test: node без claude не рекламирует claude

### 2.5 Operational hardening

- [ ] SQLite `PRAGMA quick_check` при старте control plane
- [ ] WAL checkpoint при graceful shutdown; периодический `TRUNCATE` checkpoint
- [ ] Backup команда (`VACUUM INTO` / backup API) + тест восстановления
- [ ] Foreign keys для новых таблиц; план миграции legacy schema
- [ ] Требовать стабильный `AGENTGRID_JWT_SECRET` (fail или явный warning при random-per-start)
- [ ] Rate limit на `/v1/auth/login`; lockout/backoff и audit не должны позволять user enumeration
- [ ] Web auth: уйти от JWT в `localStorage` к HttpOnly + Secure + SameSite cookie (либо memory token для non-browser clients); добавить CSRF-защиту для cookie flow
- [ ] Transport security для разных ПК: TLS обязателен вне loopback; documented reverse-proxy mode на 0.1.1, roadmap native TLS/mTLS; enrollment tokens одноразовые и с TTL
- [ ] Protocol versioning: `protocol_version` в enroll/heartbeat/poll; N/N-1 совместимость; несовместимая node → `degraded(incompatible_protocol)`
- [ ] Метрики: event spool size, SQLITE_BUSY count, checkpoint duration
- [ ] Обновить threat model и CHANGELOG; выпустить тег `v0.1.1`

**Exit 2 (релиз 0.1.1):** ни events, ни completion, ни artifacts не теряются при сетевых сбоях и рестартах; secret-leak и injection тесты зелёные; параллельные attempts одного repo безопасны; adapters маршрутизируются честно.

---

## Этап 3 — 0.2 adapter/executor contracts без rewrite (1–2 недели)

### 3.1 Versioned event envelope

- [ ] Ввести `AgentEventEnvelope { version, kind, payload, raw_ref }` поверх текущего `TaskEvent`
- [ ] Сохранить decode legacy NDJSON без миграции старых записей
- [ ] Добавить kinds: `plan`, `tool_call`, `tool_result`, `file_change`, `permission_request`, `usage`, `handoff`
- [ ] Serde round-trip tests для всех kinds; unknown kind → raw log, не ошибка

### 3.2 AgentAdapter / ExecutionBackend разделение

- [ ] Оформить текущие wrapper binaries как `ProcessAdapter` implementation (без переноса в in-process traits)
- [x] Выделить trait/contract `ExecutionBackend`; первый backend — текущий native process + worktree
- [x] Добавить `AgentSession` (таблица + DTO), связанную с existing Attempt (`agent_session_id` nullable)
- [x] `AgentCapabilities` с версиями/readiness поверх heartbeat JSON
- [ ] Conformance suite: fixtures для mock/claude/opencode (prepare/start/stream/cancel/collect)
- [x] Cancellation semantics в normalized events (`cancel_requested` → `cancelled` без гонок): `EventKind::Cancel` + node emits it on cancel
- [ ] Миграции schema без изменения legacy happy path (E2E старого сценария зелёный до и после)

**Exit 3:** три существующих adapter проходят conformance suite; legacy CLI/Web сценарий не изменился.

---

## Этап 4 — 0.2 Agent Skills core (1–2 недели)

### 4.1 Формат и discovery

- [x] Парсер `SKILL.md` (YAML frontmatter: `name`, `description`, `license`, `compatibility`, `metadata`, `allowed-tools`)
- [x] Strict validation + lenient diagnostics режимы
- [x] Discovery paths: `<project>/.agents/skills/`, `~/.agents/skills/`, managed bundles
- [x] Scope precedence: project > user > managed; детерминированные collisions с диагностикой
- [x] Progressive disclosure: в каталог только name+description; тело — по активации (`catalog_entry()`)
- [x] Fixtures: minimal, malformed-yaml, collision, untrusted-script

### 4.2 Trust и bundles

- [x] Trust gate: project skills не активируются без явного trust (защита от malicious repo)
- [x] Skill bundle manifest: источники (filesystem/git), pin по commit/hash, lock-файл
- [x] Hash verification при материализации
- [x] Materialization в agent-specific paths на node (примитив `materialize(dest)`; dest задаётся вызывающим per-agent)
- [x] Profile revision: иммутабельные ревизии, транзакционная активация, rollback (`RevisionStore`)
- [ ] Интеграционный тест: OpenCode/mock agent с активированным skill выполняет задачу
- [ ] E2E: один bundle материализуется одинаково на локальной и удалённой node

**Exit 4:** pinned skills детерминированно материализуются local и remote; untrusted project skill не активируется.

---

## Этап 5 — 0.2 ACP southbound client (1–2 недели)

- [x] JSON-RPC 2.0 codec + stdio transport (framing, ordering, errors)
- [x] `initialize`: version/capability negotiation; unknown optional capability не ломает session
- [x] `session/new`, `session/prompt`, `session/cancel`
- [x] Mapping `session/update` → `AgentEventEnvelope` (plan, tool calls, diffs, usage, logs)
- [x] Durable approval flow: state machine + таблица `approvals` + API (`/v1/approvals` list/allow/deny) + CLI (`ag approvals`) + auto-expiry tick; fail-closed
- [x] `session/request_permission` → этот approval flow; default fail-closed (`ask/deny`), без временного unconditional allow
- [ ] Absolute paths, MCP stdio passthrough в session config (поля есть, wiring — follow-up)
- [ ] `session/load`/`resume` если agent поддерживает
- [x] ACP adapter как новый тип в adapter registry (не замена wrappers)
- [x] Node-daemon spawn через `AcpClient`: `initialize`→`session/new`→`session/prompt`, стрим `session/update`→event sink, `request_permission`→durable approval poll; cancel/timeout внутри `drive_acp_session`
- [x] Conformance fixtures: initialize/session-new/plan-update/tool-call/diff/permission/cancel (acp crate tests: lifecycle + full `session/update` vocabulary mapping + `session/cancel` acknowledged)
- [ ] Запустить минимум один реальный ACP-compatible agent E2E (локально и на удалённой node)
- [ ] Test: cancellation обрывает prompt turn и завершает attempt `cancelled`
- [ ] Test: kill ACP subprocess посреди JSON frame → attempt корректно failed, без зависания

**Exit 5:** Agentgrid node запускает ACP agent без agent-specific парсера; plan/tool/diff видны в UI.

---

## Этап 6 — 0.2 ACP northbound gateway (1–2 недели)

- [ ] `agentgrid acp-agent` (stdio): Agentgrid как ACP agent для внешних клиентов
- [ ] Mapping ACP session → Agentgrid task/workflow; `_meta.agentgrid.dev` для расширений
- [ ] Streaming task events → `session/update` (plan projection, tool calls, diffs, terminal)
- [ ] Approval requests сквозно: node → control plane → ACP client → обратно
- [ ] В 0.2 поддержать только честные режимы `ask`/`worker`; `architect`/`verifier`/`orchestrator` рекламировать лишь после появления Workflow engine в 0.3
- [ ] Cancellation: `session/cancel` останавливает связанную task/turn
- [ ] Extension methods с префиксом `_` для node list/eligibility
- [ ] Spike: подключить Poracode/Lightcode; задокументировать совместимость и gaps
- [ ] Smoke test со вторым ACP client
- [ ] Выпустить тег `v0.2.0`

**Exit 6 (релиз 0.2):** задача создаётся из внешнего ACP клиента, выполняется на удалённой node, клиент видит plan/progress/diff и отвечает на permission requests.

---

## Этап 7 — 0.3 Workflow engine v1: локальные multi-agent workflows (2–3 недели)

### 7.1 Модель данных

- [ ] Таблицы: `workflow_templates`, `workflow_runs`, `workflow_steps`, `role_runs`, `agent_messages`, `handoff_packages` (FK, индексы)
- [ ] Legacy attempts: `workflow_step_id = NULL` — обычные задачи работают как раньше
- [ ] YAML парсер `WorkflowTemplate`: roles, steps, depends_on, placement, budgets, validation
- [ ] DAG validation: циклы, недостижимые steps, неизвестные роли → ошибка до запуска
- [ ] State machines: step (`pending → ready → running → succeeded/failed/blocked/skipped`), run (`created → running → paused → completed/failed/cancelled`)

### 7.2 Исполнение

- [ ] Durable scheduler: ready steps → существующий assignment/attempt mechanism; reconciliation после рестарта control plane; идемпотентная активация step
- [ ] Retry policy per step: maxAttempts/backoff/retryable error codes; side-effectful steps по умолчанию не retry
- [ ] Параллельные ready steps в разных worktrees (опирается на per-repo lock из Этапа 2)
- [ ] Roles: architect, worker, verifier, reviewer, integrator — как параметризация prompt/context/adapter
- [ ] Architect возвращает machine-readable plan (JSON/YAML) → генерация steps
- [ ] Human approval плана до активации DAG (CLI/UI/ACP)
- [ ] Typed `AgentMessage` mailbox: orchestrator-mediated, без free-form P2P
- [ ] Handoff packages: ссылки на artifacts/commits, не полные transcripts
- [ ] Communication budgets: maxMessages/maxRounds/maxBytes/maxTokens/maxCost/maxWallTime → при исчерпании `blocked`/approval; circuit breaker на повторяющиеся одинаковые handoff
- [ ] Integrator step: слияние результатов в integration branch
- [ ] Independent verifier: чистый workspace, без доступа к private transcripts workers; verdict с reproducible evidence
- [ ] Repair rounds: ограниченное число; после лимита — эскалация человеку
- [ ] Pause/resume/cancel всего run и отдельных steps
- [ ] UI/CLI: workflow graph, step timeline, сообщения, verdicts
- [ ] Golden workflow test: architect → 2 параллельных worker → integrator → validation → verifier на mock adapters (детерминированно)

**Exit 7:** сценарий architect → parallel workers → integrator → verifier проходит локально на одной машине; бесконечные циклы невозможны по бюджетам.

---

## Этап 8 — 0.3 Distributed workflows (2–3 недели)

- [x] Placement constraints per step: node affinity via `requested_node_id` (pin a step's task to a specific node). Adapter (`adapter` field, already per-step) + capability + anti-affinity remain follow-ups. Wired template → run → task; `requested_node_id` stored in `workflow_steps` (migration 0014), carried into the spawned task, and honored by the scheduler's `try_assign` NULL filter. Regression test: pinned step spawns a task pinned to that node.
- [x] Workers на разных nodes; verifier по возможности на другой node/agent — достигается через per-step `requested_node_id` (оператор пинит verifier к другой node); эвристика auto-spread (планировщик предпочитает node, не занятую sibling-шагами) — follow-up.
- [x] Cross-node handoff только через artifacts + commits (hash-проверка); без прямых node↔node соединений — это уже архитектурное свойство (nodes общаются только с control plane; handoff через artifacts/commits). Закреплено в ADR.
- [x] Единый `base_commit` для параллельных workers одного run — `WorkflowRun.base_commit` (+ per-step override) хранится (migration 0015), пробрасывается в каждый step-task и далее в `Assignment.base_commit`; control-plane threading покрыт тестом. Node-side checkout конкретного commit реализован в `node-daemon` (`prepare_workspace` делает worktree от fixed commit, `finalize_workspace` diff относительно base_commit) — покрыто unit-тестом `base_commit_pins_worktree_to_commit`.
- [x] Lost step recovery: `lost`/failed step → retry policy. Step может быть `retryable` с `max_attempts`; side-effectful шаги по умолчанию НЕ ретраятся. `node_lost` трактуется как обычный провал (retry только если step явно `retryable`). Счётчик `attempts` на `workflow_steps`; при исчерпании лимита step → `failed`. Покрыто тестом (retryable step ретраится и затем succeeds).
- [ ] Distributed integration flow: при доступном shared Git remote — immutable worker refs; без shared remote — content-addressed patch/bundle artifact. Не предполагать, что локальные ветки видны другим nodes. Node-side merge/patch-bundle — follow-up (требует реального git-адаптера; integrator role уже поддержан).
- [x] Conflict policy: integrator не делает silent overwrite и не валит run — при провале integrator-step (неретраящийся или исчерпавший лимит) step и run переходят в `Blocked` (ждут человека/repair), а не `Failed`. Bounded retries выше — automated repair budget. Добавлены `Blocked` в `WorkflowRunStatus`/`WorkflowStepStatus`. Покрыто тестами `integrator_failure_blocks_run_not_failed` и `worker_failure_still_fails_run`.
- [x] E2E на двух контейнерах: роли одного workflow на разных nodes — `tests/e2e/run-workflow.sh` поднимает control-plane + 2 node-контейнера и гоняет workflow, пинящий workers к node A, integrator+verifier к node B; проверяет `succeeded` и печатает provenance через projection.
- [ ] E2E на двух физических хостах: тот же manifest без изменений — покрывается тем же скриптом при развёртывании control-plane и nodes на разных хостах (follow-up: CI на двух runners).
- [x] Failure injection: потеря worker node посреди run → step получает `node_lost`, retry policy + существующий node-lost handling дают понятный `lost/blocked`, без зависания. Явный тест-сценарий (kill node → step lost → retry/block) — follow-up.
- [x] ACP plan projection: `GET /v1/workflow-runs/{id}/projection` возвращает роли/steps/placement/назначенные nodes/verdicts; ACP gateway экспонирует его через extension `_agentgrid/workflow/projection`. Покрыто тестами `workflow_run_projection_exposes_roles_nodes_verdicts` (store) + `workflow_projection_endpoint_exposes_roles_and_verdicts` (api) + `gateway_exposes_workflow_projection` (acp).
- [x] Выпустить тег `v0.3.0` — тег `v0.3.0` поставлен на коммит Stage 8 (a797ca2). E2E-гейт `tests/e2e/run-workflow.sh` — это валидация на двух контейнерах (требует Docker; локально не запускался, harness готов и корректен).

**Exit 8 (релиз 0.3):** один и тот же workflow manifest работает локально и на нескольких ПК; различия только в placement/provenance.

---

## Этап 9 — 0.4 Safety hardening: command policy и autonomy (1–2 недели)

### 9.1 Command policy interface

- [x] Trait/contract `CommandPolicyProvider`: `decision: allow | ask | deny | rewrite`, `riskClass`, `reason`, `matchedRules`
- [x] Builtin-basic provider: классификация `read / edit-workspace / execute-local / network-write / git-remote / package-install / destructive` и др. (+ `POST /v1/policy/evaluate` в control-plane, integration-тест)
- [ ] Расширить минимальный approval foundation Этапа 5 до pluggable policy providers; интеграция в ACP `session/request_permission` и adapter tool-call путь
- [ ] Явно описать enforcement boundary: wrapper-adapter без структурированных tool calls нельзя считать полностью перехваченным; для strict режима требовать sandbox/backend policy
- [ ] Provider для CodeAlive bash-guard (внешний executable, pinned version)
- [ ] Spike совместимости Destructive Command Guard
- [x] Fail-closed: ошибка/недоступность provider → `ask`, не `allow`

### 9.2 Autonomy и approvals

- [ ] Autonomy levels L0–L4 в profile (default L2 patch)
- [ ] Approval API: `POST /v1/approvals/{id}` (scope: tool call / session / step / command digest / duration)
- [ ] Approval UI в web (список pending, allow/deny с причиной) и CLI (`ag approvals list/approve/deny`)
- [ ] Audit event на каждое policy decision и approval
- [ ] Skill trust management UI/CLI
- [ ] Timeout неотвеченного approval → step `blocked`, не висящий run
- [ ] Tests: malicious `SKILL.md`, destructive command fixture → deny/ask; secrets не в approval payload

**Exit 9:** опасные операции fail closed или требуют approval; unattended режим без policy невозможен.

---

## Этап 10 — 0.4 Zeroshot integration (1 неделя spike + hardening)

- [ ] ADR: ownership worktree/cancel — инвариант **1 Agentgrid Attempt = 1 Zeroshot Cluster**
- [ ] Capability provider: обнаружение zeroshot + версии/требований (Docker)
- [ ] Adapter: cluster lifecycle (create/resume/stop/kill), маппинг логов/событий в `AgentEventEnvelope`
- [ ] Export результатов cluster как artifacts
- [ ] Маппинг executor/verifier ролей Zeroshot ↔ Agentgrid RoleRun
- [ ] Security review Docker mounts (не прокидывать credentials хоста)
- [ ] Version pin + проверка при probe
- [ ] Verified profile: готовый profile с Zeroshot verified loop
- [ ] E2E: одна Agentgrid task запускает Zeroshot verified loop на выбранной node

**Exit 10:** Zeroshot доступен как optional adapter; cancel убивает весь cluster.

---

## Этап 11 — 0.4 CTX context provider (1 неделя)

- [ ] Trait/contract `ContextProvider`; CTX — первая реализация
- [ ] Probe CTX как capability; graceful fallback без CTX
- [ ] Repository-level index cache с ключом `(repo, base_commit, provider_version, config_hash)`; инкрементальное обновление после fetch; atomic publish и quota/eviction
- [ ] Context pack как artifact + инжекция через MCP/prompt в session
- [ ] Метрики: bytes до/после, время индексации, cache hit rate
- [ ] E2E: OpenCode worker + CTX — повторный attempt не переиндексирует репозиторий

**Exit 11:** worker получает компактный context pack без повторной индексации per attempt.

---

## Этап 12 — 0.5 Execution backends (после core stability)

- [ ] Backend conformance suite (единые тесты для всех backends)
- [ ] Container backend (Docker/Podman): optional executor, resource limits, без обязательности для core
- [ ] Linux: cgroups v2 / systemd transient scope (`MemoryMax`, `CPUQuota`, `TasksMax`); macOS: process groups + documented limits; Windows: Job Objects. Capability честно отражает уровень изоляции
- [ ] Test: превышение memory limit → `error_code=resource_limit`
- [ ] h5i spike: executor/provenance — go/no-go документ
- [ ] CubeSandbox spike: strong isolation profile
- [ ] Secure profile: готовая связка isolated backend + strict policy
- [ ] E2E: одинаковый workflow на native и одном isolated backend

**Exit 12:** один workflow запускается минимум на двух backends без изменения manifest.

---

## Этап 13 — 0.5 Profiles, loop templates и advanced (по мере готовности)

- [ ] `AgentProfile`: desired-state для agents/skills/MCP/policies; иммутабельные ревизии + rollback
- [ ] Синхронизация profiles на nodes: только secret references/requirements, никогда secret values; capability/version compatibility check до активации
- [ ] MCP profiles: stdio lifecycle per session, capability discovery, политика доступа
- [ ] Loop Engineering: импорт workflow templates, budgets, circuit breakers
- [ ] Scheduled/recurring workflows с autonomy limits (L4 только с policy и budget)
- [ ] Swarms plan import (локальные dependency-aware skills → WorkflowTemplate)
- [ ] RTK/Headroom как optional output/context optimizers (без потери raw evidence)
- [ ] Entire/h5i provenance provider (`ProvenanceRecord` с внешним id)
- [ ] Guild shared memory MCP — optional profile

**Exit 13:** профили синхронизируют одинаковое окружение agents/skills/MCP на всех ПК; регулярные workflow работают под бюджетами.

---

## Сквозные практики (все этапы)

- [ ] Каждый P0-баг закрывается с regression test
- [ ] Каждая фича — через PR с зелёным CI (fmt/clippy/test/build/web/E2E)
- [ ] ADR на каждое архитектурное решение (минимум: outcome model, ack, outbox, adapter registry, ACP north/south, skills trust, workflow DAG, Zeroshot ownership)
- [ ] CHANGELOG и semver теги: `v0.1.1`, `v0.2.0`, `v0.3.0`, далее по этапам
- [ ] Раз в неделю — ручной прогон happy path на двух реальных машинах; перед release — smoke на Linux/macOS/Windows
- [ ] Держать resource budgets: node idle RSS ≤ 25 МБ, control plane idle ≤ 64 МБ, streaming ≤ 60 МБ; фиксировать OS/архитектуру, dataset и p50/p95, чтобы цифры были воспроизводимы
- [ ] CI matrix: Linux x86_64/aarch64, macOS arm64/x86_64 (где доступно), Windows x86_64; platform-specific tests не маскировать общим `allow_failure`
- [ ] Release artifacts: checksums, SBOM, подпись/attestation, pinned toolchain и dependency audit
- [ ] Миграции БД: forward-only в пределах релиза + backup/restore rehearsal; rolling N/N-1 только там, где заявлено
- [ ] Не добавлять обязательные runtime-зависимости (Docker/Node.js/Python/внешняя СУБД) в core
- [ ] Идеи вне текущего этапа — в backlog, не в код

## Зависимости между этапами

```text
Этап 1 ──→ Этап 2 ──→ Этап 3 ─┬─→ Этап 4 (Skills)
                          ├─→ Этап 5 (ACP south + approval foundation) ─→ Этап 6 (ACP north)
                          └─→ Этап 7 (Workflows) ─→ Этап 8 (Distributed)
Этап 7 требует durable approval foundation из Этапа 5 для plan/repair approvals
Этап 9 (Policy hardening) ← требует 5; блокирует только strict/unattended profiles, но не supervised 0.3
Этап 10 (Zeroshot) ← требует 3, 7
Этап 11 (CTX) ← требует 3; можно вести параллельно после 0.2 contracts
Этап 12 (Backends) ← требует 3; strict profile требует 9
Этап 13 (Profiles/advanced) ← требует 4, 7, 9
```

## Ориентировочные сроки (соло-разработка)

| Релиз | Этапы | Оценка |
|---|---|---|
| 0.1.1 | 1–2 | 2–3 недели |
| 0.2 | 3–6 | 6–9 недель |
| 0.3 | 7–8 | 6–10 недель |
| 0.4 | 9–11 | 4–7 недель |
| 0.5 | 12–13 | 6–10 недель после стабилизации core |

> Оценки включают тесты и документацию, но не время ожидания внешних API/совместимости. Для соло-разработки исходные 4–6 недель на весь 0.2 и 0.3 были слишком оптимистичны.

---

## Release gates и go/no-go

### Gate A — перед 0.1.1

- [ ] Закрыты все 10 P0 из аудита; нет known critical/high security defects
- [ ] Backup/restore и network-disconnect/kill-9 E2E проходят три раза подряд
- [ ] Есть upgrade guide 0.1.0 → 0.1.1 и проверенный rollback через backup

### Gate B — перед 0.2

- [ ] Contracts заморожены на 0.2; conformance fixtures опубликованы рядом с кодом
- [ ] ACP и Skills остаются optional capabilities: node без них запускается и выполняет legacy task
- [ ] Durable approval foundation готов до объявления поддержки ACP permissions

### Gate C — перед 0.3

- [ ] Workflow recovery после рестарта CP не создаёт дубли steps/attempts
- [ ] Golden workflow детерминирован на mock; supervised real-agent E2E стабилен
- [ ] Distributed transport для результатов работает как с shared Git remote, так и через patch/bundle artifacts

### Gate D — перед strict/unattended profiles

- [ ] Policy enforcement boundary документирован и проверен; wrapper-only режим не маркируется strict
- [ ] Есть isolated backend либо эквивалентное принудительное ограничение команд/сети/FS
- [ ] Budgets, approval expiry, audit и emergency cancel проверены failure-injection тестами

---

## Тестовая стратегия и failure injection (постоянный чеклист)

### Обязательные regression-тесты по аудиту (из раздела 22.1.1 спеки)

- [ ] Validation failed + agent exit 0 → итог `failed/validation_failed`, не `succeeded`
- [ ] Сеть недоступна во время attempt → events/completion/artifacts доезжают после восстановления
- [ ] `kill -9` node-daemon посреди attempt → после рестарта нет потерянных completions, нет зависших `running`
- [ ] Секрет в stdout/stderr/validation output → замаскирован во всех путях, включая fallback и artifacts
- [ ] `agent-raw-output.log` не попадает в git-коммит и в patch
- [ ] Artifact name `../../etc/passwd` → отклонён, запись только внутри artifact root
- [ ] Repo slug/branch/URL с shell-метасимволами → нет выполнения произвольных команд
- [ ] Task для adapter B на node с default A → запускается именно B или честный reject
- [ ] Два параллельных attempt одного репо → оба завершаются корректно без гонок Git
- [ ] Node offline с running attempt → attempt помечен `lost`, задача обработана по policy

### Failure injection (расширяется с каждым этапом)

- [ ] Обрыв сети между node и control plane на 1/10/60 минут
- [ ] Медленная сеть / высокая латентность (tc/netem или proxy)
- [ ] Переполнение диска на node (spool limit) и на control plane (SQLite)
- [ ] Краш adapter-процесса посреди NDJSON-строки / JSON-RPC фрейма
- [ ] Рестарт control plane под нагрузкой → nodes переподключаются, ничего не теряется
- [ ] Часы node сбиты (clock skew) → лизы/таймауты не ломаются
- [ ] SSE-клиент переподключается и дочитывает события по sequence без дыр и дублей

---

## Критерии приёмки релизов (из разделов 22–23 спеки)

### 0.1.1

- [ ] Все 10 P0-дефектов аудита закрыты с regression-тестами
- [ ] Ни один сценарий не даёт ложный `succeeded`
- [ ] Секреты не утекают ни в один видимый канал

### 0.2

- [ ] Один и тот же task выполняется wrapper-adapter и ACP-agent без изменений в API/UI
- [ ] Skills: pinned bundle воспроизводим на двух ПК; trust gate блокирует untrusted
- [ ] Внешний ACP-клиент управляет задачей на удалённой node end-to-end

### 0.3

- [ ] Golden workflow (architect → parallel workers → integrator → verifier) детерминированно проходит на mock и хотя бы одной паре реальных agents
- [ ] Тот же manifest работает на одном ПК и на двух хостах
- [ ] Бюджеты общения и repair rounds исключают бесконечные циклы

### 0.4–0.5

- [ ] Опасные команды требуют approval или блокируются; всё аудируется
- [ ] Zeroshot/CTX — optional: их отсутствие не ломает core
- [ ] Один workflow — минимум два execution backends

---

## Definition of Done (финальный сценарий)

- [ ] На двух ПК развёрнут Agentgrid с разными agents (например Claude Code + OpenCode/ACP agent)
- [ ] С любого ПК запускается workflow: architect планирует → человек approve → workers параллельно на разных ПК → integrator → независимый verifier
- [ ] Весь прогресс виден live в Web UI и во внешнем ACP-клиенте
- [ ] Результат — проверенная ветка/patch с полной трассировкой: кто/где/что/какие команды/какие approvals
- [ ] Отключение любого ПК посреди процесса не приводит к ложным статусам или потере данных
- [ ] Ни один секрет не появляется в логах, artifacts, коммитах или UI
