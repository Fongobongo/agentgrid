-- Plan 2.9 (#20): consensus aggregation writes human-review approval rows
-- not pinned to a specific attempt_id (the approval sits on the task, the
-- operator picks a winning patch). Pre-0062 schema required attempt_id
-- NOT NULL; relax it.

PRAGMA foreign_keys = OFF;
CREATE TABLE approvals_new (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL,
    attempt_id  TEXT,
    session_id  TEXT,
    permission  TEXT NOT NULL,
    status      TEXT NOT NULL,
    reason      TEXT,
    created_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    decided_at  TEXT,
    audit       TEXT,
    step_run_id TEXT,
    scope       TEXT
);
INSERT INTO approvals_new (id, task_id, attempt_id, session_id, permission, status, reason, created_at, expires_at, decided_at, audit, step_run_id, scope)
SELECT id, task_id, attempt_id, session_id, permission, status, reason, created_at, expires_at, decided_at, audit, step_run_id, scope FROM approvals;
DROP TABLE approvals;
ALTER TABLE approvals_new RENAME TO approvals;
CREATE INDEX IF NOT EXISTS idx_approvals_task ON approvals(task_id);
CREATE INDEX IF NOT EXISTS idx_approvals_status ON approvals(status);
PRAGMA foreign_keys = ON;
