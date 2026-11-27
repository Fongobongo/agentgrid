-- An idempotency key must not silently return a different request's task.
-- Existing rows predate the fingerprint; the empty default makes a replay
-- with any real body a conflict, which is the safe answer.
ALTER TABLE idempotency ADD COLUMN request_hash TEXT NOT NULL DEFAULT '';
