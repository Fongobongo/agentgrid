# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Added (0.2-completion pass, RBAC)

- **Admin / operator roles (plan 5.2):** users gain a `role` (migration
  0052; existing users stay `admin`) carried as a JWT claim. The auth
  middleware rejects any operator request that is not a view (GET/HEAD),
  an approval allow/deny, a plan approval, or logout — so an operator
  cannot create tasks, workflows, users, or node enrollment tokens (403).
  Admins manage accounts via `GET/POST /v1/users` (role defaults to
  `operator`); the first user bootstrapped via `/v1/auth/setup` is always
  an admin. Tokens minted before this change decode as admin.

### Added (0.2-completion pass, CLI)

- **`ag status` one-screen overview (plan 6.1):** server health plus
  nodes / tasks / workflow-run counts grouped by status; each section
  degrades to "(unavailable)" instead of aborting the whole overview, and
  `--json` emits the same data machine-readable.
- **`ag completions <shell>` (plan 6.1):** generates bash/zsh/fish/elvish/
  powershell completion scripts via `clap_complete`.
- **Unified API error format (plan 6.1):** non-2xx responses now share one
  message shape, and 401/403 tell you to run `ag login`. `ag --json token
  create` emits `{"token": …}` instead of the shell export line, and
  `ag nodes list` reports auth failures instead of a decode error.

### Added (0.2-completion pass, plan approval)

