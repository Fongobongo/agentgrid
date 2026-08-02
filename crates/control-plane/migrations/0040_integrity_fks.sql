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
