-- Hardening P2 item 35: observability metrics for validation and repo lock.
--
--  * nodes.repo_lock_wait_ms — cumulative repository-lock wait in ms measured
--    on the node (cross-process flock contention), reported each heartbeat.
--  * attempts.validated_at — when the attempt entered the `validating` state
--    (begin_validate), so the control plane can compute validation duration
--    (finished_at - validated_at) and validation outcome in the same place it
--    already persists completion.

ALTER TABLE nodes ADD COLUMN repo_lock_wait_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE attempts ADD COLUMN validated_at TEXT;
