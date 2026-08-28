-- Competitor-gap feature (task-level auto-retry, hatchet-inspired):
-- total attempts allowed for a task. 1 = single attempt (no auto-retry).
-- When an attempt fails and fewer than max_attempts attempts have run, the
-- control plane re-queues the task automatically (same as manual retry).
ALTER TABLE tasks ADD COLUMN max_attempts INTEGER NOT NULL DEFAULT 1;
