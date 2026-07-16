# Changelog

All notable changes to this project are documented in this file.

## [0.1.0] - Unreleased

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
