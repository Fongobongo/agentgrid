-- Hardening P2 item 659: task-level network mode.
-- Add network_mode column to tasks table.
-- Values: "none" | "restricted" | "unrestricted" (NULL = default "none")

ALTER TABLE tasks ADD COLUMN network_mode TEXT;

CREATE INDEX IF NOT EXISTS idx_tasks_network_mode ON tasks (network_mode);