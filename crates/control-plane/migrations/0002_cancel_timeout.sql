-- Stage 2.7: cancellation + per-task timeout support.
ALTER TABLE attempts ADD COLUMN cancel_requested INTEGER NOT NULL DEFAULT 0;
ALTER TABLE tasks ADD COLUMN timeout_secs INTEGER NOT NULL DEFAULT 3600;
