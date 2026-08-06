-- Hardening P0 item 5: task security profile enforcement.
-- Add security_profile column to tasks to enforce strict profiles on nodes
-- with structured permission interception.

ALTER TABLE tasks ADD COLUMN security_profile TEXT;

CREATE INDEX IF NOT EXISTS idx_tasks_security_profile ON tasks (security_profile);