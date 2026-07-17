-- Stage 1.1: surface the distinct failure category on the task itself so the
-- UI/CLI can show WHY a task failed (validation/timeout/...) without joining
-- the attempt that produced it. NULL when the task succeeded or was cleanly
-- cancelled.
ALTER TABLE tasks ADD COLUMN error_code TEXT;
