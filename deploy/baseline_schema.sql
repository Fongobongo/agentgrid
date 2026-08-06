-- AgentGrid Control Plane Baseline Schema
-- Generated from all migrations for fresh installs.
-- Run this directly against a fresh SQLite database: sqlite3 agentgrid.db < baseline_schema.sql
-- Do NOT use with sqlx migrate; sqlx will still run incremental migrations on top.

-- == 0001_init.sql ==
-- agentgrid control-plane schema (Stage 2.1)
-- One active control plane; SQLite on local disk only (no NFS/network shares).

CREATE TABLE IF NOT EXISTS nodes (
    id                 TEXT PRIMARY KEY,
    name               TEXT NOT NULL,
    status             TEXT NOT NULL,
    os                 TEXT,
    arch               TEXT,
    agent_version      TEXT,
    max_concurrency    INTEGER NOT NULL DEFAULT 1,
    adapters           TEXT NOT NULL DEFAULT '[]',
    repositories       TEXT NOT NULL DEFAULT '[]',
    active_attempts    INTEGER NOT NULL DEFAULT 0,
    last_heartbeat_at  TEXT,
    created_at         TEXT NOT NULL,
    credential_hash    TEXT,
    revoked_at         TEXT
);

CREATE TABLE IF NOT EXISTS tasks (
    id                   TEXT PRIMARY KEY,
    repository           TEXT NOT NULL,
    prompt               TEXT NOT NULL,
    adapter              TEXT NOT NULL,
    requested_node_id    TEXT,
    status               TEXT NOT NULL,
    created_at           TEXT NOT NULL,
    started_at           TEXT,
    finished_at          TEXT,
    assigned_attempt_id  TEXT
);

CREATE TABLE IF NOT EXISTS attempts (
    id                TEXT PRIMARY KEY,
    task_id           TEXT NOT NULL,
    number            INTEGER NOT NULL,
    node_id           TEXT NOT NULL,
    status            TEXT NOT NULL,
    lease_expires_at  TEXT,
    workspace_path    TEXT,
    branch_name       TEXT,
    commit_sha        TEXT,
    exit_code         INTEGER,
    error_code        TEXT,
    started_at        TEXT NOT NULL,
    finished_at       TEXT,
    UNIQUE (task_id, number)
);

CREATE TABLE IF NOT EXISTS task_events (
    id           TEXT PRIMARY KEY,
    attempt_id   TEXT NOT NULL,
    sequence     INTEGER NOT NULL,
    type         TEXT NOT NULL,
    payload      TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    UNIQUE (attempt_id, sequence)
);

CREATE INDEX IF NOT EXISTS idx_tasks_status            ON tasks (status);
CREATE INDEX IF NOT EXISTS idx_attempts_task           ON attempts (task_id);
CREATE INDEX IF NOT EXISTS idx_attempts_status_lease   ON attempts (status, lease_expires_at);
CREATE INDEX IF NOT EXISTS idx_events_attempt          ON task_events (attempt_id, sequence);
CREATE INDEX IF NOT EXISTS idx_nodes_status            ON nodes (status);


-- == 0002_cancel_timeout.sql ==
-- Stage 2.7: cancellation + per-task timeout support.
ALTER TABLE attempts ADD COLUMN cancel_requested INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN timeout_secs INTEGER NOT NULL DEFAULT 3600;


-- == 0003_enrollment_auth.sql ==
-- Stage 2.3: node enrollment, credential auth, heartbeat telemetry, audit log.
-- nodes.credential_hash and os/arch/agent_version/revoked_at already exist (0001).

