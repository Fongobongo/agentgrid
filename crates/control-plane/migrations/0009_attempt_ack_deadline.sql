-- Stage 1.3: explicit assignment acknowledgement. An attempt has a separate
-- ack_deadline; if the node never acks (crashes before starting), the
-- assignment is reverted and the task returns to the queue.
ALTER TABLE attempts ADD COLUMN ack_deadline TEXT;
