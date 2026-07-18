# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Fixed (Stage 1 — 0.1.1 correctness)
- control-plane (1.1): decide task success from the adapter **outcome** (`error_code`), not raw `exit_code==0`. A validation failure that exits 0 is now `failed`/`validation_failed`, never silently `succeeded`. Adapter timeout reports a distinct `error_code="timeout"`.
- control-plane (1.2): a node that goes `offline` (heartbeat lapse) or is `revoked` atomically loses its in-flight `assigned`/`running`/`validating` attempts (→ `lost`) and fails the owning task with `error_code="node_lost"`, freeing capacity. Late completions on a lost attempt are treated as idempotent no-ops.
- control-plane (1.4): scheduler no longer blocks on an incompatible head-of-line task — it scans queued tasks (oldest-first) and assigns the first the node can run, instead of touching only the single oldest.
- control-plane (1.3): explicit assignment acknowledgement. An attempt gains an `ack_deadline` (30s); the node daemon calls `POST /v1/node/attempts/{id}/ack` on spawn. An unacked assignment is reverted and the task re-queued by `tick_maintenance`; an acked (running) attempt is never reverted. Legacy `metric "attempt started"` events still act as an ack (N-1 node compatibility).

### Fixed (Stage 2 — 0.1.1 durable delivery & security)
- node-daemon (2.2): stop leaking secrets. The non-JSON stdout/stderr fallback now sends the **masked** line, not the raw `line` (the raw disk mirror was already masked). `mask_secrets` is unit-tested.
- node-daemon (2.1): verify the HTTP status on every node→CP call (event flush, completion, artifact upload) instead of only checking transport errors; a 5xx/429 now triggers retry with exponential backoff. A failed event batch is returned to the buffer for the flusher loop to retry while the daemon runs; completion retries until delivered (then gives up, letting the CP lease revert the attempt). Retryable-status logic is unit-tested.
- control-plane (2.5): run `PRAGMA quick_check` on startup and refuse to serve a corrupt database; warn loudly when `AGENTGRID_JWT_SECRET` is unset (a random-per-start secret invalidates previously issued node tokens after a restart).
- node-daemon (2.3): drop `sh -c` from git operations and `probe_adapter`; every git arg is passed via `Command::arg`, and `repository`/`task_id`/`default_branch`/`git_url` are validated (no shell metacharacters, no `..`, no absolute paths). Adversarial tests assert injection attempts are rejected.
- node-daemon (2.4): run strictly the adapter the control plane assigned (`adapter-<id>` binary on PATH); an unknown or missing adapter fails the attempt with `error_code="infrastructure_failed"` instead of silently falling back. Heartbeat probes every configured adapter and reports `degraded` if any binary is missing. The single `AGENTGRID_ADAPTER` env var is removed in favor of the `AGENTGRID_ADAPTERS` registry.

### Added (Stage 3.1 — versioned event envelope)
- common: `AgentEventEnvelope { version, kind, payload, raw_ref }` layered over the stored `TaskEvent`, plus an `EventKind` vocabulary (`plan`/`tool_call`/`tool_result`/`file_change`/`permission_request`/`usage`/`handoff`/...). Unknown kinds are preserved as `EventKind::Other` and never fatal; serde round-trip tested.
- node-daemon: `read_stream` decodes the new envelope (and still the legacy `{type,payload}` NDJSON); unknown kinds become raw logs, so a future adapter cannot break the pipeline. Legacy `TaskEvent`/`EventType` storage contract is unchanged.

### Added (Stage 3.2 — agent sessions)
- common: `CreateAgentSessionRequest { adapter }` and `AgentSession { id, attempt_id, adapter, started_at, ended_at, status, error_code }`.
- control-plane: `agent_sessions` table (migration 0010, FK to `attempts`). Node opens a session per attempt via `POST /v1/node/attempts/{id}/session` (auth required); the row starts `running` and is closed (`done`/`failed`) when the attempt completes. `get_agent_session` supports reporting/tests.
- node-daemon: after acknowledging an assignment it calls `POST .../session` once, so each agent execution is attributable to its attempt.
- Store: `finish_agent_session` runs inside `complete_attempt`'s transaction (previously a separate pooled connection, which deadlocked against the open write transaction and surfaced as `database is locked`).

