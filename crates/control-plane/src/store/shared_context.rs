//! Plan 1.12 (#7): shared context / memory between parallel attempts of the
//! same logical task group. Flat scoped key→value notes. Extracted from
//! `store.rs` to keep that file focused.

use super::{is_safe_opaque_id, now_iso, Store};
use agentgrid_common::SharedContextEntry;
use anyhow::Result;
use sqlx::Row;

/// Validate a shared-context key the same way we validate artifact names:
/// require a path-safe segment so a crafted key cannot escape the (group, key)
/// PK namespace. Empty is rejected.
fn is_safe_context_key(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 128
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

impl Store {
    /// Set (or overwrite) one note for a task group: upsert on (group_id, key).
    /// `group_id` is validated as an opaque id to keep the PK namespace clean.
    pub async fn set_shared_context(&self, group_id: &str, key: &str, value: &str) -> Result<()> {
        if !is_safe_opaque_id(group_id) || !is_safe_context_key(key) {
            anyhow::bail!("invalid group_id or key");
        }
        let now = now_iso();
        sqlx::query(
            "INSERT INTO shared_context (task_group_id, key, value, updated_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT(task_group_id, key) DO UPDATE SET \
            value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(group_id)
        .bind(key)
        .bind(value)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// List every note for a task group (latest values), ordered by key.
    pub async fn list_shared_context(&self, group_id: &str) -> Result<Vec<SharedContextEntry>> {
        if !is_safe_opaque_id(group_id) {
            anyhow::bail!("invalid group_id");
        }
        let rows = sqlx::query(
            "SELECT key, value, updated_at FROM shared_context \
         WHERE task_group_id = ? ORDER BY key",
        )
        .bind(group_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| SharedContextEntry {
                key: r.try_get("key").unwrap_or_default(),
                value: r.try_get("value").unwrap_or_default(),
                updated_at: r.try_get("updated_at").unwrap_or_default(),
            })
            .collect())
    }

    /// Read one note for a task group by key. `None` when absent.
    pub async fn get_shared_context(&self, group_id: &str, key: &str) -> Result<Option<String>> {
        if !is_safe_opaque_id(group_id) || !is_safe_context_key(key) {
            anyhow::bail!("invalid group_id or key");
        }
        let row =
            sqlx::query("SELECT value FROM shared_context WHERE task_group_id = ? AND key = ?")
                .bind(group_id)
                .bind(key)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| r.try_get::<String, _>("value").unwrap_or_default()))
    }

    /// Delete one note for a task group by key. No-op when absent.
    pub async fn delete_shared_context(&self, group_id: &str, key: &str) -> Result<()> {
        if !is_safe_opaque_id(group_id) || !is_safe_context_key(key) {
            anyhow::bail!("invalid group_id or key");
        }
        sqlx::query("DELETE FROM shared_context WHERE task_group_id = ? AND key = ?")
            .bind(group_id)
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
