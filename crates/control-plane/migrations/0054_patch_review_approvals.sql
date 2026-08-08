-- Stage: Patch review UI (competitor plan 1.1). A patch-review approval is
-- just an approvals row with scope='task_patch_review' — add a lookup index
-- so the UI/store can find the pending one for a task quickly.
CREATE INDEX IF NOT EXISTS idx_approvals_task_scope_status
    ON approvals(task_id, scope, status);
