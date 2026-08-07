//! Agent profile revisions (Stage 13). Extracted from `store.rs`.

use super::{now_iso, profile_from_row};
use crate::Store;
use agentgrid_common::{AgentProfile, AgentProfileCreate};
use anyhow::Result;

impl Store {
    /// Create a new immutable revision of a profile. `revision` = max(existing)+1
    /// (or 1 for the first). The new revision is **not** auto-activated; call
    /// `activate_profile` to flip the pointer.
    pub async fn create_profile_revision(
        &self,
        id: &str,
        body: &AgentProfileCreate,
        created_by: &str,
    ) -> Result<i64> {
        let now = now_iso();
        let mut tx = self.write_txn().await?;
        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(revision), 0) + 1 FROM agent_profiles WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO agent_profiles \
             (id, revision, system_prompt, autonomy, memory_max, cpu_quota, tasks_max, created_at, created_by, secret_requirements, adapter_version, mcp_server_ids) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(next)
        .bind(&body.system_prompt)
        .bind(&body.autonomy)
        .bind(body.memory_max)
        .bind(body.cpu_quota)
        .bind(body.tasks_max)
        .bind(&now)
        .bind(created_by)
        .bind(serde_json::to_string(&body.secret_requirements).unwrap_or_else(|_| "[]".into()))
        .bind(&body.adapter_version)
        .bind(serde_json::to_string(&body.mcp_server_ids).unwrap_or_else(|_| "[]".into()))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(next)
    }

    /// Flip the active-revision pointer (rollback = point at an older revision).
    /// Idempotent; the revision must exist.
    pub async fn activate_profile(&self, id: &str, revision: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO agent_profiles_active (id, active_revision) VALUES (?, ?) \
             ON CONFLICT(id) DO UPDATE SET active_revision = excluded.active_revision",
        )
        .bind(id)
        .bind(revision)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// List all revisions of a profile (newest first).
    pub async fn list_profile_revisions(&self, id: &str) -> Result<Vec<AgentProfile>> {
        let rows = sqlx::query(
            "SELECT p.id, p.revision, p.system_prompt, p.autonomy, p.memory_max, \
                    p.cpu_quota, p.tasks_max, p.created_at, p.created_by, \
                    p.secret_requirements, p.adapter_version, p.mcp_server_ids,\
                    (a.active_revision = p.revision) AS active \
             FROM agent_profiles p \
             LEFT JOIN agent_profiles_active a ON a.id = p.id \
             WHERE p.id = ? \
             ORDER BY p.revision DESC",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(profile_from_row).collect())
    }

    /// List all profile ids that have an active revision.
    pub async fn list_profiles(&self) -> Result<Vec<String>> {
        let rows: Vec<String> =
            sqlx::query_scalar("SELECT id FROM agent_profiles_active ORDER BY id ASC")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }
}
