# Changelog

All notable changes to this project are documented in this file.

## [0.3.0] — 2026-07-17

Stage 8 distributed multi-agent workflows. See the `Added (Stage 8 …)` entries
under `[Unreleased]` below for the full list: per-step node placement, shared
`base_commit` (control plane + node-side checkout), lost-step retry policy,
integrator conflict policy (`Blocked`), ACP plan projection, and the two-node
E2E harness (`tests/e2e/run-workflow.sh`). Tag `v0.3.0` marks the Stage 8 code
complete; the two-container E2E run is the release validation gate.

## [Unreleased]

### Added (control-plane — TLS termination, Step 2)
- control-plane serves HTTPS when `AGENTGRID_TLS_CERT` + `AGENTGRID_TLS_KEY` (PEM) are set: a `TlsListener` (axum 0.8 `Listener` trait over `tokio-rustls`) wraps the TCP listener; rustls with the `ring` provider, no system OpenSSL. Plaintext is retained for loopback / `--tls-cert` unset. `ag server start --tls-cert/--tls-key` forwards the paths as env. Nodes are already rustls-HTTPS clients (reqwest `rustls-tls`), so a node just needs `AGENTGRID_SERVER=https://cp`; no VPN is required for a star topology. `ag nodes install --server https://cp ...` skips the SSH reverse tunnel and points the node directly at the TLS control plane (SSH used only to `scp` the binary + start it). Covered by `tls_tests::load_tls_acceptor_missing_file_errors`. Reverse-proxy docs / mTLS remain follow-ups.

### Added (gateway — chat front-end, Stage 9.3)
- New crate `crates/gateway` (`agentgrid-gateway`): a chat bridge that lets an operator drive the grid from a phone. A `ChatProvider` trait with one implementation — Telegram, via raw `reqwest` calls to the Bot API `getUpdates`/`sendMessage` long-polling (no chat-client crate). Commands proxy to the control-plane HTTP API: `/nodes`, `/tasks`, `/run <repo> <adapter> <prompt...>`, `/show <id>`, `/logs <id>`, `/cancel <id>`, `/help`. Auth is an allowlist of numeric chat ids (`AGENTGRID_GATEWAY_ADMINS`); chats off the list are ignored. The control-plane URL + a user JWT come from `AGENTGRID_SERVER` / `AGENTGRID_GATEWAY_TOKEN`. Discord and WhatsApp sit behind the same trait but are **not implemented yet** — WhatsApp especially has no easy open bot API (the Business API is gated/heavy); both are honestly deferred rather than stubbed. Covered by `tests::fmt_*` (the pure formatting/dispatch helpers); live bot wiring needs a real Telegram token.

### Fixed (git — per-repo lock serializes shared-clone mutations, Stage 2.3)

- `prepare_workspace` now holds an in-process per-repository `Mutex` across the shared-clone mutating steps (`fetch` / `checkout -B` / `worktree add`), so two concurrent attempts of the same repo cannot race the clone state (a `checkout -B` from one attempt moving the shared branch mid-`worktree add` of another). Each attempt still gets its own worktree, so agent work stays concurrent. Covered by `parallel_prep_same_repo_does_not_race` (4 concurrent prepares, all succeed). Note: in-process lock only (single node); a cross-process file lock remains a follow-up.

### Fixed (security — artifact-name traversal, Stage 2.2)

- `GET /v1/tasks/{id}/artifacts/{name}` used to resolve `name` directly, so a `../../etc/passwd` request could read outside the artifact root. The handler now runs the same `is_safe_artifact_name` gate as the upload path (404, not 403, so a denial cannot disclose whether an artifact exists), and `Store::artifact_path` adds defense-in-depth: it canonicalizes the attempt dir and checks the resolved path stays under the artifact root, and rejects any name that is not a single safe segment. Covered by `artifact_save_rejects_traversal_names`, `artifact_read_traversal_returns_none` (store) and `artifact_get_rejects_traversal_name` (api). Note: this is a canonicalize + single-segment guard, not a descriptor-relative (`openat`/`O_NOFOLLOW`) API; that hardening remains a follow-up.

