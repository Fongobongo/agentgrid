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
                return Ok(None);
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
                return Ok(None);
            }
        }
        let mut tx = self.pool.begin().await?;
        let cands = sqlx::query(
            "SELECT id, prompt, adapter, repository, timeout_secs, validation_command, base_commit, parent_acp_session_id, created_at, security_profile, network_mode FROM tasks \
             WHERE status = 'queued' AND (requested_node_id IS NULL OR requested_node_id = ?) \
             ORDER BY created_at ASC",
        )
        .bind(node_id)
        .fetch_all(&mut *tx)
        .await?;
        for c in &cands {
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

            let node = sqlx::query(
            "SELECT id, name, status, adapters, repositories, max_concurrency, active_attempts, last_heartbeat_at, agent_version, load_avg, free_disk_mb, unsafe_active, permission_interception, outbox_bytes, artifact_spool_bytes, outbox_rows, outbox_oldest_pending_age_ms, outbox_corruption_count, outbox_completion_rows, drained \
             FROM nodes WHERE id = ?",
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?;
            let Some(node) = node else {
                let _ = tx.rollback().await;
                return Ok(None);
            };
            let nv = row_to_node_view(&node);
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
                let _ = tx.rollback().await;
                return Ok(None);
            }
            // Observability: queued→assigned latency (Stage 2.5 ops).
            if let Ok(created) = chrono::DateTime::parse_from_rfc3339(&created_at) {
                let now_ms = chrono::Utc::now().timestamp_millis();
                let latency = (now_ms - created.timestamp_millis()).max(0) as u64;
                self.scheduler_latency_ms
                    .store(latency, std::sync::atomic::Ordering::Relaxed);
                self.scheduler_assignments
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
            sqlx::query("UPDATE nodes SET active_attempts = active_attempts + 1 WHERE id = ?")
                .bind(node_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;

            let upstream_refs = self.upstream_refs_for_task(&task_id).await?;
            let (upstream_commits, upstream_task_ids): (Vec<String>, Vec<String>) =
                upstream_refs.into_iter().unzip();
            return Ok(Some(Assignment {
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
                upstream_commits,
                upstream_task_ids,
            }));
        }

        // No queued task this node can run.
        let _ = tx.rollback().await;
        Ok(None)
    }

    async fn attempt_count(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::sqlite::Sqlite>,
        task_id: &str,
    ) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS c FROM attempts WHERE task_id = ?")
            .bind(task_id)
            .fetch_one(&mut **tx)
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
