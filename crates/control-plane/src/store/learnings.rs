//! Plan 2.8 (#19): repo-learnings storage — short factual statements a
//! reviewer has approved for prompt injection.
//!
//! Two hot paths:
//!   1. `add_learning` writes a new row (default `approved = 0`).
//!   2. `top_approved_for_repo(repo, n)` returns the highest-confidence
//!      approved rows; the scheduler merges them into the attempt prompt so
//!      every future run of that repo starts "knowing" them.
//!
//! `approve_learning(id, approved)` flips the flag and is the only way data
//! leaves the `pending` bucket — nothing else is allowed to short-circuit
//! human review.

use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

use super::{now_iso, Store};

pub struct LearningRow {
    pub id: String,
    pub repository: String,
    pub statement: String,
    pub confidence: f64,
    pub source_attempt_id: Option<String>,
    pub approved: bool,
    pub created_at: String,
    pub updated_at: String,
}

fn row_to_learning(r: &sqlx::sqlite::SqliteRow) -> LearningRow {
    LearningRow {
        id: r.get("id"),
        repository: r.get("repository"),
        statement: r.get("statement"),
        confidence: r.get::<f64, _>("confidence"),
        source_attempt_id: r.get("source_attempt_id"),
        approved: r.get::<i32, _>("approved") != 0,
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

impl Store {
    /// Insert a new learning. Default: pending (approved = 0) until a human
    /// approves via `ag learn approve <id>`.
    pub async fn add_learning(
        &self,
        repository: &str,
        statement: &str,
        confidence: f64,
        source_attempt_id: Option<&str>,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        sqlx::query(
            "INSERT INTO repo_learnings (id, repository, statement, confidence, source_attempt_id, approved, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, 0, ?, ?)",
        )
        .bind(&id)
        .bind(repository)
        .bind(statement)
        .bind(confidence.clamp(0.0, 1.0))
        .bind(source_attempt_id)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    /// Flip the `approved` flag on an existing learning. Returns true when a
    /// row was actually touched (id existed).
    pub async fn approve_learning(&self, id: &str, approved: bool) -> Result<bool> {
        let now = now_iso();
        let n = sqlx::query("UPDATE repo_learnings SET approved = ?, updated_at = ? WHERE id = ?")
            .bind(if approved { 1 } else { 0 })
            .bind(&now)
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n > 0)
    }

    /// Remove a learning (admin / operator action).
    pub async fn delete_learning(&self, id: &str) -> Result<bool> {
        let n = sqlx::query("DELETE FROM repo_learnings WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(n > 0)
    }

    /// List learnings for a repo. `approved_only = true` filters to human-
    /// reviewed rows, ordered by confidence DESC, then updated_at DESC.
    pub async fn list_learnings(
        &self,
        repository: &str,
        approved_only: bool,
        limit: u32,
    ) -> Result<Vec<LearningRow>> {
        let q = if approved_only {
            sqlx::query(
                "SELECT id, repository, statement, confidence, source_attempt_id, approved, created_at, updated_at
                 FROM repo_learnings
                 WHERE repository = ? AND approved = 1
                 ORDER BY confidence DESC, updated_at DESC
                 LIMIT ?",
            )
            .bind(repository)
            .bind(limit as i64)
        } else {
            sqlx::query(
                "SELECT id, repository, statement, confidence, source_attempt_id, approved, created_at, updated_at
                 FROM repo_learnings
                 WHERE repository = ?
                 ORDER BY confidence DESC, updated_at DESC
                 LIMIT ?",
            )
            .bind(repository)
            .bind(limit as i64)
        };
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows.iter().map(row_to_learning).collect())
    }

    /// Top-N approved learnings for prompt injection. Same SQL as
    /// `list_learnings(approved_only = true, limit)` but the name signals the
    /// caller is about to bake these into a prompt — keep `n` small.
    pub async fn top_approved_for_repo(
        &self,
        repository: &str,
        n: u32,
    ) -> Result<Vec<LearningRow>> {
        self.list_learnings(repository, true, n).await
    }
}