### Added (Stage 3.2 — execution backend contract)
- adapters: `ExecutionBackend` trait + `ProcessBackend` (native subprocess-in-worktree). `node-daemon` now spawns attempts through `ProcessBackend::spawn`, isolating the execution contract from orchestration so future backends (container/ACP) drop in without touching the daemon.
- common: `AdapterCapability { id, version, ready }`; `HeartbeatRequest.capabilities` advertises per-adapter version + readiness each beat (degraded node already reports missing binaries).
- adapters: conformance smoke drives the mock adapter through `ExecutionBackend` (start → stream → collect) and asserts event output.
- common: `EventKind::Cancel`; the node daemon emits a normalized cancel event into the stream when cancellation is triggered. The atomic `cancel_task` UPDATE already makes cancel race-free (`cancel_requested` is only set on non-terminal attempts, and `complete_attempt` honors it), so the outcome is deterministic.

### Added (Stage 4.1 — Agent Skills format & discovery)
- skills (new crate `agentgrid-skills`): minimal YAML-frontmatter parser for `SKILL.md` (`name`, `description`, `license`, `compatibility`, `allowed-tools`, `metadata`) with strict + lenient modes. `discover()` scans `<project>/.agents/skills`, `~/.agents/skills`, and managed roots in precedence order (project > user > managed), resolving collisions deterministically with diagnostics. `Skill::catalog_entry()` exposes only name + description (progressive disclosure); the body is materialised on activation. Fixtures cover minimal, malformed-yaml, collision, and untrusted-script.

### Added (Stage 4.2 — skill trust & bundles)
- skills: `TrustStore` (project skills untrusted by default — malicious-repo protection; user/managed trusted), `SkillBundle` manifest (filesystem/git sources, commit/hash pin, lock file) with `verify_locks`, `materialize()` (copies original `SKILL.md` verbatim, skips untrusted project skills, verifies lock hashes), and `RevisionStore` (immutable revisions under `<root>/revisions/<id>` with a transactional `active` symlink + `rollback`). All covered by unit + fixture tests; agent/remote integration + E2E materialization remain as follow-ups.

### Added (Stage 5.1 — ACP southbound client)
- acp (new crate `agentgrid-acp`): JSON-RPC 2.0 codec (request/response/notification, newline framing) + `AcpClient` over any byte transport (stdio in prod, in-memory pipe in tests) with id-matched responses and a notification channel. `initialize` tolerates unknown optional capabilities; `session/new|prompt|cancel` convenience methods; `session/update` → `AgentEventEnvelope` mapping (plan/tool_call/diff/usage/log/permission/...). `next_approval` state machine (`pending → allowed|denied|expired|cancelled`, fail-closed) built before any ACP integration. Covered by codec round-trip + a fake-agent lifecycle test (init → session/new → prompt streaming updates → result).

### Added (Stage 5.3 — ACP node integration)
- node-daemon: ACP adapter registry type. `AdapterSpec { id, protocol }` with `AdapterProtocol::{Wrapper,Acp}`; `AGENTGRID_ADAPTERS=mock,claude,opencode:acp` selects the protocol per entry (default `Wrapper`, fully backward compatible). Heartbeat/poll/enroll advertise adapter ids as before.
- node-daemon: `drive_acp_session` drives an ACP agent over stdio via `AcpClient` — `initialize` → `session/new` → `session/prompt`, forwarding every `session/update` into the event sink (mapped to `AgentEventEnvelope`), and handling `session/cancel`/`timeout` internally. The wrapper path is unchanged.
- node-daemon + control-plane: `session/request_permission` creates a durable approval (`POST /v1/tasks/{id}/approvals`) and the daemon polls `GET /v1/approvals/{id}` until an operator answers, then replies `allow`/`deny` (fail-closed). Control plane adds the create + get-by-id endpoints.
- node-daemon: test-only ACP agent (`src/bin/adapter-fake-acp.rs`) exercises the full spawn/update/result pipeline; a unit test asserts the session succeeds and ≥2 `session/update` events stream into the sink. Control-plane API test covers approval create → pending → allow → allowed and unknown-id 404.
- acp: conformance tests cover the full `session/update` vocabulary mapping (`plan`/`tool_call`/`tool_result`/`diff`→`file_change`/`progress`/`permission_request`/`usage`/`log`, unknown→`Other`) and `session/cancel` acknowledgement, alongside the existing init→new→prompt lifecycle test.

