//! Plan 2.1 (#18): org layer — long-lived agents (identity, role, prompt,
//! skills, budget) with an immutable `agent_actions` trail and scheduled
//! heartbeats. Extracted from `store.rs`.

use super::{is_safe_opaque_id, now_iso, Store};
use agentgrid_common::{Agent, AgentAction, AgentCreate, CreateTaskRequest};
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

fn agent_from_row(r: &sqlx::sqlite::SqliteRow) -> Result<Agent> {
    let skills: Vec<String> = r
        .try_get::<String, _>("skills_json")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    Ok(Agent {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        role: r.try_get("role")?,
        prompt: r.try_get("prompt")?,
        skills,
        budget_usd: r.try_get("budget_usd")?,
        max_tasks: r.try_get("max_tasks")?,
        heartbeat_interval_secs: r.try_get("heartbeat_interval_secs")?,
        last_heartbeat_at: r.try_get("last_heartbeat_at")?,
        created_at: r.try_get("created_at")?,
        tasks_spent: r.try_get("tasks_spent")?,
    })
}

impl Store {
    /// Create an org agent. Name must be unique and path-safe; the id is a
    /// fresh uuid.
    pub async fn create_agent(&self, req: &AgentCreate) -> Result<Agent> {
        let name = req.name.trim().to_string();
        if name.is_empty() || !is_safe_opaque_id(&name) {
            anyhow::bail!("agent name must be non-empty alphanumeric/-/_");
        }
        if req.role.trim().is_empty() {
            anyhow::bail!("agent role must be non-empty");
        }
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        let skills = req.skills.join(",");
        sqlx::query(
            "INSERT INTO agents (id, name, role, prompt, skills_json, budget_usd, max_tasks, heartbeat_interval_secs, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&name)
        .bind(req.role.trim())
        .bind(&req.prompt)
        .bind(&skills)
        .bind(req.budget_usd)
        .bind(req.max_tasks)
        .bind(req.heartbeat_interval_secs)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        self.log_agent_action(&id, "created", "agent registered")
            .await?;
        self.get_agent(&id).await.map(|a| a.unwrap())
    }

    /// List all agents with their current task spend.
    pub async fn list_agents(&self) -> Result<Vec<Agent>> {
        let rows = sqlx::query(
            "SELECT a.*, (SELECT COUNT(*) FROM tasks WHERE tasks.agent_id = a.id) AS tasks_spent \
             FROM agents a ORDER BY a.name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(agent_from_row).collect()
    }

    /// Fetch one agent with its current task spend.
    pub async fn get_agent(&self, id: &str) -> Result<Option<Agent>> {
        let row = sqlx::query(
            "SELECT a.*, (SELECT COUNT(*) FROM tasks WHERE tasks.agent_id = a.id) AS tasks_spent \
             FROM agents a WHERE a.id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(agent_from_row).transpose()
    }

    /// Append an immutable trail row (audit log).
    pub async fn log_agent_action(&self, agent_id: &str, action: &str, detail: &str) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        sqlx::query(
            "INSERT INTO agent_actions (id, agent_id, action, detail, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(agent_id)
        .bind(action)
        .bind(detail)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// List an agent's action trail (newest first).
    pub async fn agent_actions(&self, agent_id: &str) -> Result<Vec<AgentAction>> {
        let rows = sqlx::query(
            "SELECT id, agent_id, action, detail, created_at FROM agent_actions \
             WHERE agent_id = ? ORDER BY created_at DESC, id DESC LIMIT 500",
        )
        .bind(agent_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|r| {
                Ok(AgentAction {
                    id: r.try_get("id")?,
                    agent_id: r.try_get("agent_id")?,
                    action: r.try_get("action")?,
                    detail: r.try_get("detail")?,
                    created_at: r.try_get("created_at")?,
                })
            })
            .collect()
    }

    /// Create a task attributed to an agent, enforcing the budget hard-stop.
    /// When the agent's `max_tasks` is exhausted the task is rejected
    /// (Err) and a `budget_exceeded` trail row is written. A missing agent id
    /// also rejects (attribution must reference a real agent).
    ///
    /// Atomic (audit follow-up): the spend read, the budget check, the trail
    /// row and the attributed insert all run in one `BEGIN IMMEDIATE`
    /// transaction. The previous check-then-act let two concurrent creations
    /// both observe `tasks_spent = max - 1` and both insert, exceeding
    /// `max_tasks`.
    pub async fn create_agent_task(
        &self,
        agent_id: &str,
        req: &CreateTaskRequest,
    ) -> Result<agentgrid_common::TaskView> {
        if self.get_agent(agent_id).await?.is_none() {
            anyhow::bail!("unknown agent {agent_id}");
        }
        let mut r = req.clone();
        r.agent_id = Some(agent_id.to_string());
        let mut tx = self.write_txn().await?;
        // Spend counted inside the same transaction as the insert.
        let tasks_spent: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks WHERE agent_id = ?")
            .bind(agent_id)
            .fetch_one(&mut *tx)
            .await?;
        let max_tasks: Option<i64> =
            sqlx::query_scalar("SELECT max_tasks FROM agents WHERE id = ?")
                .bind(agent_id)
                .fetch_one(&mut *tx)
                .await?;
        if let Some(max) = max_tasks {
            if tasks_spent >= max {
                sqlx::query(
                    "INSERT INTO agent_actions (id, agent_id, action, detail, created_at) VALUES (?, ?, 'budget_exceeded', 'task rejected', ?)",
                )
                .bind(Uuid::new_v4().to_string())
                .bind(agent_id)
                .bind(now_iso())
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                anyhow::bail!("agent {agent_id} budget exhausted ({tasks_spent} >= {max})");
            }
        }
        sqlx::query(
            "INSERT INTO agent_actions (id, agent_id, action, detail, created_at) VALUES (?, ?, 'task_created', ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(agent_id)
        .bind(format!(
            "task prompt: {}",
            req.prompt.chars().take(60).collect::<String>()
        ))
        .bind(now_iso())
        .execute(&mut *tx)
        .await?;
        let view = super::tasks::insert_task_tx(&r, &mut tx).await?;
        tx.commit().await?;
        Ok(view)
    }

    /// Agents whose heartbeat is due at `now_unix` (interval set and
    /// last_heartbeat + interval <= now, or never fired).
    pub async fn due_agents(&self, now_unix: i64) -> Result<Vec<Agent>> {
        let rows = sqlx::query(
            "SELECT a.*, (SELECT COUNT(*) FROM tasks WHERE tasks.agent_id = a.id) AS tasks_spent \
             FROM agents a \
             WHERE a.heartbeat_interval_secs IS NOT NULL \
               AND (a.last_heartbeat_at IS NULL OR \
                    CAST(strftime('%s', a.last_heartbeat_at) AS INTEGER) + a.heartbeat_interval_secs <= ?) \
             ORDER BY a.name",
        )
        .bind(now_unix)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(agent_from_row).collect()
    }

    /// Record a heartbeat fire (updates `last_heartbeat_at`).
    pub async fn record_agent_heartbeat(&self, agent_id: &str) -> Result<()> {
        let now = now_iso();
        sqlx::query("UPDATE agents SET last_heartbeat_at = ? WHERE id = ?")
            .bind(&now)
            .bind(agent_id)
            .execute(&self.pool)
            .await?;
        self.log_agent_action(agent_id, "heartbeat", "scheduled task spawned")
            .await
    }
}
