-- Plan 2.8 (#19): repo-learnings store.
-- Rows are short factual statements a human (or the agent pipeline) has
-- recorded about a repository. `approved = 0` means the learning exists but
-- is not yet eligible for prompt injection — a reviewer ticks it to 1 via
-- `ag learn approve` once the statement has been verified.

CREATE TABLE IF NOT EXISTS repo_learnings (
    id              TEXT PRIMARY KEY,
    repository      TEXT NOT NULL,
    statement       TEXT NOT NULL,
    confidence      REAL NOT NULL DEFAULT 0.5,
    source_attempt_id TEXT,
    approved        INTEGER NOT NULL DEFAULT 0,  -- 0 = pending, 1 = human-reviewed
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

-- Hot path: fetch top-N approved for a repo ordered by confidence DESC.
CREATE INDEX IF NOT EXISTS idx_repo_learnings_repo_approved_conf
    ON repo_learnings(repository, approved, confidence DESC, updated_at DESC);
