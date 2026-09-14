-- Plan 6.10 load gate: CPU count the node reported with its heartbeat.
-- Used to normalize load_avg into per-CPU pressure
-- (AGENTGRID_MAX_LOAD_PER_CPU). 0 = not reported (legacy node); the
-- scheduler falls back to 1 so a single-core host is never silently
-- flooded.
ALTER TABLE nodes ADD COLUMN cpu_count INTEGER NOT NULL DEFAULT 0;
