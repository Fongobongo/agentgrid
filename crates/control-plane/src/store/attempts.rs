//! Attempt lifecycle: completion, agent sessions, ack, cancel, retry.
//! Extracted from `store.rs`.

use super::{
    attempt_status_str, from_snake, iso_plus_secs, now_iso, status_str, Store, StoreTransitionError,
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
        let mut tx = self.write_txn().await?;
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
        // Competitor plan 1.1 (diff review UI): on a successful completion,
        // create a pending patch-review approval so the operator must
        // acknowledge the `changes.patch` before the task is treated as
        // accepted. The task status stays `succeeded`; the approval records
        // the human decision separately. 24h TTL keeps the page from
        // accumulating zombie reviews.
        // Plan 2.5 (#22b): eval-case capture. Read the task's
        // validation_command inside the txn (pre-commit) and stamp the
        // artifact AFTER commit — `save_artifact_bytes` uses `self.pool`
        // (a separate connection) and would block-deadlock on the open
        // `BEGIN IMMEDIATE` write txn otherwise.
        let mut eval_stamp_cmd: Option<String> = None;
        let mut eval_stamp_sha: Option<String> = None;
        if task_target == TaskStatus::Succeeded {
            if let Ok(row) = sqlx::query_scalar::<_, Option<String>>(
                "SELECT validation_command FROM tasks WHERE id = ?",
            )
            .bind(&task_id)
            .fetch_optional(&mut *tx)
            .await
            {
                if let Some(Some(cmd)) =
                    row.filter(|c| c.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false))
                {
                    eval_stamp_cmd = Some(cmd);
                    eval_stamp_sha = req.commit_sha.clone();
                }
            }
            let approval_id = Uuid::new_v4().to_string();
            let approval_now = now_iso();
            // 24h=86400s — reviews are not safety-critical, just human gates.
            let approval_expires = iso_plus_secs(86400);
            let perm = serde_json::json!({
                "kind": "patch_review",
                "task_id": task_id,
                "attempt_id": attempt_id,
            })
            .to_string();
            sqlx::query(
                "INSERT INTO approvals (id, task_id, attempt_id, session_id, permission, status, created_at, expires_at, step_run_id, scope) \
                 VALUES (?, ?, ?, NULL, ?, 'pending', ?, ?, NULL, 'task_patch_review')",
            )
            .bind(&approval_id)
            .bind(&task_id)
            .bind(attempt_id)
            .bind(&perm)
            .bind(&approval_now)
            .bind(&approval_expires)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        // Plan 2.5 (#22b): stamp the eval-case artifact on a free pool
        // connection AFTER commit so a failed case write can never wedge
        // the state transition above.
        if let Some(cmd) = eval_stamp_cmd {
            let case = format!(
                "id: eval-case-{attempt_id}\ncreated_by: attempt\nattempt_id: {attempt_id}\ncommit_sha: {}\ncommand: |\n  {}\n",
                eval_stamp_sha.as_deref().unwrap_or(""),
                cmd.replace('\n', "\n  ")
            );
            let name = format!("eval-case-{attempt_id}-0.yaml");
            if let Err(e) = self
                .save_artifact_bytes(
                    attempt_id,
                    &name,
                    case.as_bytes(),
                    Some("application/x-yaml"),
                    None,
                )
                .await
            {
                tracing::warn!("eval-case stamp failed for {attempt_id}: {e}");
            }
        }
        Ok(true)
    }

    /// Plan 2.9 (#20): consensus collapse. After a member attempt succeeds,
    /// check if every task in the consensus group has reached a terminal
    /// state. When they have, hash each successful member's `changes.patch`
    /// artifact and compare: agreement is a no-op; disagreement creates a
    /// `human-review` approval row pinned to one task id.
    /// Idempotent — already-collapsed groups are skipped. Caller must call
    /// AFTER the member transition committed (the helper queries the pool).
    pub async fn maybe_collapse_consensus(&self, task_id: &str) -> Result<()> {
        let Some(group_id): Option<String> = sqlx::query_scalar::<_, Option<String>>(
            "SELECT consensus_group_id FROM tasks WHERE id = ?",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?
        .flatten() else {
            return Ok(());
        };
        let rows = sqlx::query(
            "SELECT id, status, consensus_member FROM tasks \
             WHERE consensus_group_id = ? ORDER BY id",
        )
        .bind(&group_id)
        .fetch_all(&self.pool)
        .await?;
        if rows.is_empty() {
            return Ok(());
        }
        for r in &rows {
            let st: String = r.try_get("status")?;
            if !matches!(st.as_str(), "succeeded" | "failed" | "cancelled") {
                return Ok(());
            }
        }
        let mut patch_shas: Vec<(String, String)> = Vec::new();
        for r in &rows {
            let st: String = r.try_get("status")?;
            if st != "succeeded" {
                continue;
            }
            let member: Option<String> = r.try_get("consensus_member")?;
            let member_task_id: String = r.try_get("id")?;
            if let Some(meta) = self
                .read_artifact_meta(&member_task_id, "changes.patch")
                .await?
            {
                if let Some(sha) = meta.sha256 {
                    patch_shas.push((member.unwrap_or_default(), sha));
                }
            }
        }
        if patch_shas.len() < 2 {
            return Ok(());
        }
        let first_sha = patch_shas[0].1.clone();
        if !patch_shas.iter().any(|(_, s)| *s != first_sha) {
            return Ok(());
        }
        // Audit X-C7: the dedup COUNT used to be scoped to this task only,
        // and check+insert ran on the pool — two group members finishing
        // concurrently each passed their own count and inserted duplicate
        // disagreement approvals. Serialize count+insert under the global
        // write gate and scope the check to the whole consensus group.
        let mut tx = self.write_txn().await?;
        let existing: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM approvals WHERE permission LIKE '%' || ? || '%' \
             AND task_id IN (SELECT id FROM tasks WHERE consensus_group_id = ?)",
        )
        .bind(&group_id)
        .bind(&group_id)
        .fetch_one(&mut *tx)
        .await?;
        if existing > 0 {
            drop(tx);
            return Ok(());
        }
        let approval_id = Uuid::new_v4().to_string();
        let now = now_iso();
        let expires = iso_plus_secs(86400);
        let perm = serde_json::json!({
            "kind": "consensus_disagreement",
            "group": group_id,
            "members": patch_shas.iter().map(|(m, _)| m.clone()).collect::<Vec<_>>(),
        })
        .to_string();
        sqlx::query(
            "INSERT INTO approvals (id, task_id, attempt_id, session_id, permission, status, created_at, expires_at, step_run_id, scope) \
             VALUES (?, ?, NULL, NULL, ?, 'pending', ?, ?, NULL, 'consensus_disagreement')",
        )
        .bind(&approval_id)
        .bind(task_id)
        .bind(&perm)
        .bind(&now)
        .bind(&expires)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
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
        let mut tx = self.write_txn().await?;
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
            // A `cancelled` attempt was reverted by the lease reaper (or an
            // explicit cancel) and the task requeued/reassigned — a late ack
            // from the stale holder must be rejected, not reported as success:
            // otherwise the node treats the 200 as "lease mine" and runs the
            // whole agent unfenced alongside the new holder.
            return Ok(matches!(
                status.as_str(),
                "running" | "succeeded" | "failed" | "lost" | "validating"
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

    /// A node explicitly refused an assignment (ws ack `ok=false`): the
    /// attempt dies terminal (`lost`) and the task `failed` — the legal
    /// NodeLost pairing from `assigned`. Terminal, NOT a requeue: a requeue
    /// would hot-loop assign→reject on the same node (the scheduler refills
    /// the freed slot immediately), and the node-protocol doc promises
    /// "ok=false → attempt immediately failed". The old path called
    /// `complete_attempt` with a Fail transition on an `assigned` attempt —
    /// an invalid transition the handler swallowed, leaving the attempt
    /// assigned for the 30s reaper and cycling forever.
    pub async fn reject_assignment(&self, attempt_id: &str, reason: &str) -> Result<bool> {
        let mut tx = self.write_txn().await?;
        let row = sqlx::query("SELECT task_id, node_id, status FROM attempts WHERE id = ?")
            .bind(attempt_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            let _ = tx.rollback().await;
            return Ok(false);
        };
        let task_id: String = row.try_get("task_id")?;
        let node_id: String = row.try_get("node_id")?;
        let status: String = row.try_get("status")?;
        if status != "assigned" {
            let _ = tx.rollback().await;
            return Ok(false);
        }
        let now = now_iso();
        let moved = sqlx::query(
            "UPDATE attempts SET status = 'lost', finished_at = ? \
             WHERE id = ? AND status = 'assigned'",
        )
        .bind(&now)
        .bind(attempt_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if moved != 1 {
            let _ = tx.rollback().await;
            return Ok(false);
        }
        sqlx::query(
            "UPDATE tasks SET status = 'failed', finished_at = ?, \
             assigned_attempt_id = NULL \
             WHERE id = ? AND status = 'assigned'",
        )
        .bind(&now)
        .bind(&task_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE nodes SET active_attempts = MAX(0, active_attempts - 1) WHERE id = ?")
            .bind(&node_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        let _ = self
            .audit(
                "attempt",
                Some(attempt_id),
                "attempt.node_rejected",
                Some(reason),
                None,
            )
            .await;
        Ok(true)
    }

    pub async fn begin_validate(&self, attempt_id: &str) -> Result<bool> {
        let mut tx = self.write_txn().await?;
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
        let mut tx = self.write_txn().await?;
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

    /// Plan 0.3 2.2: live attempts of a task with cancel requested, with
    /// their owning node — the WS push targets (poll nodes discover the
    /// cancel via the attempt cancel probe instead).
    pub async fn cancel_targets_for_task(&self, task_id: &str) -> Result<Vec<(String, String)>> {
        let rows = sqlx::query(
            "SELECT id, node_id FROM attempts WHERE task_id = ? AND cancel_requested = 1 \
             AND status IN ('assigned','running','validating')",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                (
                    r.try_get::<String, _>("id").unwrap_or_default(),
                    r.try_get::<String, _>("node_id").unwrap_or_default(),
                )
            })
            .collect())
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
        let mut tx = self.write_txn().await?;
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
        let mut tx = self.write_txn().await?;
        let row = sqlx::query("SELECT status, prompt FROM tasks WHERE id = ?")
            .bind(task_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            let _ = tx.rollback().await;
            return Ok(false);
        };
        let status: String = row.try_get("status")?;
        let prompt: String = row.try_get("prompt")?;
        if status == "failed" || status == "cancelled" {
            sqlx::query(
                "UPDATE tasks SET status = 'queued', finished_at = NULL, assigned_attempt_id = NULL WHERE id = ?",
            )
            .bind(task_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            // Plan 2.10 (#21): build the "context ejector" digest — top-3 BM25
            // snippets from the previous attempt's events relevant to the
            // original prompt. Builder runs outside the write txn (read-only
            // pool query) so the retry assignment itself never blocks on the
            // FTS scan.
            if let Err(e) = self.bake_resume_digest(task_id, &prompt).await {
                tracing::warn!("resume digest bake failed: {e}");
            }
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

    /// Plan 2.10 (#21): scan `task_events` for the rows most relevant to the
    /// original prompt (BM25 over events_fts) and persist the digest as a
    /// `resume-context-<task_id>.md` artifact on the LATEST attempt so the
    /// retry assignment can fetch it without recomputing at poll time. The
    /// tracked metric (`tokens_avoided_bytes`) is the delta between the
    /// previous attempt's full event byte count and the size of the digest.
    /// When no events match yet (first try), no artifact is written.
    async fn bake_resume_digest(&self, task_id: &str, prompt: &str) -> Result<()> {
        let latest: Option<String> = sqlx::query_scalar(
            "SELECT id FROM attempts WHERE task_id = ? ORDER BY number DESC LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(attempt_id) = latest else {
            return Ok(());
        };

        // Total bytes of the previous attempt's task events (pre-ejection).
        let full_bytes: i64 = sqlx::query_scalar(
            "SELECT COALESCE(SUM(LENGTH(payload)), 0) FROM task_events WHERE attempt_id = ?",
        )
        .bind(&attempt_id)
        .fetch_one(&self.pool)
        .await?;

        // Top-3 BM25 fragments. Porter stemming + fold handles case folding;
        // quote the prompt tokens so FTS doesn't split on spaces.
        let query = prompt
            .split_whitespace()
            .take(10)
            .map(|t| format!("\"{}\"", t.trim_matches(|c: char| !c.is_alphanumeric())))
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" OR ");
        if query.is_empty() {
            return Ok(());
        }
        // Top-3 BM25 fragments scoped to the previous attempt. FTS5
        // column-scoped MATCH — `events_fts.attempt_id = ?` triggers the
        // T.payload_text lookup on the content table (it failed when our
        // migration used content = 'task_events'), but `attempt_id : 'foo'`
        // against events_fts alone uses only the index-side rows. Two legs:
        // MATCH for the ranking, then a JOIN-less WHERE on
        // events_fts.attempt_id for the attempt filter.
        //
        // Note `attempt_id` is a reserved column on events_fts — FTS5
        let rows: Vec<(String, f64)> = sqlx::query_as(
            "SELECT payload_text, bm25(events_fts) AS score \
             FROM events_fts \
             WHERE events_fts.attempt_id = ? AND payload_text MATCH ? \
             ORDER BY score LIMIT 3",
        )
        .bind(&attempt_id)
        .bind(&query)
        .fetch_all(&self.pool)
        .await?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut digest = String::from("# Resume digest (BM25 top-3)\n\n");
        for (frag, _score) in &rows {
            // Cap each fragment at 1 KiB so pathological events cannot blow
            // the retry prompt past the validation budget.
            let frag = if frag.len() > 1024 {
                &frag[..1024]
            } else {
                frag
            };
            digest.push_str("---\n");
            digest.push_str(frag);
            digest.push('\n');
        }
        let digest_bytes = digest.len() as i64;
        let avoided = full_bytes.saturating_sub(digest_bytes);
        let name = format!("resume-context-{task_id}.md");
        self.save_artifact_bytes(
            &attempt_id,
            &name,
            digest.as_bytes(),
            Some("text/markdown"),
            None,
        )
        .await?;
        sqlx::query("UPDATE attempts SET tokens_avoided_bytes = ? WHERE id = ?")
            .bind(avoided)
            .bind(&attempt_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

impl Store {
    /// Plan 1.3 (#13): fetch a single attempt with the owning task's prompt
    /// (so `ag resume` can inherit context without a second query).
    pub async fn show_attempt(&self, id: &str) -> Result<Option<agentgrid_common::AttemptView>> {
        let row = sqlx::query(
            "SELECT attempts.id, attempts.task_id, attempts.number, attempts.node_id, attempts.status, \
                    attempts.started_at, attempts.finished_at, attempts.commit_sha, attempts.exit_code, \
                    attempts.error_code, tasks.prompt, tasks.adapter, attempts.acp_session_id \
             FROM attempts JOIN tasks ON tasks.id = attempts.task_id \
             WHERE attempts.id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else { return Ok(None) };
        use agentgrid_common::*;
        let status: AttemptStatus =
            from_snake(&row.try_get::<String, _>("status").unwrap_or_default())
                .unwrap_or(AttemptStatus::Assigned);
        Ok(Some(AttemptView {
            id: row.try_get("id").unwrap_or_default(),
            task_id: row.try_get("task_id").unwrap_or_default(),
            number: row.try_get::<i64, _>("number").unwrap_or_default() as u32,
            node_id: row.try_get("node_id").unwrap_or_default(),
            status,
            started_at: row.try_get("started_at").unwrap_or_default(),
            finished_at: row.try_get("finished_at").ok().flatten(),
            commit_sha: row.try_get("commit_sha").ok().flatten(),
            exit_code: row
                .try_get::<Option<i64>, _>("exit_code")
                .ok()
                .flatten()
                .map(|v| v as i32),
            error_code: row.try_get("error_code").ok().flatten(),
            prompt: row.try_get("prompt").unwrap_or_default(),
            adapter: row.try_get("adapter").unwrap_or_default(),
            parent_acp_session_id: row.try_get("acp_session_id").ok().flatten(),
        }))
    }
}

impl Store {
    /// Plan 1.6 (#3b): the owning task's `(prompt, repository, adapter)` for
    /// an attempt, so "send for rework" can build a new task with the original
    /// prompt + repo. Returns `None` if the attempt does not exist.
    pub async fn attempt_origin(&self, id: &str) -> Result<Option<(String, String, String)>> {
        let row = sqlx::query(
            "SELECT tasks.prompt, tasks.repository, tasks.adapter \
             FROM attempts JOIN tasks ON tasks.id = attempts.task_id \
             WHERE attempts.id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| {
            (
                r.try_get::<String, _>("prompt").unwrap_or_default(),
                r.try_get::<String, _>("repository").unwrap_or_default(),
                r.try_get::<String, _>("adapter").unwrap_or_default(),
            )
        }))
    }

    pub async fn add_annotation(
        &self,
        attempt_id: &str,
        req: &agentgrid_common::CreateAnnotationRequest,
    ) -> Result<agentgrid_common::PatchAnnotation> {
        let id = format!("anno-{}", uuid::Uuid::new_v4());
        let created = now_iso();
        sqlx::query(
            "INSERT INTO patch_annotations (id, attempt_id, file, line_start, line_end, comment, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(attempt_id)
        .bind(&req.file)
        .bind(req.line_start)
        .bind(req.line_end)
        .bind(&req.comment)
        .bind(&created)
        .execute(&self.pool)
        .await?;
        Ok(agentgrid_common::PatchAnnotation {
            id,
            attempt_id: attempt_id.to_string(),
            file: req.file.clone(),
            line_start: req.line_start,
            line_end: req.line_end,
            comment: req.comment.clone(),
            created_at: created,
        })
    }

    pub async fn list_annotations(
        &self,
        attempt_id: &str,
    ) -> Result<Vec<agentgrid_common::PatchAnnotation>> {
        let rows = sqlx::query(
            "SELECT id, attempt_id, file, line_start, line_end, comment, created_at \
             FROM patch_annotations WHERE attempt_id = ? ORDER BY created_at ASC",
        )
        .bind(attempt_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| agentgrid_common::PatchAnnotation {
                id: r.try_get("id").unwrap_or_default(),
                attempt_id: r.try_get("attempt_id").unwrap_or_default(),
                file: r.try_get("file").unwrap_or_default(),
                line_start: r.try_get::<Option<i64>, _>("line_start").ok().flatten(),
                line_end: r.try_get::<Option<i64>, _>("line_end").ok().flatten(),
                comment: r.try_get("comment").unwrap_or_default(),
                created_at: r.try_get("created_at").unwrap_or_default(),
            })
            .collect())
    }
}
