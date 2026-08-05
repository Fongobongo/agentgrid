//! Repository registry. Extracted from `store.rs`.

use super::{now_iso, Store};
use agentgrid_common::{CreateRepositoryRequest, RepositoryView};
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

impl Store {
    // ----- repositories (Stage 2.5) -----

    pub async fn create_repository(&self, req: &CreateRepositoryRequest) -> Result<RepositoryView> {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        sqlx::query(
            "INSERT INTO repositories (id, name, git_url, default_branch, validation_command, created_at) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&req.name)
        .bind(&req.git_url)
        .bind(&req.default_branch)
        .bind(&req.validation_command)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(RepositoryView {
            id,
            name: req.name.clone(),
            git_url: req.git_url.clone(),
            default_branch: req.default_branch.clone(),
            validation_command: req.validation_command.clone(),
            created_at: now,
        })
    }

    pub async fn count_attempts(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS c FROM attempts")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<i64, _>("c")?)
    }

    pub async fn list_repositories(
        &self,
        after: Option<(String, String)>,
        limit: Option<u64>,
    ) -> Result<Vec<RepositoryView>> {
        const MAX_REPOS: i64 = 1000;
        let limit = limit.unwrap_or(100).min(MAX_REPOS as u64) as i64;
        let mut sql = String::from(
            "SELECT id, name, git_url, default_branch, validation_command, created_at FROM repositories WHERE 1=1",
        );
        if after.is_some() {
            sql.push_str(" AND (created_at > ? OR (created_at = ? AND id > ?))");
        }
        sql.push_str(" ORDER BY created_at ASC, id ASC LIMIT ?");
        let mut q = sqlx::query(&sql);
        if let Some((created_at, id)) = &after {
            q = q.bind(created_at).bind(created_at).bind(id);
        }
        let rows = q.bind(limit).fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(|r| RepositoryView {
                id: r.try_get("id").unwrap_or_default(),
                name: r.try_get("name").unwrap_or_default(),
                git_url: r.try_get("git_url").unwrap_or_default(),
                default_branch: r.try_get("default_branch").unwrap_or_default(),
                validation_command: r.try_get("validation_command").unwrap_or_default(),
                created_at: r.try_get("created_at").unwrap_or_default(),
            })
            .collect())
    }
}
