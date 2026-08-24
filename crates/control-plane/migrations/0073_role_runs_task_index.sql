-- Audit follow-up: role_runs.task_id is the probe column for several hot
-- lookups (scheduler role resolution, unacked assignments, workflow run
-- projection, upstream artifact access, per-completion workflow routing), but
-- only idx_role_runs_step (step_run_id) existed — every query was a full scan
-- of the workflow history. Index the task side too.
CREATE INDEX IF NOT EXISTS idx_role_runs_task ON role_runs (task_id);
