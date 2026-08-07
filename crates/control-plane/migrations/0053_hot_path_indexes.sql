-- Plan 0.3 item 1.3: hot-path indexes for the read path.
-- attempts by node: task list's node filter and per-node attempt lookups.
CREATE INDEX IF NOT EXISTS idx_attempts_node ON attempts (node_id);
-- latest-attempt lookup used by the task list projection
-- (correlated subquery ORDER BY number DESC LIMIT 1).
CREATE INDEX IF NOT EXISTS idx_attempts_task_number ON attempts (task_id, number DESC);
-- scheduler candidate scan orders queued tasks by created_at; task list
-- filters by status and pages by created_at.
CREATE INDEX IF NOT EXISTS idx_tasks_status_created ON tasks (status, created_at);
