-- Project assembly run options: skip the PlanReady human approval
-- (auto-approve a parseable architect plan on the next tick) and inherit a
-- GitHub write-back target into collector steps. Defaults keep legacy runs
-- unchanged (manual approval, no write-back).
ALTER TABLE workflow_runs ADD COLUMN auto_approve_plan INTEGER NOT NULL DEFAULT 0;
ALTER TABLE workflow_runs ADD COLUMN github_push INTEGER NOT NULL DEFAULT 0;
ALTER TABLE workflow_runs ADD COLUMN github_repo TEXT;
ALTER TABLE workflow_runs ADD COLUMN github_issue INTEGER;
ALTER TABLE workflow_runs ADD COLUMN github_create INTEGER NOT NULL DEFAULT 0;
