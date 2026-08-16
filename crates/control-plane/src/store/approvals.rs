//! Durable approval flow storage (Stage 5). Extracted from `store.rs`.

use super::{approval_from_row, iso_plus_secs, now_iso};
use crate::Store;
use agentgrid_common::{next_approval, ApprovalEvent, ApprovalStatus, ApprovalView};
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

impl Store {
    /// Create a pending approval for an agent permission request. `ttl_secs`
    /// controls auto-expiry (fail-closed). Returns the new approval id.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_approval(
        &self,
        task_id: &str,
        attempt_id: &str,
        session_id: Option<&str>,
        permission: &str,
        ttl_secs: i64,
        step_run_id: Option<&str>,
        scope: &str,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        let expires = iso_plus_secs(ttl_secs);
        sqlx::query(
        "INSERT INTO approvals (id, task_id, attempt_id, session_id, permission, status, created_at, expires_at, step_run_id, scope) \
         VALUES (?, ?, ?, ?, ?, 'pending', ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(task_id)
    .bind(attempt_id)
    .bind(session_id)
    .bind(permission)
    .bind(&now)
    .bind(&expires)
    .bind(step_run_id)
    .bind(scope)
    .execute(&self.pool)
    .await?;
        Ok(id)
    }

    /// Answer a pending approval. `event` must be `Allow`/`Deny` (the only
    /// operator-driven transitions); `Expire`/`Cancel` are applied by the
    /// maintenance tick. Honors the state machine — answering a terminal approval
    /// is a no-op (idempotent), not an error. Returns `true` only when this
    /// call performed the transition (concurrent answers are serialized by the
    /// `status='pending'` guard, so at most one wins).
    pub async fn answer_approval(
        &self,
        id: &str,
        event: ApprovalEvent,
        reason: Option<&str>,
        actor: &str,
    ) -> Result<bool> {
        let current: Option<String> = sqlx::query("SELECT status FROM approvals WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| r.try_get("status"))
            .transpose()?;
        let Some(current) = current else {
            return Ok(false); // unknown id: no-op
        };
        let current_status: ApprovalStatus =
            serde_json::from_value(serde_json::Value::String(current))
                .unwrap_or(ApprovalStatus::Pending);
        let Ok(next) = next_approval(current_status, event) else {
            return Ok(false); // terminal already -> idempotent no-op
        };
        let decided = now_iso();
        let audit = serde_json::json!({ "actor": actor, "event": event, "at": decided });
        let res = sqlx::query(
            "UPDATE approvals SET status = ?, decided_at = ?, reason = ?, audit = ? \
         WHERE id = ? AND status = 'pending'",
        )
        .bind(serde_json::to_value(next).map(|v| v.as_str().unwrap_or("pending").to_string())?)
        .bind(&decided)
        .bind(reason)
        .bind(audit.to_string())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Fetch a single approval by id.
    pub async fn get_approval(&self, id: &str) -> Result<Option<ApprovalView>> {
        let row = sqlx::query(
            "SELECT id, task_id, attempt_id, session_id, permission, status, reason, \
                created_at, expires_at, decided_at, scope \
         FROM approvals WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(approval_from_row))
    }

    /// List approvals, optionally filtered by status.
    /// Hardening P2 item 20: list approvals with optional status filter, a
    /// keyset cursor (`after` = `(created_at, id)`) and a server page cap.
    pub async fn list_approvals(
        &self,
        status: Option<ApprovalStatus>,
        after: Option<(String, String)>,
        limit: Option<u64>,
    ) -> Result<Vec<ApprovalView>> {
        const MAX_APPROVALS: i64 = 1000;
        let limit = limit.unwrap_or(100).min(MAX_APPROVALS as u64) as i64;
        let mut sql = String::from(
            "SELECT id, task_id, attempt_id, session_id, permission, status, reason, \
                created_at, expires_at, decided_at, scope \
             FROM approvals WHERE 1=1",
        );
        if status.is_some() {
            sql.push_str(" AND status = ?");
        }
        if after.is_some() {
            sql.push_str(" AND (created_at > ? OR (created_at = ? AND id > ?))");
        }
        sql.push_str(" ORDER BY created_at ASC, id ASC LIMIT ?");
        // audited: clauses are compile-time constants; values are bound
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
        if let Some(s) = &status {
            let v = serde_json::to_value(s).map(|v| v.as_str().unwrap_or("pending").to_string())?;
            q = q.bind(v);
        }
        if let Some((created_at, id)) = &after {
            q = q.bind(created_at).bind(created_at).bind(id);
        }
        let rows = q.bind(limit).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(approval_from_row).collect())
    }

    /// Maintenance tick: flip past-due `pending` approvals to `expired`
    /// (fail-closed). An expired approval that is linked to a workflow step
    /// blocks that step (and its run) so the run does not hang forever waiting
    /// on an operator who never answered. Returns the number expired.
    pub async fn tick_approval_expiry(&self) -> Result<usize> {
        let now = now_iso();
        let rows = sqlx::query(
            "SELECT id, step_run_id FROM approvals WHERE status = 'pending' AND expires_at < ?",
        )
        .bind(&now)
        .fetch_all(&self.pool)
        .await?;
        let mut count = 0;
        for r in &rows {
            let id: String = r.try_get("id")?;
            let step_run_id: Option<String> = r.try_get("step_run_id").ok().flatten();
            match self
                .answer_approval(&id, ApprovalEvent::Expire, Some("auto-expired"), "system")
                .await
            {
                Ok(true) => {
                    count += 1;
                    if let Some(step) = step_run_id {
                        let _ = self.block_step_and_run(&step).await;
                    }
                }
                Ok(false) => {}
                Err(e) => tracing::warn!("approval expiry for {id} failed: {e}"),
            }
        }
        Ok(count)
    }

    /// Competitor plan 1.1: find the pending `task_patch_review` approval for
    /// a task, if one exists. At most one is expected to be live at a time
    /// (one per attempt completion). Returns the latest pending.
    pub async fn find_pending_patch_review(&self, task_id: &str) -> Result<Option<ApprovalView>> {
        let row = sqlx::query(
            "SELECT id, task_id, attempt_id, session_id, permission, status, reason, \
                created_at, expires_at, decided_at, scope \
             FROM approvals \
             WHERE task_id = ? AND scope = 'task_patch_review' AND status = 'pending' \
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(approval_from_row))
    }

    /// Block a workflow step (and its run) because an approval it depended on
    /// timed out. Only non-terminal steps/runs are touched, so a finished run
    /// is never reopened. Idempotent.
    pub async fn block_step_and_run(&self, step_run_id: &str) -> Result<()> {
        sqlx::query(
            "UPDATE workflow_steps SET status = 'blocked' \
             WHERE id = ? AND status NOT IN ('succeeded','failed','blocked','skipped')",
        )
        .bind(step_run_id)
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "UPDATE workflow_runs SET status = 'blocked' \
             WHERE id = (SELECT run_id FROM workflow_steps WHERE id = ?) \
             AND status NOT IN ('succeeded','failed','cancelled','blocked','plan_ready')",
        )
        .bind(step_run_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