### Fixed (security — agent logs excluded from commit/diff, Stage 2.2)

- Node-side logs the daemon writes inside the agent worktree (`agent-raw-output.log`, `validation.log`) and its own `changes.patch` used to leak into the committed diff / `changes.patch` via `git add -A`, so raw agent output (which may contain secrets) could end up in a commit or the reviewable patch. `prepare_workspace` now scopes a per-worktree `.git/info/exclude` (resolved via `git rev-parse --git-path`, so linked worktrees get their own gitdir-scoped file rather than the shared clone's) listing those names. Covered by `raw_and_validation_logs_excluded_from_commit_and_patch`.

### Added (web — workflow run viewer with DAG, Stage 11.6)

- A Workflows page lists runs (`GET /v1/workflow-runs`) and a run detail renders the step DAG: steps are layered by dependency depth (roots left, leaves right), each card shows role, status, verdict, assigned node, attempt count, and error code; the detail auto-polls and offers Cancel on non-terminal runs. Backed by the existing `GET /v1/workflow-runs/{id}/projection`. A span-waterfall timeline is a follow-up; this is a layered DAG view.

### Added (ACP session resume, Stage 11.5)

- ACP `session/new` is now issued with `parent_session_id` when a follow-up task in a conversation should resume the prior agent session, so the agent does not re-process the transcript from scratch. The node reports the `session_id` it received back to the control plane via `CompleteAttemptRequest.acp_session_id`; the control plane persists it on the attempt (`attempts.acp_session_id`, migration `0019`) and, on the next conversation turn, looks up the last finished attempt's session as the resume parent (`Store::last_conversation_acp_session`) and threads it onto the task as `Assignment.parent_acp_session_id`. Resume is an optimization (correctness already holds: conversations compose the full history into the prompt). Covered by `acp_session_resume_links_conversation_turns`.

### Added (feedback-loop CI→agent, Stage 11.4)

- **Wrapper path**: the spawn→select→finalize→validate flow is wrapped in a retry loop. When `validation_command` is configured and the agent exits 0 but validation fails, the node re-spawns the agent with the validation error appended to the prompt (same worktree, fixes accumulate, single commit at the end), up to `AGENTGRID_FEEDBACK_RETRIES` rounds (default 0 = off, backward compatible). A `feedback` event is emitted each round so the loop is visible in the event stream.
- **ACP path bugfix**: the ACP path used to skip `finalize_workspace` and `run_validation` entirely, silently leaving `validation_command` unenforced for ACP agents. Now both run after `drive_acp_session`, before `report_complete`.

### Added (agent-profile SSOT, Stage 11.3)

- An optional system prompt per adapter, projected into the worktree before the agent runs. `AGENTGRID_AGENT_PROFILE_<ID>` is either a path to a `.md` file (read) or inline text; the node writes it to `<worktree>/AGENTS.md` (the cross-agent convention that Claude Code, opencode, pi, etc. read) and forwards it as the `AGENTGRID_SYSTEM_PROMPT` env hint. Per-agent native projection (`CLAUDE.md`, `.kiro/`) is a follow-up mapping table.

### Added (Sandbox trait, Stage 11.2)

- Agent isolation: a `Sandbox` wraps the spawned agent command so an agent can run inside a container instead of sharing the node's full environment. `NoSandbox` (default, runs directly in the worktree) and `DockerSandbox` (`docker run --rm -i -v <workdir>:/ag -w /ag <image> --`). Configured via `AGENTGRID_SANDBOX` (`none` | `docker`) and `AGENTGRID_SANDBOX_IMAGE`. The ACP path (native ACP launcher + wrapper binary) routes through `sandbox_command`; the legacy `ExecutionBackend` wrapper path is left unsandboxed with a noted TODO.

### Added (native ACP launcher + durable startup-reconcile, Stage 11.0/11.1)

- **Native ACP launcher**: a node can run any native-ACP coding agent (Claude Code, Codex, Gemini CLI, OpenCode, Kiro, …) directly over stdio by setting `AGENTGRID_ACP_LAUNCH_<ID>` (e.g. `AGENTGRID_ACP_LAUNCH_CLAUDE="claude --acp"`). The ACP path spawns that command instead of the `adapter-<id>` wrapper binary, so adding a new agent is one env var — no per-agent crate/parser. The per-CLI wrapper binaries (`adapter-claude`, `adapter-opencode`) remain as legacy fallback for agents that don't speak ACP.
- **Durable startup-reconcile**: on boot the control plane immediately runs a maintenance tick (revert expired leases, mark silent nodes offline) instead of waiting for the first background tick, and audits the reconcile with the in-flight attempt count. In-flight `running` attempts on live nodes are left alone (the node may still complete them); node-death is caught by the existing `node_lost` path. Backed by `Store::reconcile_on_startup`.

### Added (conversations — stateful multi-turn chat routed to an agent, Stage 9.5)
- New `conversations` + `conversation_messages` tables (migration `0018`). A conversation is a stateful multi-turn chat routed through the control plane to a coding agent on some node. Each user message creates a task whose **prompt is the composed conversation history** (a `user:`/`assistant:` transcript), so any node that picks the task up sees the full shared context — conversations can hop nodes, and parallel conversations are isolated by id.
- Endpoints: `POST /v1/conversations` (adapter, optional repository), `GET /v1/conversations/{id}`, `POST /v1/conversations/{id}/messages` (content → creates the task carrying the composed prompt, returns task id), `GET /v1/conversations/{id}/messages`.
- `adapter-mock` now emits a `result.text` (echoes the last non-empty prompt line) so the chat loop has a readable answer without an LLM; real adapters (`claude`/`opencode`) emit their own.

### Added (gateway — conversations + chat loop)
- The Telegram gateway now holds the current conversation id per chat and routes **plain text** (no slash) as a conversation message: it appends to the conversation, polls the task events until terminal, and replies with the agent's `result` text (best-effort: result payload, else last log/error line). `/new <adapter> [repository]` starts a conversation; plain text with no conversation bound nudges the operator to create one (and mentions `AGENTGRID_GATEWAY_CHAT_ADAPTER`, default `mock`).

### Added (node-daemon — disk-space alerting, Stage 2.5)
- A node now marks itself `Degraded` and emits a `tracing::warn!` when free disk on its workspace falls below `AGENTGRID_DISK_LOW_MB` (default 1024 MB). The value was already reported in heartbeats and stored by the control plane; this surfaces a low-disk host as `degraded` in `ag nodes list` (and adds a `DISK` column showing free space / a `!` marker under 1 GB) so the scheduler/operator is warned before a full host silently fails worktree checkouts.

### Fixed (CLI — remote node bootstrap, multi-host link test)
- `ag nodes install` now ships the `agentgrid-node-daemon` binary (found next to `ag`), not the `ag` CLI itself — the daemon takes no subcommands and reads its config from env, so the previous copy failed on the remote with `requires a subcommand`.
- The node env file sets `AGENTGRID_ALLOW_ROOT=1` so the daemon starts on hosts where the operator runs as root (it otherwise refuses: `refusing to run as root`).
- The remote data dir is created (`mkdir -p`) **before** `scp` of the binary/env, so a fresh host no longer fails with `No such file or directory`.
- The env file is sourced via `bash -c 'set -a; . file; set +a; exec node'` instead of `env $(cat file)` — the latter left literal single quotes in every value (e.g. `AGENTGRID_SERVER='http://…'` with the quotes), which made the node's HTTP client fail with `relative URL without a base`, and would have glob-expanded the `*` in `AGENTGRID_REPOSITORIES`.
- The reverse tunnel and the node start command run detached: `setsid nohup` with `stdin/stdout/stderr` set to `null`, so they survive `ag nodes install` returning and never keep the caller's stdout pipe open. `</dev/null` on the remote start closes stdin so the backgrounded `ssh` exits immediately instead of hanging.
- Verified end-to-end against a second host over password SSH: node enrolled and appeared in `ag nodes list` (status `degraded` because the `mock` adapter binary isn't installed remotely — expected; real adapters install on demand). The reverse tunnel stays up across `ag` process exits.

### Added (CLI — remote node bootstrap)
- CLI `ag nodes install --host user@host[:port] [--ssh-key ...] [--transport ssh-tunnel]` provisions a remote host as a node: mints a one-time enrollment token, `scp`s the node binary, opens a persistent reverse SSH tunnel (`remote localhost:<remote_port>` → control plane `:<local_port>`), writes a `chmod 600` env file, and starts the node in the background. The node then long-polls the control plane through the tunnel — so two hosts link automatically, working behind NAT with SSH providing encryption. `--transport wireguard` is reserved (planned; SSH used only for one-time bootstrap). Key-based auth preferred; `--password` wraps `sshpass` (SSHPASS env, never argv). User-supplied fields (`name`/`repositories`/`adapters`/`data-dir`) are validated against a safe charset (trust boundary). Covered by `node_install_tests` (parse_host, env-file format, validation).

### Added (Stage 9 — approval scope, audit, tests)
- control-plane (9): approvals gain a `scope` column (migration 0017) — `tool_call | session | step | command | duration` — so operators see what they are approving. `create_approval` threads it through; `ApprovalView` and the list/get SELECTs expose it. Covered by `approval_scope_round_trips` (api).
- control-plane (9): `POST /v1/policy/evaluate` now emits a fail-closed audit event (`policy.evaluate`) for every decision, so dangerous commands are never silent. `Store::list_audit` added for the trail. Covered by `policy_evaluate_audits_decision` (api).
- skills (9): `untrusted_project_skill_not_materialized` asserts a repo-supplied (malicious, `curl | sh`) skill is skipped by `materialize` unless an operator has explicitly trusted it. Control-plane (9): `approval_payload_has_no_secrets` asserts the approval payload never serializes secret-like fields. Destructive-command denial is covered by the policy unit tests.

### Added (Stage 9 — autonomy levels + approval timeout)
- common (9): autonomy levels `L0`–`L4` (`AutonomyLevel`, default `L2`) modulate the builtin policy. `BuiltinPolicyProvider::decide_for(level, class)` maps risk class → decision per level (L0 fully supervised → everything `ask`; L2 allows local read/edit/exec, asks network/git/install, denies destructive; L3 also allows network/git; L4 allows everything including destructive). `evaluate_with(level, command, cwd)` applies a level. Covered by `policy::tests::l0_*` / `l2_*` / `l3_*` / `l4_*`.
- control-plane (9): `POST /v1/policy/evaluate` accepts an optional `autonomy` level (`l0`–`l4`, default `l2`) and applies it. Covered by `policy_endpoint_honors_autonomy_level` (api).
- control-plane (9): an unanswered approval that times out now blocks the workflow step (and run) it is linked to, instead of leaving the run hanging. `approvals.step_run_id` (migration 0016) links an approval to a `workflow_steps` instance; `tick_approval_expiry` flips a past-due linked approval to `expired` and calls `block_step_and_run`, which sets the step and run to `Blocked` (idempotent, non-terminal only). Covered by `approval_timeout_blocks_linked_step` (store).

### Added (Stage 9 — command-policy foundation)
- common (9): command-policy foundation. `CommandPolicyProvider` trait with `evaluate(command, cwd) -> PolicyVerdict { decision, risk_class, reason, matched_rules }`; `RiskClass` (read / edit-workspace / execute-local / network-write / git-remote / package-install / destructive) and `PolicyDecision` (allow / ask / deny / rewrite). `BuiltinPolicyProvider` is a heuristic classifier mapping risk class → decision (destructive→deny, network/git/install→ask, read/edit/exec→allow). Fail-closed: an unavailable provider or an unparseable command yields `ask`, never `allow` (`PolicyVerdict::fail_closed`). Covered by `policy::tests::*` (8 unit tests).
- control-plane (9): `POST /v1/policy/evaluate` exposes the builtin provider (`{command, cwd} -> verdict`); fail-closed on provider error. Covered by `policy_endpoint_classifies_commands` (api).

### Added (Stage 8 — distributed workflows: node-side base_commit, conflict policy, ACP projection)
- node-daemon (8): honor a step's `base_commit` on the node. `prepare_workspace` checks the worktree out at the exact pinned commit (best-effort fetch, token-validated) so all attempts of one run start from the same commit; `finalize_workspace` diffs relative to `base_commit`. Covered by `base_commit_pins_worktree_to_commit`.
- control-plane (8): integrator conflict policy. A non-retryable (or retry-exhausted) integrator step transitions the step **and** the run to `Blocked` (awaiting human/repair) instead of `Failed` — an integrator never silently overwrites and never fails the whole run. `Blocked` added to `WorkflowRunStatus`/`WorkflowStepStatus`. Covered by `integrator_failure_blocks_run_not_failed` and `worker_failure_still_fails_run`.
- control-plane (8): ACP plan projection. `GET /v1/workflow-runs/{id}/projection` returns each step's role, status, placement, spawned task, assigned node, and latest verdict; the ACP gateway exposes it via the `_agentgrid/workflow/projection` extension. Covered by `workflow_run_projection_exposes_roles_nodes_verdicts` (store), `workflow_projection_endpoint_exposes_roles_and_verdicts` (api), and `gateway_exposes_workflow_projection` (acp).
- e2e: `tests/e2e/run-workflow.sh` brings up control-plane + two node containers and runs a workflow that pins workers to node A and integrator/verifier to node B, asserting `succeeded` and printing step provenance — the Stage 8 two-container release gate.

### Added (Stage 8 — distributed workflows: base_commit + lost-step recovery)
- control-plane (8): shared `base_commit` for a run's parallel workers. `WorkflowRun`/`CreateWorkflowRunRequest` gain `base_commit` (migration 0015); it is stored, threaded into every step's spawned task (`CreateTaskRequest`/`TaskView`/`Assignment` all gain `base_commit`), so all workers of one run start from the same commit. Per-step `base_commit` overrides the run-level value. `tasks.base_commit` added. Covered by `workflow_run_carries_base_commit`.
- control-plane (8): per-step retry policy (lost-step recovery). `WorkflowStep`/`WorkflowStepRun` gain `retryable` + `max_attempts` + `attempts` (migration 0015). A failed/`node_lost` step is retried up to `max_attempts` only when `retryable` is set; side-effectful steps default to no auto-retry (step → `failed`). `tick_workflow_run` bumps the attempt counter and respawns the task on retry. Covered by `retryable_step_retries_then_succeeds`.

### Added (Stage 8 — distributed workflows: placement)
- control-plane (8): per-step node placement. `WorkflowStep`/`WorkflowStepRun` gain `requested_node_id`; it is stored in `workflow_steps` (migration 0014) and carried into the Agentgrid task spawned for that step, so the scheduler's `try_assign` pins the task to the requested node (NULL = any eligible node). Honored end-to-end: template → run → task. `TaskView` now exposes `requested_node_id` for UI/CLI visibility. Covered by a store-level regression test (`step_requested_node_id_pins_task`) and the golden workflow integration test.

### Fixed (Stage 8 — distributed workflows: placement)
- control-plane: bind `workflow_steps.requested_node_id` as `Option<&str>` (via `as_deref()`) instead of `&Option<String>`, and normalize empty-string to `None` on read. Binding `&Option<String>::None` into an `ALTER TABLE … ADD COLUMN` text column stored the empty string `""` rather than NULL, which poisoned the spawned task's `requested_node_id` and broke the `try_assign` `requested_node_id IS NULL` filter (unpinned steps could never be assigned).
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

### Added (Stage 2.5 — ops hardening)
- control-plane (2.5): `POST /v1/admin/backup` runs `VACUUM INTO` to a server-side path (path validated against `..`/shell metacharacters; `VACUUM INTO` refuses to overwrite). Store methods `backup_to` + `wal_checkpoint` back it. Covered by `backup_endpoint_writes_file` (api) and `backup_round_trips` (store, re-opens the copy).
- control-plane (2.5): periodic `PRAGMA wal_checkpoint(TRUNCATE)` in the maintenance loop plus a checkpoint on graceful shutdown (Ctrl-C / SIGTERM), so the database file does not grow without bound and a restart replays nothing stale. Covered by `wal_checkpoint` use in `tick_maintenance`.
- control-plane (2.5): `POST /v1/auth/login` is brute-force limited by a per-instance sliding window (10 attempts / 60s) returning a generic `429` (no per-user signal, so it cannot be used for user enumeration). Covered by `login_rate_limit_returns_429` (api).
- control-plane (2.5): `UploadArtifactRequest.name` is validated to a single safe path segment (no separators, no `..`, no NUL) on `POST /v1/node/attempts/{id}/artifacts`; a traversal name is rejected with `400`. Covered by `artifact_name_validation_rejects_traversal` (api).
- control-plane (2.5): artifact metadata older than the 168h retention window is reaped by the maintenance loop (`cleanup_artifacts(168)`); files on disk are left for an operator cleanup job. Covered by `cleanup_old_artifacts` (store).
- control-plane (2.5): scheduler observability. `try_assign` records the queued→assigned latency (ms) and a cumulative assignment counter, exposed as `agentgrid_scheduler_latency_ms` / `agentgrid_scheduler_assignments_total` in `/metrics`. Covered by `scheduler_records_latency_metric` (store).
- control-plane (1.2, shipped): node `offline`/`revoked` atomically loses its in-flight attempts (→ `lost`) and frees `active_attempts` capacity; a late completion on a lost attempt is an idempotent no-op (`complete_on_lost_attempt_is_idempotent`). A task whose attempt is lost is failed with `error_code="node_lost"`. Marks plan items 36/37/38/40 done.
- control-plane (2.5): node protocol versioning. `EnrollRequest`/`HeartbeatRequest`/`PollRequest` carry an optional `protocol_version`; a major mismatch marks the node `degraded` (incompatible_protocol) instead of scheduling it. The node daemon advertises `NODE_PROTOCOL_VERSION` on every enroll/heartbeat/poll. Covered by `node_protocol_mismatch_marks_degraded` (api).

### Added (Stage 8 — workflow operations)
- control-plane (8): `POST /v1/workflow-runs/{id}/cancel` cancels the whole run and every non-terminal step, and cancels any spawned task (`cancel_workflow_run`). CLI `ag workflow cancel <id>` added. Covered by `cancel_workflow_run_cancels_steps_and_tasks` (store) and `cancel_workflow_run_handler_cancels` (api). Pause/resume remain a follow-up.
- control-plane (8): `POST /v1/workflows` accepts YAML bodies (content-type `application/yaml`) via `WorkflowTemplate::from_yaml`; the JSON contract is unchanged. Covered by `yaml_round_trips_to_template` (common) and `create_workflow_accepts_yaml` (api).

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

### Added (Stage 6 — ACP northbound gateway)
- acp: `GatewayAgent` speaks the ACP *agent* role so Agentgrid can be driven by an external ACP client. `session/new` mints a session id; `session/prompt` creates an Agentgrid task (prompt known only at the gateway), streams the task's `session/update` events back to the client until the task terminates, and `session/cancel` cancels the underlying task.
- acp: `AcpServer` (generic over the byte transport) drives the agent lifecycle — it decodes inbound JSON-RPC, dispatches each request on its own task (so an in-flight `session/prompt` can keep receiving the client's responses), and routes client responses back to agent→client requests via a shared pending map.
- acp: `AcpCtx::request` lets the agent issue agent→client requests (e.g. `session/request_permission`) and await the response; the server's read loop routes the client's answer back. The `AcpAgent` trait now returns `Send` futures (RPITIT).
- acp: approval requests flow end-to-end — the gateway polls `GET /v1/approvals?status=pending`, surfaces a pending approval for its task to the ACP client as `session/request_permission`, relays the client's `allow`/`deny` decision back to the control plane (`POST /v1/approvals/{id}/allow|deny`), and asks each approval exactly once. This closes the Stage 6 e2e acceptance criterion (node → control plane → ACP client → back).
- acp: `agentgrid-acp-agent` binary runs the gateway over stdio (`AGENTGRID_SERVER`, optional `AGENTGRID_TOKEN`); any ACP-compatible client can now create tasks on the control plane and watch plan/progress/diff/permission events.
- acp: integration smoke test spins up an in-process fake control plane (axum) and drives the gateway from a real ACP client over a pipe — asserts `session/update` streaming, permission round-trip, and a `succeeded` terminal result.
- acp: `_`-prefixed extension methods let an external ACP client read Agentgrid state through the gateway. `AcpServer` routes any `method` starting with `_` to `AcpAgent::handle_extension`; `GatewayAgent` implements `_agentgrid/nodes` (`GET /v1/nodes`) and `_agentgrid/task_eligibility` (`GET /v1/tasks/{id}/eligibility`). Unknown extension methods return a clean RPC error (no hang). Covered by a new integration test.
- docs: `docs/acp-interop.md` records ACP client interoperability (Poracode/Lightcode) — the standard agent role works unmodified; lists non-standard gaps (`_agentgrid/*` extensions, no `session/load`/`resume` passthrough, client `session/update` not forwarded).

### Added (Stage 7.1 — workflow data model + DAG validation)
- common: `workflow` module — `WorkflowStep` (id/prompt/depends_on/role/adapter), `WorkflowTemplate`, `WorkflowRun`, `WorkflowStepRun`, and the role/run/step status enums. `WorkflowRole` = `architect`/`worker`/`verifier` (v1 creates one role-run per step for its declared role).
- control-plane: `validate_workflow_dag` (pure) — non-empty, unique ids, existing dependencies, no cycles (Kahn). `DagError` enumerates every failure; 7 unit tests cover valid chains/diamonds and all four error kinds.
- control-plane: migration `0012_workflows.sql` (tables `workflow_templates`, `workflow_runs`, `workflow_steps`, `role_runs`). `Store` gains `create_workflow_template` (validates before insert), `get/list_workflow_template(s)`, `create_workflow_run` (instantiates steps + one role-run each, transactional), `get/list_workflow_run(s)`, `get_workflow_run_steps`. Storage round-trip covered by an integration test on a temp SQLite file.

### Added (Stage 7.2 — workflow API + CLI)
- control-plane: HTTP surface for workflows (user-authenticated, same middleware as `/v1/tasks`). `POST /v1/workflows` (define template, validates DAG), `GET /v1/workflows` (list), `GET /v1/workflows/{id}` (show), `POST /v1/workflows/{id}/runs` (start run), `GET /v1/workflow-runs` (list), `GET /v1/workflow-runs/{id}` (run + step instances). Invalid DAG → `400`; unknown id → `404`.
- common: `CreateWorkflowRequest`, `CreateWorkflowRunRequest`, `WorkflowRunWithSteps` DTOs.
- cli: `ag workflow create|list|show|run`. `create` reads steps from a JSON file; `run` starts a run of a template. Covered by two `tests/api.rs` integration tests (happy path + invalid-DAG rejection).

### Added (Stage 7.3 — DAG execution scheduler + roles)
- common: `WorkflowRole` expanded to `architect`/`worker`/`reviewer`/`integrator`/`verifier` (v1 still creates one role-run per step for its declared role).
- control-plane: migration `0013_workflows_repository.sql` adds `workflow_runs.repository` so step tasks schedule against enrolled nodes.
- control-plane: `Store::tick_workflow_run` — durable, idempotent scheduler. Marks a `pending` run `running`; activates `pending` steps whose dependencies are all `succeeded` by creating one Agentgrid task per step (tagged with the step's role); advances `running` steps whose task terminated; computes run status (succeeded when all leaves done, failed on any step failure). `create_workflow_run` now takes a `repository`.
- control-plane: `POST /v1/workflow-runs/{id}/tick` drives the scheduler (wakes the assignment notifier) and returns the run + step instances.
- tests: `tests/api.rs` golden workflow — `architect → 2 parallel workers → integrator → verifier` runs locally to a `succeeded` run on mock adapters (deterministic, exercises the full durable scheduler).

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
