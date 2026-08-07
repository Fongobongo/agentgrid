//! Attempt lifecycle: completion, agent sessions, ack, cancel, retry.
//! Extracted from `store.rs`.

use super::{
    attempt_status_str, begin_immediate, from_snake, now_iso, status_str, Store,
    StoreTransitionError,
};
use agentgrid_common::{
    next_attempt_status, next_task_status, AgentSession, AttemptStatus, AttemptTransition,
    CompleteAttemptRequest, InvalidTransition, TaskStatus, TaskTransition,
};
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

impl Store {
    pub async fn complete_attempt(
        &self,
        attempt_id: &str,
        req: &CompleteAttemptRequest,
    ) -> Result<bool> {
        let mut tx = begin_immediate(&self.pool).await?;
        let attempt = sqlx::query(
            "SELECT task_id, node_id, status, cancel_requested, validated_at FROM attempts WHERE id = ?",
        )
        .bind(attempt_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(attempt) = attempt else {
            let _ = tx.rollback().await;
            return Ok(false);
        };
        let task_id: String = attempt.try_get("task_id")?;
        let node_id: String = attempt.try_get("node_id")?;
        let attempt_status: String = attempt.try_get("status")?;
        let cancel_requested: i64 = attempt.try_get("cancel_requested")?;
        let validated_at: Option<String> = attempt.try_get("validated_at")?;
        let as_enum = from_snake::<AttemptStatus>(&attempt_status);

        // Terminal/lost attempts cannot be completed again. A node that comes
        // back and reports a completion for an attempt we already marked `lost`
        // (node died) must not corrupt the failed task status.
        if let Some(s) = as_enum {
            if matches!(
                s,
                AttemptStatus::Succeeded
                    | AttemptStatus::Failed
                    | AttemptStatus::Cancelled
                    | AttemptStatus::Lost
            ) {
                let source_state = attempt_status_str(s);
                let _ = tx.rollback().await;
                // Hardening P1 item 13: audit the rejected transition so a
                // stale/late completion from an already-terminal attempt (a node
                // coming back after we marked it `lost`) is traceable in the
                // audit log without leaking the payload.
                let _ = self
                    .audit(
                        "attempt",
                        Some(attempt_id),
                        "complete.rejected_terminal",
                        Some(&source_state),
                        None,
                    )
                    .await;
                // Already terminal: a node reporting a completion for an attempt
                // we already finalized (e.g. marked `lost` after it died) gets
                // an idempotent ack without corrupting the task status.
                return Ok(true);
            }
        }

        // Success requires a clean exit AND no distinct failure category. The
        // node reports validation/timeout failures via `error_code` even when the
        // agent process exits 0, so exit 0 alone must not be treated as success.
        let success = req.exit_code == 0 && req.error_code.as_deref().is_none();
        // Stage 3.2: close any open agent session for this attempt.
        self.finish_agent_session(
            &mut tx,
            attempt_id,
            if success { "done" } else { "failed" },
            req.error_code.as_deref(),
        )
        .await?;
        let at = if success {
            AttemptTransition::Succeed
        } else {
            AttemptTransition::Fail
        };
        let tt = if success {
            TaskTransition::Succeed
        } else {
            TaskTransition::Fail
        };

        // Hardening P1 item 13: use the state machine transitions without
        // silent fallbacks. If the current state does not allow the requested
        // transition, return an error (mapped to 409 Conflict by the handler).
        let Some(current_attempt_status) = as_enum else {
            let _ = tx.rollback().await;
            return Err(StoreTransitionError(InvalidTransition {
                from: "unknown",
                transition: if success { "succeed" } else { "fail" },
            })
            .into());
        };
        let attempt_target: AttemptStatus = next_attempt_status(current_attempt_status, at)?;

        let task_row = sqlx::query("SELECT status FROM tasks WHERE id = ?")
            .bind(&task_id)
            .fetch_one(&mut *tx)
            .await?;
        let task_status: String = task_row.try_get("status")?;
        let Some(current_task_status) = from_snake::<TaskStatus>(&task_status) else {
            let _ = tx.rollback().await;
            return Err(StoreTransitionError(InvalidTransition {
                from: "unknown",
                transition: if success { "succeed" } else { "fail" },
            })
            .into());
        };
        let task_target: TaskStatus = next_task_status(current_task_status, tt)?;

        // If cancellation was requested, the attempt ends as cancelled
        // regardless of the adapter's exit code.
        let (attempt_target, task_target) = if cancel_requested != 0 {
            let a = next_attempt_status(current_attempt_status, AttemptTransition::Cancel)?;
            let t = next_task_status(current_task_status, TaskTransition::Cancel)?;
            (a, t)
        } else {
            (attempt_target, task_target)
        };

        let now = now_iso();
        sqlx::query("UPDATE attempts SET status = ?, exit_code = ?, finished_at = ? WHERE id = ?")
            .bind(attempt_status_str(attempt_target))
            .bind(req.exit_code as i64)
            .bind(&now)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await?;
        // Hardening P2 item 35: validation metrics. Only record when the attempt
        // actually went through the `validating` state (begin_validate set
        // `validated_at`); a `running`-only completion carries no validation.
        if current_attempt_status == AttemptStatus::Validating {
            if let Some(vat) = validated_at.as_deref() {
                if let (Ok(vdt), Ok(fdt)) = (
                    chrono::DateTime::parse_from_rfc3339(vat),
                    chrono::DateTime::parse_from_rfc3339(&now),
                ) {
                    let ms = (fdt - vdt).num_milliseconds().max(0) as u64;
                    self.validation_duration_sum
                        .fetch_add(ms, std::sync::atomic::Ordering::Relaxed);
                    self.validation_duration_count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
            }
            let outcome = if success {
                "succeeded".to_string()
            } else if let Some(ec) = &req.error_code {
                ec.clone()
            } else {
                "failed".to_string()
            };
            *self
                .validation_outcomes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(outcome)
                .or_insert(0) += 1;
        }
        if let Some(sp) = req
            .provenance
            .as_ref()
            .and_then(|p| p.security_profile.as_deref())
        {
            *self
                .security_profile_attempts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(sp.to_string())
                .or_insert(0) += 1;
        }
        if let Some(sha) = &req.commit_sha {
            sqlx::query("UPDATE attempts SET commit_sha = ? WHERE id = ?")
                .bind(sha)
                .bind(attempt_id)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(sid) = &req.acp_session_id {
            sqlx::query("UPDATE attempts SET acp_session_id = ? WHERE id = ?")
                .bind(sid)
                .bind(attempt_id)
                .execute(&mut *tx)
                .await?;
        }
        // Hardening P2 item 32-5: persist the exact resolved base commit.
        if let Some(base) = &req.resolved_base_sha {
            sqlx::query("UPDATE attempts SET resolved_base_sha = ? WHERE id = ?")
                .bind(base)
                .bind(attempt_id)
                .execute(&mut *tx)
                .await?;
        }
        // Hardening P1 item 32: persist the remote HEAD at start / finish.
        if let Some(sha) = &req.remote_head_at_start {
            sqlx::query("UPDATE attempts SET remote_head_at_start = ? WHERE id = ?")
                .bind(sha)
                .bind(attempt_id)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(sha) = &req.remote_head_at_finish {
            sqlx::query("UPDATE attempts SET remote_head_at_finish = ? WHERE id = ?")
                .bind(sha)
                .bind(attempt_id)
                .execute(&mut *tx)
                .await?;
        }
        if let Some(ec) = &req.error_code {
            sqlx::query("UPDATE attempts SET error_code = ? WHERE id = ?")
                .bind(ec)
                .bind(attempt_id)
                .execute(&mut *tx)
                .await?;
        }
        // Stage 13: persist the external-origin provenance link when provided.
        let provenance_json: Option<String> = match &req.provenance {
            Some(p) => serde_json::to_string(p).ok(),
            None => None,
        };
        sqlx::query("UPDATE attempts SET provenance = ? WHERE id = ?")
            .bind(provenance_json)
            .bind(attempt_id)
            .execute(&mut *tx)
            .await?;
        // Stage 13 plan expansion: persist the architect's machine-readable
        // plan when provided (used by the workflow tick to pause the run in
        // `PlanReady` pending approval).
        if let Some(plan) = &req.plan {
            sqlx::query("UPDATE attempts SET plan = ? WHERE id = ?")
                .bind(plan)
                .bind(attempt_id)
                .execute(&mut *tx)
                .await?;
        }
        // Hardening P1 item 11: persist the set of artifacts the node could not
        // deliver before completion so operators can see what is still owed and
        // the node knows what to retry on the next startup.
        if !req.pending_artifacts.is_empty() {
            let pa = serde_json::to_string(&req.pending_artifacts)?;
            sqlx::query("UPDATE attempts SET pending_artifacts = ? WHERE id = ?")
                .bind(pa)
                .bind(attempt_id)
                .execute(&mut *tx)
                .await?;
        }
        // Normalize the failure category onto the task so the UI/CLI can show
        // WHY it failed without joining the producing attempt.
        let task_error_code: Option<String> = match task_target {
            TaskStatus::Failed => req
                .error_code
                .clone()
                .or_else(|| Some("agent_failed".into())),
            TaskStatus::Cancelled => Some("cancelled".into()),
            _ => None,
        };
        // Hardening P1 item 13: a terminal task has no active attempt — clear
        // assigned_attempt_id so the invariant "terminal task has no active
        // attempt" / "assigned_attempt_id points at the same task" holds.
        sqlx::query(
            "UPDATE tasks SET status = ?, finished_at = ?, error_code = ?, assigned_attempt_id = NULL WHERE id = ?",
        )
        .bind(status_str(task_target))
        .bind(&now)
        .bind(&task_error_code)
        .bind(&task_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE nodes SET active_attempts = MAX(0, active_attempts - 1) WHERE id = ?")
            .bind(&node_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn create_agent_session(&self, attempt_id: &str, adapter: &str) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        sqlx::query(
            "INSERT INTO agent_sessions (id, attempt_id, adapter, started_at, status) \
             VALUES (?, ?, ?, ?, 'running')",
        )
        .bind(&id)
        .bind(attempt_id)
        .bind(adapter)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn finish_agent_session(
        &self,
        tx: &mut sqlx::SqliteConnection,
        attempt_id: &str,
        status: &str,
        error_code: Option<&str>,
    ) -> Result<()> {
        let now = now_iso();
        sqlx::query(
            "UPDATE agent_sessions SET ended_at = ?, status = ?, error_code = ? \
             WHERE attempt_id = ? AND ended_at IS NULL",
        )
        .bind(&now)
        .bind(status)
        .bind(error_code)
        .bind(attempt_id)
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

    pub async fn get_agent_session(&self, id: &str) -> Result<Option<AgentSession>> {
        let row = sqlx::query(
            "SELECT id, attempt_id, adapter, started_at, ended_at, status, error_code \
             FROM agent_sessions WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| AgentSession {
            id: r.try_get("id").unwrap_or_default(),
            attempt_id: r.try_get("attempt_id").unwrap_or_default(),
            adapter: r.try_get("adapter").unwrap_or_default(),
            started_at: r.try_get("started_at").unwrap_or_default(),
            ended_at: r.try_get("ended_at").ok(),
            status: r.try_get("status").unwrap_or_default(),
            error_code: r.try_get("error_code").ok(),
        }))
    }

    /// Plan 534: the task id an attempt belongs to (service layer uses it to
    /// advance the owning workflow run when the attempt completes).
    pub async fn attempt_task_id(&self, attempt_id: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT task_id FROM attempts WHERE id = ?")
            .bind(attempt_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match row {
            Some(r) => r.try_get::<Option<String>, _>("task_id").ok().flatten(),
            None => None,
        })
    }

    pub async fn ack_attempt(&self, attempt_id: &str) -> Result<bool> {
        let mut tx = begin_immediate(&self.pool).await?;
        let row = sqlx::query("SELECT status FROM attempts WHERE id = ?")
            .bind(attempt_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            let _ = tx.rollback().await;
            return Ok(false);
        };
        let status: String = row.try_get("status")?;
        if status != "assigned" {
            let _ = tx.rollback().await;
            return Ok(matches!(
                status.as_str(),
                "running" | "succeeded" | "failed" | "cancelled" | "lost" | "validating"
            ));
        }
        let now = now_iso();
        let n = sqlx::query(
            "UPDATE attempts SET status = 'running', ack_deadline = NULL, started_at = ? \
             WHERE id = ? AND status = 'assigned'",
        )
        .bind(&now)
        .bind(attempt_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n != 1 {
            let _ = tx.rollback().await;
            return Ok(false);
        }
        // Flip the task to running only if it still points at THIS attempt — a
        // concurrent cancel/reassign may have cleared/changed it; in that case
        // leave the task untouched (the attempt is still recorded running for
        // log/event continuity).
        let _task = sqlx::query(
            "UPDATE tasks SET status = 'running', started_at = ? \
             WHERE assigned_attempt_id = ? AND status IN ('assigned','queued')",
        )
        .bind(&now)
        .bind(attempt_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn begin_validate(&self, attempt_id: &str) -> Result<bool> {
        let mut tx = begin_immediate(&self.pool).await?;
        let n = sqlx::query(
            "UPDATE attempts SET status = 'validating', validated_at = ? WHERE id = ? AND status = 'running'",
        )
        .bind(now_iso())
        .bind(attempt_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if n != 1 {
            let _ = tx.rollback().await;
            return Ok(false);
        }
        let _task = sqlx::query(
            "UPDATE tasks SET status = 'validating' \
             WHERE assigned_attempt_id = ? AND status = 'running'",
        )
        .bind(attempt_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn cancel_task(&self, task_id: &str) -> Result<bool> {
        let mut tx = begin_immediate(&self.pool).await?;
        let row = sqlx::query("SELECT status FROM tasks WHERE id = ?")
            .bind(task_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            let _ = tx.rollback().await;
            return Ok(false);
        };
        let status: String = row.try_get("status")?;
        if status == "queued" {
            sqlx::query(
                "UPDATE tasks SET status = 'cancelled', finished_at = ?, assigned_attempt_id = NULL WHERE id = ?",
            )
            .bind(now_iso())
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(true);
        }
        if matches!(status.as_str(), "assigned" | "running" | "validating") {
            sqlx::query(
                "UPDATE attempts SET cancel_requested = 1 WHERE task_id = ? AND status IN ('assigned','running','validating')",
            )
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(true);
        }
        let _ = tx.rollback().await;
        Ok(false)
    }

    pub async fn attempt_cancel_requested(&self, attempt_id: &str) -> Result<bool> {
        let row = sqlx::query("SELECT cancel_requested FROM attempts WHERE id = ?")
            .bind(attempt_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match row {
            Some(r) => r.try_get::<i64, _>("cancel_requested")? != 0,
            None => false,
        })
    }

    pub async fn cancel_workflow_run(&self, run_id: &str) -> Result<bool> {
        let mut tx = begin_immediate(&self.pool).await?;
        let run = sqlx::query("SELECT status FROM workflow_runs WHERE id = ?")
            .bind(run_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(run) = run else {
            let _ = tx.rollback().await;
            return Ok(false);
        };
        let status: String = run.try_get("status")?;
        if matches!(
            status.as_str(),
            "succeeded" | "failed" | "cancelled" | "blocked"
        ) {
            let _ = tx.rollback().await;
            return Ok(false);
        }
        sqlx::query("UPDATE workflow_runs SET status = 'cancelled' WHERE id = ?")
            .bind(run_id)
            .execute(&mut *tx)
            .await?;
        let steps = sqlx::query("SELECT id, status FROM workflow_steps WHERE run_id = ?")
            .bind(run_id)
            .fetch_all(&mut *tx)
            .await?;
        for s in &steps {
            let step_id: String = s.try_get("id")?;
            let step_status: String = s.try_get("status")?;
            if matches!(
                step_status.as_str(),
                "succeeded" | "failed" | "cancelled" | "blocked" | "skipped"
            ) {
                continue;
            }
            sqlx::query("UPDATE workflow_steps SET status = 'cancelled' WHERE id = ?")
                .bind(&step_id)
                .execute(&mut *tx)
                .await?;
            let runs = sqlx::query("SELECT task_id FROM role_runs WHERE step_run_id = ?")
                .bind(&step_id)
                .fetch_all(&mut *tx)
                .await?;
            for r in &runs {
                if let Ok(Some(task_id)) = r.try_get::<Option<String>, _>("task_id") {
                    sqlx::query(
                        "UPDATE tasks SET status = 'cancelled', assigned_attempt_id = NULL \
                         WHERE id = ? AND status = 'queued'",
                    )
                    .bind(&task_id)
                    .execute(&mut *tx)
                    .await?;
                    sqlx::query(
                        "UPDATE attempts SET cancel_requested = 1 WHERE task_id = ? \
                         AND status IN ('assigned','running','validating')",
                    )
                    .bind(&task_id)
                    .execute(&mut *tx)
                    .await?;
                }
            }
        }
        tx.commit().await?;
        Ok(true)
    }

    pub async fn set_node_degraded(&self, node_id: &str) -> Result<()> {
        sqlx::query("UPDATE nodes SET status = 'degraded' WHERE id = ? AND status != 'revoked'")
            .bind(node_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn retry_task(&self, task_id: &str) -> Result<bool> {
        let mut tx = begin_immediate(&self.pool).await?;
        let row = sqlx::query("SELECT status FROM tasks WHERE id = ?")
            .bind(task_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            let _ = tx.rollback().await;
            return Ok(false);
        };
        let status: String = row.try_get("status")?;
        if status == "failed" || status == "cancelled" {
            sqlx::query(
                "UPDATE tasks SET status = 'queued', finished_at = NULL, assigned_attempt_id = NULL WHERE id = ?",
            )
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(true);
        }
        let _ = tx.rollback().await;
        // Hardening P1 item 13: audit the rejected retry so a retry against a
        // non-terminal task (queued/running/succeeded) is traceable, with the
        // source state recorded and no payload.
        let _ = self
            .audit(
                "task",
                Some(task_id),
                "retry.rejected_nonterminal",
                Some(&status),
                None,
            )
            .await;
        Ok(false)
    }
}
