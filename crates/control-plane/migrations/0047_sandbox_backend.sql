-- Hardening P2 item 35: sandbox backend and enforced limits per node.
--
--  * nodes.sandbox_backend — the sandbox backend kind ("none" | "docker"),
--    reported each heartbeat.
--  * nodes.enforced_limits — whether the sandbox backend enforces resource
--    limits (memory, CPU, pids).

ALTER TABLE nodes ADD COLUMN sandbox_backend TEXT NOT NULL DEFAULT 'none';
ALTER TABLE nodes ADD COLUMN enforced_limits INTEGER NOT NULL DEFAULT 0;