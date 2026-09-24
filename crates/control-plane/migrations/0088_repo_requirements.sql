-- Plan 6.9: structured repository requirements (OS, arch, tools with
-- version requirements, host memory/disk floors) as JSON. NULL =
-- unconstrained (legacy rows, plain tasks). The scheduler enforces the
-- fields it has node data for (memory_mb); the rest is eligibility
-- context the UI/CLI surface.
ALTER TABLE repositories ADD COLUMN requirements TEXT;
