-- Plan 1.12 (#7): shared context / memory between parallel attempts of the
-- same logical task group. Flat key→value notes scoped to a task_group_id
-- (a free-form string the caller chooses); no RAG, no embeddings. Lookup is
-- by (group, key) PK — O(1), per-group KEY-ONLY scan is the covered index on
-- the composite PK's left-most column.
CREATE TABLE IF NOT EXISTS shared_context (
    task_group_id TEXT NOT NULL,
    key           TEXT NOT NULL,
    value         TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    PRIMARY KEY (task_group_id, key)
);

-- Optional grouping for parallel attempts that should share context/memory.
-- NULL => standalone task (no shared-context read/write). The group_id is a
-- free-form, validated, non-empty string; it is forwarded to the assigned
-- node as the AG_GROUP_ID env var (Plan 1.12).
ALTER TABLE tasks ADD COLUMN group_id TEXT;
