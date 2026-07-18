-- agentgrid control-plane schema (Stage 7: workflow engine)
-- Workflow = a DAG of steps. A run instantiates the template's steps and, for
-- each step, one role-run for the step's declared role (multi-role fan-out is
-- later). Dependencies drive execution order; the scheduler starts a step once
-- all of its dependencies have succeeded.

CREATE TABLE IF NOT EXISTS workflow_templates (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    steps_json  TEXT NOT NULL,   -- JSON array of WorkflowStep
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS workflow_runs (
    id           TEXT PRIMARY KEY,
    template_id  TEXT NOT NULL,
    status       TEXT NOT NULL,
    context      TEXT,           -- optional shared JSON context
    created_at   TEXT NOT NULL,
    finished_at  TEXT
);

CREATE TABLE IF NOT EXISTS workflow_steps (
    id           TEXT PRIMARY KEY,   -- run-scoped instance id
    run_id       TEXT NOT NULL,
    step_id      TEXT NOT NULL,      -- template step id
    prompt       TEXT NOT NULL,
    depends_on   TEXT NOT NULL DEFAULT '[]',
    role         TEXT NOT NULL,
    adapter      TEXT,
    status       TEXT NOT NULL DEFAULT 'pending',
    created_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS role_runs (
    id           TEXT PRIMARY KEY,
    step_run_id  TEXT NOT NULL,
    role         TEXT NOT NULL,
    task_id      TEXT,
    status       TEXT NOT NULL DEFAULT 'pending',
    created_at   TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_workflow_runs_template ON workflow_runs (template_id);
CREATE INDEX IF NOT EXISTS idx_workflow_steps_run     ON workflow_steps (run_id);
CREATE INDEX IF NOT EXISTS idx_role_runs_step        ON role_runs (step_run_id);
