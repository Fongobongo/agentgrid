-- Client-supplied idempotency keys for task creation. A retry after a lost
-- response must return the original task instead of starting a second agent
-- run. Keys are stored as SHA-256 digests of (user, key) so a key is never
-- persisted verbatim, and the PRIMARY KEY enforces one task per key even if
-- two requests race.
CREATE TABLE IF NOT EXISTS idempotency (
    key TEXT PRIMARY KEY,
    task_id TEXT NOT NULL,
    created_at TEXT NOT NULL
);
