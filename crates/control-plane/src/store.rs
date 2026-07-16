//! SQLite-backed storage for the control plane (Stage 2.1).
//!
//! WAL mode, `synchronous=NORMAL`, `busy_timeout=5000`, 4-connection pool.
//! Assignment is atomic: a short `BEGIN IMMEDIATE`-style write transaction
//! selects a queued task, conditionally `UPDATE ... WHERE status='queued'`,
//! and checks `rows_affected` so concurrent schedulers can never double-assign.

use std::time::Duration;

use agentgrid_common::{
    next_attempt_status, next_task_status, Assignment, AttemptStatus, AttemptTransition,
    CompleteAttemptRequest, CreateRepositoryRequest, CreateTaskRequest, EnrollRequest,
    EnrollResponse, EventType, HeartbeatRequest, IngestEventsRequest, NodeStatus, NodeView,
    PollRequest, RepositoryView, TaskEvent, TaskStatus, TaskTransition, TaskView,
    UploadArtifactRequest,
};
use anyhow::Result;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{
    Sqlite, SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqliteSynchronous,
};
use sqlx::Row;
use uuid::Uuid;

const ASSIGNMENT_LEASE_SECS: i64 = 30;

pub struct Store {
    pool: SqlitePool,
    artifact_root: std::path::PathBuf,
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn iso_plus_secs(secs: i64) -> String {
    (chrono::Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339()
}

fn event_type_str(e: EventType) -> String {
    serde_json::to_value(e)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

fn from_snake<T: serde::de::DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string())).ok()
}