### Added (Stage 5.2 — durable approval flow)
- control-plane: `approvals` table (migration 0011) + store (`create_approval`, `answer_approval` honoring the state machine, `get_approval`, `list_approvals`, `tick_approval_expiry`). API: `GET /v1/approvals`, `POST /v1/approvals/{id}/allow|deny` (user-auth, fail-closed). CLI: `ag approvals list|allow|deny`. The approval state machine moved into `agentgrid-common` so the control plane and the ACP client share one definition. Covered by an API test (create → list pending → allow → list allowed; terminal re-answer is a no-op).

## [0.1.0] - 2026-07-17

### Added (Stage 5.3 — CI / release / ops)
- GitHub Actions `ci.yml`: `rust` (fmt/clippy/test/build), `web` (build/lint), and `e2e` job that brings up the compose stack (control plane + two mock nodes) and asserts a task reaches `succeeded`.
- `tests/e2e/run.sh`: self-contained E2E harness (builds images if missing, brings up via `up.sh`, submits a task, tears down).
- `release.yml`: builds static `x86_64`/`aarch64` musl and `x86_64` gnu binaries via `cargo-zigbuild`, with a 60 MiB binary-size guardrail and uploaded artifacts.
- `adapter-claude` unit tests for the `stream-json` → event translation, plus an `#[ignore]` real-CLI smoke test (needs `claude` + `ANTHROPIC_API_KEY`).
- `docs/deploy/reverse-proxy.md`: TLS termination at Caddy/nginx in front of the plain-HTTP control plane.

### Fixed
- control-plane: refuse to start a second instance against the same SQLite DB on one host (exclusive `flock` on `<db>.lock`); a duplicate launch previously risked `database is locked` / corruption. The lock is released automatically on exit (no stale pid files).
- node-daemon: emit an `attempt started` progress event immediately after the adapter spawns, so a slow agent that is silent past the 30s assignment lease no longer loses its assignment and triggers a duplicate attempt (`bff8099`).
- node-daemon: warn when an adapter exits 0 but produces no stdout/stderr events, so a silent agent that yields an empty "succeeded" task is visible.
- Node image (`Dockerfile.node-daemon`): optional `OPENCODE_VERSION` build arg bakes the opencode CLI into the image for a self-contained opencode node; default empty preserves the operator-provided contract (AGENTS.md: no required runtime deps).

### Added (Stage 3.2 — OpenCode adapter)
- `adapter-opencode` wrapper binary: drives `opencode run --format json` headless and translates its `text`/`tool_use`/`error` events into the agentgrid contract (`log`/`tool_call`/`tool`/`error`); unknown event types are ignored (raw stdout is preserved as an artifact). Optional env `AGENTGRID_OPENCODE_BIN`/`AGENTGRID_OPENCODE_MODEL`/`AGENTGRID_OPENCODE_AUTO`. The underlying `opencode` CLI is provided by the operator (like `claude`); the wrapper is bundled into the node image.

### Added
- Cargo workspace scaffold: `common`, `control-plane`, `node-daemon`, `cli`, `adapters`.
- Shared types and API DTOs (`crates/common`): task/attempt/node status enums, event model, `/v1` request/response types, serde round-trip tests.
- In-memory control plane (`crates/control-plane`): Axum server with health, task CRUD, node long-poll assignment, event ingest (idempotent), attempt completion. First-fit scheduler respects `requested_node_id` and node capacity.
- Node daemon (`crates/node-daemon`): long-poll loop, adapter subprocess in a per-attempt worktree and separate process group, stdout/stderr streamed as batched events, completion reporting.
- Mock adapter (`crates/adapters`): deterministic `sleep:`/`write:`/`fail:`/`spam:` commands emitting JSON-line events; no LLM required.
- Minimal CLI (`crates/cli`): `task run`, `task logs --follow`, `task show`, `node list`.
- Integration test exercising the full task lifecycle and event idempotency.
- ADR recording Stage 0.1 scope decisions (`docs/decisions/0001-mvp-scope.md`).

### Scope note
This is the Stage-1 vertical prototype. Persistence (SQLite WAL), auth, Git worktrees, real adapters and web UI follow in later stages.

### Added (Stage 2.1 / 2.2 — persistence + state machine)
- SQLite storage layer (`crates/control-plane/src/store.rs`) with bundled `libsqlite3-sys`, WAL,
  `synchronous=NORMAL`, `busy_timeout=5000`, 4-connection pool, and `sqlx` migrations.
