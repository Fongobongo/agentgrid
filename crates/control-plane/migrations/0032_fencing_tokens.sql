-- Hardening P0 item 8: fencing tokens.
-- Each attempt has a monotonic fencing token (mac/uuid) generated at
-- assignment; node->CP mutations carry it and the CP rejects a stale token
-- (e.g. a node reporting for an attempt that was reassigned/lost) with 409.
-- Default '' preserves the N-1 nodes that never send a token (they are still
-- accepted under the N/N-1 policy until they upgrade).
ALTER TABLE attempts ADD COLUMN fencing_token TEXT NOT NULL DEFAULT '';
ALTER TABLE nodes ADD COLUMN lease_generation INTEGER NOT NULL DEFAULT 0;