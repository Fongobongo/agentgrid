# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Added (plan 2.1)

- **Org agents / budgets / heartbeats (#18):** long-lived agent identities
  with a role, prompt template, attached skills, a display USD budget and a
  hard task stop (`max_tasks`). Attribution flows through
  `CreateTaskRequest.agent_id` → `tasks.agent_id` (migration 0059). REST:
  `POST/GET /v1/agents`, `GET /v1/agents/{id}/actions` (immutable trail),
  `POST /v1/agents/{id}/tasks` (409 on exhausted budget). CLI:
  `ag agent add/list/actions`. Heartbeat ticker spawns the agent's prompt
  task on `heartbeat_interval_secs` (env `AGENTGRID_AGENT_TICK_SECS`,
  default 5s); every lifecycle event lands in `agent_actions`.

### Added (plan 1.12)

- **Shared context / memory (#7):** flat per-group notes for parallel
  attempts. `shared_context(task_group_id, key, value, updated_at)` table
  (migration 0058) with a (group, key) PK and upsert. Tasks opt in with a
  `group_id` (`ag run --group <id>`, `CreateTaskRequest.group_id`); the node
  forwards it to the agent as `AG_GROUP_ID`. REST surface:
  `GET/PUT/DELETE /v1/task-groups/{id}/context/{key}` and
  `GET /v1/task-groups/{id}/context` (list); CLI `ag ctx set/get/ls/del`.
  One attempt writes a note, a sibling attempt of the same group reads it;
  other groups are isolated.

### Added (plan 1.11)

- **Programmatic SDK (#8):** thin, dependency-free TypeScript
  (`sdks/ts/index.ts`, Node >= 18 `fetch`) and Python (`sdks/python/`
  `/agentgrid.py`, Python >= 3.9 stdlib) clients over the `/v1` API with a
  minimal surface: `run | wait | status | cancel | artifacts | artifact |
  login`. Auth is a JWT (`Authorization: Bearer`), from `ag login` or
  `AGENTGRID_TOKEN`. `GET /v1/tasks/{id}/artifacts` (new) lists a task's
  latest-attempt artifacts (`ArtifactMeta` gained a `name`); the SDK
  downloads one by name. `tests/e2e/run-sdk.sh` exercises the Python SDK
  end-to-end against a local control plane + mock node (no Docker).

### Added (plan 1.10)

- **Autonomous CI-fix / merge-conflict follow-ups (#1):** two GitHub
  webhooks on top of the issue webhook. `POST /v1/webhooks/github/check_run`
  spawns a CI-fix task when a check completes with `failure`/`cancelled` (the
  prompt points the agent at the check log URL). `POST
  /v1/webhooks/github/pull_request` spawns a merge-conflict resolve task
  when `mergeable_state == "conflicting"` on `opened`/`synchronize`/
  `reopened`. Both reuse the shared HMAC verifier
  (`X-Hub-Signature-256` / `AGENTGRID_GITHUB_WEBHOOK_SECRET`) and are 404
  when the secret is unset — no new config.

### Added (plan 1.9)

- **YAML workflows as code (#17):** `WorkflowTemplate::read_workflow_yaml`
  parses a strict-schema YAML file (name + step DAG with id/prompt/deps/role/
  adapter) and validates the graph before use; `ag workflow validate <path>`
  runs the same check locally (no server, exit code 1 on error) and
  `ag run --workflow <file|dir>` validates then creates+starts the run
  (a directory picks the first `*.yaml`, the `.agentgrid/workflows/`
  convention). CI validates every committed `.agentgrid/workflows/*.yaml`.

### Added (plan 1.8)

- **Account pool / provider failover (#15):** node config
  `AGENTGRID_ACCOUNTS="ENV=tok1,tok2;ENV2=tok3"` defines a per-credential
  token pool. The ACP event stream sniffs provider rate-limit errors (429 /
  rate limit / too many requests / overloaded / quota); on a hit the attempt
  runner rotates to the next token and re-drives the session without touching
  the worktree. Per-account counters (attempts / rate-limited) ride in the
  heartbeat and surface at `GET /v1/nodes/{id}/accounts/usage`.

### Added (plan 1.7)

- **Token-budget compression (#14):** `agentgrid_common::compress` —
  `dedup_lines` collapses runs of identical consecutive lines (noisy
  stack-trace / tool-output repeats) into a `…×N` marker;
  `smart_truncate` applies a hard byte cap on the last newline boundary;
  `compress` chains both and reports `saved_bytes`. The rework prompt
  pipeline (plan 1.6) now runs every annotation comment through
  `compress(comment, 4096)`, so a pasted log/diff in a review comment no
  longer blows the token budget.

### Added (plan 1.6)

- **Inline plan/diff annotations (#3b):** reviewers leave comments pinned to a
  file (and optional line range) on an attempt via
  `GET/POST /v1/attempts/{id}/annotations` (SQLite `patch_annotations` table,
  migration 0057). "Send for rework" (`POST /v1/attempts/{id}/rework`)
  creates a new task whose prompt is the original prompt plus an
  `[ANNOTATIONS]` block, so the agent takes the review feedback in a retry.

### Added (plan 1.5)

- **Executor-verifier trust loop (#16):** a rejected verifier step (the
  verifier task exits non-zero) no longer fails the run — it re-runs the
  upstream worker task with the verifier's feedback appended to the prompt,
  then resets the verifier to re-activate on the next worker success. The
  loop budget is the upstream worker's `max_attempts`; once exhausted the
  verifier rejection becomes a hard step failure. The worker step's
  `attempts` counter (1 + verifier-reject retries) is the loop count,
  exposed via `/v1/workflow-runs/{id}` `steps[].attempts`.

### Added (plan 1.4)

- **Issue-as-task (#2b CLI):** `ag issue run <N> [repo]` creates a task from a
  GitHub issue via the `gh` CLI; `ag issue ls` / `ag issue show` for
  inspection.
- **GitHub issue webhook (#2a):** `POST /v1/webhooks/github/issues` with
  `X-Hub-Signature-256` HMAC-SHA256 verification; issues carrying the `agent`
  label auto-create tasks. Secret via `AGENTGRID_GITHUB_WEBHOOK_SECRET`
  (disabled when unset).

### Added (plan 1.3)

- **FTS5 task search:** `GET /v1/search?q=` (bm25-ranked, limit 50) over a
  triggered FTS5 mirror; `ag search` CLI; search box in the web dashboard.
- **Attempt detail + resume:** `GET /v1/attempts/{id}` returns the attempt
  with the owning task's prompt; `ag resume <attempt_id>` creates a fresh
  task inheriting that prompt/context.
- **Task tags:** join table + `GET/POST/DELETE /v1/tasks/{id}/tags[/{tag}]`;
  `ag tag add/remove/list`.

### Added (plan 1.2)

- **Mobile notify webhook:** control-plane POSTs `AGENTGRID_NOTIFY_WEBHOOK` on
  terminal task states. Success with a pending patch review reports
  `awaiting_review` (not `completed`) so the operator gets a push to review.
- **Deterministic pre-merge resolve:** `deploy/pre-merge-resolve.sh` resolves
  trivial merge conflicts (both-add, import-both, formatting-only,
  delete/modify) before the LLM path. Opt-in via `AGENTGRID_PRE_MERGE_RESOLVE`;
  integrator cherry-pick/apply runs it when a conflict appears.

## [v0.3.1](https://github.com/earendil-works/agentgrid/tree/v0.3.1) - 2026-08-07

### Added (0.3 pass — final release artifacts)

- **Musl static-link release binaries:** Dockerfile.control-plane-musl produces
  `x86_64-unknown-linux-musl` control-plane + CLI, fully self-contained with web UI.
  Image `ag-cp:musl` = 11MB, no libc dependencies, runs anywhere on Linux x86_64.

- **Transport selection runbook:** docs/runbook-transport.md explains poll vs WS
  tradeoffs, deployment guidance, monitoring metrics, testing commands, and load
  baseline numbers (100-node harness: wall=30.4s, p50=21.3s, write_lock_failures=0).

### Operator Documentation

- **OPS-STARTER.md:** Quick deployment guide for Docker Compose and systemd installations,
  monitoring setup, backup procedures, and common operations.
- **TROUBLESHOOTING.md:** Comprehensive troubleshooting guide for transport issues, resource
  constraints, task execution failures, disk problems, WebSocket-specific issues, performance
  optimization, security concerns, and disaster recovery procedures.

### Metrics & Baselines

- **CP idle RSS:** 4 MiB VMRSS (well under 96 MB budget)
- **Load baseline (100 nodes):** 30.4s wall time for 1000 tasks, p50=21.3s assign latency,
  p99=29.5s, write_lock_failures=0 across both `poll` and `ws` transports
- **E2E verification:** All four transport combinations pass (`run.sh` + `run-two-host.sh`,
  both `AGENTGRID_TRANSPORT=ws` and `poll`)

### Changed (deployment fixes)

- Fixed AGENTGRID_JWT_SECRET export chain in `deploy/compose/up.sh` so compose
  interpolation succeeds reliably
- BuildKit cache-mount fix in Dockerfiles: copy to /out inside same RUN to hide binaries
  from subsequent COPY --from layers

See also:
- docs/plans/0.3-websocket-and-scale.md — full plan with stage-by-stage breakdown
- docs/load-baseline-0.3.md — detailed performance analysis and reproduction steps
- docs/node-ws-protocol.md — WebSocket protocol specification


## [v0.3.0](https://github.com/earendil-works/agentgrid/tree/v0.3.0)

*Legacy tag — superseded by v0.3.1 which includes all features plus final documentation and musl build.*

WebSocket channel implementation completed per plan 0.3 stage 2:

- ADR 0009 + node WS protocol spec
- CP endpoint `/v1/node/ws` with Bearer auth, hello/hello_ok handshake
- Node-daemon WS client (tokio-tungstenite/rustls, no OpenSSL)
- WS resilience: reconnect backoff, fencing tokens on ack path, cancel propagation
- Transport selection: `AGENTGRID_TRANSPORT=ws|poll|auto` with fallback
- E2E tests for both transports green
- Failure injection test: CP kill mid-attempt survives with durable outbox
- Initial Stage 3 load baseline measurements (Stage 3.1–3.2 finalized in v0.3.1)
