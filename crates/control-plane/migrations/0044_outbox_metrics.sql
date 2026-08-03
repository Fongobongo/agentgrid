-- Hardening P0 item 10: detailed outbox metrics columns for nodes table.
--
-- These columns are populated from the node heartbeat and surfaced via
-- the /metrics endpoint so operators can observe outbox backlog, staleness,
-- and corruption without SSH access.

ALTER TABLE nodes ADD COLUMN outbox_rows INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN outbox_oldest_pending_age_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN outbox_corruption_count INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN outbox_completion_rows INTEGER NOT NULL DEFAULT 0;