- Atomic assignment via a short write transaction with `UPDATE ... WHERE status='queued'` +
  `rows_affected` check, so concurrent schedulers can never double-assign.
- Pure task/attempt state-machine transition functions (`crates/common/src/state_machine.rs`)
  with exhaustive unit tests for allowed and forbidden transitions.
- Idempotent event ingest (`ON CONFLICT(attempt_id, sequence) DO NOTHING`).
- Background maintenance: lease-expiry revert of unconfirmed assignments; node-offline sweep.
- `health/ready` now verifies SQLite reachability; integration tests run on a temp SQLite DB.

### Verified
- End-to-end on one machine: `task run` → mock adapter writes file → `succeeded`, logs stream.
- Control-plane restart on the same SQLite file preserves queued tasks (WAL).

### Added (Stage 5.2 — metrics)
- `GET /metrics` exposes Prometheus-text counts: `agentgrid_nodes{status}`,
  `agentgrid_tasks{status}`, `agentgrid_attempts_total`.
- Test: metrics endpoint returns counts.

### Added (Stage 3.3 / 3.4 — validation command + secret masking)
- After the agent succeeds, the node runs `Assignment.validation_command` in the
  worktree (diff already committed first, so it survives a failure); non-zero exit
  reports `error_code=validation_failed`, distinct from `agent_failed`. Validation
  output is streamed as events and saved as `validation.log` artifact.
- Known secret substrings (env `AGENTGRID_SECRETS`, comma-separated) are masked to
  `***` in streamed stdout/stderr before upload.
- `CompleteAttemptRequest.error_code` recorded on the attempt.
- Node-daemon tests: secret masking + validation exit code/log.

### Added (Stage 2.6 — events streaming, SSE)
- `GET /v1/tasks/{id}/events/stream` Server-Sent-Events endpoint: streams existing and
  new attempt events (polls every 250ms, 15s keep-alive ping) for the web UI.
- Idempotent event ingest and batching were already in place (Stage 2.1/2.2).

### Added (Stage 2.8 — artifacts)
- `POST /v1/node/attempts/{id}/artifacts` (node auth) stores a text artifact on the
  control-plane filesystem under `artifact_root/<attempt_id>/<name>` and records
  metadata (idempotent per name).
- `GET /v1/tasks/{id}/artifacts/{name}` serves the latest attempt's artifact.
- Node daemon uploads `changes.patch` after finalizing a git-backed attempt.
- Schema migration `0005`: `artifacts` table.
- Test: artifact upload (node auth) + read by task id.

### Added (Stage 2.5 — repositories + git worktrees)
- `POST /v1/repositories` / `GET /v1/repositories`: register a repo (name, git_url,
  default_branch, optional validation_command) and list them.
- Assignment now carries `git_url`, `default_branch` and `validation_command`
  (resolved from the registered repo) so the node can run in a real worktree.
- Node daemon: keeps one clone per repo under `AGENTGRID_REPOSITORY_ROOT`, and for
  git-backed tasks creates a per-attempt worktree on branch `agent/<task-id>/<n>`,
  runs the adapter there, then commits changes (author `agentgrid`) and writes a
  binary `changes.patch` into the workspace; the commit SHA is reported on complete.
  Plain-dir tasks (no `git_url`) keep the old behaviour.
- `CompleteAttemptRequest.commit_sha` recorded on the attempt.
- CLI `repo add <name> <git-url> [--branch main] [--validate "cmd"]`.
- Schema migration `0004`: `repositories`, `node_repositories`.
- Tests: repo create/list; node-daemon git worktree clone/commit/patch (real git).

### Added (Stage 4.2 — full CLI)
- `ag server` starts the control plane by exec'ing the sibling `agentgrid-control-plane` binary (sets `AGENTGRID_LISTEN`/`AGENTGRID_DB`; optional one-time `--bootstrap-user`/`--bootstrap-password`).
- `task run` gains `--validate` (validation command) and `--timeout` (seconds); `--adapter`/`--node` already present.
- `node list` and `task show` gain a global `--json` flag for machine-readable output.
- `token create`, `repo add`, `task logs --follow`, `task cancel`/`retry`, `login` already present; `node list` renders an aligned table.
- Deferred: `node install` (systemd unit + enroll) — lands with packaging in Stage 5.3.

