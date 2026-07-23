-- Stage 13 plan expansion:
--   * attempts.plan TEXT  — optional machine-readable plan (YAML/JSON) emitted
--     by an expandable architect step; the run pauses in `PlanReady` and the
--     plan is expanded into new steps on approval.
--   * workflow_steps.expandable INTEGER NULL — mirrors WorkflowStep.expandable
--     (1 = the architect step produces a plan; 0/NULL = plain step).
--   * workflow_runs.plan TEXT NULL — the pending plan awaiting approval (copied
--     from the architect's winning attempt so the run can outlive the attempt).
ALTER TABLE attempts ADD COLUMN plan TEXT;

ALTER TABLE workflow_steps ADD COLUMN expandable INTEGER;

ALTER TABLE workflow_runs ADD COLUMN plan TEXT;