fn sha256_hex(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for b in out {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn event_type_of(s: &str) -> EventType {
    from_snake(s).unwrap_or(EventType::Stdout)
}

fn status_str(s: TaskStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

fn attempt_status_str(s: AttemptStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

fn node_status_str(s: NodeStatus) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

impl Store {
    pub async fn open(db_path: &str) -> Result<Self> {
        let opts = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5))
            .foreign_keys(true);
        let pool = PoolOptions::<Sqlite>::new()
            .max_connections(4)
            .connect_with(opts)
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        let artifact_root = std::path::Path::new(db_path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("artifacts");
        Ok(Self {
            pool,
            artifact_root,
        })
    }

    pub async fn health_check(&self) -> bool {
        sqlx::query("SELECT 1").execute(&self.pool).await.is_ok()
    }

    // ----- artifacts (Stage 2.8) -----

    /// Persist an artifact's bytes on the control-plane filesystem and record
    /// its metadata. `content` is treated as UTF-8 text (patches/logs).
    pub async fn save_artifact(&self, attempt_id: &str, req: &UploadArtifactRequest) -> Result<()> {
        let dir = self.artifact_root.join(attempt_id);
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(&req.name);
        tokio::fs::write(&path, &req.content).await?;
        let size = req.content.len() as i64;
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        sqlx::query(
            "INSERT INTO artifacts (id, attempt_id, name, size_bytes, stored_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT(attempt_id, name) DO UPDATE SET size_bytes = excluded.size_bytes, stored_at = excluded.stored_at",
        )
        .bind(&id)
        .bind(attempt_id)
        .bind(&req.name)
        .bind(size)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Resolve the latest attempt id for a task (artifacts are per-attempt).
    pub async fn latest_attempt_id(&self, task_id: &str) -> Result<Option<String>> {
        let row =
            sqlx::query("SELECT id FROM attempts WHERE task_id = ? ORDER BY number DESC LIMIT 1")
                .bind(task_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| r.try_get::<String, _>("id")).transpose()?)
    }

    /// Read a stored artifact's content by task id + name (latest attempt).
    pub async fn read_artifact(&self, task_id: &str, name: &str) -> Result<Option<String>> {
        let Some(attempt_id) = self.latest_attempt_id(task_id).await? else {
            return Ok(None);
        };
        let path = self.artifact_root.join(&attempt_id).join(name);
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    // ----- enrollment + node auth (Stage 2.3) -----

    /// Issue a one-time enrollment token (TTL 10 min). Only its hash is stored.
    pub async fn create_enrollment_token(&self) -> Result<(String, String)> {
        let token = Uuid::new_v4().to_string();
        let hash = sha256_hex(&token);
        let expires_at = iso_plus_secs(600);
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        sqlx::query(
            "INSERT INTO enrollment_tokens (id, token_hash, expires_at, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&hash)
        .bind(&expires_at)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok((token, expires_at))
    }

    /// Exchange a valid (unused, unexpired) token for a permanent node credential.
    pub async fn enroll_node(&self, req: &EnrollRequest) -> Result<Option<EnrollResponse>> {
        let mut tx = self.pool.begin().await?;
        let hash = sha256_hex(&req.token);
        let tok = sqlx::query(
            "SELECT id, expires_at, used_at FROM enrollment_tokens WHERE token_hash = ?",
        )
        .bind(&hash)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(tok) = tok else {
            let _ = tx.rollback().await;
            return Ok(None);
        };
        let expires: String = tok.try_get("expires_at")?;
        let used: Option<String> = tok.try_get("used_at")?;
        if used.is_some() || expires < now_iso() {
            let _ = tx.rollback().await;
            return Ok(None);
        }
        let node_id = Uuid::new_v4().to_string();
        let credential = Uuid::new_v4().to_string();
        let cred_hash = sha256_hex(&credential);
        let now = now_iso();
        let adapters = serde_json::to_string(&req.adapters)?;
        let repos = serde_json::to_string(&req.repositories)?;
        sqlx::query(
            "INSERT INTO nodes (id, name, status, agent_version, max_concurrency, adapters, repositories, active_attempts, last_heartbeat_at, credential_hash, created_at) \
             VALUES (?, ?, 'online', ?, ?, ?, ?, 0, ?, ?, ?)",
        )
        .bind(&node_id)
        .bind(&req.name)
        .bind(&req.agent_version)
        .bind(req.max_concurrency as i64)
        .bind(&adapters)
        .bind(&repos)
        .bind(&now)
        .bind(&cred_hash)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE enrollment_tokens SET used_at = ? WHERE id = ?")
            .bind(&now)
            .bind(tok.try_get::<String, _>("id")?)
            .execute(&mut *tx)
            .await?;
        self.audit_tx(&mut tx, "node", Some(&node_id), "enroll", None, None)
            .await?;
        tx.commit().await?;
        Ok(Some(EnrollResponse {
            node_id,
            credential,
        }))
    }

    /// Resolve a node credential to its node id, or None if unknown or revoked.
    pub async fn node_id_for_credential(&self, credential: &str) -> Result<Option<String>> {
        let hash = sha256_hex(credential);
        let row = sqlx::query("SELECT id, status FROM nodes WHERE credential_hash = ?")
            .bind(&hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match row {
            Some(r) => {
                let status: String = r.try_get("status")?;
                if status == "revoked" {
                    None
                } else {
                    Some(r.try_get("id")?)
                }
            }
            None => None,
        })
    }

    /// Record a heartbeat: refresh capabilities/load and last-seen time.
    pub async fn heartbeat(&self, node_id: &str, req: &HeartbeatRequest) -> Result<bool> {
        let status = req.status.unwrap_or(NodeStatus::Online);
        let adapters = serde_json::to_string(&req.adapters)?;
        let repos = serde_json::to_string(&req.repositories)?;
        let now = now_iso();
        let affected = sqlx::query(
            "UPDATE nodes SET name = ?, \
               status = CASE WHEN status = 'revoked' THEN 'revoked' ELSE ? END, \
               agent_version = ?, max_concurrency = ?, adapters = ?, repositories = ?, \
               active_attempts = ?, load_avg = ?, free_disk_mb = ?, last_heartbeat_at = ? \
             WHERE id = ?",
        )
        .bind(&req.name)
        .bind(node_status_str(status))
        .bind(&req.agent_version)
        .bind(req.max_concurrency as i64)
        .bind(&adapters)
        .bind(&repos)
        .bind(req.active_attempts as i64)
        .bind(req.load_avg)
        .bind(req.free_disk_mb as i64)
        .bind(&now)
        .bind(node_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(affected == 1)
    }

    /// Revoke a node: reject its credential immediately, mark `revoked`.
    pub async fn revoke_node(&self, node_id: &str) -> Result<bool> {
        let now = now_iso();
        let affected =
            sqlx::query("UPDATE nodes SET status = 'revoked', revoked_at = ? WHERE id = ?")
                .bind(&now)
                .bind(node_id)
                .execute(&self.pool)
                .await?
                .rows_affected();
        if affected == 1 {
            self.audit("node", Some(node_id), "revoke", None, None)
                .await?;
        }
        Ok(affected == 1)
    }

    pub async fn audit(
        &self,
        actor_type: &str,
        actor_id: Option<&str>,
        action: &str,
        subject: Option<&str>,
        payload: Option<&str>,
    ) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        sqlx::query(
            "INSERT INTO audit_events (id, actor_type, actor_id, action, subject, payload, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(actor_type)
        .bind(actor_id)
        .bind(action)
        .bind(subject)
        .bind(payload)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn audit_tx(
        &self,
        tx: &mut sqlx::Transaction<'_, Sqlite>,
        actor_type: &str,
        actor_id: Option<&str>,
        action: &str,
        subject: Option<&str>,
        payload: Option<&str>,
    ) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        sqlx::query(
            "INSERT INTO audit_events (id, actor_type, actor_id, action, subject, payload, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(actor_type)
        .bind(actor_id)
        .bind(action)
        .bind(subject)
        .bind(payload)
        .bind(&now)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    pub async fn create_task(&self, req: &CreateTaskRequest) -> Result<TaskView> {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        let timeout_secs = req.timeout_secs.unwrap_or(3600) as i64;
        sqlx::query(
            "INSERT INTO tasks (id, repository, prompt, adapter, requested_node_id, status, created_at, timeout_secs) \
             VALUES (?, ?, ?, ?, ?, 'queued', ?, ?)",
        )
        .bind(&id)
        .bind(&req.repository)
        .bind(&req.prompt)
        .bind(&req.adapter)
        .bind(&req.requested_node_id)
        .bind(&now)
        .bind(timeout_secs)
        .execute(&self.pool)
        .await?;
        Ok(TaskView {
            id,
            repository: req.repository.clone(),
            prompt: req.prompt.clone(),
            adapter: req.adapter.clone(),
            status: TaskStatus::Queued,
            created_at: now,
            finished_at: None,
            assigned_attempt_id: None,
        })
    }

    pub async fn list_tasks(&self) -> Result<Vec<TaskView>> {
        let rows = sqlx::query(
            "SELECT id, repository, prompt, adapter, status, created_at, finished_at, assigned_attempt_id \
             FROM tasks ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_task_view).collect())
    }

    pub async fn show_task(&self, id: &str) -> Result<Option<TaskView>> {
        let row = sqlx::query(
            "SELECT id, repository, prompt, adapter, status, created_at, finished_at, assigned_attempt_id \
             FROM tasks WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_task_view))
    }

    pub async fn get_events(&self, task_id: &str, after: u64) -> Result<Vec<TaskEvent>> {
        let attempt_rows = sqlx::query("SELECT id FROM attempts WHERE task_id = ?")
            .bind(task_id)
            .fetch_all(&self.pool)
            .await?;
        let mut events: Vec<TaskEvent> = Vec::new();
        for a in attempt_rows {
            let aid: String = a.try_get("id")?;
            let rows = sqlx::query(
                "SELECT attempt_id, sequence, type, payload, created_at FROM task_events \
                 WHERE attempt_id = ? AND sequence > ? ORDER BY sequence ASC",
            )
            .bind(&aid)
            .bind(after as i64)
            .fetch_all(&self.pool)
            .await?;
            for r in rows {
                let payload_text: String = r.try_get("payload")?;
                events.push(TaskEvent {
                    attempt_id: r.try_get("attempt_id")?,
                    sequence: r.try_get::<i64, _>("sequence")? as u64,
                    r#type: event_type_of(&r.try_get::<String, _>("type")?),
                    payload: serde_json::from_str(&payload_text).unwrap_or(serde_json::Value::Null),
                    created_at: r.try_get("created_at")?,
                });
            }
        }
        events.sort_by_key(|e| e.sequence);
        Ok(events)
    }

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

    pub async fn list_repositories(&self) -> Result<Vec<RepositoryView>> {
        let rows = sqlx::query(
            "SELECT id, name, git_url, default_branch, validation_command, created_at FROM repositories",
        )
        .fetch_all(&self.pool)
        .await?;
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

    pub async fn list_nodes(&self) -> Result<Vec<NodeView>> {
        let rows = sqlx::query(
            "SELECT id, name, status, adapters, repositories, max_concurrency, active_attempts, last_heartbeat_at, agent_version, load_avg, free_disk_mb \
             FROM nodes ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_node_view).collect())
    }

    /// Register a newly seen node or refresh an existing one (acts as heartbeat).
    pub async fn register_or_touch_node(&self, req: &PollRequest) -> Result<()> {
        let now = now_iso();
        let adapters = serde_json::to_string(&req.adapters)?;
        let repositories = serde_json::to_string(&req.repositories)?;
        sqlx::query(
            "INSERT INTO nodes (id, name, status, max_concurrency, adapters, repositories, active_attempts, last_heartbeat_at, created_at) \
             VALUES (?, ?, 'online', ?, ?, ?, 0, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                name = excluded.name, \
                max_concurrency = excluded.max_concurrency, \
                adapters = excluded.adapters, \
                repositories = excluded.repositories, \
                last_heartbeat_at = excluded.last_heartbeat_at, \
                status = CASE WHEN nodes.status IN ('offline','pending') THEN 'online' ELSE nodes.status END",
        )
        .bind(&req.node_id)
        .bind(&req.name)
        .bind(req.max_concurrency as i64)
        .bind(&adapters)
        .bind(&repositories)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Atomic, race-free assignment of one queued task to `node_id`.
    pub async fn try_assign(&self, node_id: &str) -> Result<Option<Assignment>> {
        let mut tx = self.pool.begin().await?;
        let cand = sqlx::query(
            "SELECT id, prompt, adapter, repository, timeout_secs FROM tasks \
             WHERE status = 'queued' AND (requested_node_id IS NULL OR requested_node_id = ?) \
             ORDER BY created_at ASC LIMIT 1",
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?;

        let Some(c) = cand else {
            let _ = tx.rollback().await;
            return Ok(None);
        };
        let task_id: String = c.try_get("id")?;
        let prompt: String = c.try_get("prompt")?;
        let adapter: String = c.try_get("adapter")?;
        let repository: String = c.try_get("repository")?;
        let timeout_secs: i64 = c.try_get("timeout_secs")?;

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
            "SELECT status, adapters, repositories, max_concurrency, active_attempts FROM nodes WHERE id = ?",
        )
        .bind(node_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(node) = node else {
            let _ = tx.rollback().await;
            return Ok(None);
        };
        let node_status: String = node.try_get("status")?;
        if node_status != "online" {
            let _ = tx.rollback().await;
            return Ok(None);
        }
        let node_adapters: String = node.try_get("adapters")?;
        let node_repos: String = node.try_get("repositories")?;
        let max_c: i64 = node.try_get("max_concurrency")?;
        let active: i64 = node.try_get("active_attempts")?;
        if active >= max_c {
            let _ = tx.rollback().await;
            return Ok(None);
        }
        let adapters: Vec<String> = serde_json::from_str(&node_adapters).unwrap_or_default();
        let repos: Vec<String> = serde_json::from_str(&node_repos).unwrap_or_default();
        if !adapters.contains(&adapter) {
            let _ = tx.rollback().await;
            return Ok(None);
        }
        if !repos.iter().any(|r| r == "*" || r == &repository) {
            let _ = tx.rollback().await;
            return Ok(None);
        }

        let attempt_id = Uuid::new_v4().to_string();
        let number = self.attempt_count(&mut tx, &task_id).await? + 1;
        let lease = iso_plus_secs(ASSIGNMENT_LEASE_SECS);
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
        sqlx::query(
            "INSERT INTO attempts (id, task_id, number, node_id, status, lease_expires_at, started_at) \
             VALUES (?, ?, ?, ?, 'assigned', ?, ?)",
        )
        .bind(&attempt_id)
        .bind(&task_id)
        .bind(number as i64)
        .bind(node_id)
        .bind(&lease)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE nodes SET active_attempts = active_attempts + 1 WHERE id = ?")
            .bind(node_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;

        Ok(Some(Assignment {
            attempt_id,
            task_id,
            repository,
            prompt,
            adapter,
            number: number as u32,
            timeout_secs: timeout_secs as u64,
            git_url,
            default_branch,
            validation_command,
        }))
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

    pub async fn ingest_events(&self, attempt_id: &str, req: &IngestEventsRequest) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let attempt = sqlx::query("SELECT task_id, status FROM attempts WHERE id = ?")
            .bind(attempt_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(attempt) = attempt else {
            let _ = tx.rollback().await;
            return Ok(false);
        };
        let task_id: String = attempt.try_get("task_id")?;
        let attempt_status: String = attempt.try_get("status")?;

        if attempt_status == "assigned" {
            sqlx::query("UPDATE attempts SET status = 'running' WHERE id = ?")
                .bind(attempt_id)
                .execute(&mut *tx)
                .await?;
            sqlx::query("UPDATE tasks SET status = 'running', started_at = ? WHERE id = ?")
                .bind(now_iso())
                .bind(&task_id)
                .execute(&mut *tx)
                .await?;
        }

        for ev in &req.events {
            let payload = serde_json::to_string(&ev.payload)?;
            let id = Uuid::new_v4().to_string();
            sqlx::query(
                "INSERT INTO task_events (id, attempt_id, sequence, type, payload, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(attempt_id, sequence) DO NOTHING",
            )
            .bind(&id)
            .bind(attempt_id)
            .bind(ev.sequence as i64)
            .bind(event_type_str(ev.r#type))
            .bind(&payload)
            .bind(now_iso())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    pub async fn complete_attempt(
        &self,
        attempt_id: &str,
        req: &CompleteAttemptRequest,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let attempt = sqlx::query(
            "SELECT task_id, node_id, status, cancel_requested FROM attempts WHERE id = ?",
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

        let at = if req.exit_code == 0 {
            AttemptTransition::Succeed
        } else {
            AttemptTransition::Fail
        };
        let tt = if req.exit_code == 0 {
            TaskTransition::Succeed
        } else {
            TaskTransition::Fail
        };

        let as_enum = from_snake::<AttemptStatus>(&attempt_status);
        let attempt_target: AttemptStatus = as_enum
            .and_then(|s| next_attempt_status(s, at).ok())
            .unwrap_or(if req.exit_code == 0 {
                AttemptStatus::Succeeded
            } else {
                AttemptStatus::Failed
            });

        let task_row = sqlx::query("SELECT status FROM tasks WHERE id = ?")
            .bind(&task_id)
            .fetch_one(&mut *tx)
            .await?;
        let task_status: String = task_row.try_get("status")?;
        let ts_enum = from_snake::<TaskStatus>(&task_status);
        let task_target: TaskStatus = ts_enum
            .and_then(|s| next_task_status(s, tt).ok())
            .unwrap_or(if req.exit_code == 0 {
                TaskStatus::Succeeded
            } else {
                TaskStatus::Failed
            });

        // If cancellation was requested, the attempt ends as cancelled
        // regardless of the adapter's exit code.
        let (attempt_target, task_target) = if cancel_requested != 0 {
            let a = as_enum
                .and_then(|s| next_attempt_status(s, AttemptTransition::Cancel).ok())
                .unwrap_or(AttemptStatus::Cancelled);
            let t = ts_enum
                .and_then(|s| next_task_status(s, TaskTransition::Cancel).ok())
                .unwrap_or(TaskStatus::Cancelled);
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
        if let Some(sha) = &req.commit_sha {
            sqlx::query("UPDATE attempts SET commit_sha = ? WHERE id = ?")
                .bind(sha)
                .bind(attempt_id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE tasks SET status = ?, finished_at = ? WHERE id = ?")
            .bind(status_str(task_target))
            .bind(&now)
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

    pub async fn cancel_task(&self, task_id: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
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
                "UPDATE tasks SET status = 'cancelled', assigned_attempt_id = NULL WHERE id = ?",
            )
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

    pub async fn retry_task(&self, task_id: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
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
        Ok(false)
    }

    /// Background maintenance: revert unconfirmed assignments (lease expired)
    /// and mark silent nodes offline. Spawns a detached task.
    pub fn start_maintenance(&self) {
        let pool = self.pool.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let now = now_iso();
                if let Err(e) = revert_expired_leases(&pool, &now).await {
                    tracing::warn!("lease maintenance failed: {e}");
                }
                if let Err(e) = mark_offline_nodes(&pool, &now).await {
                    tracing::warn!("node maintenance failed: {e}");
                }
            }
        });
    }
}

async fn revert_expired_leases(pool: &SqlitePool, now: &str) -> Result<()> {
    let rows = sqlx::query(
        "SELECT id, task_id, node_id FROM attempts WHERE status = 'assigned' AND lease_expires_at < ?",
    )
    .bind(now)
    .fetch_all(pool)
    .await?;
    for r in rows {
        let attempt_id: String = r.try_get("id")?;
        let task_id: String = r.try_get("task_id")?;
        let node_id: String = r.try_get("node_id")?;
        let mut tx = pool.begin().await?;
        sqlx::query("UPDATE attempts SET status = 'cancelled' WHERE id = ?")
            .bind(&attempt_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE tasks SET status = 'queued', assigned_attempt_id = NULL WHERE id = ?")
            .bind(&task_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE nodes SET active_attempts = MAX(0, active_attempts - 1) WHERE id = ?")
            .bind(&node_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    Ok(())
}

async fn mark_offline_nodes(pool: &SqlitePool, now: &str) -> Result<()> {
    // last_heartbeat_at older than 30s and still 'online' -> offline.
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
    sqlx::query(
        "UPDATE nodes SET status = 'offline' WHERE status = 'online' AND (last_heartbeat_at IS NULL OR last_heartbeat_at < ?)",
    )
    .bind(&cutoff)
    .execute(pool)
    .await?;
    let _ = now;
    Ok(())
}

fn row_to_task_view(r: &sqlx::sqlite::SqliteRow) -> TaskView {
    TaskView {
        id: r.try_get("id").unwrap_or_default(),
        repository: r.try_get("repository").unwrap_or_default(),
        prompt: r.try_get("prompt").unwrap_or_default(),
        adapter: r.try_get("adapter").unwrap_or_default(),
        status: from_snake(&r.try_get::<String, _>("status").unwrap_or_default())
            .unwrap_or(TaskStatus::Queued),
        created_at: r.try_get("created_at").unwrap_or_default(),
        finished_at: r.try_get("finished_at").unwrap_or_default(),
        assigned_attempt_id: r.try_get("assigned_attempt_id").unwrap_or_default(),
    }
}

fn row_to_node_view(r: &sqlx::sqlite::SqliteRow) -> NodeView {
    let adapters: String = r.try_get("adapters").unwrap_or_default();
    let repositories: String = r.try_get("repositories").unwrap_or_default();
    NodeView {
        id: r.try_get("id").unwrap_or_default(),
        name: r.try_get("name").unwrap_or_default(),
        status: from_snake(&r.try_get::<String, _>("status").unwrap_or_default())
            .unwrap_or(NodeStatus::Pending),
        adapters: serde_json::from_str(&adapters).unwrap_or_default(),
        repositories: serde_json::from_str(&repositories).unwrap_or_default(),
        max_concurrency: r.try_get::<i64, _>("max_concurrency").unwrap_or(1) as u32,
        active_attempts: r.try_get::<i64, _>("active_attempts").unwrap_or(0) as u32,
        last_heartbeat_at: r.try_get("last_heartbeat_at").unwrap_or_default(),
        agent_version: r.try_get("agent_version").unwrap_or_default(),
        load_avg: r.try_get::<f64, _>("load_avg").unwrap_or(0.0),
        free_disk_mb: r.try_get::<i64, _>("free_disk_mb").unwrap_or(0) as u64,
    }
}
