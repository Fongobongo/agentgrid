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
