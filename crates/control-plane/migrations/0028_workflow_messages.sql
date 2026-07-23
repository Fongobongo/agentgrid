-- Stage 13 typed AgentMessage mailbox: orchestrator-mediated messages between
-- workflow steps (not free-form P2P). A step publishes a typed message
-- (Stage 13 MVP: an `output` summary emitted automatically when a step
-- succeeds); downstream steps consume them on activation (the orchestrator
-- renders them into the consuming task's prompt). Only the orchestrator
-- writes rows; agents never insert directly. Keeps the loop within the
-- `WorkflowBudget.max_messages` ceiling observable.
ALTER TABLE workflow_runs ADD COLUMN message_sequence INTEGER NOT NULL DEFAULT 0;

CREATE TABLE workflow_messages (
    id          TEXT PRIMARY KEY,
    run_id      TEXT NOT NULL,
    from_step_id TEXT NOT NULL,
    -- to_step_id = '*' broadcasts to all downstream steps; otherwise pinned.
    to_step_id  TEXT NOT NULL,
    -- one of: output / plan / note
    kind        TEXT NOT NULL,
    -- structured payload (JSON object: {summary, commit_sha?}; the orchestrator
    -- emits a compact summary, never a full transcript).
    payload     TEXT NOT NULL,
    sequence    INTEGER NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE INDEX workflow_messages_run ON workflow_messages (run_id, sequence);
