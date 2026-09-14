-- Plan 6.10 resource reservations: MemAvailable minus the operator-reserved
-- slice (node env AGENTGRID_RESERVED_MEM_MB, default 256 MiB). The scheduler
-- memory gate reads this column instead of raw mem_available_mb so the OS and
-- the fleet don't compete for the same bytes. 0 = not reported (legacy node /
-- cold start); the gate only applies once a value is known.
ALTER TABLE nodes ADD COLUMN free_memory_mb INTEGER NOT NULL DEFAULT 0;
