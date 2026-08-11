-- Plan 2.9 (#20): consensus runs. A single user request can fan out to N
-- adapters as a vote; member tasks share a consensus_group_id, and each
-- member's adapter name lands in consensus_member so the reviewer sees
-- which model produced which patch.

ALTER TABLE tasks ADD COLUMN consensus_group_id TEXT;
ALTER TABLE tasks ADD COLUMN consensus_member TEXT;

CREATE INDEX IF NOT EXISTS idx_tasks_consensus_group
    ON tasks(consensus_group_id)
    WHERE consensus_group_id IS NOT NULL;
