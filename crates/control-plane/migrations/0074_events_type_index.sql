-- Audit follow-up: the workflow budget tick (every 5s per running run)
-- summed metric task_events joined across the whole run and scanned all
-- workflow_messages for the handoff streak — both grow with run lifetime.
-- A type filter lets SQLite drive from a small `metric` subset instead of
-- every event row of the run's attempts.
CREATE INDEX IF NOT EXISTS idx_events_type ON task_events (type);
