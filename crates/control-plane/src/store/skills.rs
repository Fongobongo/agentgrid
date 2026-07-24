//! Skill trust + MCP server registry (Stage 9.2 / 13). Extracted from `store.rs`.

use super::{mcp_server_from_row, now_iso, skill_trust_from_row};
use crate::Store;
use agentgrid_common::{McpServer, McpServerCreate, SkillTrustView};
use anyhow::Result;
impl Store {
    /// Set trust state for `(name, source)`. Idempotent upsert; records the
    /// operator that decided + when.
    pub async fn set_skill_trust(
        &self,
        name: &str,
        source: &str,
        trusted: bool,
        decided_by: &str,
    ) -> Result<()> {
        let now = now_iso();
        sqlx::query(
            "INSERT INTO skills_trust (name, source, trusted, decided_by, decided_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (name, source) DO UPDATE SET \
                 trusted = excluded.trusted, \
                 decided_by = excluded.decided_by, \
                 decided_at = excluded.decided_at",
        )
        .bind(name)
        .bind(source)
        .bind(if trusted { 1 } else { 0 })
        .bind(decided_by)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Read the recorded trust state for a single skill, or `untrusted` when
    /// no decision exists yet.
    pub async fn get_skill_trust(&self, name: &str, source: &str) -> Result<SkillTrustView> {
        let row = sqlx::query(
            "SELECT name, source, trusted, decided_by, decided_at FROM skills_trust \
             WHERE name = ? AND source = ?",
        )
        .bind(name)
        .bind(source)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row
            .as_ref()
            .map(skill_trust_from_row)
            .unwrap_or_else(|| SkillTrustView {
                name: name.to_string(),
                source: source.to_string(),
                trusted: false,
                decided_by: None,
                decided_at: None,
            }))
    }

    /// All recorded trust decisions, newest decision first.
    pub async fn list_skill_trust(&self) -> Result<Vec<SkillTrustView>> {
        let rows = sqlx::query(
            "SELECT name, source, trusted, decided_by, decided_at FROM skills_trust \
             ORDER BY decided_at DESC, name ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(skill_trust_from_row).collect())
    }

    /// Stage 9.2: upsert skill rows a heartbeat discovered on disk. Insert
    /// `INSERT ... ON CONFLICT DO NOTHING` so a freshly discovered skill lands
    /// as untrusted, but an existing operator decision (trusted or untrusted)
    /// is never overwritten — auto-discovery is a hint, never policy.
    pub async fn upsert_discovered_skills(&self, skills: &[(String, String)]) -> Result<()> {
        if skills.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for (name, source) in skills {
            sqlx::query(
                "INSERT INTO skills_trust (name, source, trusted, decided_by, decided_at) \
                 VALUES (?, ?, 0, 'discovery', NULL) \
                 ON CONFLICT (name, source) DO NOTHING",
            )
            .bind(name)
            .bind(source)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Stage 13: MCP server registry. Insert or replace a trusted server.
    pub async fn upsert_mcp_server(&self, body: &McpServerCreate) -> Result<McpServer> {
        let now = now_iso();
        sqlx::query(
            "INSERT INTO mcp_servers \
             (id, name, command, args, env_requirements, enabled, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                 name = excluded.name, command = excluded.command, \
                 args = excluded.args, env_requirements = excluded.env_requirements, \
                 enabled = excluded.enabled",
        )
        .bind(&body.id)
        .bind(&body.name)
        .bind(&body.command)
        .bind(serde_json::to_string(&body.args).unwrap_or_else(|_| "[]".into()))
        .bind(serde_json::to_string(&body.env_requirements).unwrap_or_else(|_| "[]".into()))
        .bind(if body.enabled { 1i64 } else { 0 })
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(self.get_mcp_server(&body.id).await?.unwrap())
    }

    pub async fn get_mcp_server(&self, id: &str) -> Result<Option<McpServer>> {
        let row = sqlx::query(
            "SELECT id, name, command, args, env_requirements, enabled, created_at \
             FROM mcp_servers WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(mcp_server_from_row))
    }

    pub async fn list_mcp_servers(&self) -> Result<Vec<McpServer>> {
        let rows = sqlx::query(
            "SELECT id, name, command, args, env_requirements, enabled, created_at \
             FROM mcp_servers ORDER BY id ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(mcp_server_from_row).collect())
    }

    pub async fn delete_mcp_server(&self, id: &str) -> Result<bool> {
        let affected = sqlx::query("DELETE FROM mcp_servers WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected == 1)
    }
}
