-- Hardening P2 item 37: node drain support.
--
-- A drained node stops receiving NEW task assignments but keeps running its
-- in-flight attempts (heartbeat stays online). The scheduler skips drained
-- nodes in `try_assign`; operators can drain before maintenance and undrain
-- afterwards. No new status variant — `drained` is an orthogonal flag so the
-- existing status machine is untouched.
ALTER TABLE nodes ADD COLUMN drained INTEGER NOT NULL DEFAULT 0;
