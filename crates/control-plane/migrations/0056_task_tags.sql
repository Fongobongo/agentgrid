-- Plan 1.3 (#13): task tags — simple label grouping for the web UI and CLI.
-- A task can have many tags; a tag can be on many tasks. No ordering, no
-- hierarchy — just a join table with a UNIQUE guard.

CREATE TABLE IF NOT EXISTS task_tags (
    task_id TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
    tag     TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (task_id, tag)
);

CREATE INDEX IF NOT EXISTS idx_task_tags_tag ON task_tags (tag);