- **Plan approval works with real agents (plan 2.3):** adapters now surface a
  machine-readable plan when the agent fences it — `adapter-opencode` scans
  assistant text and `adapter-claude` the final `result` for ` ```plan `
  blocks and emit `plan` events. The node daemon captures the last such plan
  and carries it on the attempt completion, so an `expandable` architect step
  pauses the workflow run in `PlanReady` and `POST /v1/workflow-runs/{id}/approve-plan`
  expands the approved plan into steps — no mock-only plumbing left. The
  projection now carries the pending plan (`pending_plan`) on `plan_ready`
  runs, and the Workflows UI shows it for review next to the Approve button.

### Added (0.2-completion pass, budgets)

- **Token/cost budgets actually fire (plan 1.4):** real adapters now report
  usage — `adapter-opencode` translates `step_finish` and `adapter-claude`
  the final `result` line into `progress` events with `tokens` / `cost_cents`
  (stored as `metric`). The workflow budget check sums them per run and, on a
  breach, parks the run `Blocked` **and cancels the in-flight step tasks**,
  so an over-budget run stops consuming agents instead of just pausing the
  scheduler. The run projection's budget snapshot shows the same usage.

### Added (0.2-completion pass, web/UI)

- **Change stream (plan 3.2):** `GET /v1/stream` — SSE endpoint that emits
  `hello` on connect and `change` whenever the task/node/workflow-run status
  fingerprint moves (server-side 500 ms poll of three aggregate count
  queries). Dashboard, Nodes and Workflows pages now refresh from this
  stream instead of fixed-interval polling: status changes land in the UI in
  under a second, and an idle page makes zero requests.
- **Workflow step outcomes (plan 3.3):** the workflow projection now carries
  each step's `prompt`, latest-attempt `commit_sha` (rendered as a diff link
  when the repo URL is https, plus a link to the step's task) and the latest
  adapter `result` text (capped at 2000 chars); the budget table shows usage
  as a percentage of each ceiling.
- **Audit view (plan 3.4):** `GET /v1/audit?action=&limit=` (limit 1..500,
  default 100, 503 on storage outage like other lists) and a web Audit page
  with an action filter showing the newest 100 decisions (actor, action,
  subject, payload).

## [0.4.0] — 2026-08-06

Closes the former `[Unreleased]` section below (everything between this header
and `[0.1.1]` is the 0.4.0 feature surface) and adds the MVP-completion pass
from `docs/plans/0.2-completion.md`:

### Added (0.2-completion pass)

- **Automatic backups:** the control-plane maintenance loop takes
  `VACUUM INTO` snapshots (`auto-backup-<unix-ts>.db`) every
  `AGENTGRID_BACKUP_EVERY_SECS` (default 24h) and keeps the newest
  `AGENTGRID_BACKUP_KEEP` (default 5). Metrics:
  `agentgrid_last_backup_at_unix`, `agentgrid_last_backup_age_seconds`,
  `agentgrid_backup_errors_total`. Backup path validation confines writes to
  the data directory (400 on traversal attempts).
- **Unsafe-mode ack gate:** a node with `AGENTGRID_UNSAFE_UNATTENDED=1` now
  refuses to start without the explicit operator acknowledgement
  `AGENTGRID_I_UNDERSTAND_UNSAFE=1` (fail-closed).
- **Backup/restore rehearsal:** `tests/e2e/run-restore.sh` — backup on one
  instance, restore into a fresh CP, assert old state + run a new task.
- **Docs:** `docs/adapters.md` (adapter contract), `docs/deploy/control-plane-failure.md`
  (failure/recovery runbook), `docs/deploy/credential-rotation.md`,
  ADR 0007 (gateway/ACP frozen at prototype until MVP 0.2 is done).
- **Web UI:** native browser dialogs (`window.prompt`/`confirm`) replaced with
  in-app Modal/ConfirmModal/PromptModal components (approval reasons included).
- **CI:** `cargo test -- --report-time` + 30-min timeout on the rust job;
  stress job runs the assignment race at 100 iterations
  (`AGENTGRID_RACE_ITERS`, default 10 per-PR).

### Fixed (0.2-completion pass)

- Race test now configurable (`AGENTGRID_RACE_ITERS`); per-PR suite drops
  from ~21 min to minutes while nightly keeps the 100-iteration grind.
- `validation_timeout_kills_forked_child_tree` no longer matches unrelated
  system processes (unique sleeper-script marker instead of `pgrep -f sleep`).
- Test databases under `/var/tmp/ag-test-*` are removed on `AppState` drop
  (no more thousands of leftover files per CI run).
- e2e/`deploy/compose` scripts: setup-token extraction, `ListResponse`
  envelope parsing, dead bootstrap envs removed (the scripts were silently
  broken; they now run green).
- Workflow ticker races: CAS-guarded step activation; a 20-way concurrent
  tick test proves no duplicate step tasks.
- Secret-leak regulator test asserts a configured secret never reaches
  emitted events or the raw-output artifact.

## [Unreleased]

### Breaking

- **Bootstrap:** `AGENTGRID_BOOTSTRAP_USER` / `AGENTGRID_BOOTSTRAP_PASSWORD` env vars removed — the one-time setup token (printed to stdout) is now the only bootstrap path.
- **Adapters:** `--dangerously-skip-permissions` (Claude) and `--auto` (OpenCode) are no longer enabled by default. Requires explicit `AGENTGRID_UNSAFE_UNATTENDED=1`.
- **SSH:** `ag nodes install` now fails closed on unknown host keys (no more `StrictHostKeyChecking=no`). Use `--accept-new-host-key` or `--host-key-fingerprint` to opt in.
- **API errors:** List endpoints (`/v1/tasks`, `/v1/nodes`, etc.) return `503 Service Unavailable` on DB error instead of an empty array.
- **Docker:** Production `docker-compose.yml` no longer contains default credentials (`admin/changeme`, `dev-insecure-secret-change-me`). Use `docker-compose.demo.yml` for insecure local testing.
- **Node:** Daemon refuses to run as root unless `AGENTGRID_ALLOW_ROOT=1` is explicitly set.

### Security (hardening P0/P1 — global event cursor, validation lifecycle, durable outbox/artifacts)

- **Global event cursor:** `task_events` gains a monotonic `ingest_id` (migration
  0037, single-row `event_ingest_counter`). SSE `id:` / `Last-Event-ID`,
  `GET /v1/tasks/{id}/events?after_ingest=&limit=`, the web client, CLI
  `ag logs --follow` and the TUI all resume on the global cursor, so events
  across retried attempts stay in true time order (a new attempt's seq-1 never
  renders before an old attempt's tail). Legacy `after_sequence` clients keep
  working.
- **Validation lifecycle:** new `POST /v1/node/attempts/{id}/begin_validate`
  moves attempt+task `running → validating` (CAS, ownership + fencing checked).
  `run_validation` now runs with a process group, a bounded per-attempt timeout
  (`validation_timeout_secs`, default 300), cancellation support and full
  process-tree kill; stdout/stderr are piped separately (no more `2>&1`
  shell-format) through the capped/lossy `read_stream` into `validation.log`.
  Distinct outcomes: `validation_failed` / `validation_timeout` /
  `validation_cancelled`.
- **Crash-safe completion payload:** the durable `completions.jsonl` now carries
  the full `CompleteAttemptRequest` (plan, provenance, resolved base, remote
  HEAD snapshots, pending artifacts) and re-sends all of it on redelivery —
  nothing is dropped if the first send fails.
- **Outbox quarantine + global quota:** corrupt spool lines are moved to a
  `quarantine/` directory instead of being silently dropped; a global outbox
  quota (`AGENTGRID_OUTBOX_QUOTA_BYTES`/`_MB`, default 1 GiB) caps total spool
  growth across attempts.
- **Durable artifact spool:** produced artifacts are staged into
  `<data>/artifact-spool/<attempt>/<name>` before upload (atomic), survive the
  worktree cleanup, and are retried on the next daemon startup. Completions
  report still-owed artifacts (`pending_artifacts`, migration 0038).
- **Unsafe-node visibility:** the heartbeat advertises `unsafe_active` +
  `permission_interception` (structured/wrapper/none); `ag nodes`, `ag node
  doctor`, the TUI node pane and the web Nodes table display an `UNSAFE` /
  `⚠ unsafe` badge (migration 0039).
- **Storage GC:** new `ag storage gc [--dry-run]` / `ag storage disk` +
  `POST /v1/admin/storage-gc` reconcile the artifact tree against metadata
  (orphan files unlinked, dangling rows pruned; dry-run reports only). The
  control plane now refuses NEW assignments when the artifact volume drops
  below `AGENTGRID_DISK_CRITICAL_MB` (default 512 MiB) via statvfs.
- **Bounded event flushes:** `EventSink::flush`/`flush_quick` and
  `drain_outbox` split large event buffers into chunks at the CP batch cap
  (`AGENTGRID_MAX_EVENT_BATCH`/`_KB`) instead of one oversized POST — a big
  attempt's output can no longer be rejected wholesale with 413 and pending
  spool is never held entirely in RAM.
- **Cleanup observability:** `git::PruneStats` (pruned/quarantined/
  worktrees-pruned) logged after stale-workspace pruning.
- **Typed API errors:** new `api_error(status, code, message)` envelope
  (`{"error": {code, message, request_id}}`) with stable machine-readable
  codes documented in `docs/openapi.yaml`. Fixed a real bug: an invalid
  attempt transition from `complete_attempt` returned 500 instead of 409 —
  anyhow wraps the raw `InvalidTransition` (not the `StoreTransitionError`
  marker), so the handler now downcasts both shapes. Test:
  `invalid_transition_returns_typed_error_envelope`.
- **Tasks pagination:** `GET /v1/tasks` supports keyset cursor pagination
  (`after_created_at` + `after_id`, stable `(created_at, id)` order) and a
  client `limit` (server ceiling 1000). Test:
  `list_tasks_keyset_pagination`.
- **DB integrity (migration 0040):** `attempts` gains FKs to `tasks`/`nodes`
  (ON DELETE RESTRICT) + a `status` CHECK; `task_events` and `artifacts` gain
  FKs to `attempts` (ON DELETE CASCADE). The control plane already enforced
  these invariants in handlers; the DB now backstops NEW writes (orphan rows
  from pre-0040 databases are surfaced by `count_orphan_rows`/`storage_gc`
  and cannot be recreated through the app). Tests:
  `foreign_keys_enforced_after_migration_0040`.
- **Security profile in task view:** `TaskView.security_profile` surfaces the
  latest attempt's policy (from `attempts.provenance.security_profile`); the
  web TaskDetails page shows it. Test:
  `task_view_surfaces_security_profile`.
- **Node storage pressure (migration 0041):** the heartbeat now reports
  `outbox_bytes` + `artifact_spool_bytes` (local event/completion outbox and
  staged-artifact spool totals); `ag nodes` and the web Nodes table show a
  combined `SPOOL` column so backing-up nodes are visible at a glance.
- **Node drain (migration 0042):** `POST /v1/nodes/{id}/drain?drain=` +
  `ag nodes drain <id> [--undrain]` (web Drain/Undrain button) stops NEW task
  assignments on a node for maintenance while its in-flight attempts finish
  and the heartbeat stays online. Test:
  `node_drain_blocks_new_assignments_until_undrained`.
- **Repository cache GC:** `prune_stale_workspaces` now runs
  `git gc --auto --quiet` on each bare mirror after `worktree prune`, so
  incremental pack growth is compacted without ever deleting an in-use mirror.
- **Artifact integrity hash:** artifact downloads (user + node paths) return
  `X-Artifact-Sha256` with the server-computed hash; the web TaskDetails page
  shows the truncated hash for `changes.patch` and `validation.log`.
- **CI/supply-chain hardening:** all workflow actions pinned to commit SHAs
  (Dependabot bumps them); new jobs for CodeQL (Rust+JS), gitleaks secret
  scan (full git history), CycloneDX SBOM, and Miri (`agentgrid-common`).
  Dockerfiles use BuildKit cache mounts (cargo registry + target, web
  node_modules) and pin base images by digest. Release pipeline smoke-tests
  every published binary before computing SHA256SUMS.
- **Agent env isolation (hardening P1 item 27):** the adapter subprocess no
  longer inherits the node daemon's full environment. Both spawn paths
  (`ProcessBackend` + ACP) start from a clean `PATH`/`HOME` plus the explicit
  `AGENTGRID_ADAPTER_ENV` allowlist and profile-declared secrets; the daemon's
  credentials/API keys/`AGENTGRID_SECRETS` never reach the agent. Test:
  `spawn_does_not_inherit_daemon_env`.
- **Secret redaction variants (hardening P1 item 27):** `mask_secrets` now
  also masks base64- and percent-encoded forms of each configured secret, so
  a diagnostic that printed an encoded secret cannot leak the raw value.
  Tests: `mask_secrets_masks_encoded_variants`,
  `base64_encoder_known_values`.
- **DB integrity (migration 0043):** `node_repositories` gains FKs to
  `nodes`/`repositories` (ON DELETE CASCADE) and `approvals` gains a FK to
  `tasks` + a `status` CHECK (attempt_id intentionally left unconstrained —
  approvals may precede a durable attempt row). Approval API tests updated to
  create a real task first.
- **Workflow runs pagination:** `GET /v1/workflow-runs` supports keyset cursor
  pagination (`after_created_at` + `after_id`, stable `(created_at, id)`
  order) and a server page cap. Test:
  `workflow_runs_keyset_pagination`.
- **Approvals pagination:** `GET /v1/approvals` supports the same keyset
  cursor + page cap alongside the `status` filter. Test:
  `approvals_keyset_pagination`.
- **CLI attempts in logs:** `ag logs` prefixes every event with its attempt id
  (`[seq] (att-xxxxxxxx)`) so output from a retried attempt is distinguishable
  at a glance.
- **Validation tree kill test:** `validation_timeout_kills_forked_child_tree`
  proves a validation timeout reaps the WHOLE process group — a forked
  background sleeper is killed with the shell, not orphaned (hardening P0
  item 12).

### Security (hardening P1 §25 — Docker sandbox hardening)

- **Docker run hardening:** `AGENTGRID_SANDBOX=docker` now wraps the agent in a
  locked-down container instead of a bare `docker run`. The sandbox command
  always adds `--cap-drop=ALL`, `--security-opt=no-new-privileges`, and
  `--network none` (override via `AGENTGRID_SANDBOX_NETWORK`); pins the image
  by digest when `AGENTGRID_SANDBOX_IMAGE_DIGEST` is set; and can opt into a
  read-only root + tmpfs `/tmp` (`AGENTGRID_SANDBOX_READ_ONLY=1`) and resource
  ceilings (`AGENTGRID_SANDBOX_PIDS_LIMIT`, `_MEMORY`, `_CPUS`). Only the
  worktree is mounted (`/ag`) — the Docker socket, host home and SSH/agent
  credentials are never attached. Tests assert cap-drop / no-new-privileges /
  network-none / read-only+tmpfs / pids-memory-cpus flags and the digest pin are
  emitted. `AGENTGRID_SANDBOX_IMAGE` has no default credentials change.
- **Sandbox isolation (ADR 0005):** task network modes map to docker-native
  networks (`restricted` → `--network none`, strictly more isolated than
  promised; `unrestricted` → `bridge`). `allowlist:` egress specs are
  validated but refused at startup — docker cannot express per-CIDR egress
  filtering, so running would silently mean full egress. Every container is
  stamped `--label agentgrid.node=<id>` and this daemon's orphans are removed
  at startup. Adapters are smoke-tested inside the image (`command -v <bin>`);
  a missing in-image adapter marks the node degraded. The runtime version is
  probed at startup. `AGENTGRID_SANDBOX_ARTIFACT_DIR` mounts a writable
  `/artifacts` independent of `--read-only`. `enforced_limits` in heartbeats
  is true only when Docker + resource limits + network `none` all hold.

### Security (hardening P0/P1/P2 — session hardening pass)

- **Artifacts/UI:** strict CSP (`default-src 'self'` for UI, `default-src
  'none'` for artifacts) + `X-Content-Type-Options: nosniff` +
  `Cross-Origin-Resource-Policy: same-origin` + `X-Frame-Options: DENY`, so an
  artifact is never executed or cross-read by a browser context.
- **Artifact path safety:** `attempt_id` validated as a safe opaque ID
  (`[A-Za-z0-9_-]`) before any path join; symlinked artifact dirs/files are
  rejected; a traversal/symlink `cleanup_workspace` target is refused.
- **State-machine invariants:** a terminal task now clears
  `assigned_attempt_id` and sets `finished_at` on cancel/node-lost; regression
  test `state_machine_terminal_invariants_hold`.
- **Event ingestion:** terminal attempts reject events; batches are bounded
  (`AGENTGRID_MAX_EVENT_BATCH`, `AGENTGRID_MAX_EVENT_BATCH_KB`).
- **Storage retention:** `cleanup_artifacts` deletes the backing file (not just
  metadata) and removes now-empty attempt dirs.
- **`active_attempts` reconciliation:** re-derived from attempt rows on startup
  (`reconcile_active_attempts`); orphan-row preflight detection.
- **Git:** binary diff captured as raw bytes (no lossy UTF-8); cross-process
  `flock` per repo with a timeout.
- **Output backpressure:** logical line size capped
  (`AGENTGRID_MAX_LINE_BYTES`); outbox ACK is O(1).
- **API errors:** list handlers return `503` on DB error instead of an empty
  list; `list_tasks`/`list_nodes` capped at 1000 rows.
- **Observability:** `/metrics` adds `agentgrid_cross_node_rejects_total`,
  `agentgrid_stale_fencing_tokens_total`, `agentgrid_event_rejections_total`.
- **Release/CI:** `release.yml` runs tests before build, publishes a GitHub
  Release with per-target SHA256SUMS (now including `adapter-opencode`); a
  `supply-chain.yml` workflow runs `cargo audit`; Docker images carry OCI
  labels and a healthcheck; the control plane binds loopback by default in
  `install-control-plane.sh`; compose `.env` is stripped of enrollment tokens
  after the nodes enroll.
- Docs: `SECURITY.md`, README maturity matrix + ops sections (trust/ownership,
  event delivery, retention/backup/upgrade).

### Correctness (hardening P0 item 7 — lease/ACK race conditions)

- `ack_attempt`, `revert_expired_leases`, `mark_offline_nodes`,
  `mark_node_offline`, and `revoke_node` now run under a single write
  transaction with compare-and-set `WHERE status = ...` guards and
  `rows_affected()` checks. The task is only flipped when
  `assigned_attempt_id` still points at the attempt; `active_attempts` is
  decremented only for attempts the CAS actually moved.
- This removes the double-flip and concurrency-counter-drift races between a
  late ACK and the lease sweep, and between an offline sweep and a fresh
  heartbeat.
- Regression tests: late ACK after lease expiry is idempotent (task stays
  queued, counter decremented once); ACK-then-expire leaves the running
  attempt untouched; a fresh heartbeat beats a subsequent offline sweep.

### Security (hardening P0 — safe node install)

- `ag nodes install` no longer passes `StrictHostKeyChecking=no` to SSH: it
  **fails closed** on an unknown host key by default. New flags opt in:
  `--accept-new-host-key` (ssh accept-new) and a pinning
  `--host-key-fingerprint SHA256:...` (ssh-keyscan + ssh-keygen -lf compare;
  mismatch refuses the host and the trusted key is added to `~/.ssh/known_hosts`).
- The installer no longer auto-bakes `AGENTGRID_ALLOW_ROOT=1` into the
  provisioned env — the node daemon refuses root unless the operator opts in
  with `--allow-root`. Prefer SSH-ing as an unprivileged user with a
  `--data-dir` owned by that user.
- The node daemon now scrubs `AGENTGRID_ENROLL_TOKEN` from its
  `AGENTGRID_ENV_FILE` after a successful first enrollment (atomic 0600
  temp+rename), so a rebooting node reuses `credential.json` and the one-time
  token cannot be leaked/reused off disk. Other env vars are preserved.
- `deploy/install-node.sh` keeps the enrollment token in its own `0600`
  `EnvironmentFile=`, exposes `AGENTGRID_ENV_FILE` so the daemon can scrub it,
  and adds the full systemd hardening directive set (PrivateDevices,
  ProtectKernel{Tunables,Modules,Logs}, ProtectClock/Hostname/Proc,
  ProtectControlGroups, RestrictSUIDSGID, LockPersonality, RestrictNamespaces,
  MemoryDenyWriteExecute, RestrictAddressFamilies).
- Regression tests: default install env has no `AGENTGRID_ALLOW_ROOT`;
  `--allow-root` adds it; host-key modes default strict; daemon scrubs the
  enroll-token line from the env file (atomically, 0600) and preserves other
  vars; missing file is a no-op.

### Security (hardening P0 — unsafe adapter defaults)

- The Claude Code adapter no longer adds `--dangerously-skip-permissions`
  unconditionally. The dangerous permission bypass is gated behind a single
  operator opt-in `AGENTGRID_UNSAFE_UNATTENDED=1` (default off = safe); the
  opencode adapter likewise gates `--auto` behind the same knob (the legacy
  `AGENTGRID_OPENCODE_AUTO` knob still opts in but is loudly warned). When on,
  the adapter prints a stderr warning naming the bypass; when off, it prefers
  to block on the first prompt rather than auto-approve destructive tools.
- Arg construction moved to `agentgrid_adapters::{claude_args, opencode_auto,
  unsafe_unattended_from_env, warn_unsafe}` so it is unit-testable.
- Regression tests: default args contain no dangerous flag; opt-in adds it;
  `--auto` off by default; unsafe knob resolution (1/true/0/false/garbage).

### Security (hardening P0 — static file traversal)

- The web UI SPA fallback rebuilt its target path with a lexically-flawed
  `root.join(rel)` + `fs_path.starts_with(root)`: a `/../` (or `%2e%2e/`, or
  backslash) segment passed the prefix check yet escaped the root after the
  OS resolved `..`. Now the request path is rebuilt from a **path-component
  whitelist** (`Component::Normal` only; `ParentDir`/`RootDir`/prefix
  rejected), the web root is **canonicalized** and the resolved file path is
  re-canonicalized and required to stay under it — a symlink inside the root
  pointing outside the root is blocked with `403`. axum percent-decodes the
  URI path before the handler sees it.
- `Cache-Control`: hashed assets under `assets/` get
  `public, max-age=31536000, immutable`; `index.html` and non-hashed files get
  `no-cache` so deployments take effect without a hard refresh.
- Regression test: `/../`, `/%2e%2e/`, mixed encoding, backslashes, absolute
  traversal, and a root-escaping symlink all rejected; hashed asset cached
  immutable; `index.html` `no-cache`.

### Security (hardening P0 — artifact integrity and download safety)

- Artifact uploads now **compute the SHA-256 server-side** and, when the
  caller supplies an `x-artifact-sha256` (raw) or `sha256` (JSON) hint that
  disagrees with the computed hash, reject with **422 Unprocessable Entity**;
  the artifact is not published. Only the server-computed hash is stored
  (`StoreArtifactError::HashMismatch`).
- Artifact writes are now **atomic** (write to a sibling `*.tmp.upload` then
  `rename`), so a crash between the write and the metadata commit cannot leave
  a half-written published artifact on disk.
- Artifact downloads are **stored-XSS safe**: a small allowlist of inline-safe
  media types may be served with their stored `Content-Type`; HTML / SVG /
  JavaScript / XML and unknown types are forced to `application/octet-stream`
  with `Content-Disposition: attachment` (RFC 6266 safe filename encoding,
  control/separator/quote chars and leading dots stripped) and
  `X-Content-Type-Options: nosniff` is added to every artifact response.
  Browsers never render an uploaded artifact inline. Added `artifact_response`.
- Regression tests: server-side hash enforcement + wrong-hash 422, atomic
  write, HTML/SVG downgraded to attachment + octet-stream + nosniff, unknown
  type forced to attachment, NUL/backslash/traversal name rejection expansions.

### Security (hardening P0 — auth fail-closed and safe bootstrap)

- User routes are now **fail-closed** during the bootstrap window: before
  the first user exists, all `/v1/` routes return `503 Service Unavailable`
  except `/v1/auth/setup` (plus `/health/*` and static UI). The previous
  open bootstrap window that let anyone create tasks/repositories/tokens is
  closed.
- `require_user_auth` fails **closed on DB error** (returns `503`, never
  falls through to `401`/allow) so a transient SQLite outage cannot be
  confused with "no users / public".
- First-run bootstrap now requires a **one-time setup token** (32 hex chars,
  15-min TTL) minted when no users exist and printed to stdout/console only.
  `POST /v1/auth/setup` rejects without it and **consumes it on first use**;
  a second setup after a user exists returns `409`.
- **Env bootstrap removed** (`AGENTGRID_BOOTSTRAP_USER/PASSWORD` no longer
  auto-create the first user) — it was an unauthenticated credential
  backdoor. The setup token is now the only bootstrap path.
- Production mode (`AGENTGRID_ENV=production`) **refuses to start** without
  `AGENTGRID_JWT_SECRET` ≥ 32 bytes; a random per-run secret is used only
  in non-production.
- Docker defaults hardened: the production `docker-compose.yml` has **no
  baked-in `admin/changeme` or `dev-insecure-secret`"; secrets are generated
  by `deploy/compose/up.sh` (random JWT secret + admin password, reads the
  setup token from the control-plane logs to complete bootstrap). A separate
  `docker-compose.demo.yml` keeps insecure static defaults for local hacking
  only.
- `deploy/compose/down.sh` gained `--demo` to target the demo compose file.
- `deploy/install-control-plane.sh` no longer bakes `admin/changeme` into
  the systemd unit; first-run setup is done via the printed setup token.
- Web UI setup form now requires the one-time setup token field.
- Regression tests: closed bootstrap window (user routes `503` pre-user,
  setup requires token, token is one-time, second setup `409`), DB-error
  fail-closed, production JWT-secret bail.

### Security (hardening P0 — cross-node isolation)

- All `/v1/node/attempts/*` mutation endpoints (event ingest, complete, ack,
  cancel-poll, agent-session create, artifact upload JSON + raw) now enforce
  that the authenticated node owns the target attempt. A foreign attempt
  yields `403 Forbidden`; a missing attempt yields `404` (no existence
  disclosure). Previously any node credential could mutate any attempt by id.
- `GET /v1/node/tasks/{id}/artifacts/{name}` (upstream `changes.patch` fetch)
  is now authorized: only the consumer node whose workflow step depends on
  the producer task may read it. Unrelated nodes get `404`. Added
  `Store::attempt_owner` and `Store::can_node_read_upstream_artifact`.
- Artifact name traversal validation now runs before the ownership check so
  a crafted name never reaches the store or discloses attempt existence.
- Threat model updated: T2 (artifact traversal) and new T15 (cross-node
  isolation) reflect the enforced mitigations.
- Regression tests: cross-node ACK/events/complete/session/artifact-upload/
  cancel-poll/revoke all rejected; positive case for upstream artifact read
  by a workflow consumer node.

### Added (tests — two-host E2E on physical hosts)

- `tests/e2e/run-two-host.sh`: process-based, no Docker, no second CI runner.
  Brings up the local control plane (listening on 0.0.0.0) + a local node,
  uploads the debug-gnu `agentgrid-node-daemon` + `adapter-mock` binaries to
  the remote Linux host over SSH (`tests/e2e/remote-ssh.py`, same glibc 2.36),
  enrolls the remote node with a fresh single-use token, then defines a
  workflow pinning workers to the remote host and integrator+verifier to the
  local host. Asserts `succeeded` and that the projection shows steps ran on
  both node ids (provenance). Closes the  "the same
  manifest works on one PC and on two hosts". Remote host creds come from
  `.env` (`AG_REMOTE_*`); the documented follow-up is wiring this into CI on
  a second runner.

### Added (control-plane+node — per-profile MCP server subset, )

- `AgentProfile.mcp_server_ids` (and `AgentProfileCreate.mcp_server_ids`):
  an optional allow-list of MCP server ids the profile attaches to its
  sessions. Non-empty = attach only the listed registry servers (by id);
  empty (default) = attach every enabled server. Migration `0031_profile_mcp_subset.sql`.
- Node `mcp_servers_payload(client, server, &subset)` now filters the
  operator-trusted registry by the per-profile subset before serializing
  `SessionNewParams.mcp`. Lets an operator restrict MCP tools per adapter
  without splitting the registry. Real stdio spawn / `tools/list` discovery
  stays inside the ACP adapter (it consumes the `mcp` field we project), so
  this completes the registry-side half of the follow-up.
- Tests: node `mcp_payload_subset_filters_to_profile_allow_list`;
  common `profile_carries_secret_requirements_and_adapter_version` asserts
  `mcp_server_ids` round-trips; CP `agent_profile_revisions_immutable_and_roll_back`
  asserts the subset persists through create→activate→fetch.

### Added (control-plane+node — distributed patch-bundle fallback)

- `Assignment.upstream_task_ids` (parallel to `upstream_commits`, same order):
  control-plane resolves each upstream worker's `(commit_sha, task_id)` pair
  via `upstream_refs_for_task` and surfaces both in the assignment, so a node
  running an integrator/verifier step can fetch each worker's `changes.patch`
  artifact from the control plane as a fallback when the SHA is not reachable
  via the shared Git remote (distributed workflow without a shared remote,
  e.g. workers and integrator on different physical hosts without a common
  origin).
- Node `git::prepare_workspace` now, for each upstream SHA, runs
  `git fetch origin <sha>` + `git cat-file -e <sha>`; if the commit object is
  absent it falls back to `git apply --3way` of the worker's `changes.patch`
  artifact fetched up front (`GET /v1/node/tasks/{id}/artifacts/changes.patch`).
  Local cherry-pick is still the fast path when the SHA is reachable.
  Best-effort: an upstream with neither a reachable SHA nor a patch is skipped
  (integrator still runs with one fewer merged worker).
- New control-plane route `GET /v1/node/tasks/{id}/artifacts/{name}` mirrors
  `GET /v1/tasks/{id}/artifacts/{name}` but authenticates with the node's
  own credential (`require_node_auth`) — nodes have no user JWT.
- Tests: node `integrator_applies_patch_bundle_when_sha_not_reachable`
  (git unit; fake-unreachable SHA + patch-only path lands the worker change);
  CP `artifact_upload_and_read` extended to assert a second node-credential can
  fetch the upstream `changes.patch`; CP store tests assert `upstream_task_ids`
  parallel to `upstream_commits` for integrator + verifier.

### Added (tests — slow-network failure injection)

- `tests/e2e/run-slow-net.sh` + `tests/e2e/throttle-proxy.py`: process-based,
  no Docker / no `tc`. The proxy sleeps before every socket write
  (`AG_PROXY_DELAY_MS`, default 200 ms) so a chatty mock task (`spam:200`, 200
  events) traverses a 250 ms/write-inflated link; the run still reaches
  `succeeded` with contiguous event sequences (no gaps / no duplicates / no
  spurious timeouts). Covers the last open failure-injection checklist item.

### Added (cli — full-screen TUI dashboard `ag tui`)

- New `ag tui` subcommand: a full-screen monitoring dashboard over the
  control plane, built on ratatui + crossterm. Layout: header bar (server /
  task count / online node count / live phase) + sidebar (task list with
  phase-colored markers + a node-status sub-list) + main pane (the selected
  task's event stream, colored by kind, scrollable, with follow-the-latest
  mode) + footer keybind hint bar. Overlays: `?` help, `i` task detail.
- Keys: ↑/↓ or j/k to move the focused pane; PgUp/PgDn (Ctrl-U/D) scroll the
  events; Tab toggles focus Sidebar ↔ Main; `r` refresh; `Enter` reload events;
  `f` toggle follow; `?` help; `i` detail; `q`/Esc/Ctrl-C quit. Esc/q closes an
  open overlay instead of quitting.
- Read-only (task creation/cancellation stays on `ag run` / `ag task cancel`);
  no store/migration change (queries existing /v1/tasks, /v1/nodes,
  /v1/tasks/{id}/events, /v1/approvals). State/render split mirrors herdr's
  `app/state` vs `ui` boundary (no god object).
- RAII `TerminalGuard` restores the terminal on panic/err so raw mode never
  leaks. `--no-color` disables ANSI.
- Tests: `phase_from_event_table`, `format_event_tool_and_file`,
  `sidebar_move_and_scroll_clamp`, `map_key_basics`.
- Inspired by [herdr](https://github.com/ogulcancelik/herdr)'s state/render
  separation and lifecycle-state idea; we did not port herdr's PTY-pane
  multiplexer (different domain — we monitor an orchestrator, not manage
  terminal panes).

### Added (cli — live lifecycle phase + colored `ag logs`)

- `ag logs` now derives and prints a client-side lifecycle `Phase` (`starting |
  working | blocked | done`) per iteration, orthogonal to the terminal
  `AttemptStatus`. `working` follows tool/file/stdout events; `done` follows
  `result`/`error` or terminal status; `blocked` overlays when a durable
  approval is pending for the task (queried from the existing approvals table,
  no store/migration change). Mirrors the herdr agent-state idea but computed
  client-side from the events the control plane already emits.
- Colored output (ANSI): tool cyan, stderr red, result green, error bright red,
  status yellow; `--no-color` to disable.
- Pretty event payload: `tool_call` → tool + input, `file_change` → op +
  path, not just the `text` field.
- Tests: `phase_from_event_lifecycle`, `paint_no_color_passthrough`.
- Idea credited to [herdr](https://github.com/ogulcancelik/herdr)'s agent
  lifecycle enum; a full-screen TUI (ratatui multi-task dashboard) is backlog,
  YAGNI until operator-side ssh friction proves it.

### Added (node-daemon — per-agent native projection, )

- The agent profile is now also projected into each adapter's native convention
  file, not just `AGENTS.md`. `native_projection_files(adapter_id)` maps
  `claude → CLAUDE.md` (verbatim copy of the profile text); other adapters
  honor `AGENTS.md` and return empty. Table is meant to grow as adapters are
  observed to ignore `AGENTS.md`.
- Test: `native_projection_files_table`.

### Fixed (node-daemon — sandbox the legacy wrapper-binary spawn, )

- The legacy `ExecutionBackend` wrapper-binary spawn path (raw adapter subprocess)
  bypassed the sandbox that the ACP path applies via `sandbox_command`. New
  `SpawnRequest::sandbox_prefix_args` + `sandbox::sandbox_prefix(kind, workdir,
  program)` route the legacy spawn through the configured sandbox too.
  `AGENTGRID_SANDBOX=docker` now wraps the `adapter-<id>` binary in `docker run
  --rm -i -v <wd>:/ag -w /ag <image> --` for legacy attempts; default `none` is
  passthrough as before.
- Tests: `none_prefix_passthrough`, `docker_prefix_wraps_program`.

### Fixed (node-daemon — bounded ACP session_cancel on interrupt)

- `drive_acp_session`'с `session_cancel` RPC on the cancel branch was unbounded: an
  ACP subprocess already tearing down (or ignoring `session/cancel`) could park
  `drive_acp_session` forever past the cancel, blocking the attempt from
  reaching its terminal `cancelled` state. Now wrapped in a 2s timeout, after
  which `terminate_group` + bounded reap still force-terminates the subprocess.
- New test: `drive_acp_session_cancel_mid_prompt_turn` (dummy cancel-ready CP,
  fake-acp hang mode made interruptible via a stdin-reader thread).

### Fixed (node-daemon — adapter crash mid-line, )

- `read_stream` no longer silently drops an adapter's final partial output when
  the process is killed mid-line (no trailing newline on EOF). It previously
  used `BufReader::lines()`, which swallows a partial tail; now it reads byte by
  byte and flushes the remainder as a final raw `stdout`/`stderr` line, so a
  crashed adapter's last half-event is preserved (best-effort) instead of lost.
- Test: `read_stream_preserves_trailing_partial_line_on_eof`.
- Note: NDJSON-frame parsing on the ACP `session/update` path lives in the
  `agentgrid_acp` crate (Content-Length framed); a mid-JSON-RPC-frame crash
  regression test there is a follow-up (needs a mock ACP peer).

### Added (node-daemon — cluster executor capability probe)

- The node now actually probes the `zeroshot` cluster-executor adapter's
  capability at startup and on each heartbeat (the pure `cluster::probe_decision`
  contract existed but was never wired in). New `probe_cluster_adapter` checks
  the container runtime (docker) presence, the executor binary presence, and
  the executor `--version` against `AGENTGRID_ZEROSHOT_VERSION` (default `"0."`)
  via the pure `probe_decision` helper. Fail-closed: missing runtime / missing
  binary / version-prefix mismatch → `ready = false`, so the node never claims a
  `zeroshot` task it cannot run (capability honesty, same discipline as the
  wrapper-adapter boundary in ).
- Tests: `cluster_probe_fail_closed_when_runtime_missing`,
  `cluster_probe_fail_closed_when_executor_missing`,
  `cluster_probe_fail_closed_on_version_mismatch`.

### Added (control-plane — independent verifier workspace isolation)

- The independent verifier step now starts from the worker's tree without
  ever touching the worker's private transcripts. By the 
  (`HandoffPackage` references commits, not transcripts), `render_handoff_block`
  already injects only summary + commit SHA; the verifier's worktree is its
  own. The bridge to the tree was missing: `upstream_commits_for_task` now
  carries the verifier's upstream worker SHA(s) (previously restricted to
  the `integrator` role), and the node's `prepare_workspace` cherry-picks the
  single upstream worker commit onto the verifier's base — so the verifier
  starts at the worker's tree (can read the change for the verdict) but never
  sees the worker's logs. Isolation holds by construction.
- Test: `verifier_assignment_carries_upstream_worker_commit_for_isolation`.

### Added (common + control-plane + node-daemon — integrator integration branch)

- An integrator workflow step now lands its upstream workers' commits into
  its worktree as an integration branch before the agent runs, instead of
  the integrator agent doing the merge by hand.
  - common: `Assignment.upstream_commits: Vec<String>` carries each upstream
    worker's winning commit SHA (default empty, `skip_serializing_if = empty`,
    backward compatible with legacy nodes).
  - control-plane: `Store::upstream_commits_for_task` resolves an integrator
    step's `depends_on` → each upstream worker step's task → the winning
    attempt `commit_sha`, and fills the assignment at `try_assign`. Non-
    workflow / non-integrator tasks and missing SHAs yield `[]` (best-effort,
    no block).
  - node-daemon: `prepare_workspace(..., upstream_commits)` cherry-picks each
    SHA onto the integrator's fresh worktree branch (best-effort `git fetch`
    so the object is present; token-validated SHAs as defense-in-depth). On a
    conflict it aborts the cherry-pick and surfaces a non-zero prep error,
    leaving the branch clean on `start_point` (no partial merge committed).
- Tests: `integrator_assignment_carries_upstream_worker_commits` (CP store),
  `integrator_cherry_picks_nonconflicting_worker_commits` (node git).

### Resolved (audit — regression-test checklist, Spec 22.1.1)

- Marked three regression-test checkboxes as covered after re-auditing the
  existing coverage rather than writing new code, so the gap list reflects
  reality:
  - `kill -9 node mid-running → no lost completions, no stuck running`
    (process E2E `tests/e2e/run-outbox.sh` scenario C: kill mid-running →
    node offline → attempt lost → task failed/node_lost → retry → restart
    node → succeeded; completion durability is modeled by scenario A).
  - `agent-raw-output.log` not leaking into the git commit/patch — already
    covered by `raw_and_validation_logs_excluded_from_commit_and_patch`
    (git.rs path-filter excludes raw + validation logs).
  - `node clock skew does not break leases/timeouts` — not applicable: the
    CP compares only its own wall clock (it stamps lease_expires_at /
    ack_deadline at assign time; heartbeat staleness is cut against CP now).
    The node never sends its own timestamp in HeartbeatRequest and times the
    agent off a monotonic `tokio::time::sleep`. With a single-instance CP
    there is no skew-sensitive path to break.

### Audit (Spec 22.1.1 / release — follow-up, second pass)

- Re-audit closed three more regression-checklist checkboxes by reading the
  existing tests/code rather than writing new code, plus one release artifact
  item:
  - Secret leakage in artifacts (`validation.log` / `agent-raw-output.log` as
    artifacts) — already safe: `run_validation(secrets)` masks both into the
    `validation.log` payload, and `read_stream` masks before writing the raw
    log. `upload_if_exists` carries the already-masked bytes. Artifacts
    inherit the same masking as the event stream.
  - Approval UI in web + CLI — confirmed covered (existing `#/approvals` web
    UI and `ag approvals list/allow/deny --reason`, plus
    `approval_flow_allow_deny_and_expiry` CP test). Parent checkbox closed.
  - Skill trust management UI/CLI — confirmed complete except the explicit
    "real enforcement on agent load" follow-up (marked "не сейчас"). Parent
    checkbox closed.

### Known Limitations

- Wrapper adapters (Claude Code, OpenCode) do not support structured permission interception — only the `AGENTGRID_UNSAFE_UNATTENDED=1` bypass knob is available. Running without a sandbox in unsafe mode grants full host access.
- Workflow engine (schedules, plan expansion, DAG execution) is marked **experimental** and may change without notice.
- ACP gateway and Telegram gateway are **experimental** — protocol support is incomplete.
- Skills/profiles/MCP server registry are **experimental** — node-side enforcement of secret requirements and adapter version compatibility is partial.
- No built-in cgroup/Docker sandbox is enforced by default; the node daemon reports `enforced_limits=false`.
- Repository cache and workspace GC policies are not yet implemented — operator must manage disk manually.
- Event SSE cursor is per-attempt sequence only; no global monotonic `ingest_id` for cross-attempt ordering.
- Crash-safe outbox uses segmented JSONL files; no SQLite outbox option yet.
- Durable artifact uploads (resumable/chunked, retry after restart) are not implemented.

### Release (release.yml — SHA256 checksums)

- Release binaries now ship a per-target `SHA256SUMS` so consumers can audit
  downloaded artifacts; generated after the build step and uploaded alongside
  the binaries. MSRV stays `rust-version = "1.85"` in Cargo.toml. SBOM and
  signing/attestation (cosign, attest-build-provenance) deferred.

### Added (control-plane — background workflow ticker / restart recovery)

- Workflow runs no longer strand after a control-plane restart or when a
  node finishes a step task out-of-band (no one calls
  `POST /v1/workflow-runs/{id}/tick`). New `Store::start_workflow_ticker` runs
  a background task that, every `AGENTGRID_WORKFLOW_TICK_SECS` (default 5 s),
  lists all `Running` workflow runs via `running_workflow_run_ids` and calls
  `tick_workflow_run` on each. Recovery is best-effort per-run (a failing run
  is logged and skipped so one bad run cannot stall the ticker) and
  drop-the-first-sleep so a fresh boot picks up in-flight runs immediately.
- `tick_workflow_run` was already idempotent for in-flight steps (an
  already-`Running` step is not re-activated), so restart re-progresses
  runs without duplicating steps or tasks. Test:
  `restart_does_not_duplicate_in_flight_workflow_step_tasks`.

### Added (common + control-plane + node-daemon — heartbeat skill auto-discovery, )

- The trust ledger now auto-fills from what nodes discover on disk, closing
  the "heartbeat-report discovered skills для автозаполнения таблицы"
  follow-up. `HeartbeatRequest` carries a new `discovered_skills:
  Vec<HeartbeatSkill{ name, source }>` (default empty — backward compatible
  with legacy nodes). Each heartbeat, the node runs the skills crate
  `discover(standard_roots(...))` over the project/user/managed roots and
  advertises the resolved `(name, source)` pairs.
- Control plane: `Store::upsert_discovered_skills` does an idempotent
  `INSERT ... ON CONFLICT(name, source) DO NOTHING` — a fresh skill lands as
  untrusted (`trusted=0`, `decided_by='discovery'`) so it surfaces in the
  Skills UI for review, but an existing operator decision (trusted or
  untrusted) is never overwritten. Auto-discovery stays a hint: it never
  blocks a task and a lookup error degrades to a no-op.
- Tests: `upsert_discovered_skills_defaults_untrusted_and_preserves_operator_decision`
  (store), `heartbeat_auto_fills_skill_trust_ledger` (api).

### Added (tests/e2e — variable CP-outage failure injection, )

- `tests/e2e/run-outbox.sh` Scenario D: a tunable (`AG_E2E_OUTAGE_SECS`,
  default 10s) control-plane outage while the node stays alive and streams; on
  CP return the durable outbox redelivers 200 events contiguously (no gap, no
  dup). Models a hard network failure injection between node and CP, sized under
  the ack/lease window.
- New `stop_cp` helper stops the CP fast (SIGTERM → short grace → SIGKILL) so a
  long-poll / graceful-shutdown does not stretch the outage past the lease.

### Added (control-plane + node-daemon — parallel ready steps / distinct worktrees, )

- Tests codify that two independent ready steps (same repository) activate in a
  single workflow tick, each getting its own distinct task_id — parallel
  execution later runs as independent worktrees under the per-repo lock.
  `parallel_ready_steps_of_same_repo_activate_in_one_tick` (CP store).
- Node: `parallel_prep_same_repo_does_not_race` now also asserts four parallel
  attempt preparations produce four distinct worktree paths.

### Added (control-plane — Loop Engineering bytes + circuit breaker, )

- The workflow tick and the projection budget snapshot now observe the
  `max_bytes` and `max_repeated_handoffs` ceilings (previously left at 0).
  `Store::workflow_message_bytes` sums orchestrator-emitted payload lengths;
  `Store::workflow_repeated_handoffs` reports the longest consecutive same
  `(from_step_id, to_step_id)` handoff streak — broadcast outputs (`to: "*"`)
  reset the streak (a step-succeeded broadcast is healthy, not a solo
  ping-pong). A streak that exceeds the breaker parks the run `Blocked`.
- Tests: `budget_bytes_enforced_from_message_payload_size`,
  `circuit_breaker_trips_on_repeated_step_to_step_handoffs` (store).

### Added (control-plane + node-daemon + common — binary-safe artifact API, )

- Artifacts round-trip as raw bytes instead of UTF-8 JSON text, so binary
  diffs / archives / images are not corrupted. New node→CP endpoint
  `POST /v1/node/attempts/{id}/artifacts/raw` carries the bytes as the body
  and the name / optional media type / optional hex SHA-256 in headers;
  idempotent per `(attempt_id, name)` as before.
- common: `UploadArtifactRequest` gained optional `media_type`/`sha256`;
  new `ArtifactMeta { size_bytes, media_type?, sha256? }`.
- control-plane: `Store::save_artifact_bytes` / `read_artifact_bytes` /
  `read_artifact_meta`; the legacy JSON upload forwards to the binary store.
  `GET /v1/tasks/{id}/artifacts/{name}` now serves the stored content type
  (default `application/octet-stream`) and the raw bytes.
- Migration `0029_artifact_binary.sql` adds `media_type`/`sha256` columns.
- node-daemon: `upload_if_exists` switched to the raw-bytes path with a
  best-effort artifact media-type map.
- Tests: `artifact_binary_round_trip_preserves_bytes_media_and_hash` (store),
  `artifact_binary_raw_upload_round_trips` (api); existing text/upload and
  traversal tests still pass.

### Added (common + control-plane — handoff packages reference commits, )

- Handoff messages now carry compact *references*, never full transcripts. New
  `agentgrid_common::HandoffPackage { summary, commit_sha?, artifacts[] }`
  + pure `build_handoff_payload(summary, commit_sha, artifacts) -> String`.
- A step that succeeds emits an `output` message whose payload is a
  `HandoffPackage` (summary + the winning attempt's commit SHA; `artifacts`
  stays empty until a real artifact-store is wired).
- `render_handoff_block` unpacks the package fields (`- summary`/`- commit`/
  `- artifacts`) rather than dumping the raw JSON; `Note`/`Plan` kinds fall
  back to raw text.
- Tests: `handoff_payload_references_commit_and_artifacts_not_transcripts`,
  `render_handoff_block_injects_typed_messages_and_passes_when_empty` (common).

### Added (common + control-plane — typed AgentMessage mailbox, )

- Orchestrator-mediated typed inter-step messages (no free-form P2P).
  - common: `AgentMessage { from_step_id, to_step_id, kind, payload }` with a
    fixed `AgentMessageKind { Output, Plan, Note }` (no free-form kind — P2P
    backdoor closed). Pure `render_handoff_block(prompt, &[msg]) -> String`
    prepends a compact handoff block to the consuming step's prompt.
  - Migration `0028_workflow_messages.sql`: `workflow_messages`, plus a
    monotonic per-run `message_sequence`.
  - Control plane: `emit_workflow_message`, `messages_for_step` (targeted or
    broadcast `*`), and `workflow_message_count`. A step that succeeds has its
    `output` message broadcast by the orchestrator; on the next pending step's
    activation the matching messages render into the task prompt.
  - `BudgetUsage.messages` is now observable on both the tick enforcement path
    and the workflow projection snapshot.
- Tests: `render_handoff_block_injects_typed_messages_and_passes_when_empty`,
  `agent_message_kind_round_trips_snake_case` (common),
  `typed_mailbox_emits_output_and_renders_handoff_block_in_pending_step_prompt`
  (CP).

### Added (common + control-plane — architect plan expansion, )

- An architect workflow step can declare `expandable: Option<bool>`; when its
  winning attempt completes with a `CompleteAttemptRequest.plan` (YAML or JSON
  array of worker steps), the workflow tick pauses the run in a new terminal
  `WorkflowRunStatus::PlanReady`. The plan is stamped on the run row
  (`workflow_runs.plan`) so it outlives the attempt.
- New `agentgrid_common::parse_plan_steps(plan) -> Result<Vec<WorkflowStep>>`
  (pure): parses YAML/JSON, runs `validate_dag` on the resulting steps; rejects
  empty/cyclic plans.
- Migration `0027_plan_expansion.sql` adds `attempts.plan`,
  `workflow_steps.expandable`, `workflow_runs.plan`.
- API: `GET /v1/workflow-runs/{id}/plan` (projection incl. plan-ready status),
  `POST /v1/workflow-runs/{id}/approve-plan` (parse + insert expanded steps
  + resume Running; fail-closed 409 if not PlanReady / bad plan).
- Web UI renders an "Approve plan" button on a `PlanReady` run and an
  approveable-status popover.
- Tests: `parse_plan_steps_yaml_and_json_round_trip` (common),
  `architect_expandable_plan_pauses_planready_then_approve_expands_steps` (CP).

### Added (control-plane — repair-budget escalation, )

- A `retryable` workflow step that exhausts `max_attempts` now escalates to a
  human (`step Blocked` + run `Blocked`) instead of hard-failing the run. Only
  a non-retryable worker fast-fails (`Failed`). The Integrator conflict policy
  is unchanged. Integrates with `tick_workflow_run`'s transition path.
- Test: `retryable_step_exhausting_repair_budget_escalates_blocked`.

### Added (control-plane — budget snapshot in workflow projection, )

- `WorkflowProjection.budget: Option<BudgetSnapshot>` exposes the run's
  Loop Engineering budget state (limits + observable usage + first breach) so
  clients/UIs can render live budget health. Mirrors the enforcement path in
  `tick_workflow_run` (wall = now - created_at, rounds = count of steps past
  `Pending`). None when the template declares no budget.
- New `agentgrid_common::BudgetSnapshot { limits, usage, breach }`.
- Web UI `WorkflowDetails` now renders a Budget panel (per-field used/limit,
  breach highlighted) from the snapshot.
- Test: `workflow_projection_surfaces_budget_snapshot_when_template_has_budget`.

### Added (common — L4 schedule ratify gate, )

- Pure `agentgrid_common::ratify_l4_schedule(template, autonomy)`: a
  fully-autonomous `l4` schedule is fail-closed unless the template declares a
  `WorkflowBudget` (an unbounded loop must never be set on a timer). Non-l4
  schedules always pass. The node still routes spawned tasks through the
  configured command policy (external provider / default fail-closed `Ask`), so
  the command-policy check is not re-decided here.
- `Store::create_workflow_schedule` calls `ratify_l4_schedule` after the
  autonomy parse; a violation fails the create (callers surface it as
  `400 BAD_REQUEST` on `POST /v1/workflows/{tid}/schedules`).
- Tests: `ratify_l4_schedule_requires_budget_and_passes_lower_autonomy`
  (common), `l4_schedule_ratify_gate_refuses_without_budget_accepts_with` (CP).

### Added (control-plane — Loop Engineering budget enforcement, )

- `tick_workflow_run` now enforces a workflow template's budget. Each tick it
  fetches the template's `WorkflowBudget`, computes a coarse `BudgetUsage`
  snapshot via the pure `agentgrid_common::compute_budget_usage(created_at_unix,
  task_count, now_unix)` (`wall_seconds = now - created_at`, `rounds = count of
  step instances past Pending`), and parks the run `Blocked` on the first
  ceiling breach (`budget.check()`). `Blocked` is terminal-until-approval, so
  the loop stops starting new steps.
- New pure helper `agentgrid_common::compute_budget_usage`.
- Tests: `compute_budget_usage_wall_and_rounds_proxy` (common),
  `budget_enforcement_parks_run_blocked_on_rounds_breach` (CP store).
- Follow-up: the messages/bytes/tokens/cost proxies and the
  `max_repeated_handoffs` circuit breaker need per-attempt adapter
  observation + handoff history.

### Added (common — Loop Engineering budgets, )

- `WorkflowBudget` (max_messages / max_rounds / max_bytes / max_tokens /
  max_cost_cents / max_wall_seconds / max_repeated_handoffs, all optional)
  + `BudgetUsage` / `BudgetBreach` in `agentgrid-common`. Pure
  `WorkflowBudget::check(&usage) -> Option<BudgetBreach>`: reports the first set
  ceiling exceeded (strict `>`, so equal-to-limit is *not* a breach); unset
  ceilings are unbounded. `max_repeated_handoffs` is the circuit breaker on
  identical sequential handoffs.
- `WorkflowTemplate.budget` and `CreateWorkflowRequest.budget`; migration
  `0026_workflow_budget.sql` adds `budget_json TEXT` (NULL = unbounded).
- Control plane persists and returns the budget on create/get/list, on both the
  YAML and JSON `POST /v1/workflows` paths.
- Tests: `budget_check_no_breach_when_unset_or_within`,
  `budget_check_reports_first_breach`, `budget_round_trips_in_template_yaml`
  (common); `workflow_budget_round_trips_via_json_create_and_get` (CP).
- Follow-up: runtime enforcement in the scheduler/loop tick (park the run
  `Blocked` on a breach + a repeated-handoff counter) needs scheduler-side
  usage tracking and handoff history.

### Added (common — MCP server registry, )

- `McpServer`/`McpServerCreate` in `agentgrid-common`: an operator-managed
  registry of MCP stdio servers a profile may attach to a session. Carries the
  spawn contract (id, command, args) + `env_requirements` (names only — values
  resolved at spawn from the node env, the same  secret-ref model) + an
  `enabled` gate. Migration `0025_mcp_servers.sql`.
- Control plane: `upsert/get/list/delete_mcp_server` + endpoints
  `POST/GET /v1/mcp-servers` and `DELETE /v1/mcp-servers/{id}`.
- Node `mcp_servers_payload(frame)` fetches the registry, keeps enabled servers,
  projects `{servers:[...]}` into the ACP `session/new` `mcp` field —
  fail-closed to `Null` when the CP is unreachable, and disabled servers are
  dropped (so an agent never auto-spawns a server the operator didn't vet).
- CLI `ag mcp {list|create|delete}`.
- Tests: `mcp_server_registry_round_trips_and_gates_disabled` (CP),
  `mcp_payload_projects_enabled_servers_and_drops_disabled` (node),
  `server_round_trips_without_secret_values` (serde).
- Follow-up: per-profile server subset + real stdio lifecycle/spawn + MCP
  capability discovery (`mcp/list_tools`) need ACP adapter-side work.

### Added (common — provenance record, )

- `ProvenanceRecord {originator, external_id, optional label}` in
  `agentgrid-common`: a provenance link between an attempt and the external
  system that originated it (Entire/h5i/Guild). Only identifiers — never
  secrets — so safe to persist and surface in the UI/API.
- `CompleteAttemptRequest` and `Assignment` now carry an optional
  `ProvenanceRecord`; the node builds it from env
  (`AGENTGRID_PROVENANCE_ORIGINATOR | _EXTERNAL_ID | _LABEL`) or echoes the
  one the CP attached to the assignment. The CP persists it to
  `attempts.provenance` (migration `0024_attempt_provenance.sql`).
- `CompleteAttemptRequest` and `Assignment` now derive `Default` (cleaner
  test fixtures).
- Tests: `completion_propagates_provenance` (CP round-trip into attempts
  row), `provenance_from_env_builds_record` (node env build).

### Added (control-plane — scheduled/recurring workflows, )

- A workflow template now has scheduled triggers that fire a new `WorkflowRun`
  on a fixed interval (MVP).  recurring workflows:
  - `WorkflowSchedule`/`WorkflowScheduleCreate` in `agentgrid-common`, migration
    `0023_workflow_schedules.sql` (`workflow_schedules`: id, template_id,
    interval_seconds, autonomy, last_run_at, enabled).
  - Store: `create_workflow_schedule` (validates template + interval +
    autonomy), `list_workflow_schedules`, `delete_workflow_schedule`,
    `tick_workflow_schedules` (fires one run per due schedule, stamps
    `last_run_at` as the passed unix epoch).
  - Endpoints `POST/GET /v1/workflows/{tid}/schedules` and
    `DELETE /v1/workflows/{tid}/schedules/{sid}`.
  - `tick_maintenance` runs `tick_workflow_schedules(now)` so schedules fire as
    part of the existing background loop.
  - CLI `ag workflows schedules {list|create --interval-seconds N --autonomy lN
    [--paused] |delete <sid>}`.
  - Test: `workflow_schedule_fires_run_on_tick` (fire → skip within interval →
    fire again after interval → deleted never fires).
  - Follow-up: L4 autonomy requires command policy + budget (no budget infra
    yet).

- An agent profile now carries **secret requirements** (names only, never
  values) and an optional **adapter version** target, completing the node-side
  sync contract ():
  - `SecretRequirement { env, required }` — a profile declares which secret env
    names it needs; the node checks its own env at apply time. A **required**
    secret that's unset is fail-closed: the node refuses to run the agent
    (`infrastructure_failed`) rather than launch one that will silently fail
    its first tool call. An **optional** unset secret only warns.
  - `versions_compatible(declared, installed)` — equal SemVer major is
    compatible; `None` declared = no check; an unparseable installed version is
    fail-closed. The predicate is landed and tested; node-side enforcement
    (cached adapter probe) is a follow-up.
    — **done**: `check_adapter_compatibility` in node uses cached
    `cfg.adapter_versions` (probed at startup); ACP path fail-closed refuse
    (`infrastructure_failed`) on mismatch; raw path warns (deferred). Tests:
    `check_adapter_compatibility_fails_on_major_mismatch`.
  - Migration `0022_profile_secrets_caps.sql` adds the columns.
  - CLI `ag profiles create --secret-required ENV --secret-optional ENV
    --adapter-version 1.4.0`.
  - Tests: `agent_profile_carries_secret_requirements_and_version` (CP),
    `check_profile_secrets_fail_closed_on_required_unset` (node),
    `profile::tests::{secret_requirement_is_name_only_no_value,
    profile_carries_secret_requirements_and_adapter_version,
    adapter_version_compatible_when_equal_major}`.

### Added (cp — SSE resume + event id, audit 22.1.1)

- `events_stream` now emits the SSE `id:` field (the event sequence) and an
  `event: task-event` type, and seeds the `after` cursor from the
  `Last-Event-ID` header on reconnect — so a browser that auto-reconnects
  resumes after the last delivered sequence (no gaps, no duplicates). An
  explicit `after_sequence` query still wins. Extracted to `sse_resume_after`
  (pure) and covered by `sse_tests::resume_*` (query/header/max/none/garbage).
- Regression-backlog ticked (already covered): `agent-raw-output.log` excluded
  from git commit/patch (`.git/info/exclude` + `finalize_workspace` assert),
  and two parallel attempts of one repo don't race git
  (`parallel_prep_same_repo_does_not_race`).

### Fixed (node — mask secrets in validation output, audit 22.1.1)

- `run_validation` now masks configured secrets in BOTH the streamed events
  and the `validation.log` artifact — before, validation stdout could leak a
  secret that `AGENTGRID_SECRETS` was supposed to redact (stdout/stderr were
  already masked via `mask_secrets`; validation output was not).
- `mask_secrets` signature relaxed to `&[String]` (was `&Vec<String>`).
- Covered by `validation_command_masks_secrets_in_output_and_log` (asserts the
  secret is absent from `validation.log` and `***` is present). Existing
  `validation_command_reports_exit_and_log` and `mask_secrets_*` updated.
- Regression backlog ticked (already covered): `validation_failure_must_not_
  report_success` (validation failed + exit 0 → `failed/validation_failed`).

### Added (common — RSS budget probe, audit 22.1.1)

- `agentgrid_common::rss::current_rss()` reads `/proc/self/status` `VmRSS:`
  and returns the resident set size in bytes (Linux only; `None` elsewhere
  or on read error) so budget checks (node idle ≤ 25 MB, control plane idle
  ≤ 64 MB, streaming ≤ 60 MB) have a single probe to call without platform
  gating. Covered by `parses_vmrss_line` + `current_rss_returns_something_on_linux`.
- Regression-test backlog audit (22.1.1): confirmed and ticked the three
  already-covered scenarios — repo/branch/URL shell-metachar injection
  (`rejects_injection_in_repo_branch_or_url`), adapter-mismatch task stays
  queued (`scheduler_skips_incompatible_head_of_line` + `task_eligibility`),
  and node-offline marks attempts `lost` + task `failed/node_lost`
  (`node_offline_loses_attempt_then_retry_succeeds`,
  `complete_on_lost_attempt_is_idempotent`).

### Added (node — apply profile autonomy + resource limits, )

- The node now applies the active agent profile's autonomy and resource
  ceilings, not just the system prompt:
  - `effective_autonomy` takes the **stricter** of the node's configured
    `cfg.autonomy` and the profile's autonomy — a server-side profile can
    tighten an agent, never raise its ceiling (fail-closed). An unknown /
    empty profile autonomy is ignored.
  - `profile_limits` maps `memory_max` / `cpu_quota` / `tasks_max` from the
    profile onto `ResourceLimits` in the `SpawnRequest` (negatives/zero → no
    ceiling; `None` profile → `ResourceLimits::default()`). The process
    backend still reports `enforced_limits=false` (capability honesty); this
    lands the wiring + payload so a real cgroup backend can enforce them.
- Covered by `effective_autonomy_takes_stricter_level`,
  `profile_limits_maps_positive_ceilings` (plus the existing
  `fetch_agent_profile_*`).

### Added (node — profile fetch from CP + DAG validation,  / ADR 0004)

- Node now fetches the active agent profile revision from the control plane
  (`fetch_agent_profile` → `GET /v1/profiles/{id}`) and prefers it over the
  env-based `AGENTGRID_AGENT_PROFILE_<ID>` fallback. Any CP error / missing
  active profile / empty prompt transparently falls back to the env, so the
  node keeps working without a server-side profile. Covered by
  `fetch_agent_profile_picks_active_revision`,
  `fetch_agent_profile_none_when_no_active`,
  `fetch_agent_profile_none_on_empty_prompt` (dummy CP servers).
- **ADR 0004: Workflow DAG invariants** (`docs/decisions/0004-workflow-dag-invariants.md`):
  the step graph is validated at template-create time — unique ids, no
  self-dep, no orphan dep, acyclic — so a malformed graph never reaches the
  scheduler (loud fail, BAD_REQUEST). `WorkflowTemplate::validate_dag` in
  `agentgrid-common::workflow` (DFS colour-mark, O(V+E)); `POST /v1/workflows`
  calls it on both the YAML and JSON paths. Covered by
  `workflow::tests::validate_dag_*` and
  `workflow_create_rejects_cycle_duplicate_self_dep` (CP integration).
- Follow-up: wire the profile's `autonomy` + `ResourceLimits` into the node's
  `SpawnRequest.limits`/`cfg.autonomy` (today only the system_prompt is read);
  secret-reference sync + capability/version check before activation.

### Added (policy — external provider registration, )

- `ExternalPolicyProvider` in `agentgrid-common::policy`: shells out to a
  pinned executable (env `AGENTGRID_POLICY_BINARY` + `AGENTGRID_POLICY_VERSION`)
  that reads `<version> <command>` on argv and prints a JSON `PolicyVerdict` on
  stdout. The first concrete third-party targets are CodeAlive bash-guard and a
  Destructive Command Guard; both now plug in behind the same trait with **no
  code change** once the binary is on the node — only env config.
- Fail-closed: a missing binary → `Err` (→ `Ask`), a non-zero exit → `Ask`, and
  unparseable stdout → `Ask`, never `Allow`.
- The node's `policy_decision` now prefers the external provider when
  `AGENTGRID_POLICY_BINARY` is set, else the builtin — same Allow/Deny
  short-circuit, fall-through to the approval flow otherwise.
- Covered by `external_provider_fail_closed_on_missing_binary`,
  `external_provider_fail_closed_on_nonzero_exit`,
  `external_provider_fail_closed_on_garbage_stdout`,
  `external_provider_parses_json_verdict`.

### Added (profiles — immutable revisions + rollback, )

- Agent profile desired-state ledger (migration `0021_agent_profiles`): a
  profile is a chain of **immutable revisions** (system prompt + autonomy +
  resource limits); an `agent_profiles_active` pointer selects the live one,
  so **rollback = activate an older revision** without losing history. Endpoints:
  `GET /v1/profiles` (active ids), `GET /v1/profiles/{id}` (all revisions),
  `POST /v1/profiles/{id}` (new revision, not auto-activated),
  `POST /v1/profiles/{id}/activate` (flip the pointer). Every create/activate
  is audited (`profile.create`/`profile.activate`). `AgentProfile`/
  `AgentProfileCreate`/`ActivateProfile` live in `agentgrid-common`.
- CLI: `ag profiles list`, `show <id>`, `create <id> [--system-prompt …] [--autonomy l2] [--memory-max N] [--cpu-quota N] [--tasks-max N]`, `activate <id> <rev>`.
- Covered by `agent_profile_revisions_immutable_and_roll_back` (CP integration).
- Follow-up: node-side fetch of the active profile from the CP (today the node
  still reads `AGENTGRID_AGENT_PROFILE_<ID>` from env), secret-reference sync
  (carries requirements, never values), capability/version compatibility check
  before activation.

### Added (backends — resource limits + error mapping, )

- `ExecutionBackend` contract extended (in `agentgrid-adapters::backend`):
  - `SpawnRequest.limits: ResourceLimits` — `memory_max` / `cpu_quota_percent` /
    `tasks_max` (maps to systemd `MemoryMax`/`CPUQuota`/`TasksMax` or Docker
    `--memory`/`--cpus`/`--pids-limit`). A backend applies what it can.
  - `BackendProcess::enforced_limits` — `false` for `ProcessBackend` (no
    cgroup), `true` for a cgroup/container backend. Capability honesty: a strict
    profile refuses to start on a backend that reports `false`.
  - `BackendOutcome` (`Exited`/`Killed`/`ResourceLimit`) + `classify_exit` +
    `BackendOutcome::error_code()` yields `resource_limit:<reason>` for a hit
    ceiling (alongside `timeout`/`validation_failed`).
- **ADR 0003: Execution backends** (`docs/decisions/0003-execution-backends.md`)
  records the capability-honest discipline: limits ride the spawn request, the
  backend reports what it enforced, the conformance suite drives any backend
  through one smoke, and `error_code=resource_limit` is a first-class terminal
  outcome a retry policy can treat specially (never auto-retry an OOM).
- Covered by `process_backend_does_not_enforce_limits`, `classify_exit_maps_cleanup`,
  `outcome_error_code_distinguishes_resource_limit`, and the existing
  conformance suite.
- Follow-ups (gated on cgroup/container impl): the concrete Linux cgroup/
  systemd scope backend, the Docker/Podman adapter, the secure profile, the
  OOM-kill E2E, the h5i/CubeSandbox spikes. The contract + error mapping +
  conformance hook are in place now.

### Added (zeroshot — ownership ADR + capability probe contract, )

- **ADR 0002: Zeroshot ownership** (`docs/decisions/0002-zeroshot-ownership.md`)
  fixes the lifecycle invariant: **1 Agentgrid attempt = 1 Zeroshot cluster**, 1:1.
  Cancel kills the whole cluster, a daemon kill reclaims orphans (kill only — no
  resume across a Zeroshot boundary), retry = newer cluster; results are exported
  as artifacts; `cluster_id` piggybacks on `session_id`; host credentials never
  mount through ( backend policy).
- New `agentgrid-common::cluster` contract: `ProbedExecutor` (capability probe:
  is the container runtime present, the executor binary present, its version
  pinned?) and a pure `probe_decision(runtime_present, executor_version,
  required_prefix, executor_present)` a node uses to decide whether it can serve
  a `zeroshot` task — fail-closed: a negative probe means the node does **not**
  claim it (same capability-honesty discipline as the wrapper-adapter boundary,
  ). `ClusterStep`/`ClusterHandle` model the create/kill lifecycle;
  the concrete Zeroshot adapter (shelling out to the Zeroshot CLI) is a later
  spike. Covered by `cluster::tests::probe_*`.
- Follow-ups: real shell-out probe (`which docker`, `zeroshot --version`) in the
  node, the create/stream/kill adapter impl, artifact export, role mapping, the
  Docker-mount security rereview, the verified profile, and the one-task E2E —
  all gated on the Zeroshot binary landing.

### Added (context — CTX provider contract + prompt injection, )

- New `ContextProvider` contract in `agentgrid-common` (`context` module):
  `ContextPack` carries the repo digest + metrics (`bytes_in`/`bytes_out`/
  `index_ms`/`cache_hit`), `cache_key_for(repo, base_commit, provider_version,
  config_hash)` is the canonical cache key, and `NoopContextProvider` is the
  graceful fallback (empty pack, never re-indexes). The first real impl is CTX
  (an external repo indexer); it plugs in behind the same trait without touching
  callers.
- Node daemon: `compose_context_block` builds a pack for the attempt's
  `(repository, base_commit)` via the configured provider (Noop by default) and
  appends `pack.body` to the prompt before the skills block; a `context_pack`
  status event streams the before/after bytes + cache-hit metrics. An empty pack
  (Noop) or any provider error emits nothing and never blocks the task.
- Covered by `context::tests::noop_is_empty_and_cached` and
  `context::tests::cache_key_is_deterministic`.
- Follow-ups: the real CTX-binary probe + an on-disk repo-index cache (atomic
  publish, quota/eviction) so a repeated attempt on the same key skips
  re-indexing — the  exit criterion. The trait, key shape, injection
  point, and metrics are ready; only the indexer impl is missing.

### Added (node — skill discovery wired into the prompt, )

- The node daemon now discovers skills in the attempt worktree
  (`<worktree>/.agents/skills`) and the user home (`~/.agents/skills`) before
  `session_prompt`, and appends an "Available agent skills (operator-trusted)"
  block to the prompt — but **only for skills the operator explicitly trusted**
  on the control plane (`GET /v1/skills`). Untrusted / unknown skills are omitted
  (fail-closed); any trust-ledger fetch error yields an empty block, so the task
  is never blocked by the skills wiring (skills are a hint, not a hard
  dependency). This closes the trust loop: the ledger (`POST
  /v1/skills/{name}/trust`) the operator edits is enforced at prompt-composition
  time on the node.
- The node-daemon now depends on `agentgrid-skills` (previously unused by any
  binary); `discover` + `standard_roots` are reused verbatim.
- Covered by `render_trusted_skills_block_filters_and_sorts` (pure render).
  Heartbeat-side skill reporting (so the operator sees discovered-but-untrusted
  skills in the UI automatically) and hard load/execute enforcement against an
  agent that reads `SKILL.md` itself remain follow-ups.

### Added (skills — trust ledger UI/CLI, )

- New control-plane skill-trust ledger (migration `0020_skill_trust`):
  `GET /v1/skills[?source=]`, `GET /v1/skills/{name}?source=`, and
  `POST /v1/skills/{name}/trust|untrust?source=`. Trust is keyed by
  `(name, source)` where `source` is the skill discovery tier (`project|user|managed`).
  A skill **absent from the ledger is `untrusted` (fail-closed)**: the agent may
  not load or execute it until the operator explicitly trusts it. Every decision
  is recorded in the audit log as `skill.trust`. `SkillTrustView` lives in
  `agentgrid-common`.
- CLI: `ag skills list [--source <tier>]`, `ag skills trust <name> [--source <tier>]`,
  `ag skills untrust <name> [--source <tier>]`.
- Web UI: new Skills view at `#/skills` (nav button next to Approvals) — a trust table
  (✅/⛔) with a Trust/Untrust toggle per row (confirm prompt) and a 5s auto-poll.
  Banner states the fail-closed default.
- Covered by `skill_trust_defaults_untrusted_then_round_trips` (CP integration test).
  Node-side skill discovery wiring (heartbeat report + enforcement on load) is a
  follow-up — the ledger, endpoints, and operator surfaces are complete now.

### Added (node — command-policy integration into ACP permission flow, )

- The node daemon now short-circuits `session/request_permission` through the
  builtin `CommandPolicyProvider` **before** creating an operator approval. For
  a Bash-style request (`permission = {tool:"Bash", input:"<cmd>"}`):
  - `Allow` (e.g. `cat`, `ls` at L2) → the agent proceeds with no operator
    round-trip; a `permission_decision` status event is still streamed so the
    operator sees what was auto-permitted.
  - `Deny` (e.g. `rm -rf` at L2) → the request is rejected outright.
  - `Ask` (e.g. `git push`, package installs) → falls through to the durable
    approval flow (`POST /v1/tasks/{id}/approvals`) unchanged.
- Any non-Bash tool, a missing `input`, or a provider error also reaches the
  approval flow — fail-closed to the operator. Autonomy level is read from
  `AGENTGRID_AUTONOMY` (`l0`..`l4`, default `l2`); the CP `POST /v1/policy/evaluate`
  mirrors the same matrix. Covered by `policy_decision_short_circuits_bash` /
  `policy_decision_non_bash_is_none`.
- **Enforcement boundary documented** in `docs/acp-interop.md`: a wrapper
  adapter (an arbitrary binary emitting JSON lines, without structured tool
  calls) cannot be fully intercepted by this layer — there is no
  `session/request_permission` to hook. For a strict/unattended profile, pair a
  wrapper adapter with a sandbox/backend policy (); the ACP native
  launcher is the forward path and is fully intercepted.

### Added (approvals — operator UI + CLI reason, )

- Control plane `POST /v1/approvals/{id}/{allow|deny}` now accepts an optional
  `{ "reason": "…" }` JSON body; the reason is persisted on the approval and
  surfaced back via `GET /v1/approvals[?status=]` / `GET /v1/approvals/{id}`
  (audit trail). Empty/absent body keeps the prior behavior (allow = no reason,
  deny = `denied by operator`). Covered by `approval_flow_allow_deny_and_expiry`
  (allow-with-reason round-trip assertion).
- CLI `ag approvals allow/deny <id> --reason "…"` sends that body. `list`
  was already present (); unchanged.
- Web UI: new Approvals view at `#/approvals` (nav button). Lists approvals —
  default filter `pending`, an `?…` shows all statuses — with status / scope /
  permission / task / attempt / created / expires / reason columns, and
  Allow/Deny buttons on pending rows. The decision prompts for an operator
  reason (deny requires a non-empty reason), then POSTs the answer; the list
  auto-polls every 3s so a fresh `session/request_permission` surfaces without
  a manual refresh. Closes the Этап 9.2 checkbox for an operator approval UI.

- node-daemon: bound the child reap after ACP session cancel/timeout. A child
  that ignored SIGTERM (or a pidfd that never fired) could previously park
  `drive_acp_session` forever after the session timeout — now wrapped in
  `tokio::time::timeout(12s, child.wait())` matching the SIGKILL escalation.
- node-daemon: `AG_FAKE_HANG` test mode in the fake ACP agent (writes a
  truncated JSON-RPC line then blocks) + test `drive_acp_session_hang_mid_frame_times_out`
  covering "kill ACP subprocess mid-JSON-frame → attempt failed, no hang"
  .

## [0.1.1] — correctness & security hardening

–2 hardening of the 0.1.0 MVP: truthful statuses / outcome model, lost-node
recovery, explicit ack, scheduler fairness (); durable node outbox, secret
+ artifact safety, git isolation, adapter registry, operational hardening ().
A full threat model is in `docs/decisions/threat-model.md`; an upgrade guide for
0.1.0 → 0.1.1 is in `docs/upgrade-0.1.0-to-0.1.1.md`. This release tracks the
exit criteria of Этапы 1–2 of `agentgrid-development-plan.md` (Gate A).

Gate A status: the durable-delivery E2E (`tests/e2e/run-outbox.sh`, process-
based) passes both scenarios repeatedly — kill -9 daemon with a completion in
the outbox (redelivered on restart) and a mid-stream control-plane outage
(events spooled, redelivered contiguous, no dup/gap). The E2E uses real
`agentgrid-control-plane` + `agentgrid-node-daemon` debug binaries over HTTP,
not a Docker compose harness.

Key changes delivered in this push (see the detailed entries under `[Unreleased]`):

- Outcome model distinct from agent exit code; `validation_failed`/`timeout`/
  `node_lost`/`infrastructure_failed` error codes; cancel always yields `cancelled`.
- Lost-node recovery: non-terminal attempts → `lost` atomically; capacity released;
  idempotent completion redelivery.
- Explicit `POST /v1/node/attempts/{id}/ack` + `ack_deadline`; lease decoupled from
  output ingest; N/N-1 compatibility for legacy nodes.
- Scheduler: oldest-eligible-task (no head-of-line blocking), `requested_node_id`
  honored, scheduler latency metric.
- Durable node JSONL outbox (events + completions); startup redelivery; idempotent
  complete redelivery. (RAM/spool size limits + `output_truncated` + E2E pending.)
- Safety: secret masking in all paths, agent logs excluded from commit/diff,
  artifact-name traversal guard on GET + defense-in-depth `Store::artifact_path`.
- Git: per-repo in-process lock serializes shared-clone mutations; `sh -c` removed;
  token + URL validation; adversarial tests.
- Adapter registry: probed adapters on heartbeat, `assignment.adapter` enforced.
- Ops: WAL checkpoint + backup, `quick_check` at boot, stable-JWT-secret requirement,
  login rate limit + audit (no user enumeration), protocol versioning, disk-space
  `degraded`, checkpoint-duration + `SQLITE_BUSY` metrics.
- Web auth: HttpOnly + SameSite=Strict session cookie (no JWT in `localStorage`);
  `POST /v1/auth/logout`; CSRF mitigated via SameSite=Strict.

Not yet closed (carried forward): binary-safe streaming artifact API +
"descriptor-relative (`openat`/`O_NOFOLLOW`) writes; SQLite outbox, outbox size
limits / `output_truncated` backpressure, artifacts-in-spool; E2E `network
disconnect` / `kill -9 daemon`; legacy-schema FK migration; bare-mirror shared
clone / cross-process repo lock." — **binary-safe artifact API closed post-0.1.1** (see `[Unreleased]` /  above).

## [0.2.0] — tagged retrospectively ( boundary)

ACP interoperability (Stages 3–6), tracked post-hoc at the last  commit.
Subsequent workflow work shipped as 0.3.0; this tag freezes the 0.2 feature
surface.

- **** — versioned `AgentEventEnvelope { version, kind, payload,
  raw_ref }` over `TaskEvent`; extended kinds (`plan`, `tool_call`, `tool_result`,
  `file_change`, `permission_request`, `usage`, `handoff`); `ExecutionBackend`
  trait (native process + worktree first); `AgentSession` table + DTO;
  cancellation normalized (`EventKind::Cancel` + node emits on cancel).
- **** — `SKILL.md` parser (YAML frontmatter), strict validation +
  lenient diagnostics, discovery paths + scope precedence, progressive
  disclosure, trust gate (project skills not active without explicit trust),
  bundle manifest + hash verification + materialization, `RevisionStore`.
- **** — ACP southbound (`agentgrid-acp`): JSON-RPC 2.0 codec + stdio,
  `initialize` / `session/new` / `session/prompt` / `session/cancel`,
  `session/update` → `AgentEventEnvelope` mapping, durable approval flow
  (state machine, `approvals` table, `/v1/approvals`, `ag approvals`,
  auto-expiry, fail-closed), ACP adapter in registry.
- **** — ACP northbound gateway (`agentgrid acp-agent` over stdio);
  ACP session ↔ Agentgrid task/workflow mapping, streaming events →
  `session/update`, approval passthrough, `session/cancel`, `_agentgrid.dev`
  extensions; honest `ask`/`worker` modes only (`architect`/`verifier`/
  `orchestrator` advertised only after 0.3).

Follow-up left open against the 0.2 plan: ProcessAdapter wrapper wrapping,
  conformance suite fixtures, legacy-schema no-break migration audit, live
  ACP-agent E2E, absolute-path / MCP stdio passthrough wiring, Skill E2E
  round trips — see `agentgrid-development-plan.md` unchecked –5 items.


## [0.3.0] — 2026-07-17

 distributed multi-agent workflows. See the `Added ( …)` entries
under `[Unreleased]` below for the full list: per-step node placement, shared
`base_commit` (control plane + node-side checkout), lost-step retry policy,
integrator conflict policy (`Blocked`), ACP plan projection, and the two-node
E2E harness (`tests/e2e/run-workflow.sh`). Tag `v0.3.0` marks the  code
complete; the two-container E2E run is the release validation gate.

## [Unreleased]

### Added (control-plane — TLS termination, Step 2)
- control-plane serves HTTPS when `AGENTGRID_TLS_CERT` + `AGENTGRID_TLS_KEY` (PEM) are set: a `TlsListener` (axum 0.8 `Listener` trait over `tokio-rustls`) wraps the TCP listener; rustls with the `ring` provider, no system OpenSSL. Plaintext is retained for loopback / `--tls-cert` unset. `ag server start --tls-cert/--tls-key` forwards the paths as env. Nodes are already rustls-HTTPS clients (reqwest `rustls-tls`), so a node just needs `AGENTGRID_SERVER=https://cp`; no VPN is required for a star topology. `ag nodes install --server https://cp ...` skips the SSH reverse tunnel and points the node directly at the TLS control plane (SSH used only to `scp` the binary + start it). Covered by `tls_tests::load_tls_acceptor_missing_file_errors`. Reverse-proxy docs / mTLS remain follow-ups.

### Added (gateway — chat front-end, )
- New crate `crates/gateway` (`agentgrid-gateway`): a chat bridge that lets an operator drive the grid from a phone. A `ChatProvider` trait with one implementation — Telegram, via raw `reqwest` calls to the Bot API `getUpdates`/`sendMessage` long-polling (no chat-client crate). Commands proxy to the control-plane HTTP API: `/nodes`, `/tasks`, `/run <repo> <adapter> <prompt...>`, `/show <id>`, `/logs <id>`, `/cancel <id>`, `/help`. Auth is an allowlist of numeric chat ids (`AGENTGRID_GATEWAY_ADMINS`); chats off the list are ignored. The control-plane URL + a user JWT come from `AGENTGRID_SERVER` / `AGENTGRID_GATEWAY_TOKEN`. Discord and WhatsApp sit behind the same trait but are **not implemented yet** — WhatsApp especially has no easy open bot API (the Business API is gated/heavy); both are honestly deferred rather than stubbed. Covered by `tests::fmt_*` (the pure formatting/dispatch helpers); live bot wiring needs a real Telegram token.

### Added (node — durable outbox hardening + E2E, )

- Fixed the startup completion-redelivery path: it was using an unauthenticated
  client, so `/v1/node/attempts/{id}/complete` returned 401 and the redelivery
  never acked. Moved redelivery into `poll_loop` after the credentialed client is
  built.
- Record the terminal completion to the durable outbox promptly when the
  adapter exits (before the post-adapter event flush / artifact uploads, which
  block on a down CP), so a daemon kill during that window still redelivers the
  completion. `CompletionOutbox::record` is now idempotent per attempt (latest
  wins, replacing any prior pending line).
- Added `EventSink::flush_quick` (single-shot drain, no long retry) for the
  post-adapter flush, and `EventSink::drain` (loop flush until buffer empty) run
  before and after `report_complete`, so events buffered during a CP outage are
  delivered before the task is marked terminal (and not lost when the flusher is
  aborted). The flusher is now kept alive through `report_complete`.
- Fixed `buf_bytes` backpressure accounting: released only on a successful ack
  (a failed flush pushes the batch back), so the cap isn't effectively raised
  during a prolonged outage.
- Replaced the pre/post-completion RAM-buffer `drain` with `drain_outbox`,
  which redelivers directly from the durable outbox on disk — events dropped
  from RAM when the flusher is aborted mid-flush are still on disk and get
  redelivered rather than orphaned on a terminal attempt. This closed the
  last event-continuity gap in Scenario B.
- Process-based E2E `tests/e2e/run-outbox.sh` now has three scenarios:
  Scenario A — kill -9 node after the completion is durably recorded, restart
  CP + node → completion redelivered → task succeeds. Scenario B — CP down
  mid-stream, node spools events + completion, CP back → 200 events delivered
  contiguous, no dup/gap. Scenario C — kill -9 node mid-running → maintenance
  marks the node offline → task `failed`/`node_lost` → retry → restart node
  → `succeeded`. All three pass 10/10.

### Added (cp — maintenance cadence, SQLITE_BUSY fix)

- `start_maintenance` now ticks every 15s (node staleness threshold is 30s, so
  a dead node is still marked offline within ~30–45s) and runs `wal_checkpoint`
  only every 4th tick (~60s). Running a TRUNCATE checkpoint every 5s held the
  SQLite writer and caused `database is locked` (SQLITE_BUSY) on user `BEGIN
  IMMEDIATE` writes such as `retry_task` under load — observed as a 500 on
  retry in the E2E. Less frequent checkpoints eliminate the contention.

### Added (node — worktree/branch cleanup, )

- Every terminal attempt now reclaims its per-attempt worktree dir and branch:
  `git worktree remove --force` plus `git branch -D` for git tasks, a plain
  `rm -rf` for non-git tasks. Runs best-effort in `spawn_blocking` on both the
  ACP and raw-adapter paths so a stuck worktree never turns a successful
  attempt terminal. Previously these leaked disk every run.
- Node startup now runs `prune_stale_workspaces`: removes workspace dirs older
  than `AGENTGRID_WORKSPACE_RETENTION_HOURS` (default 24, 0 disables) and runs
  `git worktree prune` per repo. This sweeps dirs a killed daemon left behind
  (a `kill -9` skips the graceful cleanup); a periodic background job is
  deferred since startup reconcile + per-attempt cleanup covers the common
  cases. Covered by `cleanup_workspace_removes_worktree_and_branch`,
  `cleanup_workspace_plain_dir_no_git`, `prune_stale_workspaces_removes_old_keeps_fresh`.

### Changed (node — bare-mirror shared clone, )

- The per-repository shared clone is now a `git clone --mirror` (bare): it has
  no working tree and no HEAD to mutate. Prior runs did `git checkout -B db
  origin/db` into the shared clone on every attempt, flapping HEAD between
  parallel attempts that used different default branches / base commits — the
  per-repo lock serialized it but the semantics were wrong. Now `fetch origin
  --prune` refreshes all mirror refs (under the same names, so the default
  branch is addressed by `db` with no `origin/` prefix) and `git worktree add
  -b branch ws <base>` pins the start point; `checkout -B` is gone. Covered by
  the existing git tests (`worktree_commit_and_patch`, `base_commit`,
  `parallel_prep_same_repo_does_not_race`), which now run against a bare
  mirror clone.

### Added (node — event backpressure + `output_truncated`, )

- `EventSink` now caps its RAM buffer per attempt at `AGENTGRID_EVENT_BUF_BYTES` (default 4 MiB). Once over the cap, ordinary log/usage events (`stdout`/`stderr`/`metric`) are dropped and exactly one `output_truncated` status notice is emitted; terminal-state events (`status`/`result`/`error`) and `tool` calls are never dropped, so logs can't starve terminal state. The budget is released as the flusher drains. Covered by `event_sink_drops_logs_over_cap_but_keeps_terminal_state`.

### Added (cp ops metrics — checkpoint duration + SQLITE_BUSY, )

- `/metrics` now exposes `agentgrid_sqlite_checkpoint_ms` (last `wal_checkpoint(TRUNCATE)` duration) and `agentgrid_sqlite_busy_total` (cumulative SQLITE_BUSY/locked-class failures observed during checkpoints). `wal_checkpoint` now times itself and counts busy/locked errors distinctly so they surface in metrics rather than only logs.

### Added (node — durable event/completion outbox, )

- The node daemon now persists streamed events and attempt completions to a durable JSONL outbox (`<data_dir>/outbox/<attempt_id>.jsonl` for events, `completions.jsonl` for terminal reports) before any send attempt, and removes a record only after the control plane acks it (HTTP 2xx). So a daemon crash or `kill` no longer drops the in-flight tail of events, nor a completion that was recorded but not yet acked. On startup the daemon redelivers any pending completion records (idempotent — `complete_attempt` is a no-op on already-terminal attempts); pending event records are re-queued when their attempt next runs (CP ingest is idempotent on `(attempt_id, sequence)`). Redelivery respects sequence order. Covered by `event_outbox_persists_and_acks`, `event_outbox_keeps_unacked_after_partial_ack`, `completion_outbox_record_and_ack`. Note: JSONL (not SQLite), no RAM/spool size limits or `output_truncated` backpressure yet, and no artifacts in the spool (artifacts already retry with per-name idempotency); those remain follow-ups.

### Changed (security — web session cookie, )

- The web UI no longer stores the JWT in `localStorage` (XSS-readable); instead `/v1/auth/login` and `/v1/auth/setup` set an `HttpOnly` + `SameSite=Strict` session cookie (the browser JS cannot read it). The web client sends `credentials: include` on all requests (including the SSE event stream) and calls a new `POST /v1/auth/logout` to clear it. The `Authorization: Bearer` header is still accepted (CLI, gateway, node stay unaffected), and the login/setup JSON body still returns the token for non-browser clients. `Secure` is added only when `AGENTGRID_COOKIE_SECURE=1` so local plaintext dev keeps working (enable it behind TLS/reverse-proxy in prod). `SameSite=Strict` is the CSRF guard (a cross-site request carries no cookie, so it can't forge a state-changing call). Covered by `login_sets_cookie_and_cookie_auths`.

### Fixed (git — per-repo lock serializes shared-clone mutations, )

- `prepare_workspace` now holds an in-process per-repository `Mutex` across the shared-clone mutating steps (`fetch` / `checkout -B` / `worktree add`), so two concurrent attempts of the same repo cannot race the clone state (a `checkout -B` from one attempt moving the shared branch mid-`worktree add` of another). Each attempt still gets its own worktree, so agent work stays concurrent. Covered by `parallel_prep_same_repo_does_not_race` (4 concurrent prepares, all succeed). Note: in-process lock only (single node); a cross-process file lock remains a follow-up.

### Fixed (security — artifact-name traversal, )

- `GET /v1/tasks/{id}/artifacts/{name}` used to resolve `name` directly, so a `../../etc/passwd` request could read outside the artifact root. The handler now runs the same `is_safe_artifact_name` gate as the upload path (404, not 403, so a denial cannot disclose whether an artifact exists), and `Store::artifact_path` adds defense-in-depth: it canonicalizes the attempt dir and checks the resolved path stays under the artifact root, and rejects any name that is not a single safe segment. Covered by `artifact_save_rejects_traversal_names`, `artifact_read_traversal_returns_none` (store) and `artifact_get_rejects_traversal_name` (api). Note: this is a canonicalize + single-segment guard, not a descriptor-relative (`openat`/`O_NOFOLLOW`) API; that hardening remains a follow-up.

### Fixed (security — agent logs excluded from commit/diff, )

- Node-side logs the daemon writes inside the agent worktree (`agent-raw-output.log`, `validation.log`) and its own `changes.patch` used to leak into the committed diff / `changes.patch` via `git add -A`, so raw agent output (which may contain secrets) could end up in a commit or the reviewable patch. `prepare_workspace` now scopes a per-worktree `.git/info/exclude` (resolved via `git rev-parse --git-path`, so linked worktrees get their own gitdir-scoped file rather than the shared clone's) listing those names. Covered by `raw_and_validation_logs_excluded_from_commit_and_patch`.

### Added (web — workflow run viewer with DAG, )

- A Workflows page lists runs (`GET /v1/workflow-runs`) and a run detail renders the step DAG: steps are layered by dependency depth (roots left, leaves right), each card shows role, status, verdict, assigned node, attempt count, and error code; the detail auto-polls and offers Cancel on non-terminal runs. Backed by the existing `GET /v1/workflow-runs/{id}/projection`. A span-waterfall timeline is a follow-up; this is a layered DAG view.

### Added (ACP session resume, )

- ACP `session/new` is now issued with `parent_session_id` when a follow-up task in a conversation should resume the prior agent session, so the agent does not re-process the transcript from scratch. The node reports the `session_id` it received back to the control plane via `CompleteAttemptRequest.acp_session_id`; the control plane persists it on the attempt (`attempts.acp_session_id`, migration `0019`) and, on the next conversation turn, looks up the last finished attempt's session as the resume parent (`Store::last_conversation_acp_session`) and threads it onto the task as `Assignment.parent_acp_session_id`. Resume is an optimization (correctness already holds: conversations compose the full history into the prompt). Covered by `acp_session_resume_links_conversation_turns`.

### Added (feedback-loop CI→agent, )

- **Wrapper path**: the spawn→select→finalize→validate flow is wrapped in a retry loop. When `validation_command` is configured and the agent exits 0 but validation fails, the node re-spawns the agent with the validation error appended to the prompt (same worktree, fixes accumulate, single commit at the end), up to `AGENTGRID_FEEDBACK_RETRIES` rounds (default 0 = off, backward compatible). A `feedback` event is emitted each round so the loop is visible in the event stream.
- **ACP path bugfix**: the ACP path used to skip `finalize_workspace` and `run_validation` entirely, silently leaving `validation_command` unenforced for ACP agents. Now both run after `drive_acp_session`, before `report_complete`.

### Added (agent-profile SSOT, )

- An optional system prompt per adapter, projected into the worktree before the agent runs. `AGENTGRID_AGENT_PROFILE_<ID>` is either a path to a `.md` file (read) or inline text; the node writes it to `<worktree>/AGENTS.md` (the cross-agent convention that Claude Code, opencode, pi, etc. read) and forwards it as the `AGENTGRID_SYSTEM_PROMPT` env hint. Per-agent native projection (`CLAUDE.md`, `.kiro/`) is a follow-up mapping table.

### Added (Sandbox trait, )

- Agent isolation: a `Sandbox` wraps the spawned agent command so an agent can run inside a container instead of sharing the node's full environment. `NoSandbox` (default, runs directly in the worktree) and `DockerSandbox` (`docker run --rm -i -v <workdir>:/ag -w /ag <image> --`). Configured via `AGENTGRID_SANDBOX` (`none` | `docker`) and `AGENTGRID_SANDBOX_IMAGE`. The ACP path (native ACP launcher + wrapper binary) routes through `sandbox_command`; the legacy `ExecutionBackend` wrapper path is left unsandboxed with a noted TODO.

### Added (native ACP launcher + durable startup-reconcile, /11.1)

- **Native ACP launcher**: a node can run any native-ACP coding agent (Claude Code, Codex, Gemini CLI, OpenCode, Kiro, …) directly over stdio by setting `AGENTGRID_ACP_LAUNCH_<ID>` (e.g. `AGENTGRID_ACP_LAUNCH_CLAUDE="claude --acp"`). The ACP path spawns that command instead of the `adapter-<id>` wrapper binary, so adding a new agent is one env var — no per-agent crate/parser. The per-CLI wrapper binaries (`adapter-claude`, `adapter-opencode`) remain as legacy fallback for agents that don't speak ACP.
- **Durable startup-reconcile**: on boot the control plane immediately runs a maintenance tick (revert expired leases, mark silent nodes offline) instead of waiting for the first background tick, and audits the reconcile with the in-flight attempt count. In-flight `running` attempts on live nodes are left alone (the node may still complete them); node-death is caught by the existing `node_lost` path. Backed by `Store::reconcile_on_startup`.

### Added (conversations — stateful multi-turn chat routed to an agent, )
- New `conversations` + `conversation_messages` tables (migration `0018`). A conversation is a stateful multi-turn chat routed through the control plane to a coding agent on some node. Each user message creates a task whose **prompt is the composed conversation history** (a `user:`/`assistant:` transcript), so any node that picks the task up sees the full shared context — conversations can hop nodes, and parallel conversations are isolated by id.
- Endpoints: `POST /v1/conversations` (adapter, optional repository), `GET /v1/conversations/{id}`, `POST /v1/conversations/{id}/messages` (content → creates the task carrying the composed prompt, returns task id), `GET /v1/conversations/{id}/messages`.
- `adapter-mock` now emits a `result.text` (echoes the last non-empty prompt line) so the chat loop has a readable answer without an LLM; real adapters (`claude`/`opencode`) emit their own.

### Added (gateway — conversations + chat loop)
- The Telegram gateway now holds the current conversation id per chat and routes **plain text** (no slash) as a conversation message: it appends to the conversation, polls the task events until terminal, and replies with the agent's `result` text (best-effort: result payload, else last log/error line). `/new <adapter> [repository]` starts a conversation; plain text with no conversation bound nudges the operator to create one (and mentions `AGENTGRID_GATEWAY_CHAT_ADAPTER`, default `mock`).

### Added (node-daemon — disk-space alerting, )
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

### Added ( — approval scope, audit, tests)
- control-plane (9): approvals gain a `scope` column (migration 0017) — `tool_call | session | step | command | duration` — so operators see what they are approving. `create_approval` threads it through; `ApprovalView` and the list/get SELECTs expose it. Covered by `approval_scope_round_trips` (api).
- control-plane (9): `POST /v1/policy/evaluate` now emits a fail-closed audit event (`policy.evaluate`) for every decision, so dangerous commands are never silent. `Store::list_audit` added for the trail. Covered by `policy_evaluate_audits_decision` (api).
- skills (9): `untrusted_project_skill_not_materialized` asserts a repo-supplied (malicious, `curl | sh`) skill is skipped by `materialize` unless an operator has explicitly trusted it. Control-plane (9): `approval_payload_has_no_secrets` asserts the approval payload never serializes secret-like fields. Destructive-command denial is covered by the policy unit tests.

### Added ( — autonomy levels + approval timeout)
- common (9): autonomy levels `L0`–`L4` (`AutonomyLevel`, default `L2`) modulate the builtin policy. `BuiltinPolicyProvider::decide_for(level, class)` maps risk class → decision per level (L0 fully supervised → everything `ask`; L2 allows local read/edit/exec, asks network/git/install, denies destructive; L3 also allows network/git; L4 allows everything including destructive). `evaluate_with(level, command, cwd)` applies a level. Covered by `policy::tests::l0_*` / `l2_*` / `l3_*` / `l4_*`.
- control-plane (9): `POST /v1/policy/evaluate` accepts an optional `autonomy` level (`l0`–`l4`, default `l2`) and applies it. Covered by `policy_endpoint_honors_autonomy_level` (api).
- control-plane (9): an unanswered approval that times out now blocks the workflow step (and run) it is linked to, instead of leaving the run hanging. `approvals.step_run_id` (migration 0016) links an approval to a `workflow_steps` instance; `tick_approval_expiry` flips a past-due linked approval to `expired` and calls `block_step_and_run`, which sets the step and run to `Blocked` (idempotent, non-terminal only). Covered by `approval_timeout_blocks_linked_step` (store).

### Added ( — command-policy foundation)
- common (9): command-policy foundation. `CommandPolicyProvider` trait with `evaluate(command, cwd) -> PolicyVerdict { decision, risk_class, reason, matched_rules }`; `RiskClass` (read / edit-workspace / execute-local / network-write / git-remote / package-install / destructive) and `PolicyDecision` (allow / ask / deny / rewrite). `BuiltinPolicyProvider` is a heuristic classifier mapping risk class → decision (destructive→deny, network/git/install→ask, read/edit/exec→allow). Fail-closed: an unavailable provider or an unparseable command yields `ask`, never `allow` (`PolicyVerdict::fail_closed`). Covered by `policy::tests::*` (8 unit tests).
- control-plane (9): `POST /v1/policy/evaluate` exposes the builtin provider (`{command, cwd} -> verdict`); fail-closed on provider error. Covered by `policy_endpoint_classifies_commands` (api).

### Added ( — distributed workflows: node-side base_commit, conflict policy, ACP projection)
- node-daemon (8): honor a step's `base_commit` on the node. `prepare_workspace` checks the worktree out at the exact pinned commit (best-effort fetch, token-validated) so all attempts of one run start from the same commit; `finalize_workspace` diffs relative to `base_commit`. Covered by `base_commit_pins_worktree_to_commit`.
- control-plane (8): integrator conflict policy. A non-retryable (or retry-exhausted) integrator step transitions the step **and** the run to `Blocked` (awaiting human/repair) instead of `Failed` — an integrator never silently overwrites and never fails the whole run. `Blocked` added to `WorkflowRunStatus`/`WorkflowStepStatus`. Covered by `integrator_failure_blocks_run_not_failed` and `worker_failure_still_fails_run`.
- control-plane (8): ACP plan projection. `GET /v1/workflow-runs/{id}/projection` returns each step's role, status, placement, spawned task, assigned node, and latest verdict; the ACP gateway exposes it via the `_agentgrid/workflow/projection` extension. Covered by `workflow_run_projection_exposes_roles_nodes_verdicts` (store), `workflow_projection_endpoint_exposes_roles_and_verdicts` (api), and `gateway_exposes_workflow_projection` (acp).
- e2e: `tests/e2e/run-workflow.sh` brings up control-plane + two node containers and runs a workflow that pins workers to node A and integrator/verifier to node B, asserting `succeeded` and printing step provenance — the  two-container release gate.

### Added ( — distributed workflows: base_commit + lost-step recovery)
- control-plane (8): shared `base_commit` for a run's parallel workers. `WorkflowRun`/`CreateWorkflowRunRequest` gain `base_commit` (migration 0015); it is stored, threaded into every step's spawned task (`CreateTaskRequest`/`TaskView`/`Assignment` all gain `base_commit`), so all workers of one run start from the same commit. Per-step `base_commit` overrides the run-level value. `tasks.base_commit` added. Covered by `workflow_run_carries_base_commit`.
- control-plane (8): per-step retry policy (lost-step recovery). `WorkflowStep`/`WorkflowStepRun` gain `retryable` + `max_attempts` + `attempts` (migration 0015). A failed/`node_lost` step is retried up to `max_attempts` only when `retryable` is set; side-effectful steps default to no auto-retry (step → `failed`). `tick_workflow_run` bumps the attempt counter and respawns the task on retry. Covered by `retryable_step_retries_then_succeeds`.

### Added ( — distributed workflows: placement)
- control-plane (8): per-step node placement. `WorkflowStep`/`WorkflowStepRun` gain `requested_node_id`; it is stored in `workflow_steps` (migration 0014) and carried into the Agentgrid task spawned for that step, so the scheduler's `try_assign` pins the task to the requested node (NULL = any eligible node). Honored end-to-end: template → run → task. `TaskView` now exposes `requested_node_id` for UI/CLI visibility. Covered by a store-level regression test (`step_requested_node_id_pins_task`) and the golden workflow integration test.

### Fixed ( — distributed workflows: placement)
- control-plane: bind `workflow_steps.requested_node_id` as `Option<&str>` (via `as_deref()`) instead of `&Option<String>`, and normalize empty-string to `None` on read. Binding `&Option<String>::None` into an `ALTER TABLE … ADD COLUMN` text column stored the empty string `""` rather than NULL, which poisoned the spawned task's `requested_node_id` and broke the `try_assign` `requested_node_id IS NULL` filter (unpinned steps could never be assigned).
### Fixed ( — 0.1.1 correctness)
- control-plane (1.1): decide task success from the adapter **outcome** (`error_code`), not raw `exit_code==0`. A validation failure that exits 0 is now `failed`/`validation_failed`, never silently `succeeded`. Adapter timeout reports a distinct `error_code="timeout"`.
- control-plane (1.2): a node that goes `offline` (heartbeat lapse) or is `revoked` atomically loses its in-flight `assigned`/`running`/`validating` attempts (→ `lost`) and fails the owning task with `error_code="node_lost"`, freeing capacity. Late completions on a lost attempt are treated as idempotent no-ops.
- control-plane (1.4): scheduler no longer blocks on an incompatible head-of-line task — it scans queued tasks (oldest-first) and assigns the first the node can run, instead of touching only the single oldest.
- control-plane (1.3): explicit assignment acknowledgement. An attempt gains an `ack_deadline` (30s); the node daemon calls `POST /v1/node/attempts/{id}/ack` on spawn. An unacked assignment is reverted and the task re-queued by `tick_maintenance`; an acked (running) attempt is never reverted. Legacy `metric "attempt started"` events still act as an ack (N-1 node compatibility).

### Fixed ( — 0.1.1 durable delivery & security)
- node-daemon (2.2): stop leaking secrets. The non-JSON stdout/stderr fallback now sends the **masked** line, not the raw `line` (the raw disk mirror was already masked). `mask_secrets` is unit-tested.
- node-daemon (2.1): verify the HTTP status on every node→CP call (event flush, completion, artifact upload) instead of only checking transport errors; a 5xx/429 now triggers retry with exponential backoff. A failed event batch is returned to the buffer for the flusher loop to retry while the daemon runs; completion retries until delivered (then gives up, letting the CP lease revert the attempt). Retryable-status logic is unit-tested.
- control-plane (2.5): run `PRAGMA quick_check` on startup and refuse to serve a corrupt database; warn loudly when `AGENTGRID_JWT_SECRET` is unset (a random-per-start secret invalidates previously issued node tokens after a restart).
- node-daemon (2.3): drop `sh -c` from git operations and `probe_adapter`; every git arg is passed via `Command::arg`, and `repository`/`task_id`/`default_branch`/`git_url` are validated (no shell metacharacters, no `..`, no absolute paths). Adversarial tests assert injection attempts are rejected.
- node-daemon (2.4): run strictly the adapter the control plane assigned (`adapter-<id>` binary on PATH); an unknown or missing adapter fails the attempt with `error_code="infrastructure_failed"` instead of silently falling back. Heartbeat probes every configured adapter and reports `degraded` if any binary is missing. The single `AGENTGRID_ADAPTER` env var is removed in favor of the `AGENTGRID_ADAPTERS` registry.

### Added ( — ops hardening)
- control-plane (2.5): `POST /v1/admin/backup` runs `VACUUM INTO` to a server-side path (path validated against `..`/shell metacharacters; `VACUUM INTO` refuses to overwrite). Store methods `backup_to` + `wal_checkpoint` back it. Covered by `backup_endpoint_writes_file` (api) and `backup_round_trips` (store, re-opens the copy).
- control-plane (2.5): periodic `PRAGMA wal_checkpoint(TRUNCATE)` in the maintenance loop plus a checkpoint on graceful shutdown (Ctrl-C / SIGTERM), so the database file does not grow without bound and a restart replays nothing stale. Covered by `wal_checkpoint` use in `tick_maintenance`.
- control-plane (2.5): `POST /v1/auth/login` is brute-force limited by a per-instance sliding window (10 attempts / 60s) returning a generic `429` (no per-user signal, so it cannot be used for user enumeration). Covered by `login_rate_limit_returns_429` (api).
- control-plane (2.5): `UploadArtifactRequest.name` is validated to a single safe path segment (no separators, no `..`, no NUL) on `POST /v1/node/attempts/{id}/artifacts`; a traversal name is rejected with `400`. Covered by `artifact_name_validation_rejects_traversal` (api).
- control-plane (2.5): artifact metadata older than the 168h retention window is reaped by the maintenance loop (`cleanup_artifacts(168)`); files on disk are left for an operator cleanup job. Covered by `cleanup_old_artifacts` (store).
- control-plane (2.5): scheduler observability. `try_assign` records the queued→assigned latency (ms) and a cumulative assignment counter, exposed as `agentgrid_scheduler_latency_ms` / `agentgrid_scheduler_assignments_total` in `/metrics`. Covered by `scheduler_records_latency_metric` (store).
- control-plane (1.2, shipped): node `offline`/`revoked` atomically loses its in-flight attempts (→ `lost`) and frees `active_attempts` capacity; a late completion on a lost attempt is an idempotent no-op (`complete_on_lost_attempt_is_idempotent`). A task whose attempt is lost is failed with `error_code="node_lost"`. Marks plan items 36/37/38/40 done.
- control-plane (2.5): node protocol versioning. `EnrollRequest`/`HeartbeatRequest`/`PollRequest` carry an optional `protocol_version`; a major mismatch marks the node `degraded` (incompatible_protocol) instead of scheduling it. The node daemon advertises `NODE_PROTOCOL_VERSION` on every enroll/heartbeat/poll. Covered by `node_protocol_mismatch_marks_degraded` (api).

### Added ( — workflow operations)
- control-plane (8): `POST /v1/workflow-runs/{id}/cancel` cancels the whole run and every non-terminal step, and cancels any spawned task (`cancel_workflow_run`). CLI `ag workflow cancel <id>` added. Covered by `cancel_workflow_run_cancels_steps_and_tasks` (store) and `cancel_workflow_run_handler_cancels` (api). Pause/resume remain a follow-up.
- control-plane (8): `POST /v1/workflows` accepts YAML bodies (content-type `application/yaml`) via `WorkflowTemplate::from_yaml`; the JSON contract is unchanged. Covered by `yaml_round_trips_to_template` (common) and `create_workflow_accepts_yaml` (api).

### Added ( — versioned event envelope)
- common: `AgentEventEnvelope { version, kind, payload, raw_ref }` layered over the stored `TaskEvent`, plus an `EventKind` vocabulary (`plan`/`tool_call`/`tool_result`/`file_change`/`permission_request`/`usage`/`handoff`/...). Unknown kinds are preserved as `EventKind::Other` and never fatal; serde round-trip tested.
- node-daemon: `read_stream` decodes the new envelope (and still the legacy `{type,payload}` NDJSON); unknown kinds become raw logs, so a future adapter cannot break the pipeline. Legacy `TaskEvent`/`EventType` storage contract is unchanged.

### Added ( — agent sessions)
- common: `CreateAgentSessionRequest { adapter }` and `AgentSession { id, attempt_id, adapter, started_at, ended_at, status, error_code }`.
- control-plane: `agent_sessions` table (migration 0010, FK to `attempts`). Node opens a session per attempt via `POST /v1/node/attempts/{id}/session` (auth required); the row starts `running` and is closed (`done`/`failed`) when the attempt completes. `get_agent_session` supports reporting/tests.
- node-daemon: after acknowledging an assignment it calls `POST .../session` once, so each agent execution is attributable to its attempt.
- Store: `finish_agent_session` runs inside `complete_attempt`'s transaction (previously a separate pooled connection, which deadlocked against the open write transaction and surfaced as `database is locked`).

### Added ( — execution backend contract)
- adapters: `ExecutionBackend` trait + `ProcessBackend` (native subprocess-in-worktree). `node-daemon` now spawns attempts through `ProcessBackend::spawn`, isolating the execution contract from orchestration so future backends (container/ACP) drop in without touching the daemon.
- common: `AdapterCapability { id, version, ready }`; `HeartbeatRequest.capabilities` advertises per-adapter version + readiness each beat (degraded node already reports missing binaries).
- adapters: conformance smoke drives the mock adapter through `ExecutionBackend` (start → stream → collect) and asserts event output.
- common: `EventKind::Cancel`; the node daemon emits a normalized cancel event into the stream when cancellation is triggered. The atomic `cancel_task` UPDATE already makes cancel race-free (`cancel_requested` is only set on non-terminal attempts, and `complete_attempt` honors it), so the outcome is deterministic.

### Added ( — Agent Skills format & discovery)
- skills (new crate `agentgrid-skills`): minimal YAML-frontmatter parser for `SKILL.md` (`name`, `description`, `license`, `compatibility`, `allowed-tools`, `metadata`) with strict + lenient modes. `discover()` scans `<project>/.agents/skills`, `~/.agents/skills`, and managed roots in precedence order (project > user > managed), resolving collisions deterministically with diagnostics. `Skill::catalog_entry()` exposes only name + description (progressive disclosure); the body is materialised on activation. Fixtures cover minimal, malformed-yaml, collision, and untrusted-script.

### Added ( — skill trust & bundles)
- skills: `TrustStore` (project skills untrusted by default — malicious-repo protection; user/managed trusted), `SkillBundle` manifest (filesystem/git sources, commit/hash pin, lock file) with `verify_locks`, `materialize()` (copies original `SKILL.md` verbatim, skips untrusted project skills, verifies lock hashes), and `RevisionStore` (immutable revisions under `<root>/revisions/<id>` with a transactional `active` symlink + `rollback`). All covered by unit + fixture tests; agent/remote integration + E2E materialization remain as follow-ups.

### Added ( — ACP southbound client)
- acp (new crate `agentgrid-acp`): JSON-RPC 2.0 codec (request/response/notification, newline framing) + `AcpClient` over any byte transport (stdio in prod, in-memory pipe in tests) with id-matched responses and a notification channel. `initialize` tolerates unknown optional capabilities; `session/new|prompt|cancel` convenience methods; `session/update` → `AgentEventEnvelope` mapping (plan/tool_call/diff/usage/log/permission/...). `next_approval` state machine (`pending → allowed|denied|expired|cancelled`, fail-closed) built before any ACP integration. Covered by codec round-trip + a fake-agent lifecycle test (init → session/new → prompt streaming updates → result).

### Added ( — ACP node integration)
- node-daemon: ACP adapter registry type. `AdapterSpec { id, protocol }` with `AdapterProtocol::{Wrapper,Acp}`; `AGENTGRID_ADAPTERS=mock,claude,opencode:acp` selects the protocol per entry (default `Wrapper`, fully backward compatible). Heartbeat/poll/enroll advertise adapter ids as before.
- node-daemon: `drive_acp_session` drives an ACP agent over stdio via `AcpClient` — `initialize` → `session/new` → `session/prompt`, forwarding every `session/update` into the event sink (mapped to `AgentEventEnvelope`), and handling `session/cancel`/`timeout` internally. The wrapper path is unchanged.
- node-daemon + control-plane: `session/request_permission` creates a durable approval (`POST /v1/tasks/{id}/approvals`) and the daemon polls `GET /v1/approvals/{id}` until an operator answers, then replies `allow`/`deny` (fail-closed). Control plane adds the create + get-by-id endpoints.
- node-daemon: test-only ACP agent (`src/bin/adapter-fake-acp.rs`) exercises the full spawn/update/result pipeline; a unit test asserts the session succeeds and ≥2 `session/update` events stream into the sink. Control-plane API test covers approval create → pending → allow → allowed and unknown-id 404.
- acp: conformance tests cover the full `session/update` vocabulary mapping (`plan`/`tool_call`/`tool_result`/`diff`→`file_change`/`progress`/`permission_request`/`usage`/`log`, unknown→`Other`) and `session/cancel` acknowledgement, alongside the existing init→new→prompt lifecycle test.

### Added ( — ACP northbound gateway)
- acp: `GatewayAgent` speaks the ACP *agent* role so Agentgrid can be driven by an external ACP client. `session/new` mints a session id; `session/prompt` creates an Agentgrid task (prompt known only at the gateway), streams the task's `session/update` events back to the client until the task terminates, and `session/cancel` cancels the underlying task.
- acp: `AcpServer` (generic over the byte transport) drives the agent lifecycle — it decodes inbound JSON-RPC, dispatches each request on its own task (so an in-flight `session/prompt` can keep receiving the client's responses), and routes client responses back to agent→client requests via a shared pending map.
- acp: `AcpCtx::request` lets the agent issue agent→client requests (e.g. `session/request_permission`) and await the response; the server's read loop routes the client's answer back. The `AcpAgent` trait now returns `Send` futures (RPITIT).
- acp: approval requests flow end-to-end — the gateway polls `GET /v1/approvals?status=pending`, surfaces a pending approval for its task to the ACP client as `session/request_permission`, relays the client's `allow`/`deny` decision back to the control plane (`POST /v1/approvals/{id}/allow|deny`), and asks each approval exactly once. This closes the  e2e acceptance criterion (node → control plane → ACP client → back).
- acp: `agentgrid-acp-agent` binary runs the gateway over stdio (`AGENTGRID_SERVER`, optional `AGENTGRID_TOKEN`); any ACP-compatible client can now create tasks on the control plane and watch plan/progress/diff/permission events.
- acp: integration smoke test spins up an in-process fake control plane (axum) and drives the gateway from a real ACP client over a pipe — asserts `session/update` streaming, permission round-trip, and a `succeeded` terminal result.
- acp: `_`-prefixed extension methods let an external ACP client read Agentgrid state through the gateway. `AcpServer` routes any `method` starting with `_` to `AcpAgent::handle_extension`; `GatewayAgent` implements `_agentgrid/nodes` (`GET /v1/nodes`) and `_agentgrid/task_eligibility` (`GET /v1/tasks/{id}/eligibility`). Unknown extension methods return a clean RPC error (no hang). Covered by a new integration test.
- docs: `docs/acp-interop.md` records ACP client interoperability (Poracode/Lightcode) — the standard agent role works unmodified; lists non-standard gaps (`_agentgrid/*` extensions, no `session/load`/`resume` passthrough, client `session/update` not forwarded).

### Added ( — workflow data model + DAG validation)
- common: `workflow` module — `WorkflowStep` (id/prompt/depends_on/role/adapter), `WorkflowTemplate`, `WorkflowRun`, `WorkflowStepRun`, and the role/run/step status enums. `WorkflowRole` = `architect`/`worker`/`verifier` (v1 creates one role-run per step for its declared role).
- control-plane: `validate_workflow_dag` (pure) — non-empty, unique ids, existing dependencies, no cycles (Kahn). `DagError` enumerates every failure; 7 unit tests cover valid chains/diamonds and all four error kinds.
- control-plane: migration `0012_workflows.sql` (tables `workflow_templates`, `workflow_runs`, `workflow_steps`, `role_runs`). `Store` gains `create_workflow_template` (validates before insert), `get/list_workflow_template(s)`, `create_workflow_run` (instantiates steps + one role-run each, transactional), `get/list_workflow_run(s)`, `get_workflow_run_steps`. Storage round-trip covered by an integration test on a temp SQLite file.

### Added ( — workflow API + CLI)
- control-plane: HTTP surface for workflows (user-authenticated, same middleware as `/v1/tasks`). `POST /v1/workflows` (define template, validates DAG), `GET /v1/workflows` (list), `GET /v1/workflows/{id}` (show), `POST /v1/workflows/{id}/runs` (start run), `GET /v1/workflow-runs` (list), `GET /v1/workflow-runs/{id}` (run + step instances). Invalid DAG → `400`; unknown id → `404`.
- common: `CreateWorkflowRequest`, `CreateWorkflowRunRequest`, `WorkflowRunWithSteps` DTOs.
- cli: `ag workflow create|list|show|run`. `create` reads steps from a JSON file; `run` starts a run of a template. Covered by two `tests/api.rs` integration tests (happy path + invalid-DAG rejection).

### Added ( — DAG execution scheduler + roles)
- common: `WorkflowRole` expanded to `architect`/`worker`/`reviewer`/`integrator`/`verifier` (v1 still creates one role-run per step for its declared role).
- control-plane: migration `0013_workflows_repository.sql` adds `workflow_runs.repository` so step tasks schedule against enrolled nodes.
- control-plane: `Store::tick_workflow_run` — durable, idempotent scheduler. Marks a `pending` run `running`; activates `pending` steps whose dependencies are all `succeeded` by creating one Agentgrid task per step (tagged with the step's role); advances `running` steps whose task terminated; computes run status (succeeded when all leaves done, failed on any step failure). `create_workflow_run` now takes a `repository`.
- control-plane: `POST /v1/workflow-runs/{id}/tick` drives the scheduler (wakes the assignment notifier) and returns the run + step instances.
- tests: `tests/api.rs` golden workflow — `architect → 2 parallel workers → integrator → verifier` runs locally to a `succeeded` run on mock adapters (deterministic, exercises the full durable scheduler).

### Added ( — durable approval flow)
- control-plane: `approvals` table (migration 0011) + store (`create_approval`, `answer_approval` honoring the state machine, `get_approval`, `list_approvals`, `tick_approval_expiry`). API: `GET /v1/approvals`, `POST /v1/approvals/{id}/allow|deny` (user-auth, fail-closed). CLI: `ag approvals list|allow|deny`. The approval state machine moved into `agentgrid-common` so the control plane and the ACP client share one definition. Covered by an API test (create → list pending → allow → list allowed; terminal re-answer is a no-op).

## [0.1.0] - 2026-07-17

### Added ( — CI / release / ops)
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

### Added ( — OpenCode adapter)
- `adapter-opencode` wrapper binary: drives `opencode run --format json` headless and translates its `text`/`tool_use`/`error` events into the agentgrid contract (`log`/`tool_call`/`tool`/`error`); unknown event types are ignored (raw stdout is preserved as an artifact). Optional env `AGENTGRID_OPENCODE_BIN`/`AGENTGRID_OPENCODE_MODEL`/`AGENTGRID_OPENCODE_AUTO`. The underlying `opencode` CLI is provided by the operator (like `claude`); the wrapper is bundled into the node image.

### Added
- Cargo workspace scaffold: `common`, `control-plane`, `node-daemon`, `cli`, `adapters`.
- Shared types and API DTOs (`crates/common`): task/attempt/node status enums, event model, `/v1` request/response types, serde round-trip tests.
- In-memory control plane (`crates/control-plane`): Axum server with health, task CRUD, node long-poll assignment, event ingest (idempotent), attempt completion. First-fit scheduler respects `requested_node_id` and node capacity.
- Node daemon (`crates/node-daemon`): long-poll loop, adapter subprocess in a per-attempt worktree and separate process group, stdout/stderr streamed as batched events, completion reporting.
- Mock adapter (`crates/adapters`): deterministic `sleep:`/`write:`/`fail:`/`spam:` commands emitting JSON-line events; no LLM required.
- Minimal CLI (`crates/cli`): `task run`, `task logs --follow`, `task show`, `node list`.
- Integration test exercising the full task lifecycle and event idempotency.
- ADR recording  scope decisions (`docs/decisions/0001-mvp-scope.md`).

### Scope note
This is the Stage-1 vertical prototype. Persistence (SQLite WAL), auth, Git worktrees, real adapters and web UI follow in later stages.

### Added ( / 2.2 — persistence + state machine)
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

### Added ( — metrics)
- `GET /metrics` exposes Prometheus-text counts: `agentgrid_nodes{status}`,
  `agentgrid_tasks{status}`, `agentgrid_attempts_total`.
- Test: metrics endpoint returns counts.

### Added ( / 3.4 — validation command + secret masking)
- After the agent succeeds, the node runs `Assignment.validation_command` in the
  worktree (diff already committed first, so it survives a failure); non-zero exit
  reports `error_code=validation_failed`, distinct from `agent_failed`. Validation
  output is streamed as events and saved as `validation.log` artifact.
- Known secret substrings (env `AGENTGRID_SECRETS`, comma-separated) are masked to
  `***` in streamed stdout/stderr before upload.
- `CompleteAttemptRequest.error_code` recorded on the attempt.
- Node-daemon tests: secret masking + validation exit code/log.

### Added ( — events streaming, SSE)
- `GET /v1/tasks/{id}/events/stream` Server-Sent-Events endpoint: streams existing and
  new attempt events (polls every 250ms, 15s keep-alive ping) for the web UI.
- Idempotent event ingest and batching were already in place (/2.2).

### Added ( — artifacts)
- `POST /v1/node/attempts/{id}/artifacts` (node auth) stores a text artifact on the
  control-plane filesystem under `artifact_root/<attempt_id>/<name>` and records
  metadata (idempotent per name).
- `GET /v1/tasks/{id}/artifacts/{name}` serves the latest attempt's artifact.
- Node daemon uploads `changes.patch` after finalizing a git-backed attempt.
- Schema migration `0005`: `artifacts` table.
- Test: artifact upload (node auth) + read by task id.

### Added ( — repositories + git worktrees)
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

### Added ( — full CLI)
- `ag server` starts the control plane by exec'ing the sibling `agentgrid-control-plane` binary (sets `AGENTGRID_LISTEN`/`AGENTGRID_DB`; optional one-time `--bootstrap-user`/`--bootstrap-password`).
- `task run` gains `--validate` (validation command) and `--timeout` (seconds); `--adapter`/`--node` already present.
- `node list` and `task show` gain a global `--json` flag for machine-readable output.
- `token create`, `repo add`, `task logs --follow`, `task cancel`/`retry`, `login` already present; `node list` renders an aligned table.
- Deferred: `node install` (systemd unit + enroll) — lands with packaging in .

### Added ( — observability)
- `GET /metrics` expanded (Prometheus text): task duration histogram, terminal outcome
  counters (`agentgrid_tasks_total`), per-node `free_disk_mb`/`load_avg` gauges from heartbeat,
  and SQLite main/WAL file size gauges.
- `GET /health/ready` now also probes writability of the database directory.
- Control plane and node daemon emit structured JSON logs (tracing `fmt().json()`).
- Deferred (instrumentation needed): scheduler/heartbeat latency, event-buffer size,
  `SQLITE_BUSY`/checkpoint/write-lock metrics.

### Added ( — security)
- Request size limits (trust-boundary input validation), overridable via env, returning 413:
  `AGENTGRID_MAX_PROMPT_KB` (64), `AGENTGRID_MAX_EVENT_KB` (1024), `AGENTGRID_MAX_ARTIFACT_MB` (50).
  A global `DefaultBodyLimit` caps request bodies at the artifact ceiling; the prompt and
  per-event payload ceilings are enforced in the handlers.
- Node daemon refuses to start as uid 0 unless `AGENTGRID_ALLOW_ROOT=1` is set.
- Audit events on all user actions (login, user.create, task.create/cancel/retry, repo.add)
  plus existing node enroll/revoke. `AuthedUser` is attached by the user-auth middleware
  so handlers can record the acting username.
- Enrollment token (one-time, TTL ≤ 10 min, hash-only) and per-node unique credential with
  immediate revoke already landed in ; marked verified here.

### Added ( — web UI)
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

### Added ( — user authentication)
- Local users: `users` table (argon2id password hash). First user created via `POST /v1/auth/setup` (only while no users exist) or via `AGENTGRID_BOOTSTRAP_USER`/`AGENTGRID_BOOTSTRAP_PASSWORD` env at startup.
- `POST /v1/auth/login` exchanges username+password for a 12h HS256 JWT. Secret from `AGENTGRID_JWT_SECRET` (random per start if unset).
- `require_user_auth` middleware protects all `/v1/*` user endpoints (tasks, repositories, enrollment-token, nodes management). Open only during the bootstrap window (no users yet); node endpoints keep their own credential auth.
- CLI `ag login` stores the JWT at `~/.config/agentgrid/credentials` (0600) and attaches it as `Bearer` to all user requests.
- Integration test: setup→login→protected endpoint 401 without token / 201 with token; wrong password 401; second setup rejected.

### Added ( — Claude Code adapter)
- `adapter-claude` wrapper binary (ADR #12): launches `claude -p --output-format stream-json --verbose --dangerously-skip-permissions` and translates its output into the agentgrid event contract (`log`/`tool_call`/`tool`/`result`); unrecognized lines/blocks fall back to raw `log`.
- Exit code is claude's; a `result` with `is_error:true` forces a non-zero exit so the daemon records `agent_failed`. API key supplied via env (`ANTHROPIC_API_KEY`) forwarded by the daemon through `AGENTGRID_ADAPTER_ENV`.
- Verified end-to-end with a fake `claude` shim (translate + exit-code paths). Unit tests cover the `translate` mapping. Real-key run left as a manual `#[ignore]`-style check ( exit criteria).

### Added ( — adapter contract finalized + capability discovery)
- Adapter contract documented (subprocess model: `prepare`=worktree, `start`=`--prompt`, `stream`=NDJSON stdout, `cancel`=SIGTERM process group, `collect`=artifacts). Unknown stdout lines fall back to raw `log` so a future CLI format change cannot break the pipeline.
- Capability discovery (): the daemon probes the adapter binary in `PATH` at startup and on every heartbeat; a missing binary makes the node report `degraded` so the scheduler excludes it. Detected version is logged.
- Adapter config: `AGENTGRID_ADAPTER_ENV` forwards `KEY=VALUE` pairs (e.g. API keys) to the adapter subprocess.
- Raw adapter output is mirrored to `agent-raw-output.log` in the worktree and uploaded as an artifact on completion (format-change safety net, spec risk #1).
- Integration tests: `probe_adapter` (found/missing) and `read_stream` raw-log mirroring.

### Added ( — scheduler filters + `no_eligible_nodes` visibility)
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

### Added ( — node lifecycle: enrollment, heartbeat, revoke)
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

### Added ( — cancellation + timeout)
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
