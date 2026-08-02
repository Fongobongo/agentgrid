-- Hardening P2 item 35: surface node-local storage pressure to operators.
--
-- `outbox_bytes` = total bytes buffered in the node's durable event/completion
-- outbox (pending delivery to the CP); `artifact_spool_bytes` = bytes staged
-- in the node's artifact spool (uploaded artifacts not yet acked). Reported by
-- the heartbeat so `ag nodes`/the web UI show nodes that are backing up.
ALTER TABLE nodes ADD COLUMN outbox_bytes INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN artifact_spool_bytes INTEGER NOT NULL DEFAULT 0;
