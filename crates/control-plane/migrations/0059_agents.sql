-- Plan 2.1 (#18): org layer — long-lived agent identities with roles,
-- budgets and heartbeats. Tasks are workload attributed to an agent; the
-- scheduler hard-stops an agent whose budget is exhausted; heartbeats spawn
-- scheduled autonomous work.
CREATE TABLE IF NOT EXISTS agents (
    id                     TEXT PRIMARY KEY,
    name                   TEXT NOT NULL UNIQUE,
    role                   TEXT NOT NULL DEFAULT 'worker',
    prompt                 TEXT NOT NULL DEFAULT '',
    skills_json            TEXT NOT NULL DEFAULT '[]',
    budget_usd             REAL NOT NULL DEFAULT 0,
    max_tasks              INTEGER,          -- NULL = unlimited
    heartbeat_interval_secs INTEGER,          -- NULL = no scheduled heartbeat
    last_heartbeat_at      TEXT,
    created_at             TEXT NOT NULL
);

-- Immutable trail of agent lifecycle events (plan 2.1 #18): task creation
-- attributed to the agent, budget rejections, heartbeat fires. Rows are
-- append-only; nothing in the app updates or deletes them.
CREATE TABLE IF NOT EXISTS agent_actions (
    id          TEXT PRIMARY KEY,
    agent_id    TEXT NOT NULL REFERENCES agents(id),
    action      TEXT NOT NULL,
    detail      TEXT NOT NULL DEFAULT '',
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_agent_actions_agent_created
    ON agent_actions (agent_id, created_at);

-- Attribute tasks to an agent (NULL = unmanaged task).
ALTER TABLE tasks ADD COLUMN agent_id TEXT;
CREATE INDEX IF NOT EXISTS idx_tasks_agent_id ON tasks (agent_id);
