//! Scheduler: atomic assignment + task eligibility. Extracted from `store.rs`.

use super::{
    iso_plus_secs, node_ineligibility, now_iso, row_to_node_view, Store, ACK_DEADLINE_SECS,
    ASSIGNMENT_LEASE_SECS,
};
use agentgrid_common::{Assignment, NodeEligibility, NodeView, TaskEligibility};
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

/// Task-level fields an `Assignment` is built from. Both construction sites
/// (fresh assignment in `try_assign_batch`, redelivery in
/// `unacked_assignments`) collect exactly these values — attempt identity
/// (attempt_id / fencing_token / number) is supplied separately by the
/// caller, and the constant defaults live in `finish_assignment` once.
struct AssignmentFields {
    repository: String,
    prompt: String,
    adapter: String,
    timeout_secs: i64,
    git_url: String,
    default_branch: String,
    validation_command: Option<String>,
    base_commit: Option<String>,
    parent_acp_session_id: Option<String>,
    network_mode: Option<String>,
    group_id: Option<String>,
    read_only: bool,
    eval_cases: Vec<String>,
    consensus_group_id: Option<String>,
    consensus_member: Option<String>,
    opencode_override: Option<String>,
    github_push: bool,
    github_repo: Option<String>,
    github_issue: Option<i64>,
    github_base_ref: Option<String>,
}

fn finish_assignment(
    attempt_id: String,
    fencing_token: String,
    task_id: String,
    number: i64,
    f: AssignmentFields,
) -> Assignment {
    Assignment {
        attempt_id,
        fencing_token,
        task_id,
        repository: f.repository,
        prompt: f.prompt,
        adapter: f.adapter,
        number: number as u32,
        timeout_secs: f.timeout_secs as u64,
        git_url: f.git_url,
        default_branch: f.default_branch,
        validation_command: f.validation_command,
        validation_timeout_secs: None,
        base_commit: f.base_commit,
        parent_acp_session_id: f.parent_acp_session_id,
        network_mode: f.network_mode,
        provenance: None,
        upstream_commits: Vec::new(),
        upstream_task_ids: Vec::new(),
        group_id: f.group_id,
        read_only: f.read_only,
        eval_cases: f.eval_cases,
        consensus_group_id: f.consensus_group_id,
        consensus_member: f.consensus_member,
        opencode_override: f
            .opencode_override
            .and_then(|s| serde_json::from_str::<agentgrid_common::OpencodeOverride>(&s).ok()),
        github_push: f.github_push,
        github_repo: f.github_repo,
        github_issue: f.github_issue,
        github_base_ref: f.github_base_ref,
    }
}

impl Store {
    pub async fn try_assign(&self, node_id: &str) -> Result<Option<Assignment>> {
        Ok(self.try_assign_batch(node_id, 1).await?.into_iter().next())
    }

