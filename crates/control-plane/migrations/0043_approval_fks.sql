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
