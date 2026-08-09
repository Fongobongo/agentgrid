-- Plan 1.6 (#3b): inline plan/diff annotations. Reviewers leave per-region
-- comments on an attempt's diff/plan; "send for rework" aggregates them into a
-- new task prompt so the agent takes the feedback in a retry.
CREATE TABLE IF NOT EXISTS patch_annotations (
    id          TEXT PRIMARY KEY,
    attempt_id  TEXT NOT NULL REFERENCES attempts(id) ON DELETE CASCADE,
    file        TEXT NOT NULL,            -- file path ("" = whole-patch/plan)
    line_start  INTEGER,                 -- 1-based; NULL = whole file
    line_end    INTEGER,                 -- inclusive; NULL/0 = single line
    comment     TEXT NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE INDEX IF NOT EXISTS idx_patch_annotations_attempt
    ON patch_annotations(attempt_id);
