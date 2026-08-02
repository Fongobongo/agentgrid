-- Hardening P0 item 9: global monotonic event cursor.
--
-- task_events previously carried only a per-attempt `sequence` (restarts at
-- 1 for every attempt), so a client resuming after a retry could not order
-- events across attempts and the SSE `id:`/`Last-Event-ID` cursor was
-- ambiguous. This migration adds a global, monotonically increasing
-- `ingest_id` allocated from a dedicated single-row counter table inside the
-- ingest transaction, plus a unique index so every read path can resume on a
-- global cursor.
--
-- Idempotency of ingestion is unchanged: dedup stays on
-- `(attempt_id, sequence)` via `ON CONFLICT DO NOTHING`; `ingest_id` is only
-- required to be monotonic (gaps are fine — a duplicate redelivery consumes a
-- counter value but lands nowhere).

ALTER TABLE task_events ADD COLUMN ingest_id INTEGER NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS event_ingest_counter (
    id       INTEGER PRIMARY KEY CHECK (id = 1),
    next_val INTEGER NOT NULL DEFAULT 1
);

INSERT OR IGNORE INTO event_ingest_counter (id, next_val) VALUES (1, 1);

-- Backfill pre-existing rows. SQLite `rowid` is monotonic per insert order,
-- which is a good-enough approximation for historical data so old events
-- remain orderable and resumable.
UPDATE task_events SET ingest_id = rowid WHERE ingest_id = 0;

CREATE UNIQUE INDEX IF NOT EXISTS ux_events_ingest ON task_events (ingest_id);
