-- Stage 5: durable approval flow (prerequisite for ACP session/request_permission).
CREATE TABLE approvals (
    id          TEXT PRIMARY KEY,
    task_id     TEXT NOT NULL,
    attempt_id  TEXT NOT NULL,
    session_id  TEXT,
    permission  TEXT NOT NULL,
    status      TEXT NOT NULL,                 -- pending|allowed|denied|expired|cancelled
    reason      TEXT,
    created_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    decided_at  TEXT,
    audit       TEXT                          -- who/what decided (JSON)
);

CREATE INDEX idx_approvals_task ON approvals(task_id);
CREATE INDEX idx_approvals_status ON approvals(status);
