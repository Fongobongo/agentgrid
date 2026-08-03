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