    /// Attempts assigned to `node_id` that were never acked — the
    /// assignment message may have gone to a connection that died
    /// mid-delivery. The WS reconnect path redelivers these; without the
    /// pull they would sit `assigned` until the ack deadline.
    pub async fn unacked_assignments(&self, node_id: &str) -> Result<Vec<Assignment>> {
        let rows = sqlx::query(
            "SELECT a.id AS attempt_id, a.number, a.fencing_token, \
                    t.id AS task_id, t.prompt, t.adapter, t.repository, t.timeout_secs, \
                    t.validation_command AS task_validation, t.base_commit, \
                    t.parent_acp_session_id, t.network_mode, t.group_id, \
                    t.consensus_group_id, t.consensus_member, t.opencode_override, \
                    t.github_push, t.github_repo, t.github_issue, t.github_base_ref, \
                    r.git_url, r.default_branch, r.validation_command AS repo_validation \
             FROM attempts a \
             JOIN tasks t ON t.id = a.task_id \
             LEFT JOIN repositories r ON r.name = t.repository \
             WHERE a.node_id = ? AND a.status = 'assigned' \
             ORDER BY a.started_at ASC",
        )
        .bind(node_id)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::with_capacity(rows.len());
        for c in &rows {
            let task_id: String = c.try_get("task_id")?;
            let role = sqlx::query_scalar::<_, String>(
                "SELECT ws.role FROM role_runs rr \
                 JOIN workflow_steps ws ON ws.id = rr.step_run_id \
                 WHERE rr.task_id = ? LIMIT 1",
            )
            .bind(&task_id)
            .fetch_optional(&self.pool)
            .await?;
            let read_only = role.as_deref() == Some("verifier");
            let number: i64 = c.try_get("number")?;
            let eval_cases: Vec<String> = if number > 1 {
                sqlx::query_scalar(
                    "SELECT a.name FROM artifacts a \
                     JOIN attempts at ON at.id = a.attempt_id \
                     WHERE at.task_id = ? AND a.name LIKE 'eval-case-%' \
                     ORDER BY a.name",
                )
                .bind(&task_id)
                .fetch_all(&self.pool)
                .await?
            } else {
                Vec::new()
            };
            let repo_validation: Option<String> = c.try_get("repo_validation")?;
            let task_validation: Option<String> = c.try_get("task_validation")?;
            out.push(finish_assignment(
                c.try_get("attempt_id")?,
                c.try_get("fencing_token")?,
                task_id,
                number,
                AssignmentFields {
                    prompt: c.try_get("prompt")?,
                    adapter: c.try_get("adapter")?,
                    repository: c.try_get("repository")?,
                    timeout_secs: c.try_get("timeout_secs")?,
                    git_url: c
                        .try_get::<Option<String>, _>("git_url")?
                        .unwrap_or_default(),
                    default_branch: c
                        .try_get::<Option<String>, _>("default_branch")?
                        .unwrap_or_default(),
                    validation_command: task_validation.or(repo_validation),
                    base_commit: c.try_get("base_commit").ok().flatten(),
                    parent_acp_session_id: c.try_get("parent_acp_session_id").ok().flatten(),
                    network_mode: c.try_get("network_mode").ok().flatten(),
                    group_id: c.try_get("group_id").ok().flatten(),
                    read_only,
                    eval_cases,
                    consensus_group_id: c.try_get("consensus_group_id").ok().flatten(),
                    consensus_member: c.try_get("consensus_member").ok().flatten(),
                    opencode_override: c
                        .try_get::<Option<String>, _>("opencode_override")
                        .ok()
                        .flatten(),
                    github_push: c.try_get::<bool, _>("github_push").unwrap_or_default(),
                    github_repo: c.try_get("github_repo").ok().flatten(),
                    github_issue: c.try_get("github_issue").ok().flatten(),
                    github_base_ref: c.try_get("github_base_ref").ok().flatten(),
                },
            ));
        }
        Ok(out)
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
            "SELECT id, prompt, adapter, repository, timeout_secs, validation_command, base_commit, parent_acp_session_id, created_at, security_profile, network_mode, group_id, consensus_group_id, consensus_member, opencode_override, github_push, github_repo, github_issue, github_base_ref FROM tasks \
             WHERE status = 'queued' AND (requested_node_id IS NULL OR requested_node_id = ?) \
             ORDER BY created_at ASC",
        )
        .bind(node_id)
        .fetch_all(&mut *tx)
        .await?;

