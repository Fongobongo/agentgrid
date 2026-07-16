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
