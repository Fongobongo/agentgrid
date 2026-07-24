//! Workflow schedules, runs, step projections, and the background ticker
//! (Stage 6 / 8 / 14). Extracted from `store.rs`.

use super::{
    from_snake, iso_to_unix, now_iso, parse_autonomy_level, role_str, role_str_status,
    schedule_from_row, unix_to_iso, workflow_budget_from_col,
};
use crate::Store;
use agentgrid_common::{
    CreateTaskRequest, TaskStatus, WorkflowBudget, WorkflowRole, WorkflowRun, WorkflowRunStatus,
    WorkflowSchedule, WorkflowScheduleCreate, WorkflowStep, WorkflowStepRun, WorkflowStepStatus,
    WorkflowTemplate,
};
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

impl Store {
    /// Create a workflow template. Validates the DAG up front so a broken
    /// template can never be persisted.
    pub async fn create_workflow_template(
        &self,
        name: &str,
        steps: &[WorkflowStep],
        budget: &Option<WorkflowBudget>,
    ) -> Result<WorkflowTemplate> {
        crate::workflow::validate_workflow_dag(steps)
            .map_err(|e| anyhow::anyhow!("invalid workflow DAG: {e:?}"))?;
        let id = format!("wft-{}", Uuid::new_v4());
        let created_at = now_iso();
        let steps_json = serde_json::to_string(steps)?;
        let budget_json = match budget {
            Some(b) => Some(serde_json::to_string(b)?),
            None => None,
        };
        sqlx::query(
            "INSERT INTO workflow_templates (id, name, steps_json, budget_json, created_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(name)
        .bind(&steps_json)
        .bind(&budget_json)
        .bind(&created_at)
        .execute(&self.pool)
        .await?;
        Ok(WorkflowTemplate {
            id,
            name: name.to_string(),
            steps: steps.to_vec(),
            budget: budget.clone(),
            created_at,
        })
    }

    pub async fn get_workflow_template(&self, id: &str) -> Result<Option<WorkflowTemplate>> {
        let row = sqlx::query(
            "SELECT id, name, steps_json, budget_json, created_at FROM workflow_templates WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| WorkflowTemplate {
            id: r.try_get("id").unwrap_or_default(),
            name: r.try_get("name").unwrap_or_default(),
            steps: serde_json::from_str(&r.try_get::<String, _>("steps_json").unwrap_or_default())
                .unwrap_or_default(),
            budget: workflow_budget_from_col("budget_json", &r),
            created_at: r.try_get("created_at").unwrap_or_default(),
        }))
    }

    pub async fn list_workflow_templates(&self) -> Result<Vec<WorkflowTemplate>> {
        let rows = sqlx::query(
            "SELECT id, name, steps_json, budget_json, created_at FROM workflow_templates ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| WorkflowTemplate {
                id: r.try_get("id").unwrap_or_default(),
                name: r.try_get("name").unwrap_or_default(),
                steps: serde_json::from_str(
                    &r.try_get::<String, _>("steps_json").unwrap_or_default(),
                )
                .unwrap_or_default(),
                budget: workflow_budget_from_col("budget_json", r),
                created_at: r.try_get("created_at").unwrap_or_default(),
            })
            .collect())
    }

    /// Instantiate a template into a run. Creates one step instance per
    /// template step and one role-run per step (for its declared role).
    pub async fn create_workflow_run(
        &self,
        template_id: &str,
        context: Option<&str>,
        repository: Option<&str>,
        base_commit: Option<&str>,
    ) -> Result<WorkflowRun> {
        let tpl = self
            .get_workflow_template(template_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown workflow template {template_id}"))?;
        let run_id = format!("wfr-{}", Uuid::new_v4());
        let created_at = now_iso();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO workflow_runs (id, template_id, status, context, repository, base_commit, created_at, finished_at) \
             VALUES (?, ?, 'pending', ?, ?, ?, ?, NULL)",
        )
        .bind(&run_id)
        .bind(template_id)
        .bind(context)
        .bind(repository)
        .bind(base_commit)
        .bind(&created_at)
        .execute(&mut *tx)
        .await?;
        for step in &tpl.steps {
            let step_run_id = format!("wfs-{}", Uuid::new_v4());
            let depends_json = serde_json::to_string(&step.depends_on)?;
            sqlx::query(
                "INSERT INTO workflow_steps \
                 (id, run_id, step_id, prompt, depends_on, role, adapter, requested_node_id, base_commit, retryable, max_attempts, attempts, status, created_at, expandable) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?)",
            )
            .bind(&step_run_id)
            .bind(&run_id)
            .bind(&step.id)
            .bind(&step.prompt)
            .bind(&depends_json)
            .bind(role_str(step.role))
            .bind(&step.adapter)
            .bind(step.requested_node_id.as_deref())
            .bind(step.base_commit.as_deref())
            .bind(step.retryable.map(|b| if b { 1i64 } else { 0 }))
            .bind(step.max_attempts.map(|m| m as i64))
            .bind(0i64)
            .bind(&created_at)
            .bind(step.expandable.map(|b| if b { 1i64 } else { 0 }))
            .execute(&mut *tx)
            .await?;
            let role_run_id = format!("wrr-{}", Uuid::new_v4());
            sqlx::query(
                "INSERT INTO role_runs (id, step_run_id, role, task_id, status, created_at) \
                 VALUES (?, ?, ?, NULL, 'pending', ?)",
            )
            .bind(&role_run_id)
            .bind(&step_run_id)
            .bind(role_str(step.role))
            .bind(&created_at)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(WorkflowRun {
            id: run_id,
            template_id: template_id.to_string(),
            status: WorkflowRunStatus::Pending,
            created_at,
            finished_at: None,
            context: context.map(|s| s.to_string()),
            repository: repository.map(|s| s.to_string()),
            base_commit: base_commit.map(|s| s.to_string()),
        })
    }

    pub async fn get_workflow_run(&self, id: &str) -> Result<Option<WorkflowRun>> {
        let row = sqlx::query(
            "SELECT id, template_id, status, context, repository, base_commit, created_at, finished_at \
             FROM workflow_runs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| WorkflowRun {
            id: r.try_get("id").unwrap_or_default(),
            template_id: r.try_get("template_id").unwrap_or_default(),
            status: from_snake(&r.try_get::<String, _>("status").unwrap_or_default())
                .unwrap_or(WorkflowRunStatus::Pending),
            created_at: r.try_get("created_at").unwrap_or_default(),
            finished_at: r.try_get("finished_at").ok(),
            context: r.try_get("context").ok(),
            repository: r.try_get("repository").ok(),
            base_commit: r
                .try_get::<Option<String>, _>("base_commit")
                .ok()
                .flatten()
                .filter(|s| !s.is_empty()),
        }))
    }

    pub async fn list_workflow_runs(&self) -> Result<Vec<WorkflowRun>> {
        let rows = sqlx::query(
            "SELECT id, template_id, status, context, repository, base_commit, created_at, finished_at \
             FROM workflow_runs ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| WorkflowRun {
                id: r.try_get("id").unwrap_or_default(),
                template_id: r.try_get("template_id").unwrap_or_default(),
                status: from_snake(&r.try_get::<String, _>("status").unwrap_or_default())
                    .unwrap_or(WorkflowRunStatus::Pending),
                created_at: r.try_get("created_at").unwrap_or_default(),
                finished_at: r.try_get("finished_at").ok(),
                context: r.try_get("context").ok(),
                repository: r.try_get("repository").ok(),
                base_commit: r
                    .try_get::<Option<String>, _>("base_commit")
                    .ok()
                    .flatten()
                    .filter(|s| !s.is_empty()),
            })
            .collect())
    }

    /// Stage 8 / line 487: ids of workflow runs in the `Running` status — the
    /// background workflow tick re-advances these each interval so a CP
    /// restart (or a node completing a step task out-of-band) does not leave a
    /// step hung in `Running` forever.
    pub async fn running_workflow_run_ids(&self) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT id FROM workflow_runs WHERE status = 'running' ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| r.try_get::<String, _>("id").unwrap_or_default())
            .collect())
    }

    /// `template_id` every `interval_seconds` under `autonomy`. Fails if the
    /// template doesn't exist.
    pub async fn create_workflow_schedule(
        &self,
        template_id: &str,
        body: &WorkflowScheduleCreate,
    ) -> Result<WorkflowSchedule> {
        let exists: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM workflow_templates WHERE id = ?")
                .bind(template_id)
                .fetch_one(&self.pool)
                .await?;
        if exists == 0 {
            anyhow::bail!("unknown workflow template {template_id}");
        }
        if body.interval_seconds < 1 {
            anyhow::bail!("interval_seconds must be >= 1");
        }
        if parse_autonomy_level(&body.autonomy).is_none() {
            anyhow::bail!("unknown autonomy level: {}", body.autonomy);
        }
        // Stage 13 L4 ratify: a fully-autonomous (l4) schedule may only be
        // created when the template carries a budget — fail-closed so an
        // unbounded loop can never be set on a timer. Non-l4 passes (the
        // lower-autonomy runs still route through the command policy).
        if let Some(tpl) = self.get_workflow_template(template_id).await? {
            if let Err(reason) = agentgrid_common::ratify_l4_schedule(&tpl, &body.autonomy) {
                anyhow::bail!(reason);
            }
        }
        let id = format!("wfsch-{}", Uuid::new_v4());
        let now = now_iso();
        sqlx::query(
            "INSERT INTO workflow_schedules \
             (id, template_id, interval_seconds, autonomy, last_run_at, enabled, created_at) \
             VALUES (?, ?, ?, ?, '', ?, ?)",
        )
        .bind(&id)
        .bind(template_id)
        .bind(body.interval_seconds)
        .bind(&body.autonomy)
        .bind(if body.enabled { 1i64 } else { 0 })
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(WorkflowSchedule {
            id,
            template_id: template_id.into(),
            interval_seconds: body.interval_seconds,
            autonomy: body.autonomy.clone(),
            last_run_at: String::new(),
            enabled: body.enabled,
            created_at: now,
        })
    }

    /// List schedules (optionally for one template).
    pub async fn list_workflow_schedules(
        &self,
        template_id: Option<&str>,
    ) -> Result<Vec<WorkflowSchedule>> {
        let rows = if let Some(tid) = template_id {
            sqlx::query(
                "SELECT id, template_id, interval_seconds, autonomy, last_run_at, enabled, created_at \
                 FROM workflow_schedules WHERE template_id = ? ORDER BY created_at ASC",
            )
            .bind(tid)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT id, template_id, interval_seconds, autonomy, last_run_at, enabled, created_at \
                 FROM workflow_schedules ORDER BY created_at ASC",
            )
            .fetch_all(&self.pool)
            .await?
        };
        Ok(rows.iter().map(schedule_from_row).collect())
    }

    /// Delete a schedule. Returns whether a schedule was deleted.
    pub async fn delete_workflow_schedule(&self, id: &str) -> Result<bool> {
        let affected = sqlx::query("DELETE FROM workflow_schedules WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected == 1)
    }

    /// Stage 13: for each enabled schedule whose interval has elapsed since
    /// `last_run_at`, create a new `WorkflowRun` and stamp `last_run_at`.
    /// Returns the created run ids (mostly for tests).
    pub async fn tick_workflow_schedules(&self, now_unix: i64) -> Result<Vec<String>> {
        let schedules = self.list_workflow_schedules(None).await?;
        let mut created = Vec::new();
        for s in schedules {
            if !s.enabled {
                continue;
            }
            // last_run_at stored as ISO; parse to a unix epoch. Empty = "due now".
            let last = if s.last_run_at.is_empty() {
                0
            } else {
                iso_to_unix(&s.last_run_at).unwrap_or(0)
            };
            if now_unix - last < s.interval_seconds {
                continue;
            }
            // Create a fresh run; context/repo/commit come from the template
            // defaults only if stored (Stage 13: per-schedule overrides are a
            // follow-up; the MVP runs the template as-is).
            let run = match self
                .create_workflow_run(&s.template_id, None, None, None)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(schedule = %s.id, "tick skipped bad template: {e}");
                    continue;
                }
            };
            created.push(run.id);
            sqlx::query("UPDATE workflow_schedules SET last_run_at = ? WHERE id = ?")
                .bind(unix_to_iso(now_unix))
                .bind(&s.id)
                .execute(&self.pool)
                .await?;
        }
        Ok(created)
    }

    pub async fn get_workflow_run_steps(&self, run_id: &str) -> Result<Vec<WorkflowStepRun>> {
        let rows = sqlx::query(
            "SELECT id, run_id, step_id, prompt, depends_on, role, adapter, requested_node_id, base_commit, retryable, max_attempts, attempts, status, created_at, expandable, started_at, finished_at \
             FROM workflow_steps WHERE run_id = ? ORDER BY created_at ASC",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| WorkflowStepRun {
                id: r.try_get("id").unwrap_or_default(),
                run_id: r.try_get("run_id").unwrap_or_default(),
                step_id: r.try_get("step_id").unwrap_or_default(),
                prompt: r.try_get("prompt").unwrap_or_default(),
                depends_on: serde_json::from_str(
                    &r.try_get::<String, _>("depends_on").unwrap_or_default(),
                )
                .unwrap_or_default(),
                role: from_snake(&r.try_get::<String, _>("role").unwrap_or_default())
                    .unwrap_or(WorkflowRole::Worker),
                adapter: r.try_get("adapter").ok(),
                // Normalize both NULL and empty-string to `None` so an
                // unpinned step never becomes `Some("")` (which would break
                // the `try_assign` `requested_node_id IS NULL` filter).
                requested_node_id: r
                    .try_get::<Option<String>, _>("requested_node_id")
                    .ok()
                    .flatten()
                    .filter(|s| !s.is_empty()),
                base_commit: r
                    .try_get::<Option<String>, _>("base_commit")
                    .ok()
                    .flatten()
                    .filter(|s| !s.is_empty()),
                retryable: r
                    .try_get::<Option<i64>, _>("retryable")
                    .ok()
                    .flatten()
                    .map(|v| v != 0),
                max_attempts: r
                    .try_get::<Option<i64>, _>("max_attempts")
                    .ok()
                    .flatten()
                    .map(|v| v as u32),
                attempts: r.try_get::<i64, _>("attempts").unwrap_or(0) as u32,
                status: from_snake(&r.try_get::<String, _>("status").unwrap_or_default())
                    .unwrap_or(agentgrid_common::WorkflowStepStatus::Pending),
                expandable: r
                    .try_get::<Option<i64>, _>("expandable")
                    .ok()
                    .flatten()
                    .map(|v| v != 0),
                created_at: r.try_get("created_at").unwrap_or_default(),
                started_at: r
                    .try_get::<Option<String>, _>("started_at")
                    .ok()
                    .flatten()
                    .filter(|s| !s.is_empty()),
                finished_at: r
                    .try_get::<Option<String>, _>("finished_at")
                    .ok()
                    .flatten()
                    .filter(|s| !s.is_empty()),
            })
            .collect())
    }

    /// Stage 8 ACP plan projection: the live view of a run's roles, steps,
    /// placement, spawned tasks, assigned nodes and latest verdicts.
    pub async fn get_workflow_run_projection(
        &self,
        run_id: &str,
    ) -> Result<Option<agentgrid_common::WorkflowProjection>> {
        let run = match self.get_workflow_run(run_id).await? {
            Some(r) => r,
            None => return Ok(None),
        };
        let steps = self.get_workflow_run_steps(run_id).await?;
        let mut out = Vec::with_capacity(steps.len());
        for s in &steps {
            let task_row = sqlx::query("SELECT task_id FROM role_runs WHERE step_run_id = ?")
                .bind(&s.id)
                .fetch_optional(&self.pool)
                .await?;
            let task_id: Option<String> =
                task_row.and_then(|r| r.try_get::<Option<String>, _>("task_id").ok().flatten());
            let (node_id, verdict, error_code) = match &task_id {
                Some(tid) => {
                    let ts = self
                        .get_task_status(tid)
                        .await?
                        .unwrap_or(agentgrid_common::TaskStatus::Queued);
                    let att = sqlx::query(
                        "SELECT node_id, error_code FROM attempts WHERE task_id = ? ORDER BY number DESC LIMIT 1",
                    )
                    .bind(tid)
                    .fetch_optional(&self.pool)
                    .await?;
                    let node_id = att
                        .as_ref()
                        .and_then(|r| r.try_get::<Option<String>, _>("node_id").ok().flatten());
                    let error_code = att
                        .as_ref()
                        .and_then(|r| r.try_get::<Option<String>, _>("error_code").ok().flatten());
                    let verdict = match ts {
                        agentgrid_common::TaskStatus::Succeeded => "succeeded",
                        agentgrid_common::TaskStatus::Failed => "failed",
                        agentgrid_common::TaskStatus::Validating
                        | agentgrid_common::TaskStatus::Running
                        | agentgrid_common::TaskStatus::Assigned => "running",
                        _ => "pending",
                    };
                    (node_id, verdict.to_string(), error_code)
                }
                None => (None, "pending".to_string(), None),
            };
            out.push(agentgrid_common::StepProjection {
                step_id: s.step_id.clone(),
                role: s.role,
                status: s.status,
                depends_on: s.depends_on.clone(),
                requested_node_id: s.requested_node_id.clone(),
                attempts: s.attempts,
                task_id,
                node_id,
                verdict,
                error_code,
                started_at: s.started_at.clone(),
                finished_at: s.finished_at.clone(),
            });
        }
        Ok(Some(agentgrid_common::WorkflowProjection {
            run,
            steps: out,
            budget: self.workflow_run_budget_snapshot(run_id).await?,
        }))
    }

    /// Stage 13: build the `BudgetSnapshot` for a run, if its template declares
    /// a `WorkflowBudget`. Mirrors the enforcement path in `tick_workflow_run`:
    /// usage is computed from observable state (wall + task-started rounds),
    /// and `budget.check()` produces the first breach (if any).
    async fn workflow_run_budget_snapshot(
        &self,
        run_id: &str,
    ) -> Result<Option<agentgrid_common::BudgetSnapshot>> {
        let run = match self.get_workflow_run(run_id).await? {
            Some(r) => r,
            None => return Ok(None),
        };
        let tpl = match self.get_workflow_template(&run.template_id).await? {
            Some(t) => t,
            None => return Ok(None),
        };
        let bud = match tpl.budget {
            Some(b) => b,
            None => return Ok(None),
        };
        let steps = self.get_workflow_run_steps(run_id).await?;
        let task_count = steps
            .iter()
            .filter(|s| s.status != WorkflowStepStatus::Pending)
            .count() as u32;
        let created_unix = iso_to_unix(&run.created_at).unwrap_or(0);
        let now = chrono::Utc::now().timestamp();
        let mut usage = agentgrid_common::compute_budget_usage(created_unix, task_count, now);
        usage.messages = self.workflow_message_count(run_id).await.unwrap_or(0);
        // Stage 13: observe bytes + circuit breaker so the snapshot ceiling
        // syncs with the tick enforcement path.
        usage.bytes = self.workflow_message_bytes(run_id).await.unwrap_or(0);
        usage.repeated_handoffs = self.workflow_repeated_handoffs(run_id).await.unwrap_or(0);
        let breach = bud.check(&usage);
        Ok(Some(agentgrid_common::BudgetSnapshot {
            limits: bud,
            usage,
            breach,
        }))
    }

    async fn set_workflow_run_status(
        &self,
        id: &str,
        status: WorkflowRunStatus,
        finished_at: Option<&str>,
    ) -> Result<()> {
        sqlx::query("UPDATE workflow_runs SET status = ?, finished_at = ? WHERE id = ?")
            .bind(role_str_status(status))
            .bind(finished_at)
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_step_status(&self, step_run_id: &str, status: WorkflowStepStatus) -> Result<()> {
        // Stage 11.6: record timing so the web UI can render a span waterfall
        // (timeline by time, not just dependency depth). `started_at` lands
        // when the step leaves pending for running; `finished_at` lands on a
        // terminal transition.
        let now = now_iso();
        match status {
            WorkflowStepStatus::Running => {
                sqlx::query(
                    "UPDATE workflow_steps SET status = ?, started_at = COALESCE(started_at, ?) WHERE id = ?",
                )
                .bind(role_str_status(status))
                .bind(&now)
                .bind(step_run_id)
                .execute(&self.pool)
                .await?;
            }
            WorkflowStepStatus::Succeeded
            | WorkflowStepStatus::Failed
            | WorkflowStepStatus::Blocked
            | WorkflowStepStatus::Skipped
            | WorkflowStepStatus::Cancelled => {
                sqlx::query(
                    "UPDATE workflow_steps SET status = ?, finished_at = COALESCE(finished_at, ?) WHERE id = ?",
                )
                .bind(role_str_status(status))
                .bind(&now)
                .bind(step_run_id)
                .execute(&self.pool)
                .await?;
            }
            WorkflowStepStatus::Pending => {
                sqlx::query("UPDATE workflow_steps SET status = ? WHERE id = ?")
                    .bind(role_str_status(status))
                    .bind(step_run_id)
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }

    /// Stage 8: bump the attempt counter used by the lost-step retry policy.
    async fn set_step_attempts(&self, step_run_id: &str, attempts: u32) -> Result<()> {
        sqlx::query("UPDATE workflow_steps SET attempts = ? WHERE id = ?")
            .bind(attempts as i64)
            .bind(step_run_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_role_run_task(&self, step_run_id: &str, task_id: &str) -> Result<()> {
        sqlx::query("UPDATE role_runs SET task_id = ? WHERE step_run_id = ?")
            .bind(task_id)
            .bind(step_run_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn set_role_run_status_by_step(
        &self,
        step_run_id: &str,
        status: WorkflowStepStatus,
    ) -> Result<()> {
        sqlx::query("UPDATE role_runs SET status = ? WHERE step_run_id = ?")
            .bind(role_str_status(status))
            .bind(step_run_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub(crate) async fn step_task_id(&self, step_run_id: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT task_id FROM role_runs WHERE step_run_id = ?")
            .bind(step_run_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.and_then(|r| r.try_get::<Option<String>, _>("task_id").ok().flatten()))
    }

    /// Stage 13: the most recent attempt's emitted plan (if any) for a task.
    /// Used to copy an architect's plan onto the run row when the step
    /// succeeds.
    async fn attempt_plan_for_task(&self, task_id: &str) -> Result<Option<String>> {
        let row =
            sqlx::query("SELECT plan FROM attempts WHERE task_id = ? ORDER BY number DESC LIMIT 1")
                .bind(task_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row
            .and_then(|r| r.try_get::<Option<String>, _>("plan").ok().flatten())
            .filter(|s| !s.is_empty()))
    }

    /// Stage 13: the commit SHA the winning attempt produced (a handoff
    /// reference), if it ran in a git worktree. Compact info-only reference —
    /// never carries a transcript (ADR: handoffs reference commits, not logs).
    async fn attempt_commit_for_task(&self, task_id: &str) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT commit_sha FROM attempts WHERE task_id = ? ORDER BY number DESC LIMIT 1",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row
            .and_then(|r| r.try_get::<Option<String>, _>("commit_sha").ok().flatten())
            .filter(|s| !s.is_empty()))
    }

    /// Stage 8 / line 239: for an Integrator workflow step's task, resolve the
    /// winning commit SHAs of its upstream dependency steps, so the node can
    /// land them into the integrator's worktree as an integration branch.
    /// Returns `[]` for plain (non-workflow) tasks and for steps without any
    /// succeeded dependency. Best-effort: a missing commit SHA is skipped
    /// (the integrator still runs, but with one fewer merged worker).
    pub(crate) async fn upstream_commits_for_task(&self, task_id: &str) -> Result<Vec<String>> {
        // 1. task_id -> role_runs.step_run_id (the integrator step run).
        let this_step_run: Option<String> =
            sqlx::query_scalar("SELECT step_run_id FROM role_runs WHERE task_id = ? LIMIT 1")
                .bind(task_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(step_run_id) = this_step_run else {
            return Ok(Vec::new()); // plain (non-workflow) task
        };

        // 2. step_run_id -> workflow_steps row (depends_on JSON + role).
        let step_row = sqlx::query("SELECT depends_on, role FROM workflow_steps WHERE id = ?")
            .bind(&step_run_id)
            .fetch_optional(&self.pool)
            .await?;
        let Some(step_row) = step_row else {
            return Ok(Vec::new());
        };
        let role: String = step_row.try_get("role").unwrap_or_default();
        // Both Integrator and Verifier depend on upstream worker commits:
        // Integrator cherry-picks *all* upstream worker commits to integrate
        // them; Verifier (usually a single upstream worker) cherry-picks its
        // single upstream worker commit so its worktree starts at the worker's
        // tree on top of the base — letting it review/read the worker's change
        // without ever seeing the worker's private transcripts (ADR: handoffs
        // reference commits, not logs). Non-workflow / no-deps steps yield [].
        let _ = role;
        let deps_json: String = step_row
            .try_get::<String, _>("depends_on")
            .unwrap_or_default();
        let deps: Vec<String> = serde_json::from_str(&deps_json).unwrap_or_default();
        if deps.is_empty() {
            return Ok(Vec::new());
        }

        // 3. For each dependency step_id -> find its run's task -> winning commit.
        // `workflow_steps.step_id` is the template id (shared within the run),
        // so resolve it via the same run as the integrator.
        let run_id: String = sqlx::query_scalar("SELECT run_id FROM workflow_steps WHERE id = ?")
            .bind(&step_run_id)
            .fetch_one(&self.pool)
            .await?;
        let mut out = Vec::new();
        for dep_step_id in deps {
            // step run id for this dependency within the same run.
            let dep_step_run: Option<String> = sqlx::query_scalar(
                "SELECT id FROM workflow_steps WHERE run_id = ? AND step_id = ? LIMIT 1",
            )
            .bind(&run_id)
            .bind(&dep_step_id)
            .fetch_optional(&self.pool)
            .await?;
            let Some(dep_step_run) = dep_step_run else {
                continue;
            };
            // task bound to that step run.
            let dep_task: Option<String> =
                sqlx::query_scalar("SELECT task_id FROM role_runs WHERE step_run_id = ? LIMIT 1")
                    .bind(&dep_step_run)
                    .fetch_optional(&self.pool)
                    .await?;
            let Some(dep_task) = dep_task else { continue };
            if let Some(sha) = self.attempt_commit_for_task(&dep_task).await? {
                out.push(sha);
            }
        }
        Ok(out)
    }

    /// Stage 13: stamp a pending plan onto the run row (read on approval).
    async fn set_workflow_run_plan(&self, run_id: &str, plan: &str) -> Result<()> {
        sqlx::query("UPDATE workflow_runs SET plan = ? WHERE id = ?")
            .bind(plan)
            .bind(run_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Stage 13: read the pending plan awaiting approval for a run.
    pub async fn get_workflow_run_plan(&self, run_id: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT plan FROM workflow_runs WHERE id = ?")
            .bind(run_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row
            .and_then(|r| r.try_get::<Option<String>, _>("plan").ok().flatten())
            .filter(|s| !s.is_empty()))
    }

    /// Stage 13: expand a `PlanReady` run's plan into new workflow steps and
    /// resume the run. Parses the plan via `agentgrid_common::parse_plan_steps`,
    /// inserts the steps as the run's remaining DAG (the architect step stays
    /// `Succeeded`), and flips the run back to `Running`. Fails closed on any
    /// parse/insert error: the run stays `PlanReady` for the operator.
    pub async fn approve_workflow_plan(&self, run_id: &str) -> Result<()> {
        let run = self
            .get_workflow_run(run_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("unknown workflow run {run_id}"))?;
        if run.status != WorkflowRunStatus::PlanReady {
            anyhow::bail!(
                "run is not awaiting plan approval (status = {:?})",
                run.status
            );
        }
        let plan = self
            .get_workflow_run_plan(run_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no plan to approve for run {run_id}"))?;
        let steps = agentgrid_common::parse_plan_steps(&plan).map_err(anyhow::Error::msg)?;
        let now = now_iso();
        let mut tx = self.pool.begin().await?;
        for step in &steps {
            let step_run_id = format!("wfs-{}", Uuid::new_v4());
            let depends_json = serde_json::to_string(&step.depends_on)?;
            sqlx::query(
                "INSERT INTO workflow_steps \
                 (id, run_id, step_id, prompt, depends_on, role, adapter, requested_node_id, base_commit, retryable, max_attempts, attempts, status, created_at, expandable) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 'pending', ?, NULL)",
            )
            .bind(&step_run_id)
            .bind(run_id)
            .bind(&step.id)
            .bind(&step.prompt)
            .bind(&depends_json)
            .bind(role_str(step.role))
            .bind(&step.adapter)
            .bind(step.requested_node_id.as_deref())
            .bind(step.base_commit.as_deref())
            .bind(step.retryable.map(|b| if b { 1i64 } else { 0 }))
            .bind(step.max_attempts.map(|m| m as i64))
            .bind(&now)
            .execute(&mut *tx)
            .await?;
            let role_run_id = format!("wrr-{}", Uuid::new_v4());
            sqlx::query(
                "INSERT INTO role_runs (id, step_run_id, role, task_id, status, created_at) \
                 VALUES (?, ?, ?, NULL, 'pending', ?)",
            )
            .bind(&role_run_id)
            .bind(&step_run_id)
            .bind(role_str(step.role))
            .bind(&now)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        self.set_workflow_run_status(run_id, WorkflowRunStatus::Running, None)
            .await?;
        Ok(())
    }

    /// Stage 13: append a typed `AgentMessage` from one step of a run to another
    /// (or `"*"` to broadcast). The orchestrator publishes here, never an
    /// agent. Allocates a monotonic per-run sequence.
    pub(crate) async fn emit_workflow_message(
        &self,
        run_id: &str,
        from_step_id: &str,
        to_step_id: &str,
        kind: agentgrid_common::AgentMessageKind,
        payload: &str,
    ) -> Result<()> {
        let id = format!("wfm-{}", Uuid::new_v4());
        let now = now_iso();
        let mut tx = self.pool.begin().await?;
        // Increment the per-run sequence atomically under the txn.
        sqlx::query(
            "UPDATE workflow_runs SET message_sequence = message_sequence + 1 WHERE id = ?",
        )
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
        let seq: i64 =
            sqlx::query_scalar("SELECT message_sequence FROM workflow_runs WHERE id = ?")
                .bind(run_id)
                .fetch_one(&mut *tx)
                .await?;
        sqlx::query(
            "INSERT INTO workflow_messages \
             (id, run_id, from_step_id, to_step_id, kind, payload, sequence, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(run_id)
        .bind(from_step_id)
        .bind(to_step_id)
        .bind(kind.as_str())
        .bind(payload)
        .bind(seq)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Stage 13: the typed messages a specific consuming step should see at
    /// activation — the senders that produced an output and target this step
    /// or broadcast (`"*"`). Ordered by the per-run sequence the emitter
    /// stamped.
    async fn messages_for_step(
        &self,
        run_id: &str,
        step_id: &str,
    ) -> Result<Vec<agentgrid_common::AgentMessage>> {
        let rows = sqlx::query(
            "SELECT from_step_id, to_step_id, kind, payload \
             FROM workflow_messages WHERE run_id = ? AND (to_step_id = ? OR to_step_id = '*') \
             ORDER BY sequence ASC",
        )
        .bind(run_id)
        .bind(step_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let kind: agentgrid_common::AgentMessageKind =
                    r.try_get::<String, _>("kind").ok()?.parse().ok()?;
                Some(agentgrid_common::AgentMessage {
                    from_step_id: r.try_get("from_step_id").unwrap_or_default(),
                    to_step_id: r.try_get("to_step_id").unwrap_or_default(),
                    kind,
                    payload: r.try_get("payload").unwrap_or_default(),
                })
            })
            .collect())
    }

    /// Stage 13: count of messages a run has emitted (for `BudgetUsage.messages`
    /// observability + the `max_messages` budget proxy).
    pub async fn workflow_message_count(&self, run_id: &str) -> Result<u32> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workflow_messages WHERE run_id = ?")
            .bind(run_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(n as u32)
    }

    /// Stage 13 Loop Engineering: total payload byte length across all
    /// orchestrator-emitted messages in a run — a coarse proxy for the
    /// `max_bytes` ceiling until the adapter reports per-attempt counts.
    pub async fn workflow_message_bytes(&self, run_id: &str) -> Result<u64> {
        let n: Option<i64> = sqlx::query_scalar(
            "SELECT COALESCE(SUM(length(payload)), 0) FROM workflow_messages WHERE run_id = ?",
        )
        .bind(run_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(n.unwrap_or(0) as u64)
    }

    /// Stage 13 Loop Engineering circuit breaker: the longest run of
    /// *consecutive* messages that share the same `(from_step_id, to_step_id)`
    /// pair, in `sequence` order. A tight handoff ping-pong between two steps
    /// grows this streak until it trips `max_repeated_handoffs` (a runaway
    /// loop). Auto-emitted broadcast `output` to `*` is skipped, since a
    /// normal step-succeeded broadcast streak is a healthy flow and not a
    /// solo ping-pong; only truly repeated step-to-step handoffs count.
    pub async fn workflow_repeated_handoffs(&self, run_id: &str) -> Result<u32> {
        let rows = sqlx::query(
            "SELECT from_step_id, to_step_id FROM workflow_messages \
             WHERE run_id = ? ORDER BY sequence ASC",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;
        let mut longest: u32 = 0;
        let mut cur: u32 = 0;
        let mut last: (Option<String>, Option<String>) = (None, None);
        for r in rows {
            let from: Option<String> = r.try_get("from_step_id").ok();
            let to: Option<String> = r.try_get("to_step_id").ok();
            // Skip broadcast outputs to `*` so a normal all-to-all broadcast
            // streak never trips the breaker (see method doc).
            if to.as_deref() == Some("*") {
                cur = 0;
                last = (from, to);
                continue;
            }
            if last == (from.clone(), to.clone()) && cur > 0 {
                cur += 1;
            } else {
                cur = 1;
            }
            longest = longest.max(cur);
            last = (from, to);
        }
        Ok(longest)
    }

    /// Current status of a task, if it exists.
    pub async fn get_task_status(&self, id: &str) -> Result<Option<TaskStatus>> {
        Ok(self.show_task(id).await?.map(|t| t.status))
    }

    /// Durable, idempotent workflow scheduler. Reconciles a run from current
    /// state:
    /// - marks a `pending` run `running`;
    /// - activates `pending` steps whose dependencies are all `succeeded`
    ///   (creating one Agentgrid task per step, tagged with the step's role);
    /// - advances `running` steps whose task has terminated;
    /// - computes the run status (succeeded when all leaves done, failed on any
    ///   step failure).
    ///
    /// Returns the ids of tasks created during this tick (so a caller can assign
    /// and drive them). Safe to call repeatedly; it only ever moves state forward.
    pub async fn tick_workflow_run(&self, run_id: &str) -> Result<Vec<String>> {
        let run = match self.get_workflow_run(run_id).await? {
            Some(r) => r,
            None => return Ok(vec![]),
        };
        if matches!(
            run.status,
            WorkflowRunStatus::Succeeded
                | WorkflowRunStatus::Failed
                | WorkflowRunStatus::Cancelled
                | WorkflowRunStatus::Blocked
                | WorkflowRunStatus::PlanReady
        ) {
            return Ok(vec![]);
        }
        if run.status == WorkflowRunStatus::Pending {
            self.set_workflow_run_status(run_id, WorkflowRunStatus::Running, None)
                .await?;
        }
        // Stage 13 Loop Engineering: budget enforcement. Fetch the template's
        // budget (if any), compute a coarse usage snapshot from observable
        // state (wall = now - created_at, rounds = sum of step attempts = task
        // starts), and park the run `Blocked` on the first ceiling breach.
        // `Blocked` is terminal-until-human-approval (the loop stops starting
        // new steps); cost/messages/tokens/handoffs stay 0 until the adapter
        // reports per-attempt counts (a follow-up).
        if let Ok(Some(tpl)) = self.get_workflow_template(&run.template_id).await {
            if let Some(bud) = &tpl.budget {
                let steps_pre = self.get_workflow_run_steps(run_id).await?;
                // ponytail: rounds = count of step instances that have already
                // started a task (anything past `Pending`). A coarse proxy for
                // loop iterations until the adapter reports per-attempt counts.
                let task_count = steps_pre
                    .iter()
                    .filter(|s| s.status != WorkflowStepStatus::Pending)
                    .count() as u32;
                let created_unix = iso_to_unix(&run.created_at).unwrap_or(0);
                let now = chrono::Utc::now().timestamp();
                let usage = {
                    let mut u =
                        agentgrid_common::compute_budget_usage(created_unix, task_count, now);
                    u.messages = self.workflow_message_count(run_id).await.unwrap_or(0);
                    // Stage 13: observe bytes + circuit-breaker streak in the
                    // enforcement path too (see workflow_run_budget_snapshot).
                    u.bytes = self.workflow_message_bytes(run_id).await.unwrap_or(0);
                    u.repeated_handoffs =
                        self.workflow_repeated_handoffs(run_id).await.unwrap_or(0);
                    u
                };
                if let Some(breach) = bud.check(&usage) {
                    tracing::warn!(
                        "workflow run {run_id} budget breach: {} (limit {}, observed {}); parking Blocked",
                        breach.field, breach.limit, breach.observed
                    );
                    self.set_workflow_run_status(
                        run_id,
                        WorkflowRunStatus::Blocked,
                        Some(&now_iso()),
                    )
                    .await?;
                    return Ok(vec![]);
                }
            }
        }
        let steps = self.get_workflow_run_steps(run_id).await?;
        let status_by_id: std::collections::HashMap<&str, WorkflowStepStatus> = steps
            .iter()
            .map(|s| (s.step_id.as_str(), s.status))
            .collect();
        let repo = run.repository.clone().unwrap_or_default();
        let mut created = Vec::new();
        for step in &steps {
            match step.status {
                WorkflowStepStatus::Succeeded
                | WorkflowStepStatus::Failed
                | WorkflowStepStatus::Cancelled
                | WorkflowStepStatus::Skipped
                | WorkflowStepStatus::Blocked => continue,
                WorkflowStepStatus::Running => {
                    if let Some(task_id) = self.step_task_id(&step.id).await? {
                        if let Some(ts) = self.get_task_status(&task_id).await? {
                            match ts {
                                TaskStatus::Succeeded => {
                                    self.set_step_status(&step.id, WorkflowStepStatus::Succeeded)
                                        .await?;
                                    self.set_role_run_status_by_step(
                                        &step.id,
                                        WorkflowStepStatus::Succeeded,
                                    )
                                    .await?;
                                    // Stage 13 typed mailbox: emit a compact
                                    // `output` message broadcast to downstream
                                    // consuming steps (the orchestrator-mediated
                                    // handoff, not free-form P2P). The payload
                                    // is a `HandoffPackage` JSON (summary + the
                                    // winning attempt's commit SHA), so a
                                    // downstream step sees a *reference* to the
                                    // upstream commit, never a transcript.
                                    let commit_sha = self
                                        .attempt_commit_for_task(&task_id)
                                        .await
                                        .unwrap_or(None);
                                    let payload = agentgrid_common::build_handoff_payload(
                                        &format!(
                                            "step `{}` succeeded (task {task_id})",
                                            step.step_id
                                        ),
                                        commit_sha.as_deref(),
                                        &[],
                                    );
                                    let _ = self
                                        .emit_workflow_message(
                                            run_id,
                                            &step.step_id,
                                            "*",
                                            agentgrid_common::AgentMessageKind::Output,
                                            &payload,
                                        )
                                        .await;
                                    // Stage 13 plan expansion: an
                                    // `expandable` architect step that emitted
                                    // a plan (its winning attempt carried one)
                                    // pauses the run in `PlanReady` pending a
                                    // human's approval to expand the plan into
                                    // steps. The plan is stamped onto the run
                                    // row so it outlives the attempt.
                                    let expandable = step.expandable.unwrap_or(false)
                                        && step.role == WorkflowRole::Architect;
                                    if expandable {
                                        if let Some(pid) =
                                            self.attempt_plan_for_task(&task_id).await?
                                        {
                                            self.set_workflow_run_plan(run_id, &pid).await?;
                                            self.set_workflow_run_status(
                                                run_id,
                                                WorkflowRunStatus::PlanReady,
                                                None,
                                            )
                                            .await?;
                                            // Pause the loop: the run is now
                                            // awaiting approval — don't fall
                                            // through to the all-term branch
                                            // (which would mark it Succeeded).
                                            return Ok(created);
                                        }
                                    }
                                }
                                TaskStatus::Failed => {
                                    // Stage 8 lost-step recovery: a side-effectful
                                    // step must not be auto-retried unless it opted in.
                                    // `node_lost` is treated the same as any other
                                    // failure (default = step fails).
                                    let attempts = step.attempts + 1;
                                    self.set_step_attempts(&step.id, attempts).await?;
                                    let max = step.max_attempts.unwrap_or(1);
                                    let retryable = step.retryable.unwrap_or(false);
                                    if retryable && attempts < max {
                                        let req = CreateTaskRequest {
                                            prompt: step.prompt.clone(),
                                            repository: repo.clone(),
                                            adapter: step
                                                .adapter
                                                .clone()
                                                .filter(|a| !a.is_empty())
                                                .unwrap_or_else(|| "mock".to_string()),
                                            requested_node_id: step.requested_node_id.clone(),
                                            timeout_secs: None,
                                            validation_command: None,
                                            base_commit: step
                                                .base_commit
                                                .clone()
                                                .or_else(|| run.base_commit.clone()),
                                            parent_acp_session_id: None,
                                        };
                                        let tv = self.create_task(&req).await?;
                                        self.set_role_run_task(&step.id, &tv.id).await?;
                                        created.push(tv.id);
                                        // step stays `Running` pending the retry
                                    } else if step.role == WorkflowRole::Integrator {
                                        // Conflict policy (Stage 8): a failed
                                        // integrator must not silently overwrite and
                                        // must not fail the whole run. It blocks for
                                        // human/repair resolution; the bounded retries
                                        // above are the automated repair budget.
                                        self.set_step_status(&step.id, WorkflowStepStatus::Blocked)
                                            .await?;
                                        self.set_role_run_status_by_step(
                                            &step.id,
                                            WorkflowStepStatus::Blocked,
                                        )
                                        .await?;
                                    } else if retryable {
                                        // Stage 13 repair escalation: a
                                        // `retryable` step opted into repair
                                        // rounds ("repairable"), so exhausting
                                        // `max_attempts` is not a hard run
                                        // failure — it escalates to a human for
                                        // repair resolution (`Blocked` rather
                                        // than `Failed`). A non-retryable step
                                        // fails fast below.
                                        self.set_step_status(&step.id, WorkflowStepStatus::Blocked)
                                            .await?;
                                        self.set_role_run_status_by_step(
                                            &step.id,
                                            WorkflowStepStatus::Blocked,
                                        )
                                        .await?;
                                    } else {
                                        self.set_step_status(&step.id, WorkflowStepStatus::Failed)
                                            .await?;
                                        self.set_role_run_status_by_step(
                                            &step.id,
                                            WorkflowStepStatus::Failed,
                                        )
                                        .await?;
                                    }
                                }
                                // Cancelled / still in flight: leave the step as-is.
                                _ => {}
                            }
                        }
                    }
                }
                WorkflowStepStatus::Pending => {
                    let ready = step.depends_on.iter().all(|d| {
                        status_by_id.get(d.as_str()) == Some(&WorkflowStepStatus::Succeeded)
                    });
                    if ready {
                        // Stage 13 typed mailbox: render the orchestrator-mediated
                        // handoff block from upstream steps into this step's
                        // prompt (only direct deps + broadcasts emitted so far).
                        let msgs = self
                            .messages_for_step(run_id, &step.step_id)
                            .await
                            .unwrap_or_default();
                        let prompt = if msgs.is_empty() {
                            step.prompt.clone()
                        } else {
                            agentgrid_common::render_handoff_block(&step.prompt, &msgs)
                        };
                        let req = CreateTaskRequest {
                            prompt,
                            repository: repo.clone(),
                            adapter: step
                                .adapter
                                .clone()
                                .filter(|a| !a.is_empty())
                                .unwrap_or_else(|| "mock".to_string()),
                            requested_node_id: step.requested_node_id.clone(),
                            timeout_secs: None,
                            validation_command: None,
                            base_commit: step
                                .base_commit
                                .clone()
                                .or_else(|| run.base_commit.clone()),
                            parent_acp_session_id: None,
                        };
                        let tv = self.create_task(&req).await?;
                        self.set_role_run_task(&step.id, &tv.id).await?;
                        self.set_step_status(&step.id, WorkflowStepStatus::Running)
                            .await?;
                        self.set_role_run_status_by_step(&step.id, WorkflowStepStatus::Running)
                            .await?;
                        created.push(tv.id);
                    }
                }
            }
        }
        let steps2 = self.get_workflow_run_steps(run_id).await?;
        let all_term = steps2.iter().all(|s| {
            matches!(
                s.status,
                WorkflowStepStatus::Succeeded
                    | WorkflowStepStatus::Failed
                    | WorkflowStepStatus::Cancelled
                    | WorkflowStepStatus::Skipped
            )
        });
        let any_failed = steps2.iter().any(|s| {
            matches!(
                s.status,
                WorkflowStepStatus::Failed | WorkflowStepStatus::Cancelled
            )
        });
        let any_blocked = steps2
            .iter()
            .any(|s| s.status == WorkflowStepStatus::Blocked);
        if any_blocked {
            // Terminal-but-not-failed: await human/repair. No finished_at.
            self.set_workflow_run_status(run_id, WorkflowRunStatus::Blocked, None)
                .await?;
        } else if any_failed {
            self.set_workflow_run_status(run_id, WorkflowRunStatus::Failed, Some(&now_iso()))
                .await?;
        } else if all_term {
            self.set_workflow_run_status(run_id, WorkflowRunStatus::Succeeded, Some(&now_iso()))
                .await?;
        }
        Ok(created)
    }
}