### Added (Stage 5.2 — observability)
- `GET /metrics` expanded (Prometheus text): task duration histogram, terminal outcome
  counters (`agentgrid_tasks_total`), per-node `free_disk_mb`/`load_avg` gauges from heartbeat,
  and SQLite main/WAL file size gauges.
- `GET /health/ready` now also probes writability of the database directory.
- Control plane and node daemon emit structured JSON logs (tracing `fmt().json()`).
- Deferred (instrumentation needed): scheduler/heartbeat latency, event-buffer size,
  `SQLITE_BUSY`/checkpoint/write-lock metrics.

### Added (Stage 5.1 — security)
- Request size limits (trust-boundary input validation), overridable via env, returning 413:
  `AGENTGRID_MAX_PROMPT_KB` (64), `AGENTGRID_MAX_EVENT_KB` (1024), `AGENTGRID_MAX_ARTIFACT_MB` (50).
  A global `DefaultBodyLimit` caps request bodies at the artifact ceiling; the prompt and
  per-event payload ceilings are enforced in the handlers.
- Node daemon refuses to start as uid 0 unless `AGENTGRID_ALLOW_ROOT=1` is set.
- Audit events on all user actions (login, user.create, task.create/cancel/retry, repo.add)
  plus existing node enroll/revoke. `AuthedUser` is attached by the user-auth middleware
  so handlers can record the acting username.
- Enrollment token (one-time, TTL ≤ 10 min, hash-only) and per-node unique credential with
  immediate revoke already landed in Stage 2.3; marked verified here.

### Added (Stage 4.3 — web UI)
- React + TypeScript single-page UI (Vite) served as static files by the control plane
  (`web/dist`, overridable via `AGENTGRID_WEB_ROOT`); `index.html` fallback for client routing.
- Auth gate with login and first-admin setup; JWT stored in `localStorage` and sent as Bearer.
- Dashboard: node/task counters and the 10 most recent completed tasks.
- Nodes view: status, adapters, repositories, load, active/max, free disk, last heartbeat,
  with confirm-on-revoke.
- New Task form: repository, prompt, adapter, optional validation command, auto/manual node,
  optional timeout; client-side required-field validation.
- Task details: status timeline, live stdout/stderr over SSE with pause + auto-scroll,
  attempt history, `changes.patch` diff view, `validation.log`, and status-aware
  cancel/retry buttons. SSE auto-reconnects and resumes by `sequence` so no events are
  lost or duplicated across drops.
- Per-task `validation_command` wired end-to-end: `CreateTaskRequest` field, `tasks`
  migration `0007`, and assignment prefers it over the repository default. CLI
  `task run --validate` now forwards it (was previously ignored).
- `npm ci && npm run build && npm run lint` passes; built UI smoke-tested against the
  running control plane (static serving + auth + SSE).

### Added (Stage 4.1 — user authentication)
- Local users: `users` table (argon2id password hash). First user created via `POST /v1/auth/setup` (only while no users exist) or via `AGENTGRID_BOOTSTRAP_USER`/`AGENTGRID_BOOTSTRAP_PASSWORD` env at startup.
- `POST /v1/auth/login` exchanges username+password for a 12h HS256 JWT. Secret from `AGENTGRID_JWT_SECRET` (random per start if unset).
- `require_user_auth` middleware protects all `/v1/*` user endpoints (tasks, repositories, enrollment-token, nodes management). Open only during the bootstrap window (no users yet); node endpoints keep their own credential auth.
- CLI `ag login` stores the JWT at `~/.config/agentgrid/credentials` (0600) and attaches it as `Bearer` to all user requests.
- Integration test: setup→login→protected endpoint 401 without token / 201 with token; wrong password 401; second setup rejected.

