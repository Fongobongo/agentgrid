-- ADR 0028 phase 1B: adapter-env values are sealed to a per-node X25519 key.
--
-- The control plane keeps a fingerprint (answers "did I paste the right
-- thing?") and one ciphertext per node, and no plaintext at all. A copy of
-- this database, or the memory of a compromised control plane, yields
-- ciphertext: the CP holds no private key and the node credential is stored
-- as a sha256 hash, so nothing on this side can reconstruct one.
--
-- `value` is renamed rather than dropped: the first boot after this migration
-- still has the plaintext, seals it for every node that has a key, and blanks
-- the column. Nothing writes to `value_legacy` afterwards, and the name is the
-- reminder for anyone reading the table later.
ALTER TABLE adapter_env RENAME COLUMN value TO value_legacy;

-- `sha256(value)[..8]` and the value's length: enough to recognise a stored
-- secret, not enough to recover one.
ALTER TABLE adapter_env ADD COLUMN fingerprint TEXT NOT NULL DEFAULT '';
ALTER TABLE adapter_env ADD COLUMN value_len INTEGER NOT NULL DEFAULT 0;

-- The node's X25519 public key. NULL until a node announces one — new nodes at
-- enroll, existing ones on their next heartbeat.
ALTER TABLE nodes ADD COLUMN public_key TEXT;

-- One ciphertext per (entry, node). Global entries have a row per node; a
-- node-scoped entry has exactly one. No row for a node means "not delivered
-- yet", which is what `ag adapter-env ls` reports as pending.
CREATE TABLE IF NOT EXISTS adapter_env_sealed (
    entry_id INTEGER NOT NULL REFERENCES adapter_env(id) ON DELETE CASCADE,
    node_id TEXT NOT NULL,
    sealed TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    PRIMARY KEY (entry_id, node_id)
);

CREATE INDEX IF NOT EXISTS idx_adapter_env_sealed_node
    ON adapter_env_sealed (node_id);
