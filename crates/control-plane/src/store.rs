//! SQLite-backed storage for the control plane (Stage 2.1).
//!
//! WAL mode, `synchronous=NORMAL`, `busy_timeout=5000`, 4-connection pool.
//! Assignment is atomic: a short `BEGIN IMMEDIATE`-style write transaction
//! selects a queued task, conditionally `UPDATE ... WHERE status='queued'`,
//! and checks `rows_affected` so concurrent schedulers can never double-assign.

use std::time::Duration;

use agentgrid_common::{
    next_approval, next_attempt_status, next_task_status, AgentProfile, AgentProfileCreate,
    AgentSession, ApprovalEvent, ApprovalStatus, ApprovalView, ArtifactMeta, Assignment,
    AttemptStatus, AttemptTransition, CompleteAttemptRequest, CreateRepositoryRequest,
    CreateTaskRequest, EnrollRequest, EnrollResponse, EventType, HeartbeatRequest,
    IngestEventsRequest, McpServer, McpServerCreate, NodeEligibility, NodeStatus, NodeView,
    PollRequest, RepositoryView, SkillTrustView, TaskEligibility, TaskEvent, TaskStatus,
    TaskTransition, TaskView, UploadArtifactRequest, WorkflowBudget, WorkflowRole, WorkflowRun,
    WorkflowRunStatus, WorkflowSchedule, WorkflowScheduleCreate, WorkflowStep, WorkflowStepRun,
    WorkflowStepStatus, WorkflowTemplate,
};
use anyhow::Result;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{
    Sqlite, SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqliteSynchronous,
};
use sqlx::Row;
use uuid::Uuid;

const ASSIGNMENT_LEASE_SECS: i64 = 30;
/// Window after assignment within which the node must ack (Stage 1.3). An
/// unacked assignment is reverted (returned to the queue) once this passes.
const ACK_DEADLINE_SECS: i64 = 30;

#[derive(Clone)]
pub struct Store {
    pub pool: SqlitePool,
    artifact_root: std::path::PathBuf,
    /// Observability: last scheduler latency (queued→assigned) in ms and total
    /// assignments (Stage 2.5 ops). Wrapped in Arc so `Store` can derive Clone.
    pub(crate) scheduler_latency_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub(crate) scheduler_assignments: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Stage 2.5 ops: last `PRAGMA wal_checkpoint(TRUNCATE)` duration in ms.
    pub(crate) checkpoint_ms: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Stage 2.5 ops: cumulative count of `SQLITE_BUSY`-class failures.
    pub(crate) sqlite_busy: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Parse a profile autonomy string (`l0`..`l4`) into an `AutonomyLevel`.
fn parse_autonomy_level(s: &str) -> Option<agentgrid_common::AutonomyLevel> {
    serde_json::from_value(serde_json::Value::String(s.trim().to_ascii_lowercase())).ok()
}

/// Parse an RFC3339 timestamp into a unix epoch seconds.
fn iso_to_unix(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.timestamp())
}

/// Format a unix epoch seconds as RFC3339 (UTC).
fn unix_to_iso(unix: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(unix, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_default()
}

/// Build a `WorkflowSchedule` from a row.
fn schedule_from_row(r: &sqlx::sqlite::SqliteRow) -> WorkflowSchedule {
    WorkflowSchedule {
        id: r.try_get("id").unwrap_or_default(),
        template_id: r.try_get("template_id").unwrap_or_default(),
        interval_seconds: r.try_get("interval_seconds").unwrap_or(60),
        autonomy: r.try_get("autonomy").unwrap_or_else(|_| "l2".to_string()),
        last_run_at: r.try_get("last_run_at").unwrap_or_default(),
        enabled: r.try_get::<i64, _>("enabled").unwrap_or(1) != 0,
        created_at: r.try_get("created_at").unwrap_or_default(),
    }
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

/// Argon2id hash of a password (Stage 4.1).
fn hash_password(password: &str) -> Result<String> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    use argon2::Argon2;
    use rand::rngs::OsRng;
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)?
        .to_string();
    Ok(hash)
}

