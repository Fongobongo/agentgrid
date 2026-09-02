//! CP-managed adapter environment: key/value entries pushed to nodes and
//! injected into attempt process env. `adapter = "*"` applies to every
//! adapter; node_id NULL = global, node-scoped entries are appended after
//! global ones. Node-explicit `AGENTGRID_ADAPTER_ENV` wins on key collision.

use agentgrid_common::AdapterEnvEntry;
use anyhow::Result;
use serde::Serialize;

use super::Store;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct AdapterEnvView {
    pub id: i64,
    pub adapter: String,
    pub key: String,
    pub value: String,
    pub node_id: Option<String>,
    pub created_at: String,
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
        Ok(sqlx::query_as::<_, AdapterEnvView>(
            "SELECT id, adapter, key, value, NULLIF(node_id, '') AS node_id, created_at \
             FROM adapter_env ORDER BY (node_id IS NULL) DESC, adapter, key",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn upsert_adapter_env(
        &self,
        adapter: &str,
        key: &str,
        value: &str,
        node_id: Option<&str>,
    ) -> Result<i64> {
        sqlx::query(
            "INSERT INTO adapter_env (adapter, key, value, node_id) VALUES (?, ?, ?, ?) \
             ON CONFLICT (adapter, key, node_id) DO UPDATE SET value = excluded.value",
        )
        .bind(adapter)
        .bind(key)
        .bind(value)
        .bind(node_id.unwrap_or(""))
        .execute(&self.pool)
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

    pub async fn remove_adapter_env(&self, id: i64) -> Result<u64> {
        Ok(sqlx::query("DELETE FROM adapter_env WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }
}