        let node = sqlx::query(
            "SELECT id, name, status, adapters, repositories, max_concurrency, active_attempts, last_heartbeat_at, agent_version, load_avg, free_disk_mb, mem_available_mb, unsafe_active, permission_interception, outbox_bytes, artifact_spool_bytes, outbox_rows, outbox_oldest_pending_age_ms, outbox_corruption_count, outbox_completion_rows, drained, active_rss_mib, max_rss_mib \
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
        // Plan 2.14 (#27): capacity-pressure gate. The scheduler refuses to
        // assign when the node reports `active_rss_mib + active_attempts *
        // 256` exceeds its `max_rss_mib` (a hard ceiling with a sane 1 GiB
        // default). The 256 MiB per-attempt forecast is a conservative
        // floor covering an LLM stream + adapter + git worktree.
        // Env `AGENTGRID_CAPACITY_PRESSURE=0` disables the gate — test rigs
        // and tiny dev machines may need to bypass it; production keeps it on.
        let gate_on = std::env::var("AGENTGRID_CAPACITY_PRESSURE")
            .map(|v| v != "0")
            .unwrap_or(true);
        if gate_on {
            let active_rss: i64 = node.try_get("active_rss_mib").unwrap_or(0);
            let max_rss: i64 = node.try_get("max_rss_mib").unwrap_or(1024);
            let forecast_per_attempt: i64 = 256; // MiB
                                                 // Use `min(limit, free_slots)` — that's the number of attempts
                                                 // we'd actually ship; we only need to back out if shipping them
                                                 // would exceed the node's hard memory ceiling.
            let free_slots_early = nv
                .max_concurrency
                .saturating_sub(nv.active_attempts)
                .min(limit as u32) as i64;
            let projected = active_rss + free_slots_early * forecast_per_attempt;
            if projected > max_rss {
                tx.commit().await?; // close the txn before logging
                sqlx::query(
                    "INSERT INTO metrics_capacity_pressure (at, node_id, threshold_mib, active_mib, forecast_mib) \
                     VALUES (datetime('now'), ?, ?, ?, ?)",
                )
                .bind(node_id)
                .bind(max_rss)
                .bind(active_rss)
                .bind(projected)
                .execute(&self.pool)
                .await?;
                tracing::info!(
                    node_id,
                    active_rss,
                    max_rss,
                    projected,
                    "rejected_due_to_pressure"
                );
                return Ok(Vec::new());
            }
        }
        // Host memory gate: refuse new work when the node knows it has too
        // little RAM left (MemAvailable). 0 = not reported -> admit (legacy
        // nodes, cold start); only a known-below-threshold value rejects.
        // Threshold configurable via AGENTGRID_MIN_FREE_MEM_MB (default
        // 1024 MiB ~= a claude-code session + adapter + git).
        {
            let mem_avail: i64 = node.try_get("mem_available_mb").unwrap_or(0);
            let min_free_mb: i64 = std::env::var("AGENTGRID_MIN_FREE_MEM_MB")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1024);
            if mem_avail > 0 && mem_avail < min_free_mb {
                let _ = tx.rollback().await;
                tracing::info!(
                    node_id,
                    mem_avail,
                    min_free_mb,
                    "rejected_due_to_low_host_memory"
                );
                return Ok(Vec::new());
            }
        }
        // Host disk gate: same contract as the memory gate — a node that
        // reports almost no free space on its workspace root gets no new
        // work (git clone + worktree would fail midway). 0 = unknown -> admit.
        {
            let disk_mb: i64 = node.try_get("free_disk_mb").unwrap_or(0);
            let min_disk_mb: i64 = std::env::var("AGENTGRID_MIN_FREE_DISK_MB")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(2048);
            if disk_mb > 0 && disk_mb < min_disk_mb {
                let _ = tx.rollback().await;
                tracing::info!(node_id, disk_mb, min_disk_mb, "rejected_due_to_low_disk");
                return Ok(Vec::new());
            }
        }
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
        // Audit follow-up (N+1 under the single-writer gate): every per-
        // candidate lookup below used to run its own query INSIDE the
        // BEGIN IMMEDIATE transaction (~4-5 statements x candidates while
        // holding the only writer permit). They are invariant during the
        // flip loop, so resolve them in bulk BEFORE it and keep only the
        // cheap CAS UPDATE + INSERT per candidate inside the txn.
        let cap_ids: Vec<String> = cands
            .iter()
            .take(cap)
            .map(|c| c.try_get::<String, _>("id"))
            .collect::<std::result::Result<_, _>>()?;
        let placeholders = |n: usize| {
            let mut s = String::new();
            for i in 0..n {
                if i > 0 {
                    s.push(',');
                }
                s.push('?');
            }
            s
        };
        let mut in_sql = String::new();
        in_sql.push_str(
            "SELECT rr.task_id AS tid, ws.role AS role FROM role_runs rr \
             JOIN workflow_steps ws ON ws.id = rr.step_run_id \
             WHERE rr.task_id IN (",
        );
        in_sql.push_str(&placeholders(cap_ids.len()));
        in_sql.push(')');
        let mut q = sqlx::query(sqlx::AssertSqlSafe(in_sql.as_str()));
        for id in &cap_ids {
            q = q.bind(id);
        }
        let mut roles: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        for r in q.fetch_all(&mut *tx).await? {
            roles.insert(r.try_get("tid")?, r.try_get("role")?);
        }