CREATE TABLE IF NOT EXISTS enrollment_tokens (
    id          TEXT PRIMARY KEY,
    token_hash  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    used_at     TEXT,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_enroll_token_hash ON enrollment_tokens (token_hash);

CREATE TABLE IF NOT EXISTS audit_events (
    id          TEXT PRIMARY KEY,
    actor_type  TEXT NOT NULL,
    actor_id    TEXT,
    action      TEXT NOT NULL,
    subject     TEXT,
    payload     TEXT,
    created_at  TEXT NOT NULL
);

ALTER TABLE nodes ADD COLUMN load_avg REAL NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN free_disk_mb INTEGER NOT NULL DEFAULT 0;


-- == 0004_repositories.sql ==
-- Stage 2.5: repository registration and per-node clone tracking.
CREATE TABLE IF NOT EXISTS repositories (
    id                 TEXT PRIMARY KEY,
    name               TEXT NOT NULL UNIQUE,
    git_url            TEXT NOT NULL,
    default_branch     TEXT NOT NULL,
    validation_command TEXT,
    created_at         TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS node_repositories (
    node_id         TEXT NOT NULL,
    repository_id   TEXT NOT NULL,
    local_path      TEXT,
    status          TEXT NOT NULL,
    last_synced_at  TEXT,
    PRIMARY KEY (node_id, repository_id)
);


-- == 0005_artifacts.sql ==
-- Stage 2.8: artifact metadata (raw bytes live on the control-plane filesystem
-- under artifact_root/<attempt_id>/<name>; SQLite keeps only metadata).
CREATE TABLE IF NOT EXISTS artifacts (
    id          TEXT PRIMARY KEY,
    attempt_id  TEXT NOT NULL,
    name        TEXT NOT NULL,
    size_bytes  INTEGER NOT NULL,
    stored_at   TEXT NOT NULL,
    UNIQUE (attempt_id, name)
);
CREATE INDEX IF NOT EXISTS idx_artifacts_attempt ON artifacts (attempt_id);


-- == 0006_users.sql ==
-- Stage 4.1: local user accounts (argon2 password hash, JWT issued at login).
CREATE TABLE IF NOT EXISTS users (
    id           TEXT PRIMARY KEY,
    username     TEXT NOT NULL UNIQUE,
    password_hash TEXT NOT NULL,
    created_at   TEXT NOT NULL
);


-- == 0007_task_validation.sql ==
-- Stage 4.3: optional per-task validation command overriding the
-- repository default at assignment time.
ALTER TABLE tasks ADD COLUMN validation_command TEXT;


-- == 0008_task_error_code.sql ==
-- Stage 1.1: surface the distinct failure category on the task itself so the
-- UI/CLI can show WHY a task failed (validation/timeout/...) without joining
-- the attempt that produced it. NULL when the task succeeded or was cleanly
-- cancelled.
ALTER TABLE tasks ADD COLUMN error_code TEXT;


-- == 0009_attempt_ack_deadline.sql ==
-- Stage 1.3: explicit assignment acknowledgement. An attempt has a separate
-- ack_deadline; if the node never acks (crashes before starting), the
-- assignment is reverted and the task returns to the queue.
ALTER TABLE attempts ADD COLUMN ack_deadline TEXT;


-- == 0010_agent_sessions.sql ==
-- Stage 3.2: agent sessions. One row per agent execution inside an attempt,
-- used by the conformance suite and reporting. Linked to attempts(id) so a
-- session is always attributable to the attempt that spawned it.
CREATE TABLE agent_sessions (
    id TEXT PRIMARY KEY,
    attempt_id TEXT NOT NULL,
    adapter TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    status TEXT NOT NULL DEFAULT 'running',
    error_code TEXT,
    FOREIGN KEY (attempt_id) REFERENCES attempts (id)
);
CREATE INDEX idx_agent_sessions_attempt ON agent_sessions (attempt_id);


-- == 0011_approvals.sql ==
-- Stage 5: durable approval flow (prerequisite for ACP session/request_permission).
CREATE TABLE approvals (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL,
    attempt_id  TEXT NOT NULL,
    session_id  TEXT,
    permission  TEXT NOT NULL,
    status      TEXT NOT NULL,                 -- pending|allowed|denied|expired|cancelled
    reason      TEXT,
    created_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    decided_at  TEXT,
    audit       TEXT                          -- who/what decided (JSON)
);

CREATE INDEX idx_approvals_task ON approvals(task_id);
CREATE INDEX idx_approvals_status ON approvals(status);


-- == 0012_workflows.sql ==
-- agentgrid control-plane schema (Stage 7: workflow engine)
-- Workflow = a DAG of steps. A run instantiates the template's steps and, for
-- each step, one role-run for the step's declared role (multi-role fan-out is
-- later). Dependencies drive execution order; the scheduler starts a step once
-- all of its dependencies have succeeded.

CREATE TABLE IF NOT EXISTS workflow_templates (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    steps_json  TEXT NOT NULL,   -- JSON array of WorkflowStep
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS workflow_runs (
    id           TEXT PRIMARY KEY,
    template_id  TEXT NOT NULL,
    status       TEXT NOT NULL,
    context      TEXT,           -- optional shared JSON context
    created_at   TEXT NOT NULL,
    finished_at  TEXT
);

CREATE TABLE IF NOT EXISTS workflow_steps (
    id           TEXT PRIMARY KEY,   -- run-scoped instance id
    run_id       TEXT NOT NULL,
    step_id      TEXT NOT NULL,      -- template step id
    prompt       TEXT NOT NULL,
    depends_on   TEXT NOT NULL DEFAULT '[]',
    role         TEXT NOT NULL,
    adapter      TEXT,
    status       TEXT NOT NULL DEFAULT 'pending',
    created_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS role_runs (
    id           TEXT PRIMARY KEY,
    step_run_id  TEXT NOT NULL,
    role         TEXT NOT NULL,
    task_id      TEXT,
    status       TEXT NOT NULL DEFAULT 'pending',
    created_at   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_workflow_runs_template ON workflow_runs (template_id);
CREATE INDEX IF NOT EXISTS idx_workflow_steps_run     ON workflow_steps (run_id);
CREATE INDEX IF NOT EXISTS idx_role_runs_step        ON role_runs (step_run_id);


-- == 0013_workflow_repository.sql ==
-- agentgrid control-plane schema (Stage 7.3): workflow runs target a repository
-- so their step tasks can be scheduled against enrolled nodes.
ALTER TABLE workflow_runs ADD COLUMN repository TEXT;


-- == 0014_workflow_placement.sql ==
-- agentgrid control-plane schema (Stage 8): per-step node placement so a
-- workflow can spread roles across different nodes.
ALTER TABLE workflow_steps ADD COLUMN requested_node_id TEXT;


-- == 0015_workflow_base_commit_retry.sql ==
-- agentgrid control-plane schema (Stage 8): shared base_commit + per-step
-- retry policy for distributed workflows.
ALTER TABLE workflow_runs ADD COLUMN base_commit TEXT;
ALTER TABLE workflow_steps ADD COLUMN base_commit TEXT;
ALTER TABLE workflow_steps ADD COLUMN retryable INTEGER;
ALTER TABLE workflow_steps ADD COLUMN max_attempts INTEGER;
ALTER TABLE workflow_steps ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN base_commit TEXT;


-- == 0016_approval_step_link.sql ==
-- Stage 9.2: link a durable approval to the workflow step that is waiting on it,
-- so an unanswered (timed-out) approval can block that step instead of leaving
-- the run hanging.
ALTER TABLE approvals ADD COLUMN step_run_id TEXT;
CREATE INDEX IF NOT EXISTS idx_approvals_step ON approvals(step_run_id);


-- == 0017_approval_scope.sql ==
-- Stage 9.2: scope an approval request (tool_call / session / step / command /
-- duration) so operators see what they are approving.
ALTER TABLE approvals ADD COLUMN scope TEXT NOT NULL DEFAULT 'session';


-- == 0018_conversations.sql ==
-- Conversations: stateful multi-turn chat sessions routed through the control
-- plane to a coding agent on some node. One row per conversation; messages keep
-- the shared context the control plane composes into each task's prompt so any
-- node that picks the task up sees the full history.
CREATE TABLE conversations (
    id TEXT PRIMARY KEY,
    adapter TEXT NOT NULL,
    repository TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL
);

CREATE TABLE conversation_messages (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    role TEXT NOT NULL,           -- 'user' | 'assistant'
    content TEXT NOT NULL DEFAULT '',
    task_id TEXT,                 -- the task that produced (assistant) / carried (user) this message
    created_at TEXT NOT NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations (id)
);

CREATE INDEX idx_conv_msgs ON conversation_messages (conversation_id, seq);


-- == 0019_acp_session_resume.sql ==
-- Stage 11.5: ACP session resume. `attempts.acp_session_id` stores the
-- session id the node received from `session/new`; `tasks.parent_acp_session_id`
-- is the session the node should resume (passed to ACP `session/new`).
ALTER TABLE attempts ADD COLUMN acp_session_id TEXT;
ALTER TABLE tasks ADD COLUMN parent_acp_session_id TEXT;


-- == 0020_skill_trust.sql ==
-- Stage 9.2: skill trust ledger. Stores operator trust decisions per skill,
-- keyed by (name, source). A skill absent from this table defaults to
-- `untrusted` (fail-closed): the agent is not allowed to load/execute it until
-- the operator explicitly trusts it. A node that discovers a skill reports it
-- via heartbeat; the control plane answers with the recorded verdict, or
-- untrusted if no decision exists.
--
-- `source` matches SkillSource's display string (project|user|managed) so the
-- same skill name can carry different trust across where it was found.
-- `decided_by` is the operator username (or `system`); `decided_at` is ISO.
CREATE TABLE skills_trust (
    name        TEXT NOT NULL,
    source      TEXT NOT NULL,
    trusted     INTEGER NOT NULL,     -- 0 = untrusted (default), 1 = trusted
    decided_by  TEXT,
    decided_at  TEXT,
    PRIMARY KEY (name, source)
);


-- == 0021_agent_profiles.sql ==
-- Stage 13: Agent profile desired-state ledger. Immutable revisions; the
-- active revision is pointed at by `agent_profiles.active_revision`. A
-- profile carries the system prompt + autonomy + resource limits the node
-- should project for this adapter; secrets are never stored here (the node
-- resolves secret references from its env at apply time).
CREATE TABLE agent_profiles (
    id            TEXT NOT NULL,           -- profile id (e.g. adapter name)
    revision      INTEGER NOT NULL,        -- monotonically increasing per id
    system_prompt TEXT NOT NULL DEFAULT '',
    autonomy      TEXT NOT NULL DEFAULT 'l2',
    memory_max    INTEGER,                 -- bytes; NULL = no ceiling
    cpu_quota     INTEGER,                 -- percent of one core
    tasks_max     INTEGER,                 -- max PIDs
    created_at    TEXT NOT NULL,
    created_by    TEXT,
    PRIMARY KEY (id, revision)
);

-- One row per profile id pointing at the active revision (fail-closed: a
-- profile not present here is not yet active even if revisions exist).
CREATE TABLE agent_profiles_active (
    id             TEXT PRIMARY KEY,
    active_revision INTEGER NOT NULL,
    FOREIGN KEY (id, active_revision) REFERENCES agent_profiles (id, revision)
);


-- == 0022_profile_secrets_caps.sql ==
-- Stage 13: ext профиль secret requirements (names only, never values) +
-- adapter version (capability check). secret_requirements хранятся как JSON
-- массив {env,required}; adapter_version — SemVer major string или NULL.
ALTER TABLE agent_profiles ADD COLUMN secret_requirements TEXT NOT NULL DEFAULT '[]';
ALTER TABLE agent_profiles ADD COLUMN adapter_version TEXT;


-- == 0023_workflow_schedules.sql ==
-- Stage 13: scheduled/recurring workflow triggers. A schedule fires a
-- WorkflowRun of a template on a fixed interval. autonomy/budget constraints
-- are enforced at create (Stage 13 follow-up: the budget check; this lands
-- the schedule infra + interval + autonomy + enabled + last_run_at).
CREATE TABLE workflow_schedules (
    id               TEXT PRIMARY KEY,
    template_id      TEXT NOT NULL,
    interval_seconds INTEGER NOT NULL CHECK (interval_seconds >= 1),
    autonomy         TEXT NOT NULL DEFAULT 'l2',
    last_run_at      TEXT NOT NULL DEFAULT '',
    enabled          INTEGER NOT NULL DEFAULT 1,
    created_at       TEXT NOT NULL,
    FOREIGN KEY (template_id) REFERENCES workflow_templates (id) ON DELETE CASCADE
);
CREATE INDEX workflow_schedules_enabled ON workflow_schedules (enabled);


-- == 0024_attempt_provenance.sql ==
-- Stage 13: provenance — an optional external-origin link on each attempt.
-- Stored as a JSON ProvenanceRecord ({originator, external_id, optional label});
-- only identifiers, never secrets, so safe to persist + surface in the UI.
ALTER TABLE attempts ADD COLUMN provenance TEXT;


-- == 0025_mcp_servers.sql ==
-- Stage 13: MCP server registry. Operator-managed stdio servers a profile may
-- attach to a session. `env_requirements` lists env var *names* only (the node
-- resolves values from its own env at spawn; never stored here, like secret
-- requirements). `enabled` lets an operator disable a server without deleting
-- it so running sessions don't break.
CREATE TABLE mcp_servers (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    command           TEXT NOT NULL,
    args              TEXT NOT NULL DEFAULT '[]',        -- JSON array of strings
    env_requirements  TEXT NOT NULL DEFAULT '[]',       -- JSON array of names
    enabled           INTEGER NOT NULL DEFAULT 1,
    created_at        TEXT NOT NULL
);


-- == 0026_workflow_budget.sql ==
-- Stage 13 Loop Engineering: optional budget + circuit breaker attached to a
-- workflow template. Stored as JSON (WorkflowBudget serde). NULL means
-- "unbounded" (the historical default).
ALTER TABLE workflow_templates ADD COLUMN budget_json TEXT;


-- == 0027_plan_expansion.sql ==
-- Stage 13 plan expansion:
--   * attempts.plan TEXT  — optional machine-readable plan (YAML/JSON) emitted
--     by an expandable architect step; the run pauses in `PlanReady` and the
--     plan is expanded into new steps on approval.
--   * workflow_steps.expandable INTEGER NULL — mirrors WorkflowStep.expandable
--     (1 = the architect step produces a plan; 0/NULL = plain step).
--   * workflow_runs.plan TEXT NULL — the pending plan awaiting approval (copied
--     from the architect's winning attempt so the run can outlive the attempt).
ALTER TABLE attempts ADD COLUMN plan TEXT;

ALTER TABLE workflow_steps ADD COLUMN expandable INTEGER;

ALTER TABLE workflow_runs ADD COLUMN plan TEXT;


-- == 0028_workflow_messages.sql ==
-- Stage 13 typed AgentMessage mailbox: orchestrator-mediated messages between
-- workflow steps (not free-form P2P). A step publishes a typed message
-- (Stage 13 MVP: an `output` summary emitted automatically when a step
-- succeeds); downstream steps consume them on activation (the orchestrator
-- renders them into the consuming task's prompt). Only the orchestrator
-- writes rows; agents never insert directly. Keeps the loop within the
-- `WorkflowBudget.max_messages` ceiling observable.
ALTER TABLE workflow_runs ADD COLUMN message_sequence INTEGER NOT NULL DEFAULT 0;

CREATE TABLE workflow_messages (
    id          TEXT PRIMARY KEY,
    run_id      TEXT NOT NULL,
    from_step_id TEXT NOT NULL,
    -- to_step_id = '*' broadcasts to all downstream steps; otherwise pinned.
    to_step_id  TEXT NOT NULL,
    -- one of: output / plan / note
    kind        TEXT NOT NULL,
    -- structured payload (JSON object: {summary, commit_sha?}; the orchestrator
    -- emits a compact summary, never a full transcript).
    payload     TEXT NOT NULL,
    sequence    INTEGER NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE INDEX workflow_messages_run ON workflow_messages (run_id, sequence);


-- == 0029_artifact_binary.sql ==
-- Stage 2.2 binary-safe artifact API: record media type + content hash so
-- non-UTF-8 artifacts (binary patches, archives, images) round-trip without
-- the UTF-8 JSON body loss of the legacy text upload. Both columns are
-- nullable: the legacy text endpoint never set them.
ALTER TABLE artifacts ADD COLUMN media_type TEXT;
ALTER TABLE artifacts ADD COLUMN sha256     TEXT;


-- == 0030_step_timings.sql ==
-- Stage 11.6 follow-up: span waterfall (timeline by time). Steps need
-- started_at/finished_at so the web UI can position them on a time axis,
-- not just by dependency depth.
ALTER TABLE workflow_steps ADD COLUMN started_at  TEXT;
ALTER TABLE workflow_steps ADD COLUMN finished_at TEXT;


-- == 0031_profile_mcp_subset.sql ==
-- Stage 13 follow-up (per-profile MCP subset): an optional allow-list of
-- MCP server ids this profile attaches to its sessions. Empty (NULL/'[]') =
-- use every enabled server in the registry. Lets an operator restrict MCP
-- tools per adapter without splitting the registry.
ALTER TABLE agent_profiles ADD COLUMN mcp_server_ids TEXT NOT NULL DEFAULT '[]';


-- == 0032_fencing_tokens.sql ==
-- Hardening P0 item 8: fencing tokens.
-- Each attempt has a monotonic fencing token (mac/uuid) generated at
-- assignment; node->CP mutations carry it and the CP rejects a stale token
-- (e.g. a node reporting for an attempt that was reassigned/lost) with 409.
-- Default '' preserves the N-1 nodes that never send a token (they are still
-- accepted under the N/N-1 policy until they upgrade).
ALTER TABLE attempts ADD COLUMN fencing_token TEXT NOT NULL DEFAULT '';
ALTER TABLE nodes ADD COLUMN lease_generation INTEGER NOT NULL DEFAULT 0;

-- == 0033_resolved_base_sha.sql ==
-- agentgrid control-plane schema (P2 item 32-5)
-- Persist the exact upstream commit each attempt started from, so audits /
-- diffs can reconstruct the prepared base even when the task row's base_commit
-- was unset (branch default). Currently populated by the node daemon for the
-- explicitly pinned base_commit path; default-branch checkouts are left NULL
-- until the node daemon resolves HEAD per attempt (P1 follow-up).
ALTER TABLE attempts ADD COLUMN resolved_base_sha TEXT;


-- == 0034_conversation_seq_unique.sql ==
-- agentgrid control-plane schema (P2 item 21)
-- Enforce uniqueness of (conversation_id, seq) so concurrent appends cannot
-- silently collide. This is the DB-side backstop for the atomic seq
-- allocation in `append_conversation_message`; the handler still retries on
-- the rare SQLITE_BUSY/UNIQUE collision, but a dropped invariant here would
-- let two messages share a sequence and break ordered reads.
CREATE UNIQUE INDEX IF NOT EXISTS ux_conv_msgs_seq
    ON conversation_messages (conversation_id, seq);


-- == 0035_remote_head.sql ==
-- agentgrid control-plane schema (hardening P1 item 32)
-- Persist the remote (upstream) HEAD captured at attempt *start* and at
-- attempt *finish*, so audits / diffs / quarantine decisions can reconstruct
-- what upstream looked like when the attempt began and how it moved during
-- the attempt. Independent of resolved_base_sha (the prepared base the agent
-- built on). Populated by the node daemon; NULL when not a git repo or unset.
ALTER TABLE attempts ADD COLUMN remote_head_at_start TEXT;
ALTER TABLE attempts ADD COLUMN remote_head_at_finish TEXT;


-- == 0036_revoked_sessions.sql ==
-- Stage 4.2: revoked user sessions (JWT jti blocklist).
CREATE TABLE IF NOT EXISTS revoked_sessions (
    jti           TEXT PRIMARY KEY,
    username      TEXT NOT NULL,
    revoked_at    TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_revoked_sessions_username ON revoked_sessions (username);


-- == 0037_event_ingest_id.sql ==
-- Hardening P0 item 9: global monotonic event cursor.
--
-- task_events previously carried only a per-attempt `sequence` (restarts at
-- 1 for every attempt), so a client resuming after a retry could not order
-- events across attempts and the SSE `id:`/`Last-Event-ID` cursor was
-- ambiguous. This migration adds a global, monotonically increasing
-- `ingest_id` allocated from a dedicated single-row counter table inside the
-- ingest transaction, plus a unique index so every read path can resume on a
-- global cursor.
--
-- Idempotency of ingestion is unchanged: dedup stays on
-- `(attempt_id, sequence)` via `ON CONFLICT DO NOTHING`; `ingest_id` is only
-- required to be monotonic (gaps are fine — a duplicate redelivery consumes a
-- counter value but lands nowhere).

ALTER TABLE task_events ADD COLUMN ingest_id INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS event_ingest_counter (
    id       INTEGER PRIMARY KEY CHECK (id = 1),
    next_val INTEGER NOT NULL DEFAULT 1
);

INSERT OR IGNORE INTO event_ingest_counter (id, next_val) VALUES (1, 1);

-- Backfill pre-existing rows. SQLite `rowid` is monotonic per insert order,
-- which is a good-enough approximation for historical data so old events
-- remain orderable and resumable.
UPDATE task_events SET ingest_id = rowid WHERE ingest_id = 0;

CREATE UNIQUE INDEX IF NOT EXISTS ux_events_ingest ON task_events (ingest_id);


-- == 0038_attempt_pending_artifacts.sql ==
-- Hardening P1 item 11: record which artifacts a node staged locally but
-- could not deliver before completion (control plane was down). JSON array of
-- artifact names; NULL/empty = nothing owed. The node retries these on the
-- next startup; operators can see the outstanding set via the attempts row.
ALTER TABLE attempts ADD COLUMN pending_artifacts TEXT;


-- == 0039_node_unsafe.sql ==
-- Hardening P0 item 5: surface unsafe-node state to operators.
--
-- `unsafe_active` = the node is running an adapter with the unattended
-- permission bypass active (no sandbox); `permission_interception` is the
-- best-available interception across its adapters (structured | wrapper | none).
ALTER TABLE nodes ADD COLUMN unsafe_active INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN permission_interception TEXT NOT NULL DEFAULT 'wrapper';


-- == 0040_integrity_fks.sql ==
-- Hardening P1 item 21: database integrity — foreign keys + CHECK constraints.
--
-- SQLite cannot ALTER a table to add constraints, so the tables are rebuilt
-- (create-new → copy → drop → rename). sqlx runs each migration inside a
-- transaction, so PRAGMA foreign_keys is a per-file no-op here — acceptable:
-- fresh / reconciled databases have no orphan rows, and the FKs backstop NEW
-- writes. Orphan rows that predate this migration stay in place — surfaced by
-- `count_orphan_rows` / `storage_reconcile` and cleaned by maintenance.

-- ---- attempts: task_id/node_id FKs + status CHECK ----
CREATE TABLE attempts_new (
    id                TEXT PRIMARY KEY,
    task_id           TEXT NOT NULL,
    number            INTEGER NOT NULL,
    node_id           TEXT NOT NULL,
    status            TEXT NOT NULL CHECK (status IN ('assigned','running','validating','succeeded','failed','cancelled','lost')),
    lease_expires_at  TEXT,
    workspace_path    TEXT,
    branch_name       TEXT,
    commit_sha        TEXT,
    exit_code         INTEGER,
    error_code        TEXT,
    started_at        TEXT NOT NULL,
    finished_at       TEXT,
    ack_deadline      TEXT,
    cancel_requested  INTEGER NOT NULL DEFAULT 0,
    fencing_token     TEXT NOT NULL DEFAULT '',
    acp_session_id    TEXT,
    resolved_base_sha TEXT,
    remote_head_at_start TEXT,
    remote_head_at_finish TEXT,
    plan              TEXT,
    provenance        TEXT,
    pending_artifacts TEXT,
    UNIQUE (task_id, number),
    FOREIGN KEY (task_id) REFERENCES tasks (id) ON DELETE RESTRICT,
    FOREIGN KEY (node_id) REFERENCES nodes (id) ON DELETE RESTRICT
);

INSERT INTO attempts_new (
    id, task_id, number, node_id, status, lease_expires_at, workspace_path,
    branch_name, commit_sha, exit_code, error_code, started_at, finished_at,
    ack_deadline, cancel_requested, fencing_token, acp_session_id,
    resolved_base_sha, remote_head_at_start, remote_head_at_finish, plan,
    provenance, pending_artifacts
)
SELECT id, task_id, number, node_id, status, lease_expires_at, workspace_path,
       branch_name, commit_sha, exit_code, error_code, started_at, finished_at,
       ack_deadline, cancel_requested, fencing_token, acp_session_id,
       resolved_base_sha, remote_head_at_start, remote_head_at_finish, plan,
       provenance, pending_artifacts
FROM attempts;

DROP TABLE attempts;
ALTER TABLE attempts_new RENAME TO attempts;
CREATE INDEX IF NOT EXISTS idx_attempts_task ON attempts (task_id);
CREATE INDEX IF NOT EXISTS idx_attempts_status_lease ON attempts (status, lease_expires_at);

-- ---- task_events: attempt_id FK ----
CREATE TABLE task_events_new (
    id           TEXT PRIMARY KEY,
    attempt_id   TEXT NOT NULL,
    sequence     INTEGER NOT NULL,
    type         TEXT NOT NULL,
    payload      TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    ingest_id    INTEGER NOT NULL DEFAULT 0,
    UNIQUE (attempt_id, sequence),
    FOREIGN KEY (attempt_id) REFERENCES attempts (id) ON DELETE CASCADE
);

INSERT INTO task_events_new (id, attempt_id, sequence, type, payload, created_at, ingest_id)
SELECT id, attempt_id, sequence, type, payload, created_at, ingest_id FROM task_events;

DROP TABLE task_events;
ALTER TABLE task_events_new RENAME TO task_events;
CREATE INDEX IF NOT EXISTS idx_events_attempt ON task_events (attempt_id, sequence);
CREATE UNIQUE INDEX IF NOT EXISTS ux_events_ingest ON task_events (ingest_id);

-- ---- artifacts: attempt_id FK ----
CREATE TABLE artifacts_new (
    id          TEXT PRIMARY KEY,
    attempt_id  TEXT NOT NULL,
    name        TEXT NOT NULL,
    size_bytes  INTEGER NOT NULL,
    media_type  TEXT,
    sha256      TEXT,
    stored_at   TEXT NOT NULL,
    UNIQUE (attempt_id, name),
    FOREIGN KEY (attempt_id) REFERENCES attempts (id) ON DELETE CASCADE
);

INSERT INTO artifacts_new (id, attempt_id, name, size_bytes, media_type, sha256, stored_at)
SELECT id, attempt_id, name, size_bytes, media_type, sha256, stored_at FROM artifacts;

DROP TABLE artifacts;
ALTER TABLE artifacts_new RENAME TO artifacts;
CREATE INDEX IF NOT EXISTS idx_artifacts_attempt ON artifacts (attempt_id);


-- == 0041_node_storage_metrics.sql ==
-- Hardening P2 item 35: surface node-local storage pressure to operators.
--
-- `outbox_bytes` = total bytes buffered in the node's durable event/completion
-- outbox (pending delivery to the CP); `artifact_spool_bytes` = bytes staged
-- in the node's artifact spool (uploaded artifacts not yet acked). Reported by
-- the heartbeat so `ag nodes`/the web UI show nodes that are backing up.
ALTER TABLE nodes ADD COLUMN outbox_bytes INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN artifact_spool_bytes INTEGER NOT NULL DEFAULT 0;


-- == 0042_node_drain.sql ==
-- Hardening P2 item 37: node drain support.
--
-- A drained node stops receiving NEW task assignments but keeps running its
-- in-flight attempts (heartbeat stays online). The scheduler skips drained
-- nodes in `try_assign`; operators can drain before maintenance and undrain
-- afterwards. No new status variant — `drained` is an orthogonal flag so the
-- existing status machine is untouched.
ALTER TABLE nodes ADD COLUMN drained INTEGER NOT NULL DEFAULT 0;


-- == 0043_approval_fks.sql ==
-- Hardening P1 item 21: FK constraints for node_repositories and approvals.
--
-- Same table-rebuild pattern as migration 0040 (SQLite cannot ALTER-add
-- constraints). Orphan rows from pre-0043 databases stay in place — they are
-- surfaced by `count_orphan_rows` and the FKs backstop NEW writes.

-- ---- node_repositories: node + repository FKs ----
CREATE TABLE node_repositories_new (
    node_id         TEXT NOT NULL,
    repository_id   TEXT NOT NULL,
    local_path      TEXT,
    status          TEXT NOT NULL,
    last_synced_at  TEXT,
    PRIMARY KEY (node_id, repository_id),
    FOREIGN KEY (node_id) REFERENCES nodes (id) ON DELETE CASCADE,
    FOREIGN KEY (repository_id) REFERENCES repositories (id) ON DELETE CASCADE
);

INSERT INTO node_repositories_new (node_id, repository_id, local_path, status, last_synced_at)
SELECT node_id, repository_id, local_path, status, last_synced_at FROM node_repositories;

DROP TABLE node_repositories;
ALTER TABLE node_repositories_new RENAME TO node_repositories;

-- ---- approvals: task FK (attempt_id left unconstrained — approvals may be
-- created before/independent of a durable attempt row, e.g. legacy sessions) ----
CREATE TABLE approvals_new (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL,
    attempt_id  TEXT NOT NULL,
    session_id  TEXT,
    permission  TEXT NOT NULL,
    status      TEXT NOT NULL CHECK (status IN ('pending','allowed','denied','expired','cancelled')),
    reason      TEXT,
    created_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    decided_at  TEXT,
    audit       TEXT,
    scope       TEXT NOT NULL DEFAULT 'session',
    step_run_id TEXT,
    FOREIGN KEY (task_id) REFERENCES tasks (id) ON DELETE CASCADE
);

INSERT INTO approvals_new (id, task_id, attempt_id, session_id, permission, status, reason, created_at, expires_at, decided_at, audit, scope, step_run_id)
SELECT id, task_id, attempt_id, session_id, permission, status, reason, created_at, expires_at, decided_at, audit, scope, step_run_id FROM approvals;

DROP TABLE approvals;
ALTER TABLE approvals_new RENAME TO approvals;
CREATE INDEX IF NOT EXISTS idx_approvals_task ON approvals(task_id);
CREATE INDEX IF NOT EXISTS idx_approvals_status ON approvals(status);
CREATE INDEX IF NOT EXISTS idx_approvals_step ON approvals(step_run_id);


-- == 0044_outbox_metrics.sql ==
-- Hardening P0 item 10: detailed outbox metrics columns for nodes table.
--
-- These columns are populated from the node heartbeat and surfaced via
-- the /metrics endpoint so operators can observe outbox backlog, staleness,
-- and corruption without SSH access.

ALTER TABLE nodes ADD COLUMN outbox_rows INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN outbox_oldest_pending_age_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN outbox_corruption_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN outbox_completion_rows INTEGER NOT NULL DEFAULT 0;

-- == 0045_workflow_fks.sql ==
-- Hardening P1 item 21: foreign keys for workflow tables.
--
-- SQLite cannot ALTER a table to add constraints, so the tables are rebuilt
-- (create-new → copy → drop → rename). Fresh / reconciled databases have no
-- orphan rows, and the FKs backstop NEW writes.

-- ---- workflow_runs: template_id FK ----
-- Includes repository (migration 0013), base_commit (migration 0015),
-- plan (migration 0027), message_sequence (migration 0028) columns.
CREATE TABLE workflow_runs_new (
    id              TEXT PRIMARY KEY,
    template_id     TEXT NOT NULL,
    status          TEXT NOT NULL,
    context         TEXT,
    repository      TEXT,
    base_commit     TEXT,
    plan            TEXT,
    message_sequence INTEGER NOT NULL DEFAULT 0,
    created_at      TEXT NOT NULL,
    finished_at     TEXT,
    FOREIGN KEY (template_id) REFERENCES workflow_templates (id) ON DELETE RESTRICT
);

INSERT INTO workflow_runs_new (id, template_id, status, context, repository, base_commit, plan, message_sequence, created_at, finished_at)
SELECT id, template_id, status, context, repository, base_commit, plan, message_sequence, created_at, finished_at FROM workflow_runs;

DROP TABLE workflow_runs;
ALTER TABLE workflow_runs_new RENAME TO workflow_runs;
CREATE INDEX IF NOT EXISTS idx_workflow_runs_template ON workflow_runs (template_id);

-- ---- workflow_steps: run_id FK ----
-- Includes all columns added by migrations 0014, 0015, 0027, 0030
CREATE TABLE workflow_steps_new (
    id           TEXT PRIMARY KEY,
    run_id       TEXT NOT NULL,
    step_id      TEXT NOT NULL,
    prompt       TEXT NOT NULL,
    depends_on   TEXT NOT NULL DEFAULT '[]',
    role         TEXT NOT NULL,
    adapter      TEXT,
    status       TEXT NOT NULL DEFAULT 'pending',
    requested_node_id TEXT,
    base_commit  TEXT,
    retryable    INTEGER,
    max_attempts INTEGER,
    attempts     INTEGER NOT NULL DEFAULT 0,
    expandable   INTEGER,
    started_at   TEXT,
    finished_at  TEXT,
    created_at   TEXT NOT NULL,
    FOREIGN KEY (run_id) REFERENCES workflow_runs (id) ON DELETE CASCADE
);

INSERT INTO workflow_steps_new (id, run_id, step_id, prompt, depends_on, role, adapter, status, requested_node_id, base_commit, retryable, max_attempts, attempts, expandable, started_at, finished_at, created_at)
SELECT id, run_id, step_id, prompt, depends_on, role, adapter, status, requested_node_id, base_commit, retryable, max_attempts, attempts, expandable, started_at, finished_at, created_at FROM workflow_steps;

DROP TABLE workflow_steps;
ALTER TABLE workflow_steps_new RENAME TO workflow_steps;
CREATE INDEX IF NOT EXISTS idx_workflow_steps_run ON workflow_steps (run_id);
-- Hardening P1 item 21: ensure (run_id, step_id) is unique for FK references
CREATE UNIQUE INDEX IF NOT EXISTS ux_workflow_steps_run_step ON workflow_steps (run_id, step_id);

-- ---- role_runs: step_run_id FK ----
CREATE TABLE role_runs_new (
    id           TEXT PRIMARY KEY,
    step_run_id  TEXT NOT NULL,
    role         TEXT NOT NULL,
    task_id      TEXT,
    status       TEXT NOT NULL DEFAULT 'pending',
    created_at   TEXT NOT NULL,
    FOREIGN KEY (step_run_id) REFERENCES workflow_steps (id) ON DELETE CASCADE
);

INSERT INTO role_runs_new (id, step_run_id, role, task_id, status, created_at)
SELECT id, step_run_id, role, task_id, status, created_at FROM role_runs;

DROP TABLE role_runs;
ALTER TABLE role_runs_new RENAME TO role_runs;
CREATE INDEX IF NOT EXISTS idx_role_runs_step ON role_runs (step_run_id);

-- ---- workflow_messages: run_id and from_step_id FKs ----
-- Note: FK on from_step_id references workflow_steps.step_id (template step ID)
-- combined with run_id for uniqueness. The composite unique index above ensures
-- (run_id, step_id) is unique. A simple FK on from_step_id alone would not be
-- sufficient across runs, so application logic enforces referential integrity.
CREATE TABLE workflow_messages_new (
    id           TEXT PRIMARY KEY,
    run_id       TEXT NOT NULL,
    from_step_id TEXT NOT NULL,
    to_step_id   TEXT NOT NULL,
    kind         TEXT NOT NULL,
    payload      TEXT NOT NULL,
    sequence     INTEGER NOT NULL,
    created_at   TEXT NOT NULL,
    FOREIGN KEY (run_id) REFERENCES workflow_runs (id) ON DELETE CASCADE
);

INSERT INTO workflow_messages_new (id, run_id, from_step_id, to_step_id, kind, payload, sequence, created_at)
SELECT id, run_id, from_step_id, to_step_id, kind, payload, sequence, created_at FROM workflow_messages;

DROP TABLE workflow_messages;
ALTER TABLE workflow_messages_new RENAME TO workflow_messages;
CREATE INDEX IF NOT EXISTS idx_workflow_messages_run ON workflow_messages (run_id, sequence);


-- == 0046_node_observability_metrics.sql ==
-- Hardening P2 item 35: observability metrics for validation and repo lock.
--
--  * nodes.repo_lock_wait_ms — cumulative repository-lock wait in ms measured
--    on the node (cross-process flock contention), reported each heartbeat.
--  * attempts.validated_at — when the attempt entered the `validating` state
--    (begin_validate), so the control plane can compute validation duration
--    (finished_at - validated_at) and validation outcome in the same place it
--    already persists completion.

ALTER TABLE nodes ADD COLUMN repo_lock_wait_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE attempts ADD COLUMN validated_at TEXT;


-- == 0047_sandbox_backend.sql ==
-- Hardening P2 item 35: sandbox backend and enforced limits per node.
--
--  * nodes.sandbox_backend — the sandbox backend kind ("none" | "docker"),
--    reported each heartbeat.
--  * nodes.enforced_limits — whether the sandbox backend enforces resource
--    limits (memory, CPU, pids).

ALTER TABLE nodes ADD COLUMN sandbox_backend TEXT NOT NULL DEFAULT 'none';
ALTER TABLE nodes ADD COLUMN enforced_limits INTEGER NOT NULL DEFAULT 0;

-- == 0048_task_security_profile.sql ==
-- Hardening P0 item 5: task security profile enforcement.
-- Add security_profile column to tasks to enforce strict profiles on nodes
-- with structured permission interception.

ALTER TABLE tasks ADD COLUMN security_profile TEXT;

CREATE INDEX IF NOT EXISTS idx_tasks_security_profile ON tasks (security_profile);

-- == 0049_node_network_mode.sql ==
-- Hardening P2 item 809: add network_mode column to nodes table
-- Values: "none" | "restricted" | "unrestricted"

ALTER TABLE nodes ADD COLUMN network_mode TEXT NOT NULL DEFAULT 'none';

-- == 0050_repo_workspace_quota.sql ==
-- Hardening P2 item 35: repository cache and workspace quota metrics.
-- Add repo_cache_bytes and workspace_bytes columns to nodes table.

ALTER TABLE nodes ADD COLUMN repo_cache_bytes INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN workspace_bytes INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_nodes_repo_cache_bytes ON nodes (repo_cache_bytes);
CREATE INDEX IF NOT EXISTS idx_nodes_workspace_bytes ON nodes (workspace_bytes);

-- == 0051_task_network_mode.sql ==
-- Hardening P2 item 659: task-level network mode.
-- Add network_mode column to tasks table.
-- Values: "none" | "restricted" | "unrestricted" (NULL = default "none")

ALTER TABLE tasks ADD COLUMN network_mode TEXT;

CREATE INDEX IF NOT EXISTS idx_tasks_network_mode ON tasks (network_mode);