/// Verify a password against an Argon2id hash string (Stage 4.1).
fn verify_password(password: &str, hash: &str) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    use argon2::Argon2;
    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
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
        // Stage 2.5: fail fast on a corrupt database rather than serving bad state.
        sqlx::query("PRAGMA quick_check")
            .execute(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("sqlite quick_check failed: {e}"))?;
        // Warm the schema cookie on every pooled connection. A connection
        // opened after the migrations ran still recompiles statements against
        // migrated tables on first use, which is slow and briefly locks; a
        // throwaway read on each connection avoids that cost on hot paths.
        for _ in 0..4 {
            let mut c = pool.acquire().await?;
            sqlx::query("SELECT name FROM sqlite_master WHERE type = 'table'")
                .execute(&mut *c)
                .await?;
        }
        let artifact_root = std::path::Path::new(db_path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("artifacts");
        Ok(Self {
            pool,
            artifact_root,
            scheduler_latency_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            scheduler_assignments: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            checkpoint_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            sqlite_busy: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        })
    }

    pub async fn health_check(&self) -> bool {
        sqlx::query("SELECT 1").execute(&self.pool).await.is_ok()
    }

    // ----- users + auth (Stage 4.1) -----

    /// Number of local users (0 means the install is in its open bootstrap window).
    pub async fn user_count(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS c FROM users")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<i64, _>("c")?)
    }

    /// Create a local user. Returns false if the username already exists.
    pub async fn create_user(&self, username: &str, password: &str) -> Result<bool> {
        if self.user_exists(username).await? {
            return Ok(false);
        }
        let id = Uuid::new_v4().to_string();
        let hash = hash_password(password)?;
        let now = now_iso();
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(username)
        .bind(&hash)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(true)
    }

    pub async fn user_exists(&self, username: &str) -> Result<bool> {
        let row = sqlx::query("SELECT COUNT(*) AS c FROM users WHERE username = ?")
            .bind(username)
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<i64, _>("c")? > 0)
    }

    /// Verify a username/password pair. Returns the user id on success.
    pub async fn verify_user(&self, username: &str, password: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT id, password_hash FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let id: String = row.try_get("id")?;
        let hash: String = row.try_get("password_hash")?;
        Ok(if verify_password(password, &hash) {
            Some(id)
        } else {
            None
        })
    }

    // ----- artifacts (Stage 2.8) -----

    /// Persist an artifact's bytes on the control-plane filesystem and record
    /// its metadata. `content` is treated as UTF-8 text (patches/logs).
    /// Resolve `attempt_id/name` to an absolute path inside the artifact root,
    /// rejecting traversal. Canonicalizes the parent (created lazily) and checks
    /// the final name is a single safe segment so a symlinked worktree dir or a
    /// `..`-laden name cannot escape the root (Stage 2.2 defense-in-depth).
    fn artifact_path(&self, attempt_id: &str, name: &str) -> Result<std::path::PathBuf> {
        let dir = self.artifact_root.join(attempt_id);
        // Canonicalize the existing artifact dir; if it does not exist yet the
        // caller (save_artifact) creates it first, so this is mainly read-side.
        let canon_root = self
            .artifact_root
            .canonicalize()
            .unwrap_or_else(|_| self.artifact_root.clone());
        let canon_dir = dir.canonicalize().unwrap_or(dir.clone());
        if !canon_dir.starts_with(&canon_root) {
            anyhow::bail!("artifact dir escapes root");
        }
        // Single safe segment: no separators / traversal / NUL / control chars.
        if name.is_empty()
            || name.len() > 255
            || name.contains('/')
            || name.contains('\\')
            || name.contains('\0')
            || name == "."
            || name == ".."
            || name.chars().any(|c| c.is_control())
        {
            anyhow::bail!("invalid artifact name");
        }
        Ok(canon_dir.join(name))
    }

    pub async fn save_artifact(&self, attempt_id: &str, req: &UploadArtifactRequest) -> Result<()> {
        self.save_artifact_bytes(
            attempt_id,
            &req.name,
            req.content.as_bytes(),
            req.media_type.as_deref(),
            req.sha256.as_deref(),
        )
        .await
    }

    /// Stage 2.2 binary-safe artifact write: raw bytes + optional media type
    /// and hex SHA-256. Idempotent per (attempt_id, name). The legacy text
    /// endpoint forwards here with `content.as_bytes()`.
    pub async fn save_artifact_bytes(
        &self,
        attempt_id: &str,
        name: &str,
        bytes: &[u8],
        media_type: Option<&str>,
        sha256: Option<&str>,
    ) -> Result<()> {
        let dir = self.artifact_root.join(attempt_id);
        tokio::fs::create_dir_all(&dir).await?;
        let path = self.artifact_path(attempt_id, name)?;
        tokio::fs::write(&path, bytes).await?;
        let size = bytes.len() as i64;
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        sqlx::query(
            "INSERT INTO artifacts (id, attempt_id, name, size_bytes, stored_at, media_type, sha256) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(attempt_id, name) DO UPDATE SET \
                size_bytes = excluded.size_bytes, \
                stored_at = excluded.stored_at, \
                media_type = excluded.media_type, \
                sha256 = excluded.sha256",
        )
        .bind(&id)
        .bind(attempt_id)
        .bind(name)
        .bind(size)
        .bind(&now)
        .bind(media_type)
        .bind(sha256)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Read a stored artifact's metadata by task id + name (latest attempt).
    pub async fn read_artifact_meta(
        &self,
        task_id: &str,
        name: &str,
    ) -> Result<Option<ArtifactMeta>> {
        let Some(attempt_id) = self.latest_attempt_id(task_id).await? else {
            return Ok(None);
        };
        let row = sqlx::query(
            "SELECT size_bytes, media_type, sha256 FROM artifacts WHERE attempt_id = ? AND name = ?",
        )
        .bind(&attempt_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| ArtifactMeta {
            size_bytes: r.try_get::<i64, _>("size_bytes").unwrap_or(0),
            media_type: r.try_get::<Option<String>, _>("media_type").ok().flatten(),
            sha256: r.try_get::<Option<String>, _>("sha256").ok().flatten(),
        }))
    }

    /// Read a stored artifact's raw bytes by task id + name (latest attempt).
    pub async fn read_artifact_bytes(&self, task_id: &str, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(attempt_id) = self.latest_attempt_id(task_id).await? else {
            return Ok(None);
        };
        let path = match self.artifact_path(&attempt_id, name) {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        match tokio::fs::read(&path).await {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
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
        let path = match self.artifact_path(&attempt_id, name) {
            Ok(p) => p,
            // Invalid/traversal name: treat as absent rather than erroring,
            // so a crafted request cannot distinguish a valid artifact.
            Err(_) => return Ok(None),
        };
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
        if affected == 1 && status == NodeStatus::Offline {
            lose_node_attempts(&self.pool, node_id).await?;
        }
        Ok(affected == 1)
    }

    /// Revoke a node: reject its credential immediately, mark `revoked`, and
    /// lose any in-flight attempts (Stage 1.2).
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
            lose_node_attempts(&self.pool, node_id).await?;
        }
        Ok(affected == 1)
    }

    /// Mark a node offline (unless already revoked) and lose its in-flight
    /// attempts. Triggered by stale-heartbeat maintenance, a self-reported
    /// offline status, or an explicit admin action (Stage 1.2).
    pub async fn mark_node_offline(&self, node_id: &str) -> Result<bool> {
        let now = now_iso();
        let affected = sqlx::query(
            "UPDATE nodes SET status = 'offline', last_heartbeat_at = ? \
             WHERE id = ? AND status != 'revoked'",
        )
        .bind(&now)
        .bind(node_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if affected == 1 {
            lose_node_attempts(&self.pool, node_id).await?;
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

    /// Most-recent audit events (newest first), optionally filtered by action.
    pub async fn list_audit(&self, action: Option<&str>, limit: i64) -> Result<Vec<AuditEvent>> {
        let rows = match action {
            Some(a) => {
                sqlx::query(
                    "SELECT id, actor_type, actor_id, action, subject, payload, created_at \
                     FROM audit_events WHERE action = ? ORDER BY created_at DESC LIMIT ?",
                )
                .bind(a)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT id, actor_type, actor_id, action, subject, payload, created_at \
                     FROM audit_events ORDER BY created_at DESC LIMIT ?",
                )
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows.iter().map(audit_from_row).collect())
    }

    pub async fn create_task(&self, req: &CreateTaskRequest) -> Result<TaskView> {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        let timeout_secs = req.timeout_secs.unwrap_or(3600) as i64;
        sqlx::query(
            "INSERT INTO tasks (id, repository, prompt, adapter, requested_node_id, base_commit, parent_acp_session_id, status, created_at, timeout_secs, validation_command) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 'queued', ?, ?, ?)",
        )
        .bind(&id)
        .bind(&req.repository)
        .bind(&req.prompt)
        .bind(&req.adapter)
        .bind(&req.requested_node_id)
        .bind(&req.base_commit)
        .bind(&req.parent_acp_session_id)
        .bind(&now)
        .bind(timeout_secs)
        .bind(&req.validation_command)
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
            validation_command: req.validation_command.clone(),
            error_code: None,
            requested_node_id: req.requested_node_id.clone(),
            base_commit: req.base_commit.clone(),
            parent_acp_session_id: req.parent_acp_session_id.clone(),
        })
    }

    pub async fn list_tasks(&self) -> Result<Vec<TaskView>> {
        let rows = sqlx::query(
            "SELECT id, repository, prompt, adapter, status, created_at, finished_at, assigned_attempt_id, validation_command, error_code, requested_node_id, base_commit, parent_acp_session_id \
             FROM tasks ORDER BY created_at ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_task_view).collect())
    }

    pub async fn show_task(&self, id: &str) -> Result<Option<TaskView>> {
        let row = sqlx::query(
            "SELECT id, repository, prompt, adapter, status, created_at, finished_at, assigned_attempt_id, validation_command, error_code, requested_node_id, base_commit, parent_acp_session_id \
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

    pub async fn count_attempts(&self) -> Result<i64> {
        let row = sqlx::query("SELECT COUNT(*) AS c FROM attempts")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<i64, _>("c")?)
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

    // ----- conversations (stateful multi-turn chat) -----

    pub async fn create_conversation(
        &self,
        adapter: &str,
        repository: &str,
    ) -> Result<agentgrid_common::Conversation> {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        sqlx::query(
            "INSERT INTO conversations (id, adapter, repository, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(adapter)
        .bind(repository)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(agentgrid_common::Conversation {
            id,
            adapter: adapter.to_string(),
            repository: repository.to_string(),
            created_at: now,
        })
    }

    pub async fn get_conversation(
        &self,
        id: &str,
    ) -> Result<Option<agentgrid_common::Conversation>> {
        let row = sqlx::query(
            "SELECT id, adapter, repository, created_at FROM conversations WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| agentgrid_common::Conversation {
            id: r.try_get("id").unwrap_or_default(),
            adapter: r.try_get("adapter").unwrap_or_default(),
            repository: r.try_get("repository").unwrap_or_default(),
            created_at: r.try_get("created_at").unwrap_or_default(),
        }))
    }

    /// Append a message; returns its sequence number. `task_id` is the task that
    /// produced (assistant) or carried (user) the message.
    pub async fn append_conversation_message(
        &self,
        conversation_id: &str,
        role: &str,
        content: &str,
        task_id: Option<&str>,
    ) -> Result<i64> {
        let now = now_iso();
        let seq: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM conversation_messages WHERE conversation_id = ?",
        )
        .bind(conversation_id)
        .fetch_one(&self.pool)
        .await?;
        sqlx::query(
            "INSERT INTO conversation_messages (id, conversation_id, seq, role, content, task_id, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(conversation_id)
        .bind(seq)
        .bind(role)
        .bind(content)
        .bind(task_id)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(seq)
    }

    pub async fn list_conversation_messages(
        &self,
        conversation_id: &str,
    ) -> Result<Vec<agentgrid_common::ConversationMessage>> {
        let rows = sqlx::query(
            "SELECT seq, role, content, task_id, created_at FROM conversation_messages \
             WHERE conversation_id = ? ORDER BY seq ASC",
        )
        .bind(conversation_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| agentgrid_common::ConversationMessage {
                seq: r.try_get("seq").unwrap_or_default(),
                role: r.try_get("role").unwrap_or_default(),
                content: r.try_get("content").unwrap_or_default(),
                task_id: r.try_get("task_id").unwrap_or_default(),
                created_at: r.try_get("created_at").unwrap_or_default(),
            })
            .collect())
    }

    /// Stage 11.5: the most recent ACP session id produced by a finished task
    /// in this conversation, so the next task can resume it. `None` when there
    /// is no resumable session (first turn, or the prior attempt was not ACP).
    pub async fn last_conversation_acp_session(
        &self,
        conversation_id: &str,
    ) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT a.acp_session_id AS sid \
             FROM conversation_messages m \
             JOIN attempts a ON a.task_id = m.task_id \
             WHERE m.conversation_id = ? AND a.acp_session_id IS NOT NULL \
             ORDER BY m.seq DESC LIMIT 1",
        )
        .bind(conversation_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.and_then(|r| r.try_get::<Option<String>, _>("sid").ok().flatten()))
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
        let cands = sqlx::query(
            "SELECT id, prompt, adapter, repository, timeout_secs, validation_command, base_commit, parent_acp_session_id, created_at FROM tasks \
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
            "SELECT id, name, status, adapters, repositories, max_concurrency, active_attempts, last_heartbeat_at, agent_version, load_avg, free_disk_mb \
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
            if !node_ineligibility(&nv, &repository, &adapter).is_empty() {
                continue;
            }

            let attempt_id = Uuid::new_v4().to_string();
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
            "INSERT INTO attempts (id, task_id, number, node_id, status, lease_expires_at, ack_deadline, started_at) \
             VALUES (?, ?, ?, ?, 'assigned', ?, ?, ?)",
        )
        .bind(&attempt_id)
        .bind(&task_id)
        .bind(number as i64)
        .bind(node_id)
        .bind(&lease)
        .bind(&ack_deadline)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
            sqlx::query("UPDATE nodes SET active_attempts = active_attempts + 1 WHERE id = ?")
                .bind(node_id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;

            let upstream_commits = self.upstream_commits_for_task(&task_id).await?;
            return Ok(Some(Assignment {
                attempt_id,
                task_id,
                repository,
                prompt,
                adapter,
                number: number as u32,
                timeout_secs: timeout_secs as u64,
                git_url,
                default_branch,
                validation_command: task_validation.or(validation_command),
                base_commit,
                parent_acp_session_id,
                provenance: None,
                upstream_commits,
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
            sqlx::query("SELECT repository, adapter, requested_node_id FROM tasks WHERE id = ?")
                .bind(task_id)
                .fetch_optional(&self.pool)
                .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let repository: String = row.try_get("repository")?;
        let adapter: String = row.try_get("adapter")?;
        let requested: Option<String> = row.try_get("requested_node_id")?;

        let all = self.list_nodes().await?;
        let considered: Vec<NodeView> = match &requested {
            Some(id) => all.into_iter().filter(|n| &n.id == id).collect(),
            None => all,
        };

        let mut nodes = Vec::new();
        for n in &considered {
            let reasons = node_ineligibility(n, &repository, &adapter);
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
                let _ = tx.rollback().await;
                // Already terminal: a node reporting a completion for an attempt
                // we already finalized (e.g. marked `lost` after it died) gets an
                // idempotent ack without corrupting the task status.
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

        let attempt_target: AttemptStatus = as_enum
            .and_then(|s| next_attempt_status(s, at).ok())
            .unwrap_or(if success {
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
            .unwrap_or(if success {
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
        if let Some(sid) = &req.acp_session_id {
            sqlx::query("UPDATE attempts SET acp_session_id = ? WHERE id = ?")
                .bind(sid)
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
        sqlx::query("UPDATE tasks SET status = ?, finished_at = ?, error_code = ? WHERE id = ?")
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

    /// Stage 3.2: open an agent session for an attempt. Returns the new
    /// session id. The node opens at most one session per attempt (best-effort).
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

    /// Stage 3.2: close any open session for an attempt (idempotent: only
    /// updates sessions still running). Called when the attempt completes.
    pub async fn finish_agent_session(
        &self,
        tx: &mut sqlx::Transaction<'_, Sqlite>,
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
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Stage 3.2: fetch a single agent session by id (tests/reporting).
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

    /// Explicit assignment acknowledgement (Stage 1.3): atomically flips an
    /// `assigned` attempt (and its task) to `running` and clears the ack
    /// deadline. Idempotent for already-running/terminal attempts.
    pub async fn ack_attempt(&self, attempt_id: &str) -> Result<bool> {
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
        let as_enum = from_snake::<AttemptStatus>(&attempt_status);
        // Already running or terminal: idempotent no-op (a legacy metric event
        // may already have flipped it, or the attempt was lost).
        if let Some(s) = as_enum {
            if s != AttemptStatus::Assigned {
                let _ = tx.rollback().await;
                return Ok(true);
            }
        }
        let now = now_iso();
        sqlx::query(
            "UPDATE attempts SET status = 'running', ack_deadline = NULL, started_at = ? WHERE id = ?",
        )
        .bind(&now)
        .bind(attempt_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE tasks SET status = 'running', started_at = ? WHERE id = ?")
            .bind(&now)
            .bind(&task_id)
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

    /// Cancel a whole workflow run: the run and every non-terminal step move to
    /// `cancelled`, and any spawned task is cancelled (Stage 8 operation).
    /// Terminal runs (completed/failed/cancelled/blocked) are left untouched.
    pub async fn cancel_workflow_run(&self, run_id: &str) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
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
            "completed" | "failed" | "cancelled" | "blocked"
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

    /// Mark a node `degraded` (e.g. protocol incompatibility), unless revoked.
    pub async fn set_node_degraded(&self, node_id: &str) -> Result<()> {
        sqlx::query("UPDATE nodes SET status = 'degraded' WHERE id = ? AND status != 'revoked'")
            .bind(node_id)
            .execute(&self.pool)
            .await?;
        Ok(())
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
    /// Run the lease/offline maintenance tick once (used by the background
    /// task and exposed for tests/ops).
    pub async fn tick_maintenance(&self) -> Result<()> {
        let now = now_iso();
        revert_expired_leases(&self.pool, &now).await?;
        mark_offline_nodes(&self.pool, &now).await?;
        // Housekeeping: drop expired artifacts and truncate the WAL so the
        // database file does not grow without bound.
        let _ = self.cleanup_artifacts(168).await;
        let _ = self.wal_checkpoint().await;
        // Stage 13: fire any due scheduled-workflow triggers.
        let _ = self
            .tick_workflow_schedules(chrono::Utc::now().timestamp())
            .await;
        Ok(())
    }

    /// Test/debug: set an attempt's ack deadline (e.g. into the past to drive
    /// the unacked-assignment revert without waiting).
    pub async fn set_attempt_ack_deadline(&self, attempt_id: &str, iso: &str) -> Result<()> {
        sqlx::query("UPDATE attempts SET ack_deadline = ? WHERE id = ?")
            .bind(iso)
            .bind(attempt_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub fn start_maintenance(&self) {
        let store = self.clone();
        tokio::spawn(async move {
            // Tick every 15s: node-staleness is 30s, so a 15s cadence still
            // marks a dead node offline within ~45s of its last heartbeat.
            // Run the WAL checkpoint only every 4th tick (~60s): a checkpoint
            // takes the writer briefly (TRUNCATE) and serializes against user
            // BEGIN IMMEDIATE writes — running it every tick caused
            // `database is locked` (SQLITE_BUSY) on retry_task under load.
            let mut tick = 0u32;
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                let now = now_iso();
                if let Err(e) = revert_expired_leases(&store.pool, &now).await {
                    tracing::warn!("lease maintenance failed: {e}");
                }
                if let Err(e) = mark_offline_nodes(&store.pool, &now).await {
                    tracing::warn!("node maintenance failed: {e}");
                }
                let _ = store.cleanup_artifacts(168).await;
                tick = tick.wrapping_add(1);
                if tick % 4 == 0 {
                    let _ = store.wal_checkpoint().await;
                }
            }
        });
    }

    /// Stage 13 / line 487: background workflow ticker — re-advance every
    /// `Running` workflow run each interval so a CP restart (or a node
    /// completing a step task out-of-band) does not leave a run hung in
    /// `Running`. `tick_workflow_run` is idempotent (already-Running steps
    /// are skipped, terminal runs no-op), so a second tick after restart
    /// never duplicates steps or attempts. Best-effort: per-run failures are
    /// logged and swallowed so one bad run does not stall the ticker.
    pub fn start_workflow_ticker(&self) {
        let store = self.clone();
        let secs = std::env::var("AGENTGRID_WORKFLOW_TICK_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(5);
        tokio::spawn(async move {
            // Drop the first sleep so a fresh boot picks up in-flight runs
            // immediately (covers recovery after restart).
            loop {
                let ids = match store.running_workflow_run_ids().await {
                    Ok(ids) => ids,
                    Err(e) => {
                        tracing::warn!("workflow ticker listing runs failed: {e}");
                        Vec::new()
                    }
                };
                for id in &ids {
                    if let Err(e) = store.tick_workflow_run(id).await {
                        tracing::warn!("workflow tick for run {id} failed: {e}");
                    }
                }
                tokio::time::sleep(Duration::from_secs(secs)).await;
            }
        });
    }

    /// Startup reconcile (durable execution): on cp boot, immediately revert
    /// expired leases and mark silent nodes offline so the scheduler starts
    /// from a consistent state instead of waiting for the first background
    /// tick. Also audits the reconcile and logs in-flight attempt counts.
    /// In-flight `running` attempts on live nodes are left alone — the node
    /// may still complete them and report back; node-death is caught by the
    /// normal `node_lost` path. (Idea: hatchet-style durable startup-reconcile.)
    pub async fn reconcile_on_startup(&self) -> Result<()> {
        let inflight: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM attempts WHERE status IN ('assigned','running')",
        )
        .fetch_one(&self.pool)
        .await?;
        tracing::info!(
            in_flight = inflight,
            "startup reconcile: in-flight attempts"
        );
        self.tick_maintenance().await?;
        let _ = self
            .audit(
                "system",
                None,
                "startup_reconcile",
                None,
                Some(&format!("in_flight={inflight}")),
            )
            .await;
        tracing::info!("startup reconcile complete");
        Ok(())
    }

    /// Truncate the WAL into the main database (Stage 2.5 ops).
    pub async fn wal_checkpoint(&self) -> Result<()> {
        let start = std::time::Instant::now();
        let res = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&self.pool)
            .await;
        let dur = start.elapsed().as_millis() as u64;
        self.checkpoint_ms
            .store(dur, std::sync::atomic::Ordering::Relaxed);
        match res {
            Ok(_) => {
                tracing::debug!(dur_ms = dur, "wal checkpoint");
                Ok(())
            }
            Err(e) => {
                // Count SQLITE_BUSY-class failures distinctly so they surface in
                // metrics rather than only in logs.
                let msg = format!("{e}");
                if msg.to_lowercase().contains("busy") || msg.to_lowercase().contains("locked") {
                    self.sqlite_busy
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Err(e.into())
            }
        }
    }

    /// Compact copy of the database for backup/restore rehearsal (Stage 2.5 ops).
    /// The path is validated to avoid shell/SQL injection; `VACUUM INTO` refuses
    /// to overwrite an existing file.
    pub async fn backup_to(&self, path: &str) -> Result<()> {
        if path.contains('\\') || path.contains(';') || path.contains('\0') || path.contains("..") {
            return Err(anyhow::anyhow!("invalid backup path: {path}"));
        }
        let stmt = format!("VACUUM INTO '{}'", path.replace('\'', "''"));
        sqlx::query(&stmt).execute(&self.pool).await?;
        Ok(())
    }

    /// Delete artifact metadata older than `retention_hours` (default 168).
    /// Files on disk are left for an operator cleanup job (metadata only here).
    pub async fn cleanup_artifacts(&self, retention_hours: i64) -> Result<u64> {
        let cutoff = iso_plus_secs(-(retention_hours * 3600));
        let res = sqlx::query("DELETE FROM artifacts WHERE stored_at < ?")
            .bind(&cutoff)
            .execute(&self.pool)
            .await?;
        Ok(res.rows_affected())
    }
}

async fn revert_expired_leases(pool: &SqlitePool, now: &str) -> Result<()> {
    let rows = sqlx::query(
        "SELECT id, task_id, node_id FROM attempts WHERE status = 'assigned' AND ack_deadline < ?",
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

async fn mark_offline_nodes(pool: &SqlitePool, _now: &str) -> Result<()> {
    // last_heartbeat_at older than 30s and still 'online' -> offline, and any
    // in-flight attempt on that node is lost (Stage 1.2).
    let cutoff = (chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339();
    let rows = sqlx::query(
        "SELECT id FROM nodes WHERE status = 'online' AND (last_heartbeat_at IS NULL OR last_heartbeat_at < ?)",
    )
    .bind(&cutoff)
    .fetch_all(pool)
    .await?;
    for row in &rows {
        let id: String = row.try_get("id")?;
        sqlx::query("UPDATE nodes SET status = 'offline' WHERE id = ?")
            .bind(&id)
            .execute(pool)
            .await?;
        lose_node_attempts(pool, &id).await?;
    }
    Ok(())
}

/// Atomically mark a node's non-terminal attempts as `lost`, free its
/// concurrency capacity, and fail the owning tasks with `error_code =
/// node_lost`. Idempotent: a node with no in-flight attempts is a no-op.
async fn lose_node_attempts(pool: &SqlitePool, node_id: &str) -> Result<()> {
    let now = now_iso();
    let mut tx = pool.begin().await?;
    let rows = sqlx::query(
        "SELECT id, task_id FROM attempts WHERE node_id = ? AND status IN ('assigned', 'running', 'validating')",
    )
    .bind(node_id)
    .fetch_all(&mut *tx)
    .await?;
    if rows.is_empty() {
        let _ = tx.rollback().await;
        return Ok(());
    }
    let count = rows.len() as i64;
    for r in &rows {
        let aid: String = r.try_get("id")?;
        let tid: String = r.try_get("task_id")?;
        sqlx::query("UPDATE attempts SET status = 'lost', finished_at = ? WHERE id = ?")
            .bind(&now)
            .bind(&aid)
            .execute(&mut *tx)
            .await?;
        // Fail the task only if it has not already reached a terminal state.
        sqlx::query(
            "UPDATE tasks SET status = 'failed', error_code = 'node_lost', finished_at = ? \
             WHERE id = ? AND status NOT IN ('succeeded', 'failed', 'cancelled')",
        )
        .bind(&now)
        .bind(&tid)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE nodes SET active_attempts = MAX(0, active_attempts - ?) WHERE id = ?")
        .bind(count)
        .bind(node_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
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
        validation_command: r.try_get("validation_command").unwrap_or_default(),
        error_code: r.try_get("error_code").unwrap_or_default(),
        requested_node_id: r.try_get("requested_node_id").unwrap_or_default(),
        base_commit: r.try_get("base_commit").unwrap_or_default(),
        parent_acp_session_id: r.try_get("parent_acp_session_id").unwrap_or_default(),
    }
}

/// Stage 2.4 scheduler filter. Returns every reason `node` cannot run a task
/// for `(repository, adapter)`; empty => eligible. Shared by [`Store::try_assign`]
/// (per-node assignment) and [`Store::task_eligibility`] (visibility).
fn node_ineligibility(node: &NodeView, repository: &str, adapter: &str) -> Vec<String> {
    let mut reasons = Vec::new();
    if node.status != NodeStatus::Online {
        reasons.push(format!("node {} is {}", node.id, node.status));
    }
    if !node.adapters.iter().any(|a| a == adapter) {
        reasons.push(format!("missing adapter {adapter}"));
    }
    if !node
        .repositories
        .iter()
        .any(|r| r == "*" || r == repository)
    {
        reasons.push(format!("missing repository {repository}"));
    }
    if node.active_attempts >= node.max_concurrency {
        reasons.push(format!(
            "at capacity ({} >= {})",
            node.active_attempts, node.max_concurrency
        ));
    }
    reasons
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

// ---- Approvals (Stage 5 durable approval flow) ----

pub struct AuditEvent {
    pub id: String,
    pub actor_type: String,
    pub actor_id: Option<String>,
    pub action: String,
    pub subject: Option<String>,
    pub payload: Option<String>,
    pub created_at: String,
}

fn audit_from_row(r: &sqlx::sqlite::SqliteRow) -> AuditEvent {
    AuditEvent {
        id: r.try_get("id").unwrap_or_default(),
        actor_type: r.try_get("actor_type").unwrap_or_default(),
        actor_id: r.try_get("actor_id").ok(),
        action: r.try_get("action").unwrap_or_default(),
        subject: r.try_get("subject").ok(),
        payload: r.try_get("payload").ok(),
        created_at: r.try_get("created_at").unwrap_or_default(),
    }
}

fn approval_from_row(r: &sqlx::sqlite::SqliteRow) -> ApprovalView {
    ApprovalView {
        id: r.try_get("id").unwrap_or_default(),
        task_id: r.try_get("task_id").unwrap_or_default(),
        attempt_id: r.try_get("attempt_id").unwrap_or_default(),
        session_id: r.try_get("session_id").ok(),
        permission: r.try_get("permission").unwrap_or_default(),
        status: serde_json::from_value(serde_json::Value::String(
            r.try_get::<String, _>("status").unwrap_or_default(),
        ))
        .unwrap_or(ApprovalStatus::Pending),
        reason: r.try_get("reason").ok(),
        created_at: r.try_get("created_at").unwrap_or_default(),
        expires_at: r.try_get("expires_at").unwrap_or_default(),
        decided_at: r.try_get("decided_at").ok(),
        scope: r.try_get("scope").unwrap_or_else(|_| "session".to_string()),
    }
}

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
    /// is a no-op (idempotent), not an error.
    pub async fn answer_approval(
        &self,
        id: &str,
        event: ApprovalEvent,
        reason: Option<&str>,
        actor: &str,
    ) -> Result<()> {
        let current: Option<String> = sqlx::query("SELECT status FROM approvals WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| r.try_get("status"))
            .transpose()?;
        let Some(current) = current else {
            return Ok(()); // unknown id: no-op
        };
        let current_status: ApprovalStatus =
            serde_json::from_value(serde_json::Value::String(current))
                .unwrap_or(ApprovalStatus::Pending);
        let Ok(next) = next_approval(current_status, event) else {
            return Ok(()); // terminal already -> idempotent no-op
        };
        let decided = now_iso();
        let audit = serde_json::json!({ "actor": actor, "event": event, "at": decided });
        sqlx::query(
            "UPDATE approvals SET status = ?, decided_at = ?, reason = ?, audit = ? \
         WHERE id = ?",
        )
        .bind(serde_json::to_value(next).map(|v| v.as_str().unwrap_or("pending").to_string())?)
        .bind(&decided)
        .bind(reason)
        .bind(audit.to_string())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
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
    pub async fn list_approvals(
        &self,
        status: Option<ApprovalStatus>,
    ) -> Result<Vec<ApprovalView>> {
        let rows = match status {
            Some(s) => {
                let v =
                    serde_json::to_value(s).map(|v| v.as_str().unwrap_or("pending").to_string())?;
                sqlx::query(
                    "SELECT id, task_id, attempt_id, session_id, permission, status, reason, \
                        created_at, expires_at, decided_at, scope \
                 FROM approvals WHERE status = ? ORDER BY created_at ASC",
                )
                .bind(v)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT id, task_id, attempt_id, session_id, permission, status, reason, \
                        created_at, expires_at, decided_at, scope \
                 FROM approvals ORDER BY created_at ASC",
                )
                .fetch_all(&self.pool)
                .await?
            }
        };
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
            if self
                .answer_approval(&id, ApprovalEvent::Expire, Some("auto-expired"), "system")
                .await
                .is_ok()
            {
                count += 1;
                if let Some(step) = step_run_id {
                    let _ = self.block_step_and_run(&step).await;
                }
            }
        }
        Ok(count)
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
             AND status NOT IN ('completed','failed','cancelled','blocked')",
        )
        .bind(step_run_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

// ---- Skill trust (Stage 9.2) ----

impl Store {
    /// Set trust state for `(name, source)`. Idempotent upsert; records the
    /// operator that decided + when.
    pub async fn set_skill_trust(
        &self,
        name: &str,
        source: &str,
        trusted: bool,
        decided_by: &str,
    ) -> Result<()> {
        let now = now_iso();
        sqlx::query(
            "INSERT INTO skills_trust (name, source, trusted, decided_by, decided_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON CONFLICT (name, source) DO UPDATE SET \
                 trusted = excluded.trusted, \
                 decided_by = excluded.decided_by, \
                 decided_at = excluded.decided_at",
        )
        .bind(name)
        .bind(source)
        .bind(if trusted { 1 } else { 0 })
        .bind(decided_by)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Read the recorded trust state for a single skill, or `untrusted` when
    /// no decision exists yet.
    pub async fn get_skill_trust(&self, name: &str, source: &str) -> Result<SkillTrustView> {
        let row = sqlx::query(
            "SELECT name, source, trusted, decided_by, decided_at FROM skills_trust \
             WHERE name = ? AND source = ?",
        )
        .bind(name)
        .bind(source)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row
            .as_ref()
            .map(skill_trust_from_row)
            .unwrap_or_else(|| SkillTrustView {
                name: name.to_string(),
                source: source.to_string(),
                trusted: false,
                decided_by: None,
                decided_at: None,
            }))
    }

    /// All recorded trust decisions, newest decision first.
    pub async fn list_skill_trust(&self) -> Result<Vec<SkillTrustView>> {
        let rows = sqlx::query(
            "SELECT name, source, trusted, decided_by, decided_at FROM skills_trust \
             ORDER BY decided_at DESC, name ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(skill_trust_from_row).collect())
    }

    /// Stage 9.2: upsert skill rows a heartbeat discovered on disk. Insert
    /// `INSERT ... ON CONFLICT DO NOTHING` so a freshly discovered skill lands
    /// as untrusted, but an existing operator decision (trusted or untrusted)
    /// is never overwritten — auto-discovery is a hint, never policy.
    pub async fn upsert_discovered_skills(&self, skills: &[(String, String)]) -> Result<()> {
        if skills.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await?;
        for (name, source) in skills {
            sqlx::query(
                "INSERT INTO skills_trust (name, source, trusted, decided_by, decided_at) \
                 VALUES (?, ?, 0, 'discovery', NULL) \
                 ON CONFLICT (name, source) DO NOTHING",
            )
            .bind(name)
            .bind(source)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Stage 13: MCP server registry. Insert or replace a trusted server.
    pub async fn upsert_mcp_server(&self, body: &McpServerCreate) -> Result<McpServer> {
        let now = now_iso();
        sqlx::query(
            "INSERT INTO mcp_servers \
             (id, name, command, args, env_requirements, enabled, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(id) DO UPDATE SET \
                 name = excluded.name, command = excluded.command, \
                 args = excluded.args, env_requirements = excluded.env_requirements, \
                 enabled = excluded.enabled",
        )
        .bind(&body.id)
        .bind(&body.name)
        .bind(&body.command)
        .bind(serde_json::to_string(&body.args).unwrap_or_else(|_| "[]".into()))
        .bind(serde_json::to_string(&body.env_requirements).unwrap_or_else(|_| "[]".into()))
        .bind(if body.enabled { 1i64 } else { 0 })
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok(self.get_mcp_server(&body.id).await?.unwrap())
    }

    pub async fn get_mcp_server(&self, id: &str) -> Result<Option<McpServer>> {
        let row = sqlx::query(
            "SELECT id, name, command, args, env_requirements, enabled, created_at \
             FROM mcp_servers WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(mcp_server_from_row))
    }

    pub async fn list_mcp_servers(&self) -> Result<Vec<McpServer>> {
        let rows = sqlx::query(
            "SELECT id, name, command, args, env_requirements, enabled, created_at \
             FROM mcp_servers ORDER BY id ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(mcp_server_from_row).collect())
    }

    pub async fn delete_mcp_server(&self, id: &str) -> Result<bool> {
        let affected = sqlx::query("DELETE FROM mcp_servers WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected == 1)
    }
}

fn mcp_server_from_row(r: &sqlx::sqlite::SqliteRow) -> McpServer {
    let args: Vec<String> =
        serde_json::from_str(r.try_get::<String, _>("args").as_deref().unwrap_or("[]"))
            .unwrap_or_default();
    let env_requirements: Vec<String> = serde_json::from_str(
        r.try_get::<String, _>("env_requirements")
            .as_deref()
            .unwrap_or("[]"),
    )
    .unwrap_or_default();
    McpServer {
        id: r.try_get("id").unwrap_or_default(),
        name: r.try_get("name").unwrap_or_default(),
        command: r.try_get("command").unwrap_or_default(),
        args,
        env_requirements,
        enabled: r.try_get::<i64, _>("enabled").unwrap_or(1) != 0,
        created_at: r.try_get("created_at").unwrap_or_default(),
    }
}

/// Stage 13: decode the optional budget JSON column for a workflow template. A
/// NULL column is preserved as `None` (unbounded) — never synthesized.
fn workflow_budget_from_col(col: &str, r: &sqlx::sqlite::SqliteRow) -> Option<WorkflowBudget> {
    r.try_get::<Option<String>, _>(col)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
}

fn skill_trust_from_row(r: &sqlx::sqlite::SqliteRow) -> SkillTrustView {
    SkillTrustView {
        name: r.try_get("name").unwrap_or_default(),
        source: r.try_get("source").unwrap_or_default(),
        trusted: r.try_get::<i64, _>("trusted").unwrap_or(0) != 0,
        decided_by: r.try_get("decided_by").ok(),
        decided_at: r.try_get("decided_at").ok(),
    }
}

// ---- Agent profiles (Stage 13) ----

impl Store {
    /// Create a new immutable revision of a profile. `revision` = max(existing)+1
    /// (or 1 for the first). The new revision is **not** auto-activated; call
    /// `activate_profile` to flip the pointer.
    pub async fn create_profile_revision(
        &self,
        id: &str,
        body: &AgentProfileCreate,
        created_by: &str,
    ) -> Result<i64> {
        let now = now_iso();
        let mut tx = self.pool.begin().await?;
        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(revision), 0) + 1 FROM agent_profiles WHERE id = ?",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO agent_profiles \
             (id, revision, system_prompt, autonomy, memory_max, cpu_quota, tasks_max, created_at, created_by, secret_requirements, adapter_version) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(next)
        .bind(&body.system_prompt)
        .bind(&body.autonomy)
        .bind(body.memory_max)
        .bind(body.cpu_quota)
        .bind(body.tasks_max)
        .bind(&now)
        .bind(created_by)
        .bind(serde_json::to_string(&body.secret_requirements).unwrap_or_else(|_| "[]".into()))
        .bind(&body.adapter_version)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(next)
    }

    /// Flip the active-revision pointer (rollback = point at an older revision).
    /// Idempotent; the revision must exist.
    pub async fn activate_profile(&self, id: &str, revision: i64) -> Result<()> {
        sqlx::query(
            "INSERT INTO agent_profiles_active (id, active_revision) VALUES (?, ?) \
             ON CONFLICT(id) DO UPDATE SET active_revision = excluded.active_revision",
        )
        .bind(id)
        .bind(revision)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch the active revision of a profile, or None if no profile / none active.
    pub async fn get_active_profile(&self, id: &str) -> Result<Option<AgentProfile>> {
        let row = sqlx::query(
            "SELECT p.id, p.revision, p.system_prompt, p.autonomy, p.memory_max, \
                    p.cpu_quota, p.tasks_max, p.created_at, p.created_by, \
                    p.secret_requirements, p.adapter_version,\
                    (a.active_revision IS NOT NULL) AS active \
             FROM agent_profiles p \
             LEFT JOIN agent_profiles_active a ON a.id = p.id AND a.active_revision = p.revision \
             WHERE p.id = ? \
             ORDER BY p.revision DESC LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(profile_from_row))
    }

    /// List all revisions of a profile (newest first).
    pub async fn list_profile_revisions(&self, id: &str) -> Result<Vec<AgentProfile>> {
        let rows = sqlx::query(
            "SELECT p.id, p.revision, p.system_prompt, p.autonomy, p.memory_max, \
                    p.cpu_quota, p.tasks_max, p.created_at, p.created_by, \
                    p.secret_requirements, p.adapter_version,\
                    (a.active_revision = p.revision) AS active \
             FROM agent_profiles p \
             LEFT JOIN agent_profiles_active a ON a.id = p.id \
             WHERE p.id = ? \
             ORDER BY p.revision DESC",
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(profile_from_row).collect())
    }

    /// List all profile ids that have an active revision.
    pub async fn list_profiles(&self) -> Result<Vec<String>> {
        let rows: Vec<String> =
            sqlx::query_scalar("SELECT id FROM agent_profiles_active ORDER BY id ASC")
                .fetch_all(&self.pool)
                .await?;
        Ok(rows)
    }
}

fn profile_from_row(r: &sqlx::sqlite::SqliteRow) -> AgentProfile {
    let secret_requirements: Vec<agentgrid_common::SecretRequirement> = serde_json::from_str(
        r.try_get::<String, _>("secret_requirements")
            .as_deref()
            .unwrap_or("[]"),
    )
    .unwrap_or_default();
    AgentProfile {
        id: r.try_get("id").unwrap_or_default(),
        revision: r.try_get("revision").unwrap_or(0),
        system_prompt: r.try_get("system_prompt").unwrap_or_default(),
        autonomy: r.try_get("autonomy").unwrap_or_else(|_| "l2".to_string()),
        memory_max: r.try_get("memory_max").ok(),
        cpu_quota: r.try_get("cpu_quota").ok(),
        tasks_max: r.try_get("tasks_max").ok(),
        created_at: r.try_get("created_at").unwrap_or_default(),
        created_by: r.try_get("created_by").ok(),
        active: r.try_get::<bool, _>("active").unwrap_or(false),
        secret_requirements,
        adapter_version: r.try_get("adapter_version").ok(),
    }
}

// ----- workflows (Stage 7) -----

fn role_str(r: WorkflowRole) -> String {
    serde_json::to_value(r)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

/// Serialize a status enum to its `snake_case` string for storage.
fn role_str_status<T: serde::Serialize>(t: T) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

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
            "SELECT id, run_id, step_id, prompt, depends_on, role, adapter, requested_node_id, base_commit, retryable, max_attempts, attempts, status, created_at, expandable \
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
        sqlx::query("UPDATE workflow_steps SET status = ? WHERE id = ?")
            .bind(role_str_status(status))
            .bind(step_run_id)
            .execute(&self.pool)
            .await?;
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

    async fn step_task_id(&self, step_run_id: &str) -> Result<Option<String>> {
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
    async fn upstream_commits_for_task(&self, task_id: &str) -> Result<Vec<String>> {
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
    async fn emit_workflow_message(
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

#[cfg(test)]
mod workflow_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    async fn temp_store() -> Store {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("ag-wf-{nanos}-{n}.db"));
        let _ = std::fs::remove_file(&p);
        Store::open(p.to_str().unwrap()).await.unwrap()
    }

    fn step(id: &str, deps: &[&str], role: WorkflowRole) -> WorkflowStep {
        WorkflowStep {
            id: id.into(),
            prompt: format!("do {id}"),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            role,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: None,
            max_attempts: None,
            expandable: None,
        }
    }

    #[tokio::test]
    async fn rejects_invalid_dag_on_create() {
        let s = temp_store().await;
        let bad = vec![step("a", &["b"], WorkflowRole::Worker)];
        assert!(s.create_workflow_template("x", &bad, &None).await.is_err());
    }

    #[tokio::test]
    async fn create_template_and_run_roundtrips() {
        let s = temp_store().await;
        let steps = vec![
            step("a", &[], WorkflowRole::Architect),
            step("b", &["a"], WorkflowRole::Worker),
            step("c", &["a"], WorkflowRole::Verifier),
        ];
        let tpl = s
            .create_workflow_template("build", &steps, &None)
            .await
            .unwrap();
        assert!(tpl.id.starts_with("wft-"));
        assert_eq!(tpl.steps.len(), 3);

        let got = s.get_workflow_template(&tpl.id).await.unwrap().unwrap();
        assert_eq!(got.steps.len(), 3);

        let run = s
            .create_workflow_run(&tpl.id, Some(r#"{"branch":"feat"}"#), None, None)
            .await
            .unwrap();
        assert_eq!(run.status, WorkflowRunStatus::Pending);
        assert_eq!(run.context.as_deref(), Some(r#"{"branch":"feat"}"#));

        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(run_got.id, run.id);

        let steps_run = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(steps_run.len(), 3);
        // Each step instance got one role-run; verify roles carried through.
        let roles: Vec<_> = steps_run.iter().map(|x| x.role).collect();
        assert!(roles.contains(&WorkflowRole::Architect));
        assert!(roles.contains(&WorkflowRole::Worker));
        assert!(roles.contains(&WorkflowRole::Verifier));

        let all = s.list_workflow_runs().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(s.list_workflow_templates().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unknown_template_rejected_on_run() {
        let s = temp_store().await;
        assert!(s
            .create_workflow_run("wft-nope", None, None, None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn tick_activates_ready_step_and_is_idempotent() {
        let s = temp_store().await;
        // Single ready step (no deps) -> first tick spawns its task.
        let tpl = s
            .create_workflow_template("one", &[step("a", &[], WorkflowRole::Worker)], &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1);
        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(run_got.status, WorkflowRunStatus::Running);
        let steps = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(steps[0].status, WorkflowStepStatus::Running);
        assert!(steps[0].adapter.is_none() || steps[0].adapter.is_some());
        // Second tick must not spawn another task (step already running).
        let again = s.tick_workflow_run(&run.id).await.unwrap();
        assert!(again.is_empty());
    }

    #[tokio::test]
    async fn restart_does_not_duplicate_in_flight_workflow_step_tasks() {
        // line 487: a workflow run idempotently survives a "CP restart" — no
        // duplicate steps and no duplicate tasks. Steps: tick activates the only
        // ready step (a), printing its task id; a "restart" is modelled by
        // re-asking `running_workflow_run_ids` + ticking again before the task
        // finishes (must not re-spawn); then we complete a's task and confirm
        // the second tick advances to run Succeeded with exactly one step task id.
        let s = temp_store().await;
        let tpl = s
            .create_workflow_template("one-r", &[step("a", &[], WorkflowRole::Worker)], &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();

        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1, "tick spawns a single task");
        let first_task = s
            .step_task_id(&s.get_workflow_run_steps(&run.id).await.unwrap()[0].id)
            .await
            .unwrap();
        assert!(first_task.is_some(), "task bound to the step");

        // "CP restart": ticker re-lists in-flight runs and ticks; step is
        // already Running, so no duplicate task id is recorded.
        assert!(s
            .running_workflow_run_ids()
            .await
            .unwrap()
            .contains(&run.id));
        let again = s.tick_workflow_run(&run.id).await.unwrap();
        assert!(again.is_empty(), "restart tick does not re-spawn tasks");
        let still_first = s
            .step_task_id(&s.get_workflow_run_steps(&run.id).await.unwrap()[0].id)
            .await
            .unwrap();
        assert_eq!(still_first, first_task, "step still bound to the same task");

        // Node finishes the step task; tick advances the run to Succeeded with no new spawn.
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node = EnrollRequest {
            token,
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            agent_version: "test".into(),
            protocol_version: None,
        };
        let node_id = s.enroll_node(&node).await.unwrap().expect("enroll").node_id;
        let a = s.try_assign(&node_id).await.unwrap().expect("assign");
        s.complete_attempt(
            &a.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
            },
        )
        .await
        .unwrap();
        let post = s.tick_workflow_run(&run.id).await.unwrap();
        assert!(post.is_empty(), "completion tick spawns no new tasks");
        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            run_got.status,
            WorkflowRunStatus::Succeeded,
            "run succeeds when step done",
        );
    }

    #[tokio::test]
    async fn step_requested_node_id_pins_task() {
        let s = temp_store().await;
        let steps = vec![agentgrid_common::WorkflowStep {
            id: "a".into(),
            prompt: "do a".into(),
            depends_on: vec![],
            role: WorkflowRole::Worker,
            adapter: None,
            requested_node_id: Some("node-pinned".into()),
            base_commit: None,
            retryable: None,
            max_attempts: None,
            expandable: None,
        }];
        let tpl = s
            .create_workflow_template("pin", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1);
        let task = s.show_task(&created[0]).await.unwrap().unwrap();
        assert_eq!(task.requested_node_id.as_deref(), Some("node-pinned"));
        let steps_run = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(
            steps_run[0].requested_node_id.as_deref(),
            Some("node-pinned")
        );
    }

    #[tokio::test]
    async fn workflow_run_carries_base_commit() {
        let s = temp_store().await;
        let steps = vec![step("a", &[], WorkflowRole::Worker)];
        let tpl = s
            .create_workflow_template("t", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), Some("deadbeef"))
            .await
            .unwrap();
        assert_eq!(run.base_commit.as_deref(), Some("deadbeef"));
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1);
        let task = s.show_task(&created[0]).await.unwrap().unwrap();
        assert_eq!(task.base_commit.as_deref(), Some("deadbeef"));
        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(run_got.base_commit.as_deref(), Some("deadbeef"));
    }

    #[tokio::test]
    async fn retryable_step_retries_then_succeeds() {
        let s = temp_store().await;
        let steps = vec![agentgrid_common::WorkflowStep {
            id: "a".into(),
            prompt: "do a".into(),
            depends_on: vec![],
            role: WorkflowRole::Worker,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: Some(true),
            max_attempts: Some(3),
            expandable: None,
        }];
        let tpl = s
            .create_workflow_template("retry", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();

        // Tick -> first task.
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1);
        // Assign + fail it; retryable step should respawn.
        let a1 = s.try_assign("n1").await.unwrap().unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("agent_failed".into()),
                acp_session_id: None,
                provenance: None,
                plan: None,
            },
        )
        .await
        .unwrap();
        let created2 = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created2.len(), 1, "retryable step must respawn a task");
        let steps_run = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(steps_run[0].attempts, 1);
        // Assign + succeed the retry.
        let a2 = s.try_assign("n1").await.unwrap().unwrap();
        s.complete_attempt(
            &a2.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
            },
        )
        .await
        .unwrap();
        s.tick_workflow_run(&run.id).await.unwrap();
        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(run_got.status, WorkflowRunStatus::Succeeded);
    }

    #[tokio::test]
    async fn integrator_failure_blocks_run_not_failed() {
        let s = temp_store().await;
        let steps = vec![step("a", &[], WorkflowRole::Integrator)];
        let tpl = s
            .create_workflow_template("integ", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1);
        let a1 = s.try_assign("n1").await.unwrap().unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("merge_conflict".into()),
                acp_session_id: None,
                provenance: None,
                plan: None,
            },
        )
        .await
        .unwrap();
        s.tick_workflow_run(&run.id).await.unwrap();
        let steps_run = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(
            steps_run[0].status,
            WorkflowStepStatus::Blocked,
            "integrator failure must block, not fail"
        );
        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            run_got.status,
            WorkflowRunStatus::Blocked,
            "run must be blocked, not failed"
        );
    }

    #[tokio::test]
    async fn integrator_assignment_carries_upstream_worker_commits() {
        // line 239: an integrator step's assignment lists the winning commit
        // SHAs of its dependency steps under `upstream_commits` so the node can
        // land them as an integration branch. Modeled end-to-end in the store:
        // two parallel workers complete with commit SHAs, then tick activates
        // the integrator step; `try_assign` must surface both SHAs.
        let s = temp_store().await;
        let steps = vec![
            step("w1", &[], WorkflowRole::Worker),
            step("w2", &[], WorkflowRole::Worker),
            step("int", &["w1", "w2"], WorkflowRole::Integrator),
        ];
        let tpl = s
            .create_workflow_template("int", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 4,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();

        // activate w1 + w2.
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 2, "both parallel workers activate");
        let _ = created; // consume

        // Complete worker 1 with a commit sha.
        let a1 = s.try_assign("n1").await.unwrap().unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: Some("sha-worker-1".into()),
                error_code: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
            },
        )
        .await
        .unwrap();

        // Complete worker 2 with a commit sha.
        let a2 = s.try_assign("n1").await.unwrap().unwrap();
        s.complete_attempt(
            &a2.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: Some("sha-worker-2".into()),
                error_code: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
            },
        )
        .await
        .unwrap();

        // Workers done. Each tick advances its own steps; deps only become
        // visible to a pending integrator on the next tick (status_by_id is a
        // snapshot at the top of the loop), so tick twice: first tick transitions
        // workers `Running` -> `Succeeded`, second tick activates the integrator.
        s.tick_workflow_run(&run.id).await.unwrap();
        let act = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(act.len(), 1, "integrator activates after workers succeeded");

        // try_assign the integrator task and confirm upstream_commits is set.
        let int_a = s.try_assign("n1").await.unwrap().unwrap();
        let mut got = int_a.upstream_commits.clone();
        got.sort();
        assert_eq!(
            got,
            vec!["sha-worker-1".to_string(), "sha-worker-2".to_string()],
            "integrator carries upstream worker commit SHAs",
        );
    }

    #[tokio::test]
    async fn verifier_assignment_carries_upstream_worker_commit_for_isolation() {
        // line 240: an independent verifier step should start from the worker's
        // commit (so it can review the change) but never see the worker's
        // private transcripts. Modeling: verifier's `upstream_commits` carries
        // the worker's winning SHA (cherry-pick lands the worker tree on the
        // verifier's base) — the handoff block only references the SHA + summary,
        // never the transcript, so isolation holds by construction.
        let s = temp_store().await;
        let steps = vec![
            step("w1", &[], WorkflowRole::Worker),
            step("ver", &["w1"], WorkflowRole::Verifier),
        ];
        let tpl = s
            .create_workflow_template("v", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();

        // Activate + complete the worker with a commit.
        let _ = s.tick_workflow_run(&run.id).await.unwrap();
        let a = s.try_assign("n1").await.unwrap().unwrap();
        s.complete_attempt(
            &a.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: Some("sha-worker-1".into()),
                error_code: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
            },
        )
        .await
        .unwrap();
        // Two ticks to transition worker -> Succeeded and then activate verifier
        // (deps are resolved from a snapshot taken at the top of each tick).
        s.tick_workflow_run(&run.id).await.unwrap();
        let act = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(act.len(), 1, "verifier activates after worker succeeded");

        let v = s.try_assign("n1").await.unwrap().unwrap();
        assert_eq!(
            v.upstream_commits,
            vec!["sha-worker-1".to_string()],
            "verifier carries the worker's winning commit SHA (no transcript)",
        );
    }

    #[tokio::test]
    async fn retryable_step_exhausting_repair_budget_escalates_blocked() {
        // Stage 13 repair escalation: a `retryable` step that exhausts its
        // `max_attempts` escalates to a human (run `Blocked`) instead of
        // hard-failing the run. A non-retryable worker still fails fast.
        let s = temp_store().await;
        let steps_retry = vec![agentgrid_common::WorkflowStep {
            id: "a".into(),
            prompt: "do a".into(),
            depends_on: vec![],
            role: WorkflowRole::Worker,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: Some(true),
            max_attempts: Some(2),
            expandable: None,
        }];
        let tpl = s
            .create_workflow_template("rep", &steps_retry, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();

        // attempt 1 -> fail
        s.tick_workflow_run(&run.id).await.unwrap();
        let a1 = s.try_assign("n1").await.unwrap().unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("agent_failed".into()),
                acp_session_id: None,
                provenance: None,
                plan: None,
            },
        )
        .await
        .unwrap();
        s.tick_workflow_run(&run.id).await.unwrap();
        // attempt 2 -> fail (exhausts max_attempts=2)
        let a2 = s.try_assign("n1").await.unwrap().unwrap();
        s.complete_attempt(
            &a2.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("agent_failed".into()),
                acp_session_id: None,
                provenance: None,
                plan: None,
            },
        )
        .await
        .unwrap();
        s.tick_workflow_run(&run.id).await.unwrap();
        // Repair budget exhausted -> step Blocked (escalation), run Blocked.
        let rs = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(rs[0].status, WorkflowStepStatus::Blocked, "escalation");
        let after = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            after.status,
            WorkflowRunStatus::Blocked,
            "escalation parks the run"
        );

        // Sanity: a non-retryable worker fails the run outright on the first
        // attempt (fast fail).
        let steps_hard = vec![agentgrid_common::WorkflowStep {
            id: "h".into(),
            prompt: "do h".into(),
            depends_on: vec![],
            role: WorkflowRole::Worker,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: Some(false),
            max_attempts: Some(1),
            expandable: None,
        }];
        let tpl2 = s
            .create_workflow_template("hard", &steps_hard, &None)
            .await
            .unwrap();
        let run2 = s
            .create_workflow_run(&tpl2.id, None, Some("demo"), None)
            .await
            .unwrap();
        s.tick_workflow_run(&run2.id).await.unwrap();
        let b1 = s.try_assign("n1").await.unwrap().unwrap();
        s.complete_attempt(
            &b1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("agent_failed".into()),
                acp_session_id: None,
                provenance: None,
                plan: None,
            },
        )
        .await
        .unwrap();
        s.tick_workflow_run(&run2.id).await.unwrap();
        let rs2 = s.get_workflow_run_steps(&run2.id).await.unwrap();
        assert_eq!(rs2[0].status, WorkflowStepStatus::Failed, "fast fail");
        let after2 = s.get_workflow_run(&run2.id).await.unwrap().unwrap();
        assert_eq!(after2.status, WorkflowRunStatus::Failed);
    }

    #[tokio::test]
    async fn approval_timeout_blocks_linked_step() {
        let s = temp_store().await;
        let steps = vec![step("a", &[], WorkflowRole::Architect)];
        let tpl = s
            .create_workflow_template("ap", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();
        let _ = s.tick_workflow_run(&run.id).await.unwrap();
        let a1 = s.try_assign("n1").await.unwrap().unwrap();
        let steps_run = s.get_workflow_run_steps(&run.id).await.unwrap();
        let step_id = steps_run[0].id.clone();
        // Approval already expired, linked to the running step.
        let _ = s
            .create_approval(
                &a1.task_id,
                &a1.attempt_id,
                None,
                "run Bash",
                -10,
                Some(&step_id),
                "step",
            )
            .await
            .unwrap();
        let n = s.tick_approval_expiry().await.unwrap();
        assert_eq!(n, 1, "one approval should expire");
        let steps_run = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(
            steps_run[0].status,
            WorkflowStepStatus::Blocked,
            "timed-out approval must block the step, not hang"
        );
        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            run_got.status,
            WorkflowRunStatus::Blocked,
            "run must be blocked, not left hanging"
        );
    }

    #[tokio::test]
    async fn worker_failure_still_fails_run() {
        let s = temp_store().await;
        let steps = vec![step("a", &[], WorkflowRole::Worker)];
        let tpl = s
            .create_workflow_template("w", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1);
        let a1 = s.try_assign("n1").await.unwrap().unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("agent_failed".into()),
                acp_session_id: None,
                provenance: None,
                plan: None,
            },
        )
        .await
        .unwrap();
        s.tick_workflow_run(&run.id).await.unwrap();
        let steps_run = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(steps_run[0].status, WorkflowStepStatus::Failed);
        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(run_got.status, WorkflowRunStatus::Failed);
    }

    #[tokio::test]
    async fn workflow_run_projection_exposes_roles_nodes_verdicts() {
        let s = temp_store().await;
        let steps = vec![
            step("arch", &[], WorkflowRole::Architect),
            step("work", &["arch"], WorkflowRole::Worker),
        ];
        let tpl = s
            .create_workflow_template("p", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1);
        let a1 = s.try_assign("n1").await.unwrap().unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
            },
        )
        .await
        .unwrap();
        // Tick until the worker (dependent on arch) is spawned.
        for _ in 0..4 {
            s.tick_workflow_run(&run.id).await.unwrap();
        }

        let proj = s
            .get_workflow_run_projection(&run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(proj.steps.len(), 2);
        let arch = proj.steps.iter().find(|x| x.step_id == "arch").unwrap();
        assert_eq!(arch.role, WorkflowRole::Architect);
        assert_eq!(arch.verdict, "succeeded");
        assert_eq!(arch.node_id.as_deref(), Some("n1"));
        assert!(arch.task_id.is_some());
        let work = proj.steps.iter().find(|x| x.step_id == "work").unwrap();
        assert_eq!(work.role, WorkflowRole::Worker);
        assert!(work.task_id.is_some(), "worker task should be spawned");
        assert_eq!(work.node_id, None, "worker not assigned yet");
    }

    #[tokio::test]
    async fn workflow_projection_surfaces_budget_snapshot_when_template_has_budget() {
        // Stage 13 Loop Engineering: a projection of a run whose template
        // declares a budget carries a `BudgetSnapshot` with the observable
        // usage and a breach once a ceiling is exceeded. A template with no
        // budget yields no snapshot.
        let s = temp_store().await;
        let steps = vec![step("a", &[], WorkflowRole::Worker)];
        // No budget -> snapshot is None.
        let tpl_none = s
            .create_workflow_template("nobud", &steps, &None)
            .await
            .unwrap();
        let run_none = s
            .create_workflow_run(&tpl_none.id, None, Some("demo"), None)
            .await
            .unwrap();
        let proj_none = s
            .get_workflow_run_projection(&run_none.id)
            .await
            .unwrap()
            .unwrap();
        assert!(proj_none.budget.is_none(), "no budget => no snapshot");

        // With max_rounds = 0 the first tick starts the single root step
        // (rounds pre-checked at 0), and the second tick breaches.
        let budget = WorkflowBudget {
            max_rounds: Some(0),
            ..Default::default()
        };
        let tpl = s
            .create_workflow_template("looped", &steps, &Some(budget))
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        s.tick_workflow_run(&run.id).await.unwrap();
        // Snapshot mid-run before the breach fires: no breach yet.
        let mid = s
            .get_workflow_run_projection(&run.id)
            .await
            .unwrap()
            .unwrap();
        let snap = mid.budget.expect("budget template -> snapshot present");
        assert_eq!(snap.limits.max_rounds, Some(0));
        assert_eq!(snap.usage.rounds, 1, "one task started => rounds=1");
        // Rounds=1 > 0 => breach.
        assert!(snap.breach.is_some(), "rounds 1 > 0 must breach");
        assert_eq!(snap.breach.as_ref().unwrap().field, "max_rounds");
        // Tick again parks the run Blocked (enforcement path).
        s.tick_workflow_run(&run.id).await.unwrap();
        let after = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(after.status, WorkflowRunStatus::Blocked);
    }

    #[tokio::test]
    async fn backup_round_trips() {
        let s = temp_store().await;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let backup = std::env::temp_dir().join(format!("ag-backup-{stamp}.db"));
        if backup.exists() {
            let _ = std::fs::remove_file(&backup);
        }
        s.backup_to(backup.to_str().unwrap()).await.unwrap();
        assert!(backup.exists(), "VACUUM INTO must create the backup file");
        // Re-opening the backup must succeed and yield a usable store.
        let reopened = Store::open(backup.to_str().unwrap()).await.unwrap();
        assert_eq!(reopened.user_count().await.unwrap(), 0);
        let _ = std::fs::remove_file(&backup);
    }

    #[tokio::test]
    async fn cleanup_old_artifacts() {
        let s = temp_store().await;
        sqlx::query(
            "INSERT INTO artifacts (id, attempt_id, name, size_bytes, stored_at) VALUES (?,?,?,?,?)",
        )
        .bind("a-new")
        .bind("att-1")
        .bind("new.txt")
        .bind(3)
        .bind(now_iso())
        .execute(&s.pool)
        .await
        .unwrap();
        let old = iso_plus_secs(-(200 * 3600));
        sqlx::query(
            "INSERT INTO artifacts (id, attempt_id, name, size_bytes, stored_at) VALUES (?,?,?,?,?)",
        )
        .bind("a-old")
        .bind("att-1")
        .bind("old.txt")
        .bind(3)
        .bind(&old)
        .execute(&s.pool)
        .await
        .unwrap();
        let removed = s.cleanup_artifacts(168).await.unwrap();
        assert_eq!(removed, 1, "only the 200h-old artifact should be reaped");
        let remaining = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM artifacts")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(remaining, 1);
    }

    #[tokio::test]
    async fn scheduler_records_latency_metric() {
        let s = temp_store().await;
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node = EnrollRequest {
            token,
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            agent_version: "test".into(),
            protocol_version: None,
        };
        let resp = s.enroll_node(&node).await.unwrap().expect("node enroll");
        let node_id = resp.node_id;
        let task = CreateTaskRequest {
            prompt: "do".into(),
            repository: String::new(),
            adapter: "mock".into(),
            requested_node_id: None,
            timeout_secs: Some(60),
            validation_command: None,
            base_commit: None,
            parent_acp_session_id: None,
        };
        let _ = s.create_task(&task).await.unwrap();
        let before = s
            .scheduler_assignments
            .load(std::sync::atomic::Ordering::Relaxed);
        let assigned = s.try_assign(&node_id).await.unwrap();
        assert!(assigned.is_some(), "task should be assigned to the node");
        let after = s
            .scheduler_assignments
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            after,
            before + 1,
            "an assignment must increment the scheduler metric"
        );
    }

    #[tokio::test]
    async fn cancel_workflow_run_cancels_steps_and_tasks() {
        let s = temp_store().await;
        let steps = vec![WorkflowStep {
            id: "a".into(),
            prompt: "do".into(),
            depends_on: vec![],
            role: WorkflowRole::Worker,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: None,
            max_attempts: None,
            expandable: None,
        }];
        let t = s
            .create_workflow_template("t", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&t.id, None, None, None)
            .await
            .unwrap();
        // Link the step to a queued task, then cancel the whole run.
        let task_id = "task-x";
        sqlx::query(
            "INSERT INTO tasks (id, repository, prompt, adapter, status, created_at, timeout_secs) \
             VALUES (?, '', 'p', 'mock', 'queued', ?, 60)",
        )
        .bind(task_id)
        .bind(now_iso())
        .execute(&s.pool)
        .await
        .unwrap();
        let step_run_id: String =
            sqlx::query_scalar("SELECT id FROM workflow_steps WHERE run_id = ?")
                .bind(&run.id)
                .fetch_one(&s.pool)
                .await
                .unwrap();
        sqlx::query("INSERT INTO role_runs (id, step_run_id, task_id, role, created_at) VALUES (?, ?, ?, 'Worker', ?)")
            .bind(Uuid::new_v4().to_string())
            .bind(&step_run_id)
            .bind(task_id)
            .bind(now_iso())
            .execute(&s.pool)
            .await
            .unwrap();
        assert!(s.cancel_workflow_run(&run.id).await.unwrap());
        let run_status: String =
            sqlx::query_scalar("SELECT status FROM workflow_runs WHERE id = ?")
                .bind(&run.id)
                .fetch_one(&s.pool)
                .await
                .unwrap();
        assert_eq!(run_status, "cancelled");
        let step_status: String =
            sqlx::query_scalar("SELECT status FROM workflow_steps WHERE id = ?")
                .bind(&step_run_id)
                .fetch_one(&s.pool)
                .await
                .unwrap();
        assert_eq!(step_status, "cancelled");
        let task_status: String = sqlx::query_scalar("SELECT status FROM tasks WHERE id = ?")
            .bind(task_id)
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(task_status, "cancelled");
        // Already terminal: cancelling again is a no-op.
        assert!(!s.cancel_workflow_run(&run.id).await.unwrap());
    }

    #[tokio::test]
    async fn reconcile_on_startup_runs_maintenance_and_audits() {
        let s = temp_store().await;
        // No in-flight attempts: reconcile is a clean no-op that still audits.
        s.reconcile_on_startup().await.unwrap();
        let audits = s.list_audit(None, 100).await.unwrap();
        assert!(audits.iter().any(|a| a.action == "startup_reconcile"));
    }

    #[tokio::test]
    async fn acp_session_resume_links_conversation_turns() {
        // Stage 11.5: a finished turn's acp_session_id should be the parent of
        // the next turn's task assignment, so the agent resumes instead of
        // re-reading the transcript.
        let s = temp_store().await;
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node = EnrollRequest {
            token,
            name: "n".into(),
            adapters: vec!["mock".into()],
            repositories: vec![String::new()],
            max_concurrency: 2,
            agent_version: "test".into(),
            protocol_version: None,
        };
        let node_id = s.enroll_node(&node).await.unwrap().expect("enroll").node_id;

        let conv = s.create_conversation("mock", "").await.unwrap();

        // Turn 1: a task with no resume parent.
        let t1 = s
            .create_task(&CreateTaskRequest {
                prompt: "hello".into(),
                repository: String::new(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: Some(60),
                validation_command: None,
                base_commit: None,
                parent_acp_session_id: None,
            })
            .await
            .unwrap();
        s.append_conversation_message(&conv.id, "user", "hello", Some(&t1.id))
            .await
            .unwrap();
        let a1 = s.try_assign(&node_id).await.unwrap().expect("assign t1");
        assert_eq!(a1.parent_acp_session_id, None, "first turn has no parent");
        // Before completion, there is no resumable session.
        assert_eq!(
            s.last_conversation_acp_session(&conv.id).await.unwrap(),
            None
        );
        s.complete_attempt(
            &a1.attempt_id,
            &CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                acp_session_id: Some("sess-1".into()),
                provenance: None,
                plan: None,
            },
        )
        .await
        .unwrap();
        // After completion, the session is resumable.
        assert_eq!(
            s.last_conversation_acp_session(&conv.id).await.unwrap(),
            Some("sess-1".to_string())
        );

        // Turn 2: the API handler would set parent = the resumable session.
        let parent = s.last_conversation_acp_session(&conv.id).await.unwrap();
        let t2 = s
            .create_task(&CreateTaskRequest {
                prompt: "again".into(),
                repository: String::new(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: Some(60),
                validation_command: None,
                base_commit: None,
                parent_acp_session_id: parent,
            })
            .await
            .unwrap();
        assert_eq!(
            s.show_task(&t2.id)
                .await
                .unwrap()
                .unwrap()
                .parent_acp_session_id,
            Some("sess-1".to_string())
        );
        let a2 = s.try_assign(&node_id).await.unwrap().expect("assign t2");
        assert_eq!(
            a2.parent_acp_session_id.as_deref(),
            Some("sess-1"),
            "assignment carries the resume parent"
        );
    }

    #[tokio::test]
    async fn artifact_save_rejects_traversal_names() {
        let s = temp_store().await;
        for bad in ["../x", "..", ".", "/etc/passwd", "a/b", "a\\b", "", "x\0y"] {
            let r = s
                .save_artifact(
                    "att-trav",
                    &UploadArtifactRequest {
                        name: bad.into(),
                        content: "x".into(),
                        ..Default::default()
                    },
                )
                .await;
            assert!(r.is_err(), "traversal name {bad:?} should be rejected");
        }
        // A plain single-segment name is accepted.
        s.save_artifact(
            "att-trav",
            &UploadArtifactRequest {
                name: "ok.txt".into(),
                content: "ok".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn artifact_read_traversal_returns_none() {
        // Stage 2.2: a crafted read name must not escape the artifact root;
        // invalid names resolve to None (not found), not an error, so a 404 vs
        // 500 cannot leak whether an artifact exists.
        let s = temp_store().await;
        // Seed a task + attempt so latest_attempt_id resolves.
        let task_id = "task-art";
        sqlx::query(
            "INSERT INTO tasks (id, repository, prompt, adapter, status, created_at, timeout_secs) \
             VALUES (?, '', 'p', 'mock', 'queued', ?, 60)",
        )
        .bind(task_id)
        .bind(now_iso())
        .execute(&s.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO attempts (id, task_id, number, node_id, status, lease_expires_at, ack_deadline, started_at) \
             VALUES (?, ?, 1, 'n', 'succeeded', ?, ?, ?)",
        )
        .bind("att-art")
        .bind(task_id)
        .bind(now_iso())
        .bind(now_iso())
        .bind(now_iso())
        .execute(&s.pool)
        .await
        .unwrap();
        s.save_artifact(
            "att-art",
            &UploadArtifactRequest {
                name: "real.txt".into(),
                content: "data".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            s.read_artifact(task_id, "real.txt").await.unwrap(),
            Some("data".to_string()),
            "valid artifact reads back"
        );
        // No traversal name reaches the filesystem as an escape.
        for bad in ["../../../etc/passwd", "..", "/etc/passwd", "sub/dir/secret"] {
            assert_eq!(
                s.read_artifact(task_id, bad).await.unwrap(),
                None,
                "traversal read {bad:?} must be None"
            );
        }
    }

    #[tokio::test]
    async fn artifact_binary_round_trip_preserves_bytes_media_and_hash() {
        // Stage 2.2: non-UTF-8 artifacts (binary diffs, archives) must round trip
        // byte-for-byte through the binary-safe endpoint, with the stored media
        // type and caller-supplied hash read back unchanged.
        let s = temp_store().await;
        let task_id = "task-bart";
        sqlx::query(
            "INSERT INTO tasks (id, repository, prompt, adapter, status, created_at, timeout_secs) \
             VALUES (?, '', 'p', 'mock', 'queued', ?, 60)",
        )
        .bind(task_id)
        .bind(now_iso())
        .execute(&s.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO attempts (id, task_id, number, node_id, status, lease_expires_at, ack_deadline, started_at) \
             VALUES (?, ?, 1, 'n', 'succeeded', ?, ?, ?)",
        )
        .bind("att-bart")
        .bind(task_id)
        .bind(now_iso())
        .bind(now_iso())
        .bind(now_iso())
        .execute(&s.pool)
        .await
        .unwrap();
        // 0xFF 0xFE 0x00 invalid as UTF-8; would be mangled by read_to_string.
        let bytes: &[u8] = &[0xFFu8, 0xFEu8, 0x00u8, 0x01u8, 0x02u8];
        let sha = "7f83b1657ff1fc53b92dc18148a1d65dfc2d4b1e3c89e4f0a6f8e8d6f0e2c7b3";
        s.save_artifact_bytes("att-bart", "blob.bin", bytes, Some("image/png"), Some(sha))
            .await
            .unwrap();
        assert_eq!(
            s.read_artifact_bytes(task_id, "blob.bin").await.unwrap(),
            Some(bytes.to_vec()),
            "binary bytes must round trip unchanged"
        );
        let meta = s
            .read_artifact_meta(task_id, "blob.bin")
            .await
            .unwrap()
            .expect("meta present");
        assert_eq!(meta.size_bytes, bytes.len() as i64);
        assert_eq!(meta.media_type.as_deref(), Some("image/png"));
        assert_eq!(meta.sha256.as_deref(), Some(sha));
    }

    #[tokio::test]
    async fn budget_enforcement_parks_run_blocked_on_rounds_breach() {
        // Stage 13 Loop Engineering: a template with `max_rounds = 0` allows
        // zero step starts past the budget. The first tick starts both root
        // steps (both ready, no deps); the next tick's pre-check then finds
        // rounds >= 1 > 0 => breach => run `Blocked`, and a further tick stays
        // Blocked (terminal-until-approval).
        let s = temp_store().await;
        let steps = vec![
            step("a", &[], WorkflowRole::Worker),
            step("b", &[], WorkflowRole::Worker),
        ];
        let budget = WorkflowBudget {
            max_rounds: Some(0),
            ..Default::default()
        };
        let tpl = s
            .create_workflow_template("looped", &steps, &Some(budget))
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, None, None)
            .await
            .unwrap();
        // First tick: runsatе both root steps (no deps). Rounds is 0 at the
        // pre-check (nothing past Pending yet), so no breach this tick.
        s.tick_workflow_run(&run.id).await.unwrap();
        let s1 = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            s1.status,
            WorkflowRunStatus::Running,
            "first tick starts steps; budget not yet breached"
        );
        let started = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(
            started
                .iter()
                .filter(|s| s.status == WorkflowStepStatus::Running)
                .count(),
            2,
            "both root steps started on the first tick"
        );

        // Second tick pre-checks the budget: two steps past Pending =>
        // rounds=2 > 0 => breach => run Blocked.
        s.tick_workflow_run(&run.id).await.unwrap();
        let s2 = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            s2.status,
            WorkflowRunStatus::Blocked,
            "budget breach parks Blocked"
        );
        let after = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(
            after
                .iter()
                .filter(|s| s.status == WorkflowStepStatus::Running)
                .count(),
            2,
            "started steps remain Running; no further activity on the blocked run"
        );
        // A further tick stays Blocked (terminal-until-approval).
        s.tick_workflow_run(&run.id).await.unwrap();
        let s3 = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(s3.status, WorkflowRunStatus::Blocked);
    }

    #[tokio::test]
    async fn budget_bytes_enforced_from_message_payload_size() {
        // Stage 13: `max_bytes` counts orchestrator-emitted payload bytes, so a
        // handoff streak that pounds long payloads parks the run `Blocked`, and
        // read-back reports the bytes + breach.
        let s = temp_store().await;
        let steps = vec![
            step("a", &[], WorkflowRole::Worker),
            step("b", &[], WorkflowRole::Worker),
        ];
        let budget = WorkflowBudget {
            max_bytes: Some(5),
            ..Default::default()
        };
        let tpl = s
            .create_workflow_template("looped", &steps, &Some(budget))
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, None, None)
            .await
            .unwrap();
        // Each emit appends a payload -- 6 bytes over the 5-byte ceiling.
        s.emit_workflow_message(
            &run.id,
            "a",
            "b",
            agentgrid_common::AgentMessageKind::Output,
            "hello!",
        )
        .await
        .unwrap();
        assert_eq!(
            s.workflow_message_bytes(&run.id).await.unwrap(),
            6,
            "byte count reflects payload length"
        );
        // tick sees bytes > max_bytes -> breach -> Blocked.
        s.tick_workflow_run(&run.id).await.unwrap();
        let after = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            after.status,
            WorkflowRunStatus::Blocked,
            "byte budget breach parks Blocked"
        );
    }

    #[tokio::test]
    async fn circuit_breaker_trips_on_repeated_step_to_step_handoffs() {
        // Stage 13: a tight ping-pong of step->step handoffs with the same
        // (from, to) pair trips the repeated-handoffs circuit breaker. A
        // broadcast to `*` resets the streak (a step-succeeded broadcast to all
        // downstream steps is a healthy flow, not a solo ping-pong).
        let s = temp_store().await;
        let steps = vec![
            step("a", &[], WorkflowRole::Worker),
            step("b", &[], WorkflowRole::Worker),
        ];
        let budget = WorkflowBudget {
            max_repeated_handoffs: Some(2),
            ..Default::default()
        };
        let tpl = s
            .create_workflow_template("looped", &steps, &Some(budget))
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, None, None)
            .await
            .unwrap();
        // a->b, a->b (streak 2) then broadcast a->* (streak reset, still 2).
        for _ in 0..2 {
            s.emit_workflow_message(
                &run.id,
                "a",
                "b",
                agentgrid_common::AgentMessageKind::Output,
                "out",
            )
            .await
            .unwrap();
        }
        s.emit_workflow_message(
            &run.id,
            "a",
            "*",
            agentgrid_common::AgentMessageKind::Output,
            "broadcast",
        )
        .await
        .unwrap();
        assert_eq!(
            s.workflow_repeated_handoffs(&run.id).await.unwrap(),
            2,
            "streak is the longest consecutive same-pair run; broadcast resets"
        );
        // The check uses `>` (not `>=`), so streak=2 vs limit=2 is fine. Keep
        // going to streak 3 to trip the breaker (3 > 2).
        for _ in 0..3 {
            s.emit_workflow_message(
                &run.id,
                "a",
                "b",
                agentgrid_common::AgentMessageKind::Output,
                "out",
            )
            .await
            .unwrap();
        }
        assert_eq!(
            s.workflow_repeated_handoffs(&run.id).await.unwrap(),
            3,
            "streak grows past the breaker threshold"
        );
        s.tick_workflow_run(&run.id).await.unwrap();
        let after = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            after.status,
            WorkflowRunStatus::Blocked,
            "repeated-handoffs breaker trips -> Blocked"
        );
    }

    #[tokio::test]
    async fn parallel_ready_steps_of_same_repo_activate_in_one_tick() {
        // Stage 7.2: two independent (no deps) worker steps pointing at the
        // same repository must be activated in a single tick — both get tasks
        // queued (later run as independent worktrees under the per-repo lock).
        // The push does NOT serialize the steps: each gets its own task_id and
        // both are `Running`.
        let s = temp_store().await;
        let steps = vec![
            step("a", &[], WorkflowRole::Worker),
            step("b", &[], WorkflowRole::Worker),
        ];
        let tpl = s
            .create_workflow_template("par", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, Some("repo-x"), None, None)
            .await
            .unwrap();
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 2, "both root steps activate in one tick");
        let st = s.get_workflow_run_steps(&run.id).await.unwrap();
        let running: Vec<_> = st
            .iter()
            .filter(|x| x.status == WorkflowStepStatus::Running)
            .collect();
        assert_eq!(running.len(), 2, "both steps Running");
        // Each step has a distinct task_id (one worktree per step later).
        let mut tasks = std::collections::HashSet::new();
        for r in &running {
            let t = s.step_task_id(&r.id).await.unwrap().unwrap();
            assert!(tasks.insert(t), "distinct task per parallel step");
        }
        assert_eq!(tasks.len(), 2, "two distinct task ids");
    }

    #[tokio::test]
    async fn upsert_discovered_skills_defaults_untrusted_and_preserves_operator_decision() {
        // Stage 9.2: a heartbeat that reports a new skill lands it as
        // untrusted; a second heartbeat does not duplicate or flip trust; an
        // operator decision (trusted) survives subsequent discovery.
        let s = temp_store().await;
        // Fresh skill -> untrusted discovery row.
        s.upsert_discovered_skills(&[("git-helper".into(), "user".into())])
            .await
            .unwrap();
        let v = s.get_skill_trust("git-helper", "user").await.unwrap();
        assert!(!v.trusted, "freshly discovered defaults untrusted");
        // Idempotent: a second heartbeat with the same discovery changes nothing.
        s.upsert_discovered_skills(&[("git-helper".into(), "user".into())])
            .await
            .unwrap();
        let v = s.get_skill_trust("git-helper", "user").await.unwrap();
        assert!(!v.trusted);
        // Operator trusts it; a later discovery must NOT revert trust.
        s.set_skill_trust("git-helper", "user", true, "alice")
            .await
            .unwrap();
        s.upsert_discovered_skills(&[("git-helper".into(), "user".into())])
            .await
            .unwrap();
        let v = s.get_skill_trust("git-helper", "user").await.unwrap();
        assert!(v.trusted, "operator decision preserved across discovery");
        assert_eq!(v.decided_by.as_deref(), Some("alice"));
        // Empty discovery is a cheap no-op (does not error).
        s.upsert_discovered_skills(&[]).await.unwrap();
    }
}