### Added (Stage 3.2 — Claude Code adapter)
- `adapter-claude` wrapper binary (ADR #12): launches `claude -p --output-format stream-json --verbose --dangerously-skip-permissions` and translates its output into the agentgrid event contract (`log`/`tool_call`/`tool`/`result`); unrecognized lines/blocks fall back to raw `log`.
- Exit code is claude's; a `result` with `is_error:true` forces a non-zero exit so the daemon records `agent_failed`. API key supplied via env (`ANTHROPIC_API_KEY`) forwarded by the daemon through `AGENTGRID_ADAPTER_ENV`.
- Verified end-to-end with a fake `claude` shim (translate + exit-code paths). Unit tests cover the `translate` mapping. Real-key run left as a manual `#[ignore]`-style check (Stage 3.5 exit criteria).

### Added (Stage 3.1 — adapter contract finalized + capability discovery)
- Adapter contract documented (subprocess model: `prepare`=worktree, `start`=`--prompt`, `stream`=NDJSON stdout, `cancel`=SIGTERM process group, `collect`=artifacts). Unknown stdout lines fall back to raw `log` so a future CLI format change cannot break the pipeline.
- Capability discovery (Stage 3.1): the daemon probes the adapter binary in `PATH` at startup and on every heartbeat; a missing binary makes the node report `degraded` so the scheduler excludes it. Detected version is logged.
- Adapter config: `AGENTGRID_ADAPTER_ENV` forwards `KEY=VALUE` pairs (e.g. API keys) to the adapter subprocess.
- Raw adapter output is mirrored to `agent-raw-output.log` in the worktree and uploaded as an artifact on completion (format-change safety net, spec risk #1).
- Integration tests: `probe_adapter` (found/missing) and `read_stream` raw-log mirroring.

### Added (Stage 2.4 — scheduler filters + `no_eligible_nodes` visibility)
- Scheduler filter centralised in `node_ineligibility` (shared by assignment and
  visibility): only `online` nodes, with the task's adapter, the task's
  repository (or wildcard `*`), and spare capacity (`active_attempts <
  max_concurrency`).
- `GET /v1/tasks/{id}/eligibility` returns per-node `NodeEligibility`
  (`eligible` + `reasons`) and a `no_eligible_nodes` summary listing the
  distinct reasons the task stays queued (empty when at least one node is
  eligible). Honours `requested_node_id`: only that node is considered, and a
  missing/offline requested node yields a clear reason.
- CLI `task show` prints the `no_eligible_nodes` reasons for still-queued tasks.
- Integration tests: empty pool, missing adapter, missing repository, at
  capacity, and requested-node scoping.

### Added (Stage 2.3 — node lifecycle: enrollment, heartbeat, revoke)
- Enrollment tokens: `POST /v1/nodes/enrollment-token` issues a one-time token
  (TTL 10 min; only its SHA-256 hash is stored).
- `POST /v1/node/enroll` exchanges a token for a permanent node credential
  (random secret; only its hash stored). Token is single-use.
- Node endpoints (`/v1/node/poll`, `/v1/node/heartbeat`, attempt events/complete/cancel)
  require `Authorization: Bearer <credential>`; the control plane resolves the
  credential to its node and rejects revoked/unknown ones with 401.
- `POST /v1/node/heartbeat` publishes status, load, free disk, version and
  capabilities; refreshes `last_heartbeat_at` (node-offline sweep unchanged).
- `DELETE /v1/nodes/:id` revokes a node immediately (status `revoked`, auth denied).
- Audit events logged for enroll/revoke.
- Node daemon: enrolls on first start (token via `AGENTGRID_ENROLL_TOKEN`), persists
  credential to `AGENTGRID_DATA_DIR/credential.json`, sends Bearer on every node
  request, and runs a periodic heartbeat loop (load from `/proc/loadavg`, free disk
  via `statvfs`).
- CLI `token create` prints an enrollment token to export.
- Schema migration `0003`: `enrollment_tokens`, `audit_events`, node `load_avg`/`free_disk_mb`.
- Integration tests: enroll+auth flow; revoked node gets 401 on heartbeat and poll.

### Added (Stage 2.7 — cancellation + timeout)
- `cancel_task`: `queued` → `cancelled` immediately; `assigned|running|validating` → sets
  `cancel_requested` on the attempt and reports `cancelled` once the node confirms completion.
- `retry_task`: `failed|cancelled` → `queued` (new attempt created on next assign).
- CLI `task cancel` / `task retry` subcommands.
- Node daemon polls `GET /v1/node/attempts/{id}/cancel`; on cancel request or `timeout_secs`
  elapse it SIGTERMs the attempt's process group (SIGKILL after 10s grace), killing the whole
  adapter tree (no orphaned children).
- Per-task `timeout_secs` (default 3600s) carried from request → assignment → node; schema
  migration `0002_cancel_timeout.sql`.
- Completion is cancellation-aware: a `cancel_requested` attempt finishes `cancelled` regardless
  of the adapter exit code.
- Integration tests: cancel queued, cancel-running-then-node-confirms, retry failed.

### Verified
