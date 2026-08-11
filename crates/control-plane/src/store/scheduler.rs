//! Scheduler: atomic assignment + task eligibility. Extracted from `store.rs`.

use super::{
    iso_plus_secs, node_ineligibility, now_iso, row_to_node_view, Store, ACK_DEADLINE_SECS,
    ASSIGNMENT_LEASE_SECS,
};
use agentgrid_common::{Assignment, NodeEligibility, NodeView, TaskEligibility};
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

impl Store {
    pub async fn try_assign(&self, node_id: &str) -> Result<Option<Assignment>> {
        Ok(self.try_assign_batch(node_id, 1).await?.into_iter().next())
    }

    /// Plan 0.3 item 1.2: assign up to `limit` queued tasks to this node in a
    /// single `BEGIN IMMEDIATE` transaction (was: one transaction per task).
    /// The batch is capped by the node's free concurrency slots, so a poll
    /// fills the node instead of one slot per round trip.
    pub async fn try_assign_batch(&self, node_id: &str, limit: usize) -> Result<Vec<Assignment>> {
        // Hardening P1 item 15: critical-disk watermark — refuse NEW
        // assignments when the artifact-root filesystem is nearly full, so the
        // disk cannot be driven to zero by queued work. Node-side low-disk
        // (heartbeat) already degrades the node; this is the control-plane's
        // own ceiling. Env `AGENTGRID_DISK_CRITICAL_MB` (default 512 MiB).
        {
            let crit_mb = std::env::var("AGENTGRID_DISK_CRITICAL_MB")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(512);
            let free = self.free_bytes() / (1024 * 1024);
            if crit_mb > 0 && free < crit_mb {
                tracing::warn!(
                    node_id,
                    free_mb = free,
                    crit_mb,
                    "critical disk watermark reached; refusing new assignments"
                );
                return Ok(Vec::new());
            }
        }
        // Hardening P2 item 37: a drained node stops receiving NEW assignments
        // (in-flight attempts keep running; heartbeat stays online).
        {
            let drained: i64 = sqlx::query_scalar("SELECT drained FROM nodes WHERE id = ?")
                .bind(node_id)
                .fetch_optional(&self.pool)
                .await?
                .unwrap_or(0);
            if drained != 0 {
                tracing::info!(node_id, "node is drained; skipping assignment");
                return Ok(Vec::new());
            }
        }
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut tx = self.write_txn().await?;
        let cands = sqlx::query(
            "SELECT id, prompt, adapter, repository, timeout_secs, validation_command, base_commit, parent_acp_session_id, created_at, security_profile, network_mode, group_id, consensus_group_id, consensus_member FROM tasks \
             WHERE status = 'queued' AND (requested_node_id IS NULL OR requested_node_id = ?) \
             ORDER BY created_at ASC",
        )
        .bind(node_id)
        .fetch_all(&mut *tx)
        .await?;

        let node = sqlx::query(
            "SELECT id, name, status, adapters, repositories, max_concurrency, active_attempts, last_heartbeat_at, agent_version, load_avg, free_disk_mb, unsafe_active, permission_interception, outbox_bytes, artifact_spool_bytes, outbox_rows, outbox_oldest_pending_age_ms, outbox_corruption_count, outbox_completion_rows, drained \
             FROM nodes WHERE id = ?",
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(node) = node else {
            let _ = tx.rollback().await;
            return Ok(Vec::new());
        };
        let nv = row_to_node_view(&node);
        // Batch cap: the node's free concurrency slots (plan 0.3 1.2).
        let free_slots = nv.max_concurrency.saturating_sub(nv.active_attempts) as usize;
        let cap = limit.min(free_slots);
        if cap == 0 {
            let _ = tx.rollback().await;
            return Ok(Vec::new());
        }

        struct Pending {
            assignment: Assignment,
            created_at: String,
        }
        let mut batch: Vec<Pending> = Vec::new();
        for c in &cands {
            if batch.len() >= cap {
                break;
            }
            let task_id: String = c.try_get("id")?;
            let prompt: String = c.try_get("prompt")?;
            let adapter: String = c.try_get("adapter")?;
            let repository: String = c.try_get("repository")?;
            let timeout_secs: i64 = c.try_get("timeout_secs")?;
            let task_validation: Option<String> = c.try_get("validation_command")?;
            let base_commit: Option<String> = c.try_get("base_commit").ok().flatten();
            let parent_acp_session_id: Option<String> =
                c.try_get("parent_acp_session_id").ok().flatten();
            let created_at: String = c.try_get("created_at")?;
            let security_profile: Option<String> = c.try_get("security_profile").ok().flatten();
            let network_mode: Option<String> = c.try_get("network_mode").ok().flatten();
            let group_id: Option<String> = c.try_get("group_id").ok().flatten();

            // Plan 2.4 (#22a): if this task is a workflow step with role
            // `verifier`, mark the assignment read-only so the node bind-
            // mounts the worktree with `:ro`. A verifier validates; it must
            // not silently edit the code it is checking.
            let role = sqlx::query_scalar::<_, String>(
                "SELECT ws.role FROM role_runs rr \
                 JOIN workflow_steps ws ON ws.id = rr.step_run_id \
                 WHERE rr.task_id = ? LIMIT 1",
            )
            .bind(&task_id)
            .fetch_optional(&mut *tx)
            .await?;
            let read_only = role.as_deref() == Some("verifier");

            // Plan 2.9 (#20): consensus run tag — when the task was created
            // as part of an `--consensus N --models ...` batch the columns
            // carry the batch id + this member's adapter so downstream
            // observability (assignment payload) can show "vote 2/3 from
            // adapter claude" without an extra query.
            let consensus_group_id: Option<String> = c.try_get("consensus_group_id").ok().flatten();
            let consensus_member: Option<String> = c.try_get("consensus_member").ok().flatten();

            // Plan 2.5 (#22b): on retry (attempt number > 1) ship every
            // eval-case artifact the previous attempts landed so the node
            // can probe the new fix against the accumulated suite. Naming
            // `eval-case-<attempt>-<n>.yaml` is deterministic, set by the CP
            // when it records a passed attempt.
            let next_number = self.attempt_count(&mut tx, &task_id).await? + 1;
            let eval_cases: Vec<String> = if next_number > 1 {
                sqlx::query_scalar(
                    "SELECT a.name FROM artifacts a \
                     JOIN attempts at ON at.id = a.attempt_id \
                     WHERE at.task_id = ? AND a.name LIKE 'eval-case-%' \
                     ORDER BY a.name",
                )
                .bind(&task_id)
                .fetch_all(&mut *tx)
                .await?
            } else {
                Vec::new()
            };

            // Resolve repository git info (absent for plain-dir tasks).
            let repo = sqlx::query(
                "SELECT git_url, default_branch, validation_command FROM repositories WHERE name = ?",
            )
            .bind(&repository)
            .fetch_optional(&mut *tx)
            .await?;
            let (git_url, default_branch, validation_command) = match repo {
                Some(r) => (
                    r.try_get::<String, _>("git_url")?,
                    r.try_get::<String, _>("default_branch")?,
                    r.try_get::<Option<String>, _>("validation_command")?,
                ),
                None => (String::new(), String::new(), None),
            };

            let inelig = node_ineligibility(
                &nv,
                &repository,
                &adapter,
                security_profile.as_deref(),
                network_mode.as_deref(),
            );
            if !inelig.is_empty() {
                continue;
            }

            let attempt_id = Uuid::new_v4().to_string();
            // Hardening P0 item 8: a fresh fencing token per assignment. The
            // node echoes it back on every mutation; the CP rejects a stale
            // token (reassigned/lost) with 409.
            let fencing_token = Uuid::new_v4().to_string();
            let number = self.attempt_count(&mut tx, &task_id).await? + 1;
            let lease = iso_plus_secs(ASSIGNMENT_LEASE_SECS);
            let ack_deadline = iso_plus_secs(ACK_DEADLINE_SECS);
            let now = now_iso();

            let affected = sqlx::query(
            "UPDATE tasks SET status = 'assigned', assigned_attempt_id = ? WHERE id = ? AND status = 'queued'",
        )
        .bind(&attempt_id)
        .bind(&task_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
            if affected != 1 {
                continue;
            }
            sqlx::query(
            "INSERT INTO attempts (id, task_id, number, node_id, status, lease_expires_at, ack_deadline, started_at, fencing_token) \
             VALUES (?, ?, ?, ?, 'assigned', ?, ?, ?, ?)",
        )
        .bind(&attempt_id)
        .bind(&task_id)
        .bind(number as i64)
        .bind(node_id)
        .bind(&lease)
        .bind(&ack_deadline)
        .bind(&now)
        .bind(&fencing_token)
        .execute(&mut *tx)
        .await?;
            batch.push(Pending {
                assignment: Assignment {
                    attempt_id,
                    fencing_token,
                    task_id,
                    repository,
                    prompt,
                    adapter,
                    number: number as u32,
                    timeout_secs: timeout_secs as u64,
                    git_url,
                    default_branch,
                    validation_command: task_validation.or(validation_command),
                    validation_timeout_secs: None,
                    base_commit,
                    parent_acp_session_id,
                    network_mode: network_mode.clone(),
                    provenance: None,
                    upstream_commits: Vec::new(),
                    upstream_task_ids: Vec::new(),
                    group_id,
                    read_only,
                    eval_cases,
                    consensus_group_id,
                    consensus_member,
                },
                created_at,
            });
        }

        if batch.is_empty() {
            let _ = tx.rollback().await;
            return Ok(Vec::new());
        }
        sqlx::query("UPDATE nodes SET active_attempts = active_attempts + ? WHERE id = ?")
            .bind(batch.len() as i64)
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        // Post-commit work: upstream refs (read-only) + latency observability.
        let mut out = Vec::with_capacity(batch.len());
        for p in batch {
            // Observability: queued→assigned latency (Stage 2.5 ops).
            if let Ok(created) = chrono::DateTime::parse_from_rfc3339(&p.created_at) {
                let now_ms = chrono::Utc::now().timestamp_millis();
                let latency = (now_ms - created.timestamp_millis()).max(0) as u64;
                self.scheduler_latency_ms
                    .store(latency, std::sync::atomic::Ordering::Relaxed);
                self.scheduler_assignments
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let mut assignment = p.assignment;
            let upstream_refs = self.upstream_refs_for_task(&assignment.task_id).await?;
            let (upstream_commits, upstream_task_ids): (Vec<String>, Vec<String>) =
                upstream_refs.into_iter().unzip();
            assignment.upstream_commits = upstream_commits;
            assignment.upstream_task_ids = upstream_task_ids;
            // Plan 2.8 (#19): prepend the top approved repo-learnings to the
            // attempt prompt. Small cap keeps the prompt from drowning; all
            // rows are `approved = 1` because the human-review gate lives in
            // the store query.
            if let Ok(learnings) = self.top_approved_for_repo(&assignment.repository, 5).await {
                if !learnings.is_empty() {
                    let mut block = String::from("\n\n## Repository instincts (human-approved)\n");
                    for l in &learnings {
                        block.push_str(&format!("- (conf {:.2}) {}\n", l.confidence, l.statement));
                    }
                    assignment.prompt.push_str(&block);
                }
            }
            out.push(assignment);
        }
        Ok(out)
    }

    async fn attempt_count(&self, tx: &mut sqlx::SqliteConnection, task_id: &str) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS c FROM attempts WHERE task_id = ?")
            .bind(task_id)
            .fetch_one(&mut *tx)
            .await?;
        Ok(row.try_get::<i64, _>("c")?)
    }

    /// Stage 2.4: per-node eligibility for a task plus a `no_eligible_nodes`
    /// summary (why it stays queued). Returns None if the task does not exist.
    pub async fn task_eligibility(&self, task_id: &str) -> Result<Option<TaskEligibility>> {
        let row =
            sqlx::query("SELECT repository, adapter, requested_node_id, security_profile, network_mode FROM tasks WHERE id = ?")
                .bind(task_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let repository: String = row.try_get("repository")?;
        let adapter: String = row.try_get("adapter")?;
        let requested: Option<String> = row.try_get("requested_node_id")?;
        let security_profile: Option<String> = row.try_get("security_profile").ok().flatten();
        let network_mode: Option<String> = row.try_get("network_mode").ok().flatten();

        let all = self.list_nodes(None, None).await?;
        let considered: Vec<NodeView> = match &requested {
            Some(id) => all.into_iter().filter(|n| &n.id == id).collect(),
            None => all,
        };

        let mut nodes = Vec::new();
        for n in &considered {
            let reasons = node_ineligibility(
                n,
                &repository,
                &adapter,
                security_profile.as_deref(),
                network_mode.as_deref(),
            );
            nodes.push(NodeEligibility {
                node_id: n.id.clone(),
                status: n.status,
                eligible: reasons.is_empty(),
                reasons,
            });
        }

        let no_eligible_nodes = if nodes.iter().any(|n| n.eligible) {
            Vec::new()
        } else {
            let mut seen = std::collections::HashSet::new();
            let mut out = Vec::new();
            for n in &nodes {
                for r in &n.reasons {
                    if seen.insert(r.clone()) {
                        out.push(r.clone());
                    }
                }
            }
            if out.is_empty() {
                out.push(match &requested {
                    Some(id) => format!("requested node {id} not registered"),
                    None => "no nodes registered".to_string(),
                });
            }
            out
        };

        Ok(Some(TaskEligibility {
            task_id: task_id.to_string(),
            no_eligible_nodes,
            nodes,
        }))
    }
}
