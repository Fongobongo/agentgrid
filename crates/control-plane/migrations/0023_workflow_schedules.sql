-- Stage 13: scheduled/recurring workflow triggers. A schedule fires a
-- WorkflowRun of a template on a fixed interval. autonomy/budget constraints
-- are enforced at create (Stage 13 follow-up: the budget check; this lands
-- the schedule infra + interval + autonomy + enabled + last_run_at).
CREATE TABLE workflow_schedules (
    id               TEXT PRIMARY KEY,
    template_id      TEXT NOT NULL,
    interval_seconds INTEGER NOT NULL CHECK (interval_seconds >= 1),
    autonomy         TEXT NOT NULL DEFAULT 'l2',
    last_run_at      TEXT NOT NULL DEFAULT '',
    enabled          INTEGER NOT NULL DEFAULT 1,
    created_at       TEXT NOT NULL,
    FOREIGN KEY (template_id) REFERENCES workflow_templates (id) ON DELETE CASCADE
);
CREATE INDEX workflow_schedules_enabled ON workflow_schedules (enabled);
