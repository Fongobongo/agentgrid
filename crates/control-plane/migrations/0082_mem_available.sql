-- Latest MemAvailable (MiB) the node reported with its heartbeat.
-- 0 = not reported yet (legacy node / cold start); the scheduler memory
-- gate (AGENTGRID_MIN_FREE_MEM_MB) only applies when a value is known.
ALTER TABLE nodes ADD COLUMN mem_available_mb INTEGER NOT NULL DEFAULT 0;