        let mut cnt_sql = String::new();
        cnt_sql.push_str("SELECT task_id AS tid, COUNT(*) AS c FROM attempts WHERE task_id IN (");
        cnt_sql.push_str(&placeholders(cap_ids.len()));
        cnt_sql.push_str(") GROUP BY task_id");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(cnt_sql.as_str()));
        for id in &cap_ids {
            q = q.bind(id);
        }
        let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for r in q.fetch_all(&mut *tx).await? {
            counts.insert(r.try_get("tid")?, r.try_get("c")?);
        }

        // Eval cases matter only for retries (next_number > 1); one query for
        // exactly those tasks.
        let retry_ids: Vec<&String> = cap_ids
            .iter()
            .filter(|id| counts.get(*id).copied().unwrap_or(0) + 1 > 1)
            .collect();
        let mut eval_cases_by_task: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        if !retry_ids.is_empty() {
            let mut ev_sql = String::new();
            ev_sql.push_str(
                "SELECT at.task_id AS tid, a.name AS name FROM artifacts a \
                 JOIN attempts at ON at.id = a.attempt_id \
                 WHERE at.task_id IN (",
            );
            ev_sql.push_str(&placeholders(retry_ids.len()));
            ev_sql.push_str(") AND a.name LIKE 'eval-case-%' ORDER BY a.name");
            let mut q = sqlx::query(sqlx::AssertSqlSafe(ev_sql.as_str()));
            for id in &retry_ids {
                q = q.bind(id);
            }
            for r in q.fetch_all(&mut *tx).await? {
                eval_cases_by_task
                    .entry(r.try_get::<String, _>("tid")?)
                    .or_default()
                    .push(r.try_get("name")?);
            }
        }

        // Repository rows (absent for plain-dir tasks) in one query.
        let repo_names: Vec<String> = cands
            .iter()
            .take(cap)
            .map(|c| c.try_get::<String, _>("repository"))
            .collect::<std::result::Result<_, _>>()?;
        struct RepoRow {
            git_url: String,
            default_branch: String,
            validation_command: Option<String>,
        }
        let mut repos: std::collections::HashMap<String, RepoRow> =
            std::collections::HashMap::new();
        if !repo_names.is_empty() {
            let mut rp_sql = String::new();
            rp_sql.push_str(
                "SELECT name, git_url, default_branch, validation_command FROM repositories \
                 WHERE name IN (",
            );
            rp_sql.push_str(&placeholders(repo_names.len()));
            rp_sql.push(')');
            let mut q = sqlx::query(sqlx::AssertSqlSafe(rp_sql.as_str()));
            for n in &repo_names {
                q = q.bind(n);
            }
            for r in q.fetch_all(&mut *tx).await? {
                repos.insert(
                    r.try_get("name")?,
                    RepoRow {
                        git_url: r.try_get("git_url")?,
                        default_branch: r.try_get("default_branch")?,
                        validation_command: r.try_get("validation_command")?,
                    },
                );
            }
        }

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
            let read_only = roles.get(&task_id).map(String::as_str) == Some("verifier");

            // Plan 2.9 (#20): consensus run tag — when the task was created
            // as part of an `--consensus N --models ...` batch the columns
            // carry the batch id + this member's adapter so downstream
            // observability (assignment payload) can show "vote 2/3 from
            // adapter claude" without an extra query.
            let consensus_group_id: Option<String> = c.try_get("consensus_group_id").ok().flatten();
            let consensus_member: Option<String> = c.try_get("consensus_member").ok().flatten();
            // Feature "opencode profiles": stored JSON text → typed override;
            // parse failure => None (malformed JSON degrades to no override,
            // the assignment path stays alive).

            // Plan 2.5 (#22b): on retry (attempt number > 1) ship every
            // eval-case artifact the previous attempts landed so the node
            // can probe the new fix against the accumulated suite. Naming
            // `eval-case-<attempt>-<n>.yaml` is deterministic, set by the CP
            // when it records a passed attempt.
            let next_number = counts.get(&task_id).copied().unwrap_or(0) + 1;
            let eval_cases = eval_cases_by_task
                .get(&task_id)
                .cloned()
                .unwrap_or_default();

            // Resolve repository git info (absent for plain-dir tasks).
            let (git_url, default_branch, validation_command) = match repos.get(&repository) {
                Some(r) => (
                    r.git_url.clone(),
                    r.default_branch.clone(),
                    r.validation_command.clone(),
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
            let number = next_number;
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
        .bind(number)
        .bind(node_id)
        .bind(&lease)
        .bind(&ack_deadline)
        .bind(&now)
        .bind(&fencing_token)
        .execute(&mut *tx)
        .await?;
            batch.push(Pending {
                assignment: finish_assignment(
                    attempt_id,
                    fencing_token,
                    task_id,
                    number,
                    AssignmentFields {
                        repository,
                        prompt,
                        adapter,
                        timeout_secs,
                        git_url,
                        default_branch,
                        validation_command: task_validation.or(validation_command),
                        base_commit,
                        parent_acp_session_id,
                        network_mode: network_mode.clone(),
                        group_id,
                        read_only,
                        eval_cases,
                        consensus_group_id,
                        consensus_member,
                        opencode_override: c
                            .try_get::<Option<String>, _>("opencode_override")
                            .ok()
                            .flatten(),
                        github_push: c.try_get::<bool, _>("github_push").unwrap_or_default(),
                        github_repo: c.try_get("github_repo").ok().flatten(),
                        github_issue: c.try_get("github_issue").ok().flatten(),
                        github_base_ref: c.try_get("github_base_ref").ok().flatten(),
                    },
                ),
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
            // Plan 2.10 (#21): on a retry (number > 1), the previous attempt
            // has already baked the BM25 resume digest as
            // `resume-context-<task>.md` via retry_task → bake_resume_digest.
            // Ship its name with the assignment; the node's runner pulls the
            // bytes via /v1/node/tasks/{t}/artifacts/{name} at start and
            // inlines them into the agent prompt. Name-only keeps the
            // scheduler payload tiny; bytes are fetched on demand.
            if assignment.number > 1 {
                let resume_name = format!("resume-context-{}.md", assignment.task_id);
                let exists: bool = sqlx::query_scalar::<_, i64>(
                    "SELECT COUNT(*) FROM artifacts a JOIN attempts at ON at.id = a.attempt_id \
                     WHERE at.task_id = ? AND a.name = ?",
                )
                .bind(&assignment.task_id)
                .bind(&resume_name)
                .fetch_one(&self.pool)
                .await?
                    > 0;
                if exists {
                    assignment.prompt.push_str(&format!(
                        "\n\n## Resume digest\nFetch `{resume_name}` via the artifacts endpoint and inline it. Top-3 BM25 extracts from the previous attempt's events.\n"
                    ));
                }
            }
            out.push(assignment);
        }
        Ok(out)
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
