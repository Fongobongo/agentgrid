-- Hardening P0 item 5: surface unsafe-node state to operators.
--
-- `unsafe_active` = the node is running an adapter with the unattended
-- permission bypass active (no sandbox); `permission_interception` is the
-- best-available interception across its adapters (structured | wrapper | none).
ALTER TABLE nodes ADD COLUMN unsafe_active INTEGER NOT NULL DEFAULT 0;
ALTER TABLE nodes ADD COLUMN permission_interception TEXT NOT NULL DEFAULT 'wrapper';
