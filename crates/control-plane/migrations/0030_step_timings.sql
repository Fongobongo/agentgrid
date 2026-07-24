-- Stage 11.6 follow-up: span waterfall (timeline by time). Steps need
-- started_at/finished_at so the web UI can position them on a time axis,
-- not just by dependency depth.
ALTER TABLE workflow_steps ADD COLUMN started_at  TEXT;
ALTER TABLE workflow_steps ADD COLUMN finished_at TEXT;
