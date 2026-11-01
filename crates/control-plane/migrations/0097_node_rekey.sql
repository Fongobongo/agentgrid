-- ADR 0028 (reduced phase 3): a re-key token is bound to an existing node.
--
-- A plain enrollment token creates a node; a re-key token replaces the
-- credential of the node named here and leaves everything else alone — the id,
-- the task history, and the X25519 key its adapter-env secrets are sealed to.
-- That last part is why this exists: before it, the only answer to a leaked
-- `credential.json` was revoke -> re-enroll -> a NEW node id, which also drops
-- every ciphertext in `adapter_env_sealed` and forces every secret to be
-- re-entered by hand.
--
-- Nullable on purpose: every token minted before this migration, and every
-- plain one after it, has no node and creates one. The column is not a foreign
-- key with a cascade because a token for a node that has since been deleted
-- must simply stop working, not delete itself quietly.

ALTER TABLE enrollment_tokens ADD COLUMN node_id TEXT;
