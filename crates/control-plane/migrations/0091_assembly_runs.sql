-- One-button assembly (`POST /v1/assembly`): runs owning an ephemeral
-- architect template. `assembly` gates the system-appended collector step on
-- plan approval; `adapter` is the default for expanded steps that omit one
-- (NULL = legacy runs, activation falls back to "mock" as before).
ALTER TABLE workflow_runs ADD COLUMN assembly INTEGER NOT NULL DEFAULT 0;
ALTER TABLE workflow_runs ADD COLUMN adapter TEXT;
