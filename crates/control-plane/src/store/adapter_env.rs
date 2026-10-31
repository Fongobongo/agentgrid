//! CP-managed adapter environment, sealed per node (ADR 0028).
//!
//! The control plane stores a **fingerprint** and one ciphertext per node, and
//! never the plaintext. `adapter = "*"` applies to every adapter; `node_id`
//! NULL = global, node-scoped entries are appended after global ones. A
//! node-explicit `AGENTGRID_ADAPTER_ENV` still wins on key collision.
//!
//! Delivery is pull: `adapter_env_for` returns the sealed blobs for the node
//! asking, and the node opens them with a private key that never leaves the
//! host. A node that has not announced a public key yet gets nothing — the
//! entry shows up as `pending` in the list instead of being delivered as
//! something the node cannot read.

use std::collections::BTreeMap;

use agentgrid_common::seal::{self, entry_aad};
use agentgrid_common::SealedAdapterEnvEntry;
use anyhow::Result;
use serde::Serialize;

use super::Store;

/// What the read path is allowed to know about a stored secret.
///
/// Deliberately **not** a row type: there is no field that can hold the value,
/// so a future handler cannot serialize one by accident. `fingerprint` is
/// `sha256(value)[..8]` — enough to answer "is the key I just pasted the one
/// that is stored?" and not enough to recover anything.
#[derive(Debug, Clone, Serialize)]
pub struct AdapterEnvView {
    pub id: i64,
    pub adapter: String,
    pub key: String,
    pub fingerprint: String,
    pub value_len: i64,
    pub node_id: Option<String>,
    pub created_at: String,
    /// Nodes holding a ciphertext for this entry.
    pub sealed_for: Vec<String>,
    /// Nodes this entry is meant for that cannot receive it yet (no public
    /// key announced). Empty is the healthy state.
    pub pending: Vec<String>,
}

/// Result of a write: which nodes got a ciphertext, and which could not.
#[derive(Debug, Clone, Serialize)]
pub struct UpsertOutcome {
    pub id: i64,
    pub sealed_for: Vec<String>,
    pub pending: Vec<String>,
}

/// Short, non-reversible identifier for a stored secret.
pub fn fingerprint(value: &str) -> String {
    agentgrid_common::sha256_hex(value.as_bytes())[..8].to_string()
}

/// One `adapter_env` row as the list query returns it.
type ListRow = (i64, String, String, String, i64, Option<String>, String);

