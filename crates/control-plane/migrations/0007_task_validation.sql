-- Stage 4.3: optional per-task validation command overriding the
-- repository default at assignment time.
ALTER TABLE tasks ADD COLUMN validation_command TEXT;
