//! CP-managed adapter environment: key/value entries pushed to nodes and
//! injected into attempt process env. `adapter = "*"` applies to every
//! adapter; node_id NULL = global, node-scoped entries are appended after
//! global ones. Node-explicit `AGENTGRID_ADAPTER_ENV` wins on key collision.

use agentgrid_common::AdapterEnvEntry;
use anyhow::Result;
use serde::Serialize;

use super::Store;

/// What the read path is allowed to know about a stored secret.
///
/// Deliberately **not** a row type: there is no field that can hold the value,
/// so a future handler cannot serialize one by accident. `fingerprint` is
/// `sha256(value)[..8]` — enough to answer "is the key I just pasted the one
/// that is stored?" and not enough to recover anything, which is the whole
/// point of a secret store's list endpoint.
#[derive(Debug, Clone, Serialize)]
pub struct AdapterEnvView {
    pub id: i64,
    pub adapter: String,
    pub key: String,
    pub fingerprint: String,
    pub value_len: usize,
    pub node_id: Option<String>,
    pub created_at: String,
}

/// Short, non-reversible identifier for a stored secret.
pub fn fingerprint(value: &str) -> String {
    agentgrid_common::sha256_hex(value.as_bytes())[..8].to_string()
}

impl Store {
    /// Effective entries for a node, global rows first.
    pub async fn adapter_env_for(&self, node_id: &str) -> Result<Vec<AdapterEnvEntry>> {
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT adapter, key, value FROM adapter_env \
             WHERE node_id = '' OR node_id = ? \
             ORDER BY (node_id = '') DESC, id",
        )
        .bind(node_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(adapter, key, value)| AdapterEnvEntry {
                adapter,
                key,
                value,
            })
            .collect())
    }

    pub async fn list_adapter_env(&self) -> Result<Vec<AdapterEnvView>> {
        let rows: Vec<(i64, String, String, String, Option<String>, String)> = sqlx::query_as(
            // Audit X-C6: globals are stored as '' (see upsert), never NULL —
            // the old `node_id IS NULL` tied everywhere and the global-first
            // order was arbitrary. Matches `adapter_env_for`'s predicate.
            "SELECT id, adapter, key, value, NULLIF(node_id, '') AS node_id, created_at \
             FROM adapter_env ORDER BY (node_id = '') DESC, adapter, key",
        )
        .fetch_all(&self.pool)
        .await?;
        // The value is dropped here, inside the store: it never reaches a
        // handler, a log line, or the CLI's terminal.
        Ok(rows
            .into_iter()
            .map(
                |(id, adapter, key, value, node_id, created_at)| AdapterEnvView {
                    id,
                    adapter,
                    key,
                    value_len: value.chars().count(),
                    fingerprint: fingerprint(&value),
                    node_id,
                    created_at,
                },
            )
            .collect())
    }

    pub async fn upsert_adapter_env(
        &self,
        adapter: &str,
        key: &str,
        value: &str,
        node_id: Option<&str>,
    ) -> Result<i64> {
        self.write_one(
            sqlx::query(
                "INSERT INTO adapter_env (adapter, key, value, node_id) VALUES (?, ?, ?, ?) \
                 ON CONFLICT (adapter, key, node_id) DO UPDATE SET value = excluded.value",
            )
            .bind(adapter)
            .bind(key)
            .bind(value)
            .bind(node_id.unwrap_or("")),
        )
        .await?;
        Ok(sqlx::query_scalar::<_, i64>(
            "SELECT id FROM adapter_env \
                 WHERE adapter = ? AND key = ? AND node_id = ?",
        )
        .bind(adapter)
        .bind(key)
        .bind(node_id.unwrap_or(""))
        .fetch_one(&self.pool)
        .await?)
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
}