impl Store {
    /// Node ids with their announced public keys.
    async fn node_public_keys(&self) -> Result<BTreeMap<String, String>> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT id, public_key FROM nodes WHERE public_key IS NOT NULL")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows.into_iter().collect())
    }

    async fn all_node_ids(&self) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT id FROM nodes ORDER BY id")
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Effective entries for a node, as sealed blobs. Entries the node has no
    /// ciphertext for are simply absent — the node falls back to its local
    /// `AGENTGRID_ADAPTER_ENV`, which is the only correct behaviour.
    pub async fn adapter_env_for(&self, node_id: &str) -> Result<Vec<SealedAdapterEnvEntry>> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT e.adapter, e.key, s.sealed \
             FROM adapter_env e \
             JOIN adapter_env_sealed s ON s.entry_id = e.id \
             WHERE s.node_id = ? AND (e.node_id = '' OR e.node_id = ?) \
             ORDER BY (e.node_id = '') DESC, e.id",
        )
        .bind(node_id)
        .bind(node_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(adapter, key, sealed)| SealedAdapterEnvEntry {
                adapter,
                key,
                sealed,
            })
            .collect())
    }

    pub async fn list_adapter_env(&self) -> Result<Vec<AdapterEnvView>> {
        let rows: Vec<ListRow> = sqlx::query_as(
            // Audit X-C6: globals are stored as '' (see upsert), never NULL —
            // the old `node_id IS NULL` tied everywhere and the global-first
            // order was arbitrary. Matches `adapter_env_for`'s predicate.
            "SELECT id, adapter, key, fingerprint, value_len, NULLIF(node_id, '') AS node_id, \
                    created_at \
             FROM adapter_env ORDER BY (node_id = '') DESC, adapter, key",
        )
        .fetch_all(&self.pool)
        .await?;
        let sealed: Vec<(i64, String)> =
            sqlx::query_as("SELECT entry_id, node_id FROM adapter_env_sealed")
                .fetch_all(&self.pool)
                .await?;
        let mut by_entry: BTreeMap<i64, Vec<String>> = BTreeMap::new();
        for (entry_id, node_id) in sealed {
            by_entry.entry(entry_id).or_default().push(node_id);
        }
        let nodes = self.all_node_ids().await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, adapter, key, fingerprint, value_len, node_id, created_at)| {
                    let mut sealed_for = by_entry.remove(&id).unwrap_or_default();
                    sealed_for.sort();
                    let targets: Vec<String> = match &node_id {
                        Some(n) => vec![n.clone()],
                        None => nodes.clone(),
                    };
                    let pending = targets
                        .into_iter()
                        .filter(|n| !sealed_for.contains(n))
                        .collect();
                    AdapterEnvView {
                        id,
                        adapter,
                        key,
                        fingerprint,
                        value_len,
                        node_id,
                        created_at,
                        sealed_for,
                        pending,
                    }
                },
            )
            .collect())
    }

    /// Store a value: fingerprint it, seal it for every node that should get
    /// it, and keep no plaintext.
    ///
    /// The caller's `value` is dropped at the end of this function — it lives
    /// in the control plane's memory for the duration of one request and
    /// nowhere else.
    pub async fn upsert_adapter_env(
        &self,
        adapter: &str,
        key: &str,
        value: &str,
        node_id: Option<&str>,
    ) -> Result<UpsertOutcome> {
        let fp = fingerprint(value);
        let len = value.chars().count() as i64;
        let keys = self.node_public_keys().await?;
        let targets: Vec<String> = match node_id {
            Some(n) => vec![n.to_string()],
            None => self.all_node_ids().await?,
        };

        let mut tx = self.write_txn().await?;
        sqlx::query(
            "INSERT INTO adapter_env (adapter, key, value_legacy, node_id, fingerprint, value_len) \
             VALUES (?, ?, '', ?, ?, ?) \
             ON CONFLICT (adapter, key, node_id) DO UPDATE SET \
                 value_legacy = '', fingerprint = excluded.fingerprint, \
                 value_len = excluded.value_len",
        )
        .bind(adapter)
        .bind(key)
        .bind(node_id.unwrap_or(""))
        .bind(&fp)
        .bind(len)
        .execute(&mut *tx)
        .await?;
        let id: i64 = sqlx::query_scalar(
            "SELECT id FROM adapter_env WHERE adapter = ? AND key = ? AND node_id = ?",
        )
        .bind(adapter)
        .bind(key)
        .bind(node_id.unwrap_or(""))
        .fetch_one(&mut *tx)
        .await?;

        let mut sealed_for = Vec::new();
        let mut pending = Vec::new();
        for target in &targets {
            match keys.get(target) {
                Some(public) => {
                    let sealed = seal::seal(public, value, &entry_aad(target, adapter, key))?;
                    sqlx::query(
                        "INSERT INTO adapter_env_sealed (entry_id, node_id, sealed) VALUES (?, ?, ?) \
                         ON CONFLICT (entry_id, node_id) DO UPDATE SET \
                             sealed = excluded.sealed, \
                             created_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')",
                    )
                    .bind(id)
                    .bind(target)
                    .bind(&sealed)
                    .execute(&mut *tx)
                    .await?;
                    sealed_for.push(target.clone());
                }
                None => pending.push(target.clone()),
            }
        }
        tx.commit().await?;
        Ok(UpsertOutcome {
            id,
            sealed_for,
            pending,
        })
    }

    /// Remove an entry, and actually erase it.
    ///
    /// A plain `DELETE` frees the row but leaves the text in the page: measured
    /// on a real DB, the value was still `grep`-able in `cp.db` after
    /// `DELETE` + checkpoint, and only `VACUUM` removed it. Removal is the only
    /// revocation verb this API has, so it has to mean revocation:
    /// `secure_delete` zeroes the freed cells, and truncating the WAL drops the
    /// pre-delete image that would otherwise survive in a second file.
    pub async fn remove_adapter_env(&self, id: i64) -> Result<u64> {
        let mut conn = self.pool.acquire().await?;
        sqlx::query("PRAGMA secure_delete = ON")
            .execute(&mut *conn)
            .await?;
        // Foreign keys are not enabled pool-wide, so the ciphertexts go first.
        sqlx::query("DELETE FROM adapter_env_sealed WHERE entry_id = ?")
            .bind(id)
            .execute(&mut *conn)
            .await?;
        let removed = sqlx::query("DELETE FROM adapter_env WHERE id = ?")
            .bind(id)
            .execute(&mut *conn)
            .await?
            .rows_affected();
        // Best effort: a checkpoint failure must not turn a successful delete
        // into an error the operator would retry (and then 404 on).
        let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_all(&mut *conn)
            .await;
        Ok(removed)
    }

    /// Record (or replace) a node's public key.
    ///
    /// Returns true when the stored key changed. A **different** key means
    /// every ciphertext this node holds was sealed to a key it no longer has:
    /// those rows are dropped so the entries report as `pending` instead of
    /// looking delivered to a node that cannot open them.
    pub async fn set_node_public_key(&self, node_id: &str, public_key: &str) -> Result<bool> {
        let current: Option<Option<String>> =
            sqlx::query_scalar("SELECT public_key FROM nodes WHERE id = ?")
                .bind(node_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(current) = current else {
            // Unknown node: enrollment stores the key itself, so nothing to do.
            return Ok(false);
        };
        if current.as_deref() == Some(public_key) {
            return Ok(false);
        }
        // `current` being `Some(None)` means the node exists but never announced
        // a key, so there is nothing sealed to a *previous* key to drop.
        let had_key = current.as_deref().is_some();
        let mut tx = self.write_txn().await?;
        sqlx::query("UPDATE nodes SET public_key = ? WHERE id = ?")
            .bind(public_key)
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
        if had_key {
            let dropped = sqlx::query("DELETE FROM adapter_env_sealed WHERE node_id = ?")
                .bind(node_id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            tracing::warn!(
                node = %node_id,
                dropped,
                "node announced a different public key; its sealed secrets must be re-entered"
            );
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Seal adapter-env values that are still stored in the clear.
    ///
    /// Runs once after the schema migration that renamed `value` to
    /// `value_legacy`, and is a no-op afterwards (the query finds nothing).
    /// This is the only moment the control plane has the plaintext and a node
    /// key at the same time, which is why it is a startup step rather than a
    /// migration: SQL cannot seal.
    pub async fn seal_legacy_adapter_env(&self) -> Result<(usize, usize)> {
        let rows: Vec<(i64, String, String, String, String)> = sqlx::query_as(
            "SELECT id, adapter, key, node_id, value_legacy FROM adapter_env \
             WHERE value_legacy <> ''",
        )
        .fetch_all(&self.pool)
        .await?;
        if rows.is_empty() {
            return Ok((0, 0));
        }
        let keys = self.node_public_keys().await?;
        let mut entries = 0usize;
        let mut blobs = 0usize;
        for (id, adapter, key, scope, value) in rows {
            let mut tx = self.write_txn().await?;
            for (node_id, public) in &keys {
                if !scope.is_empty() && scope != *node_id {
                    continue;
                }
                let sealed = seal::seal(public, &value, &entry_aad(node_id, &adapter, &key))?;
                sqlx::query(
                    "INSERT INTO adapter_env_sealed (entry_id, node_id, sealed) VALUES (?, ?, ?) \
                     ON CONFLICT (entry_id, node_id) DO UPDATE SET sealed = excluded.sealed",
                )
                .bind(id)
                .bind(node_id)
                .bind(&sealed)
                .execute(&mut *tx)
                .await?;
                blobs += 1;
            }
            sqlx::query(
                "UPDATE adapter_env SET value_legacy = '', fingerprint = ?, value_len = ? WHERE id = ?",
            )
            .bind(fingerprint(&value))
            .bind(value.chars().count() as i64)
            .bind(id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            entries += 1;
        }
        Ok((entries, blobs))
    }
}
