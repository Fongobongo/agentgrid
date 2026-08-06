//! SQLite-backed storage for the control plane (Stage 2.1).
//!
//! WAL mode, `synchronous=NORMAL`, `busy_timeout=5000`, 4-connection pool.
//! Assignment is atomic: a short `BEGIN IMMEDIATE`-style write transaction
//! selects a queued task, conditionally `UPDATE ... WHERE status='queued'`,
//! and checks `rows_affected` so concurrent schedulers can never double-assign.

use std::time::Duration;

use agentgrid_common::{
    AgentProfile, ApprovalStatus, ApprovalView, AttemptStatus, EventType, InvalidTransition,
    McpServer, NodeStatus, NodeView, PollRequest, SkillTrustView, TaskStatus, TaskView,
    WorkflowBudget, WorkflowSchedule,
};
use anyhow::Result;
use sqlx::pool::PoolOptions;
use sqlx::sqlite::{
    Sqlite, SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqliteSynchronous,
};
use sqlx::Row;

mod approvals;
mod artifacts;
mod attempts;
mod conversations;
mod events;
mod maintenance;
mod nodes;
mod profiles;
mod repositories;
mod scheduler;
mod skills;
mod tasks;
mod users;
mod workflows;

const ASSIGNMENT_LEASE_SECS: i64 = 30;
/// Window after assignment within which the node must ack (Stage 1.3). An
/// unacked assignment is reverted (returned to the queue) once this passes.
const ACK_DEADLINE_SECS: i64 = 30;
/// Hardening P0 item 9: server-side page size cap for `GET /v1/tasks/{id}/events`.
const DEFAULT_EVENT_PAGE: u64 = 1000;

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
    /// Unix timestamp of the last successful automatic backup (0 = never).
    pub(crate) last_backup_at: std::sync::Arc<std::sync::atomic::AtomicI64>,
    /// Cumulative count of failed automatic backups.
    pub(crate) backup_errors: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Hardening P2 item 35: cumulative count of expired-lease reverts
    /// (the lease/ACK race path that re-queues an unconfirmed assignment).
    pub(crate) lease_reverts: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Hardening P2 item 35: cumulative count of nodes whose `active_attempts`
    /// counter was found drifted from the live attempt rows on a reconcile.
    pub(crate) active_attempt_drift: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Hardening P2 item 35: cumulative bytes reclaimed by artifact retention.
    pub(crate) artifact_cleanup_bytes: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Number of artifact cleanup runs.
    pub(crate) artifact_cleanup_runs: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Number of artifact cleanup failures.
    pub(crate) artifact_cleanup_failures: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Total duration of artifact cleanup runs in seconds.
    pub(crate) artifact_cleanup_duration_secs: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Hardening P2 item 35: validation duration histogram (CP-computed from
    /// the `validating`-state window) and outcome distribution.
    pub(crate) validation_duration_sum: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub(crate) validation_duration_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub(crate) validation_outcomes:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, u64>>>,
    /// Hardening P2 item 35: security-profile distribution across attempts
    /// (from provenance).
    pub(crate) security_profile_attempts:
        std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, u64>>>,
    /// TTL cache of the statvfs result: free_bytes() is called on every
    /// try_assign (async hot path) and the syscall must not run each time.
    pub(crate) free_bytes_cache:
        std::sync::Arc<std::sync::Mutex<Option<(std::time::Instant, u64)>>>,
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// True when an sqlx error is a SQLite lock-contention failure (`database is
/// locked` / `database table is locked`), which is safe to retry with backoff.
fn is_locked_err(e: &anyhow::Error) -> bool {
    if let Some(sqlx::Error::Database(dberr)) = e.downcast_ref::<sqlx::Error>() {
        let m = dberr.message().to_ascii_lowercase();
        return m.contains("database is locked") || m.contains("database table is locked");
    }
    false
}

/// Begin a `BEGIN IMMEDIATE` write transaction. The default `pool.begin()` uses
/// a deferred BEGIN that takes the write lock only at the first UPDATE, so two
/// readers can both pass a `SELECT ... WHERE status=...` guard and then race
/// on the flip. `BEGIN IMMEDIATE` takes the RESERVED lock up front, serializing
/// the flip and making compare-and-set guards sound (hardening P0 item 7).
/// Retries once on `SQLITE_BUSY` with the configured busy_timeout.
async fn begin_immediate(pool: &sqlx::SqlitePool) -> Result<sqlx::Transaction<'_, sqlx::Sqlite>> {
    Ok(pool.begin_with("BEGIN IMMEDIATE").await?)
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

/// Hardening P0: an opaque ID (attempt/node/task/session id) is a short token
/// of `[A-Za-z0-9_-]` only. No path separators, no dots, no control chars, no
/// traversal — safe to join into a filesystem path or interpolate into a SQL
/// bound parameter. UUIDv4 / ULID / nanoid all fit; anything else is rejected.
pub fn is_safe_opaque_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Plan 535: artifact name safety (no traversal, no control chars) — shared by
/// the routes and the artifact service. A crafted name must never escape the
/// artifact root via a path join; reject early as 404/400 so a denial does not
/// disclose whether the task/artifact exists.
pub fn is_safe_artifact_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    if name.contains('/') || name.contains('\\') || name.contains('\0') {
        return false;
    }
    if name == "." || name == ".." || name.starts_with("../") || name.starts_with("..\\") {
        return false;
    }
    name.chars().all(|c| !c.is_control())
}

#[cfg(test)]
mod opaque_id_tests {
    use super::is_safe_opaque_id;
    #[test]
    fn accepts_uuid_and_safe_tokens() {
        assert!(is_safe_opaque_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(is_safe_opaque_id("01HXYZABCDEF0123456789"));
        assert!(is_safe_opaque_id("abc-123_def"));
    }
    #[test]
    fn rejects_traversal_and_separators() {
        assert!(!is_safe_opaque_id(".."));
        assert!(!is_safe_opaque_id("../etc"));
        assert!(!is_safe_opaque_id("a/b"));
        assert!(!is_safe_opaque_id("a\\b"));
        assert!(!is_safe_opaque_id("a.b"));
        assert!(!is_safe_opaque_id(""));
        assert!(!is_safe_opaque_id("has space"));
        assert!(!is_safe_opaque_id(&"x".repeat(65)));
    }
}

fn sha256_hex(s: &str) -> String {
    sha256_bytes_hex(s.as_bytes())
}

/// SHA-256 of raw bytes as lowercase hex (used for artifact integrity).
fn sha256_bytes_hex(b: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b);
    let out = h.finalize();
    let mut s = String::with_capacity(out.len() * 2);
    for byte in out {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

/// Error from [`Store::save_artifact_bytes`]. `HashMismatch` means the
/// caller-supplied sha256 hint disagrees with the server-computed SHA-256
/// of the uploaded bytes; the handler maps it to `422`.
#[derive(Debug)]
pub enum StoreArtifactError {
    HashMismatch { expected: String, computed: String },
    InvalidAttemptId,
    Other(anyhow::Error),
}

impl std::fmt::Display for StoreArtifactError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreArtifactError::HashMismatch { expected, computed } => {
                write!(
                    f,
                    "artifact sha256 mismatch: header={expected} computed={computed}"
                )
            }
            StoreArtifactError::InvalidAttemptId => write!(f, "invalid attempt_id"),
            StoreArtifactError::Other(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StoreArtifactError {}

impl From<anyhow::Error> for StoreArtifactError {
    fn from(e: anyhow::Error) -> Self {
        StoreArtifactError::Other(e)
    }
}

impl From<std::io::Error> for StoreArtifactError {
    fn from(e: std::io::Error) -> Self {
        StoreArtifactError::Other(e.into())
    }
}

impl From<sqlx::Error> for StoreArtifactError {
    fn from(e: sqlx::Error) -> Self {
        StoreArtifactError::Other(e.into())
    }
}

/// Error from a state-machine transition in the store. Returned when an
/// operation attempts an invalid status transition (e.g., completing an
/// already-terminal attempt, retrying a non-terminal task). Mapped to 409
/// Conflict by the HTTP layer.
#[derive(Debug, thiserror::Error)]
#[error("invalid transition: {0}")]
pub struct StoreTransitionError(pub InvalidTransition);

impl From<InvalidTransition> for StoreTransitionError {
    fn from(e: InvalidTransition) -> Self {
        StoreTransitionError(e)
    }
}

/// Argon2id hash of a password (Stage 4.1).
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
        // Hardening P1 item 15: create the artifact root eagerly so the
        // critical-disk watermark (statvfs on the root) is meaningful from the
        // first assignment, even before any artifact is written.
        let _ = std::fs::create_dir_all(&artifact_root);
        Ok(Self {
            pool,
            artifact_root,
            scheduler_latency_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            scheduler_assignments: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            checkpoint_ms: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            sqlite_busy: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_backup_at: std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
            backup_errors: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            lease_reverts: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            active_attempt_drift: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            artifact_cleanup_bytes: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            artifact_cleanup_runs: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            artifact_cleanup_failures: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            artifact_cleanup_duration_secs: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(
                0,
            )),
            validation_duration_sum: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            validation_duration_count: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            validation_outcomes: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            security_profile_attempts: std::sync::Arc::new(std::sync::Mutex::new(
                std::collections::HashMap::new(),
            )),
            free_bytes_cache: std::sync::Arc::new(std::sync::Mutex::new(None)),
        })
    }

    pub async fn health_check(&self) -> bool {
        sqlx::query("SELECT 1").execute(&self.pool).await.is_ok()
    }

    // ----- users + auth (Stage 4.1) -----

    pub async fn list_nodes(
        &self,
        after: Option<(String, String)>,
        limit: Option<u64>,
    ) -> Result<Vec<NodeView>> {
        const MAX_NODES: i64 = 1000;
        let limit = limit.unwrap_or(100).min(MAX_NODES as u64) as i64;
        let mut sql = String::from(
            "SELECT id, name, status, adapters, repositories, max_concurrency, active_attempts, last_heartbeat_at, agent_version, load_avg, free_disk_mb, unsafe_active, permission_interception, outbox_bytes, artifact_spool_bytes, outbox_rows, outbox_oldest_pending_age_ms, outbox_corruption_count, outbox_completion_rows, repo_lock_wait_ms, sandbox_backend, enforced_limits, drained, created_at \
             FROM nodes WHERE 1=1",
        );
        if after.is_some() {
            sql.push_str(" AND (created_at > ? OR (created_at = ? AND id > ?))");
        }
        sql.push_str(" ORDER BY created_at ASC, id ASC LIMIT ?");
        let mut q = sqlx::query(&sql);
        if let Some((created_at, id)) = &after {
            q = q.bind(created_at).bind(created_at).bind(id);
        }
        let rows = q.bind(limit).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(row_to_node_view).collect())
    }

    /// Register a newly seen node or refresh an existing one (acts as heartbeat).
    pub async fn register_or_touch_node(&self, req: &PollRequest) -> Result<()> {
        let now = now_iso();
        let adapters = serde_json::to_string(&req.adapters)?;
        let repositories = serde_json::to_string(&req.repositories)?;
        sqlx::query(
            "INSERT INTO nodes (id, name, status, max_concurrency, adapters, repositories, active_attempts, last_heartbeat_at, created_at, outbox_rows, outbox_oldest_pending_age_ms, outbox_corruption_count, outbox_completion_rows) \
             VALUES (?, ?, 'online', ?, ?, ?, 0, ?, ?, 0, 0, 0, 0) \
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
}

async fn revert_expired_leases(pool: &SqlitePool, now: &str) -> Result<usize> {
    // Race-safe (hardening P0 item 7): a single `BEGIN IMMEDIATE` transaction
    // selects AND cancels expired leases so a concurrent `ack_attempt` cannot
    // double-flip. The per-attempt cancel is CAS (`status = 'assigned'`), and we
    // only requeue the task / decrement `active_attempts` for rows we actually
    // moved — never for an attempt that was already ACKed or terminal.
    let mut tx = begin_immediate(pool).await?;
    let rows = sqlx::query(
        "SELECT id, task_id, node_id FROM attempts WHERE status = 'assigned' AND ack_deadline < ?",
    )
    .bind(now)
    .fetch_all(&mut *tx)
    .await?;
    let mut reverted = 0usize;
    for r in rows {
        let attempt_id: String = r.try_get("id")?;
        let task_id: String = r.try_get("task_id")?;
        let node_id: String = r.try_get("node_id")?;
        let moved = sqlx::query(
            "UPDATE attempts SET status = 'cancelled', finished_at = ? WHERE id = ? AND status = 'assigned'",
        )
        .bind(now)
        .bind(&attempt_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if moved != 1 {
            continue;
        }
        reverted += 1;
        sqlx::query(
            "UPDATE tasks SET status = 'queued', assigned_attempt_id = NULL \
             WHERE id = ? AND assigned_attempt_id = ? AND status = 'assigned'",
        )
        .bind(&task_id)
        .bind(&attempt_id)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE nodes SET active_attempts = MAX(0, active_attempts - 1) WHERE id = ?")
            .bind(&node_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(reverted)
}

async fn mark_offline_nodes(pool: &SqlitePool, now: &str) -> Result<()> {
    // Race-safe (hardening P0 item 7): CAS `online` -> `offline` under a single
    // write transaction and only run `lose_node_attempts` for nodes we actually
    // flipped. A concurrent heartbeat re-asserting `online` (via
    // upsert_heartbeat's own CAS) therefore never loses to this sweep.
    let cutoff = chrono::DateTime::parse_from_rfc3339(now)
        .map(|d| (d - chrono::Duration::seconds(30)).to_rfc3339())
        .unwrap_or_else(|_| (chrono::Utc::now() - chrono::Duration::seconds(30)).to_rfc3339());
    let mut tx = begin_immediate(pool).await?;
    let rows = sqlx::query(
        "SELECT id FROM nodes WHERE status = 'online' AND (last_heartbeat_at IS NULL OR last_heartbeat_at < ?)",
    )
    .bind(&cutoff)
    .fetch_all(&mut *tx)
    .await?;
    for row in &rows {
        let id: String = row.try_get("id")?;
        let moved =
            sqlx::query("UPDATE nodes SET status = 'offline' WHERE id = ? AND status = 'online'")
                .bind(&id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
        if moved != 1 {
            continue;
        }
        lose_node_attempts(&mut tx, &id).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// Atomically mark a node's non-terminal attempts as `lost`, free its
/// concurrency capacity, and fail the owning tasks with `error_code =
/// node_lost`. Idempotent: a node with no in-flight attempts is a no-op.
/// Runs inside the caller's `BEGIN IMMEDIATE` transaction (no cascade races).
async fn lose_node_attempts(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    node_id: &str,
) -> Result<()> {
    let now = now_iso();
    let rows = sqlx::query(
        "SELECT id, task_id FROM attempts WHERE node_id = ? AND status IN ('assigned', 'running', 'validating')",
    )
    .bind(node_id)
    .fetch_all(&mut **tx)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let count = rows.len() as i64;
    for r in &rows {
        let aid: String = r.try_get("id")?;
        let tid: String = r.try_get("task_id")?;
        sqlx::query("UPDATE attempts SET status = 'lost', finished_at = ? WHERE id = ?")
            .bind(&now)
            .bind(&aid)
            .execute(&mut **tx)
            .await?;
        // Fail the task only if it has not already reached a terminal state.
        // Hardening P1 item 13: clear assigned_attempt_id so a terminal task
        // has no active attempt.
        sqlx::query(
            "UPDATE tasks SET status = 'failed', error_code = 'node_lost', finished_at = ?, assigned_attempt_id = NULL \
             WHERE id = ? AND status NOT IN ('succeeded', 'failed', 'cancelled')",
        )
        .bind(&now)
        .bind(&tid)
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query("UPDATE nodes SET active_attempts = MAX(0, active_attempts - ?) WHERE id = ?")
        .bind(count)
        .bind(node_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn row_to_task_view(r: &sqlx::sqlite::SqliteRow) -> TaskView {
    // Hardening P2 item 36: extract the security profile from the latest
    // attempt's provenance JSON (ProvenanceRecord.security_profile), if any.
    let security_profile: Option<String> = r
        .try_get::<Option<String>, _>("attempt_provenance")
        .ok()
        .flatten()
        .and_then(|json| {
            serde_json::from_str::<serde_json::Value>(&json)
                .ok()
                .and_then(|v| {
                    v.get("security_profile")
                        .and_then(|s| s.as_str())
                        .map(String::from)
                })
        });
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
        network_mode: r.try_get("network_mode").unwrap_or_default(),
        security_profile,
    }
}

/// Stage 2.4 scheduler filter. Returns every reason `node` cannot run a task
/// for `(repository, adapter)`; empty => eligible. Shared by [`Store::try_assign`]
/// (per-node assignment) and [`Store::task_eligibility`] (visibility).
fn node_ineligibility(
    node: &NodeView,
    repository: &str,
    adapter: &str,
    security_profile: Option<&str>,
    task_network_mode: Option<&str>,
) -> Vec<String> {
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
    // Hardening P0 item 5: strict security profile (ending in -strict) requires
    // structured permission interception (not wrapper).
    if let Some(profile) = security_profile {
        if profile.ends_with("-strict") && node.permission_interception == "wrapper" {
            reasons.push("requires structured permission interception".to_string());
        }
    }
    // Hardening P2 item 659: task network mode must not exceed node network mode.
    // Order: none < restricted < unrestricted
    let task_mode = task_network_mode.unwrap_or("none");
    let node_mode = node.network_mode.as_str();
    let mode_rank = |m: &str| match m {
        "none" => 0,
        "restricted" => 1,
        "unrestricted" => 2,
        _ => 0,
    };
    if mode_rank(task_mode) > mode_rank(node_mode) {
        reasons.push(format!(
            "task network_mode '{task_mode}' exceeds node max '{node_mode}'"
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
        unsafe_active: r.try_get::<i64, _>("unsafe_active").unwrap_or(0) != 0,
        permission_interception: r.try_get("permission_interception").unwrap_or_default(),
        outbox_bytes: r.try_get::<i64, _>("outbox_bytes").unwrap_or(0) as u64,
        artifact_spool_bytes: r.try_get::<i64, _>("artifact_spool_bytes").unwrap_or(0) as u64,
        outbox_rows: r.try_get::<i64, _>("outbox_rows").unwrap_or(0) as u64,
        outbox_oldest_pending_age_ms: r
            .try_get::<i64, _>("outbox_oldest_pending_age_ms")
            .unwrap_or(0) as u64,
        outbox_corruption_count: r.try_get::<i64, _>("outbox_corruption_count").unwrap_or(0) as u64,
        outbox_completion_rows: r.try_get::<i64, _>("outbox_completion_rows").unwrap_or(0) as u64,
        repo_lock_wait_ms: r.try_get::<i64, _>("repo_lock_wait_ms").unwrap_or(0) as u64,
        sandbox_backend: r
            .try_get::<String, _>("sandbox_backend")
            .unwrap_or_else(|_| "none".to_string()),
        enforced_limits: r.try_get::<i64, _>("enforced_limits").unwrap_or(0) != 0,
        drained: r.try_get::<i64, _>("drained").unwrap_or(0) != 0,
        repo_cache_bytes: r.try_get::<i64, _>("repo_cache_bytes").unwrap_or(0) as u64,
        workspace_bytes: r.try_get::<i64, _>("workspace_bytes").unwrap_or(0) as u64,
        network_mode: r
            .try_get::<String, _>("network_mode")
            .unwrap_or_else(|_| "none".to_string()),
        created_at: r.try_get("created_at").unwrap_or_default(),
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
        mcp_server_ids: serde_json::from_str(
            r.try_get::<String, _>("mcp_server_ids")
                .as_deref()
                .unwrap_or("[]"),
        )
        .unwrap_or_default(),
    }
}

// ----- workflows (Stage 7) -----

/// Serialize a role/status enum to its `snake_case` string for storage.
fn role_str_status<T: serde::Serialize>(t: T) -> String {
    serde_json::to_value(t)
        .ok()
        .and_then(|v| v.as_str().map(|s| s.to_string()))
        .unwrap_or_default()
}

#[cfg(test)]
mod workflow_tests {
    use super::*;
    use agentgrid_common::{
        CompleteAttemptRequest, CreateTaskRequest, EnrollRequest, IncomingEvent,
        IngestEventsRequest, UploadArtifactRequest, WorkflowRole, WorkflowRunStatus, WorkflowStep,
        WorkflowStepStatus,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    async fn temp_store() -> Store {
        // Disable critical disk watermark for tests (temp fs often has < 512 MB free).
        std::env::set_var("AGENTGRID_DISK_CRITICAL_MB", "0");
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // std::env::temp_dir() returns /tmp which doesn't exist on this system.
        // Use /var/tmp which is the actual temp directory.
        let temp_dir = std::path::Path::new("/var/tmp");
        let p = temp_dir.join(format!("ag-wf-{nanos}-{n}.db"));
        let _ = std::fs::remove_file(&p);
        let path_str = p.to_str().unwrap();
        Store::open(path_str).await.unwrap()
    }

    /// Seed a real node + task + attempt so FK-backed tables (migration 0040)
    /// accept the rows. Returns (node_id, task_id).
    async fn seed_task_attempt(s: &Store, task_id: &str, att_id: &str) -> (String, String) {
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node_id = s
            .enroll_node(&EnrollRequest {
                token,
                name: "n".into(),
                adapters: vec!["mock".into()],
                repositories: vec!["*".into()],
                max_concurrency: 2,
                agent_version: "test".into(),
                protocol_version: None,
                permission_interception: "wrapper".into(),
            })
            .await
            .unwrap()
            .unwrap()
            .node_id;
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
             VALUES (?, ?, 1, ?, 'succeeded', ?, ?, ?)",
        )
        .bind(att_id)
        .bind(task_id)
        .bind(&node_id)
        .bind(now_iso())
        .bind(now_iso())
        .bind(now_iso())
        .execute(&s.pool)
        .await
        .unwrap();
        (node_id, task_id.to_string())
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

        let all = s.list_workflow_runs(None, None).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(
            s.list_workflow_templates(None, None).await.unwrap().len(),
            1
        );
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

    /// Plan 0.2 item 2.2: the background ticker and the complete_attempt
    // path can tick the same run concurrently. 20 simultaneous ticks must
    // still spawn exactly one task for the ready step (CAS, no duplicates).
    #[tokio::test]
    async fn concurrent_ticks_do_not_duplicate_step_tasks() {
        let s = temp_store().await;
        let tpl = s
            .create_workflow_template("conc", &[step("a", &[], WorkflowRole::Worker)], &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let mut handles = Vec::new();
        for _ in 0..20 {
            let st = s.clone();
            let id = run.id.clone();
            handles.push(tokio::spawn(async move { st.tick_workflow_run(&id).await }));
        }
        let mut spawned = 0usize;
        for h in handles {
            spawned += h.await.unwrap().unwrap().len();
        }
        assert_eq!(spawned, 1, "exactly one concurrent tick may spawn the step");
        let steps = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(steps.len(), 1);
        let task = s.step_task_id(&steps[0].id).await.unwrap();
        assert!(task.is_some(), "step bound to exactly one task");
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
            permission_interception: "wrapper".into(),
        };
        let node_id = s.enroll_node(&node).await.unwrap().expect("enroll").node_id;
        let a = s.try_assign(&node_id).await.unwrap().expect("assign");
        s.ack_attempt(&a.attempt_id).await.unwrap();
        s.complete_attempt(
            &a.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
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
        s.ack_attempt(&a1.attempt_id).await.unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("agent_failed".into()),
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
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
        s.ack_attempt(&a2.attempt_id).await.unwrap();
        s.complete_attempt(
            &a2.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
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
        s.ack_attempt(&a1.attempt_id).await.unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("merge_conflict".into()),
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
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
        s.ack_attempt(&a1.attempt_id).await.unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: Some("sha-worker-1".into()),
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
            },
        )
        .await
        .unwrap();

        // Complete worker 2 with a commit sha.
        let a2 = s.try_assign("n1").await.unwrap().unwrap();
        s.ack_attempt(&a2.attempt_id).await.unwrap();
        s.complete_attempt(
            &a2.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: Some("sha-worker-2".into()),
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
            },
        )
        .await
        .unwrap();

        // Workers done. status_by_id is updated as steps transition inside the
        // loop (plan 534 fix), so ONE tick both advances the workers to
        // Succeeded and activates the pending integrator whose deps are now met.
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
        // Stage 8 / line 257: parallel task_ids are also surfaced so the node
        // can fetch each worker's changes.patch artifact as a fallback when
        // the SHA is not reachable via a shared Git remote.
        assert_eq!(
            int_a.upstream_commits.len(),
            int_a.upstream_task_ids.len(),
            "upstream_task_ids parallel to upstream_commits",
        );
        assert!(
            !int_a.upstream_task_ids.is_empty(),
            "integrator carries upstream worker task ids",
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
        s.ack_attempt(&a.attempt_id).await.unwrap();
        s.complete_attempt(
            &a.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: Some("sha-worker-1".into()),
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
            },
        )
        .await
        .unwrap();
        // One tick: worker -> Succeeded and verifier activates in the same pass
        // (status_by_id updates in-loop, plan 534 fix).
        let act = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(act.len(), 1, "verifier activates after worker succeeded");

        let v = s.try_assign("n1").await.unwrap().unwrap();
        assert_eq!(
            v.upstream_commits,
            vec!["sha-worker-1".to_string()],
            "verifier carries the worker's winning commit SHA (no transcript)",
        );
        assert_eq!(
            v.upstream_task_ids.len(),
            1,
            "verifier carries the upstream worker task id for patch fallback",
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
        s.ack_attempt(&a1.attempt_id).await.unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("agent_failed".into()),
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
            },
        )
        .await
        .unwrap();
        s.tick_workflow_run(&run.id).await.unwrap();
        // attempt 2 -> fail (exhausts max_attempts=2)
        let a2 = s.try_assign("n1").await.unwrap().unwrap();
        s.ack_attempt(&a2.attempt_id).await.unwrap();
        s.complete_attempt(
            &a2.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("agent_failed".into()),
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
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
        s.ack_attempt(&b1.attempt_id).await.unwrap();
        s.complete_attempt(
            &b1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("agent_failed".into()),
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
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
        s.ack_attempt(&a1.attempt_id).await.unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("agent_failed".into()),
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
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
        s.ack_attempt(&a1.attempt_id).await.unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
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
        // Stage 11.6: timing lands on transitions for the span waterfall.
        assert!(arch.started_at.is_some(), "started_at set when step ran");
        assert!(arch.finished_at.is_some(), "finished_at set on terminal");
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
        // Hardened backup_to only accepts a plain file name and confines the
        // output to the data dir (parent of the artifact root).
        let name = format!("ag-backup-{stamp}.db");
        assert!(
            s.backup_to("/var/tmp/evil.db").await.is_err(),
            "absolute paths must be rejected"
        );
        assert!(
            s.backup_to("../evil.db").await.is_err(),
            "path separators must be rejected"
        );
        let backup = s.artifact_root().parent().unwrap().join(&name);
        if backup.exists() {
            let _ = std::fs::remove_file(&backup);
        }
        s.backup_to(&name).await.unwrap();
        assert!(backup.exists(), "VACUUM INTO must create the backup file");
        // Re-opening the backup must succeed and yield a usable store.
        let reopened = Store::open(backup.to_str().unwrap()).await.unwrap();
        assert_eq!(reopened.user_count().await.unwrap(), 0);
        let _ = std::fs::remove_file(&backup);
    }

    #[tokio::test]
    async fn cleanup_old_artifacts() {
        let s = temp_store().await;
        // FK-valid attempt (migration 0040) so the artifacts FK accepts rows.
        let (_node_id, _task_id) = seed_task_attempt(&s, "task-att1", "att-1").await;
        // Hardening P1 item 15: plant the backing files so we can assert the
        // reaped row's file is unlinked while the kept row's file survives.
        let old_path = s.artifact_path("att-1", "old.txt").unwrap();
        let new_path = s.artifact_path("att-1", "new.txt").unwrap();
        tokio::fs::create_dir_all(old_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&old_path, b"old").await.unwrap();
        tokio::fs::write(&new_path, b"new").await.unwrap();
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
        // File-level invariant: the reaped artifact's file is gone; the kept
        // artifact's file survives.
        assert!(
            !tokio::fs::try_exists(&old_path).await.unwrap(),
            "reaped artifact file must be deleted"
        );
        assert!(
            tokio::fs::try_exists(&new_path).await.unwrap(),
            "kept artifact file must survive"
        );
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
            permission_interception: "wrapper".into(),
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
            security_profile: None,
            network_mode: None,
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
            permission_interception: "wrapper".into(),
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
                security_profile: None,
                network_mode: None,
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
        s.ack_attempt(&a1.attempt_id).await.unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: Some("sess-1".into()),
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
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
                security_profile: None,
                network_mode: None,
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
    async fn conversation_append_allocates_unique_seq_under_concurrency() {
        // Hardening P2 item 21: concurrent appends to the same conversation
        // must each get a distinct, gap-free sequence. The per-message seq is
        // now allocated atomically by a single INSERT ... (SELECT MAX+1), with
        // a UNIQUE(conversation_id, seq) index backstopping the invariant.
        let s = temp_store().await;
        let conv = s.create_conversation("mock", "").await.unwrap();
        const N: usize = 32;
        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let s = s.clone();
            let id = conv.id.clone();
            handles.push(tokio::spawn(async move {
                s.append_conversation_message(&id, "user", &format!("m{i}"), None)
                    .await
                    .unwrap()
            }));
        }
        let mut seqs = Vec::with_capacity(N);
        for h in handles {
            seqs.push(h.await.unwrap());
        }
        // Each append returned a distinct seq in 1..=N.
        seqs.sort_unstable();
        let expected: Vec<i64> = (1..=N as i64).collect();
        assert_eq!(seqs, expected, "sequences must be unique and gap-free");
        // And the persisted rows agree with the returned seqs.
        let msgs = s
            .list_conversation_messages(&conv.id, 0, 1000)
            .await
            .unwrap();
        let persisted: Vec<i64> = msgs.iter().map(|m| m.seq).collect();
        assert_eq!(persisted, expected);
    }

    #[tokio::test]
    async fn conversation_messages_pagination_works() {
        // Hardening P2 item 20: cursor pagination for conversation messages.
        let s = temp_store().await;
        let conv = s.create_conversation("mock", "").await.unwrap();
        for i in 1..=10 {
            s.append_conversation_message(&conv.id, "user", &format!("msg{i}"), None)
                .await
                .unwrap();
        }
        // First page: after_seq=0, limit=3
        let page1 = s.list_conversation_messages(&conv.id, 0, 3).await.unwrap();
        assert_eq!(page1.len(), 3);
        assert_eq!(page1[0].seq, 1);
        assert_eq!(page1[2].seq, 3);
        // Second page: after_seq=3, limit=3
        let page2 = s.list_conversation_messages(&conv.id, 3, 3).await.unwrap();
        assert_eq!(page2.len(), 3);
        assert_eq!(page2[0].seq, 4);
        assert_eq!(page2[2].seq, 6);
        // Third page: after_seq=6, limit=3
        let page3 = s.list_conversation_messages(&conv.id, 6, 3).await.unwrap();
        assert_eq!(page3.len(), 3);
        assert_eq!(page3[0].seq, 7);
        assert_eq!(page3[2].seq, 9);
        // Fourth page: after_seq=9, limit=3 (only 1 remaining)
        let page4 = s.list_conversation_messages(&conv.id, 9, 3).await.unwrap();
        assert_eq!(page4.len(), 1);
        assert_eq!(page4[0].seq, 10);
        // After end: after_seq=10, limit=3
        let page5 = s.list_conversation_messages(&conv.id, 10, 3).await.unwrap();
        assert_eq!(page5.len(), 0);
        // Limit clamping: limit=0 -> 1, limit=2000 -> 1000
        let clamped = s.list_conversation_messages(&conv.id, 0, 0).await.unwrap();
        assert_eq!(clamped.len(), 1);
        let clamped2 = s
            .list_conversation_messages(&conv.id, 0, 2000)
            .await
            .unwrap();
        assert_eq!(clamped2.len(), 10);
    }

    #[tokio::test]
    async fn ingest_events_reports_contiguous_prefix_and_dedup() {
        // Hardening P1 item 14: the ACK returns the contiguous sequence prefix
        // (1..=N) and dedups repeated sequence ids via the unique index.
        let s = temp_store().await;
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node_id = s
            .enroll_node(&EnrollRequest {
                token,
                name: "n".into(),
                adapters: vec!["mock".into()],
                repositories: vec![String::new()],
                max_concurrency: 2,
                agent_version: "test".into(),
                protocol_version: None,
                permission_interception: "wrapper".into(),
            })
            .await
            .unwrap()
            .unwrap()
            .node_id;
        let _task = s
            .create_task(&CreateTaskRequest {
                prompt: "p".into(),
                repository: String::new(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: Some(60),
                validation_command: None,
                base_commit: None,
                parent_acp_session_id: None,
                security_profile: None,
                network_mode: None,
            })
            .await
            .unwrap();
        let a = s.try_assign(&node_id).await.unwrap().unwrap();

        let mk = |seq, text: &str| IncomingEvent {
            sequence: seq,
            r#type: EventType::Stdout,
            payload: serde_json::json!({"text": text}),
        };
        // Land 1,2 then 3,4 → contiguous prefix 4.
        let ack = s
            .ingest_events(
                &a.attempt_id,
                &IngestEventsRequest {
                    events: vec![mk(1, "a"), mk(2, "b")],
                },
            )
            .await
            .unwrap();
        assert_eq!(ack.accepted, 2);
        assert_eq!(ack.highest_contiguous_sequence, Some(2));

        let ack = s
            .ingest_events(
                &a.attempt_id,
                &IngestEventsRequest {
                    events: vec![mk(3, "c"), mk(4, "d")],
                },
            )
            .await
            .unwrap();
        assert_eq!(ack.accepted, 2);
        assert_eq!(ack.highest_contiguous_sequence, Some(4));

        // Re-send 4 (duplicate) → accepted 0, prefix still 4 (idempotent replay).
        let ack = s
            .ingest_events(
                &a.attempt_id,
                &IngestEventsRequest {
                    events: vec![mk(4, "d")],
                },
            )
            .await
            .unwrap();
        assert_eq!(ack.accepted, 0);
        assert_eq!(ack.highest_contiguous_sequence, Some(4));

        // Send 6 (gap at 5) → contiguous prefix stays at 4 until 5 arrives.
        let ack = s
            .ingest_events(
                &a.attempt_id,
                &IngestEventsRequest {
                    events: vec![mk(6, "f")],
                },
            )
            .await
            .unwrap();
        assert_eq!(ack.accepted, 1);
        assert_eq!(ack.highest_contiguous_sequence, Some(4));

        // Backfill 5 → prefix advances to 6 (the prior gap closes).
        let ack = s
            .ingest_events(
                &a.attempt_id,
                &IngestEventsRequest {
                    events: vec![mk(5, "e")],
                },
            )
            .await
            .unwrap();
        assert_eq!(ack.accepted, 1);
        assert_eq!(ack.highest_contiguous_sequence, Some(6));
    }

    #[tokio::test]
    async fn artifact_save_rejects_traversal_names() {
        let s = temp_store().await;
        // FK-valid attempt (migration 0040) so the rejected-name assertions
        // test the NAME guard, not the FK.
        let (_node_id, _task_id) = seed_task_attempt(&s, "task-trav", "att-trav").await;
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
        // Seed a task + attempt (FK-valid, migration 0040) so latest_attempt_id
        // resolves and the artifacts FK accepts the rows.
        let (_node_id, task_id) = seed_task_attempt(&s, "task-art", "att-art").await;
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
            s.read_artifact(&task_id, "real.txt").await.unwrap(),
            Some("data".to_string()),
            "valid artifact reads back"
        );
        // No traversal name reaches the filesystem as an escape.
        for bad in ["../../../etc/passwd", "..", "/etc/passwd", "sub/dir/secret"] {
            assert_eq!(
                s.read_artifact(&task_id, bad).await.unwrap(),
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
        let (_node_id, task_id) = seed_task_attempt(&s, "task-bart", "att-bart").await;
        // 0xFF 0xFE 0x00 invalid as UTF-8; would be mangled by read_to_string.
        let bytes: &[u8] = &[0xFFu8, 0xFEu8, 0x00u8, 0x01u8, 0x02u8];
        let sha = sha256_bytes_hex(bytes);
        s.save_artifact_bytes("att-bart", "blob.bin", bytes, Some("image/png"), Some(&sha))
            .await
            .unwrap();
        assert_eq!(
            s.read_artifact_bytes(&task_id, "blob.bin").await.unwrap(),
            Some(bytes.to_vec()),
            "binary bytes must round trip unchanged"
        );
        let meta = s
            .read_artifact_meta(&task_id, "blob.bin")
            .await
            .unwrap()
            .expect("meta present");
        assert_eq!(meta.size_bytes, bytes.len() as i64);
        assert_eq!(meta.media_type.as_deref(), Some("image/png"));
        // Only the server-computed hash is stored (hardening P0), so it equals
        // the computed hash, not any client value — and for a correct hint it's identical.
        assert_eq!(meta.sha256.as_deref(), Some(sha.as_str()));
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

    /// Hardening P0: a malformed attempt_id (traversal/separator) must never
    /// reach a filesystem path join. save_artifact_bytes rejects it with
    /// InvalidAttemptId before creating any directory.
    #[tokio::test]
    async fn save_artifact_rejects_traversal_attempt_id() {
        let s = temp_store().await;
        for bad in &["..", "../etc", "a/b", "a\\b", "a.b", "has space", ""] {
            let err = s
                .save_artifact_bytes(bad, "ok.txt", b"x", None, None)
                .await
                .expect_err("malformed attempt_id rejected");
            assert!(
                matches!(err, StoreArtifactError::InvalidAttemptId),
                "{bad:?} -> {err:?}"
            );
        }
        // The store rejected every malformed id at the boundary; no
        // traversal-target directory was created. (We assert the rejection
        // itself above; artifact_root may legitimately hold other test data,
        // so we do not assert emptiness.)
    }
    /// Hardening P0: a symlinked artifact directory must be rejected — a node
    /// (or a prior compromise) must not redirect artifact writes outside root.
    #[tokio::test]
    async fn save_artifact_rejects_symlink_dir() {
        let s = temp_store().await;
        // Plant a symlink where the attempt dir would live, pointing outside.
        let real_id = "550e8400-e29b-41d4-a716-446655440000";
        let attempt_dir = s.artifact_root.join(real_id);
        tokio::fs::create_dir_all(&s.artifact_root).await.unwrap();
        let outside = std::path::Path::new("/var/tmp").join("ag-symlink-outside");
        tokio::fs::create_dir_all(&outside).await.unwrap();
        // Clean any symlink/dir left by a prior run so the test is repeatable.
        let _ = tokio::fs::remove_file(&attempt_dir).await;
        let _ = tokio::fs::remove_dir_all(&attempt_dir).await;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &attempt_dir).unwrap();
        let err = s
            .save_artifact_bytes(real_id, "ok.txt", b"x", None, None)
            .await
            .expect_err("symlinked attempt dir rejected");
        assert!(matches!(err, StoreArtifactError::Other(_)), "{err:?}");
        // The escape never reached the outside target.
        assert!(tokio::fs::read(outside.join("ok.txt")).await.is_err());
    }

    /// Hardening P1 item 21: count_orphan_rows is 0 on a healthy DB and >0
    /// once a parent row is removed out-of-band (simulating corruption).
    #[tokio::test]
    async fn orphan_row_detection_works() {
        use sqlx::Connection;
        let s = temp_store().await;
        // Healthy: no orphans.
        assert_eq!(s.count_orphan_rows().await.unwrap(), 0);
        // Simulate pre-FK corruption on a DEDICATED connection with foreign
        // keys off: the app connection now enforces FKs (migration 0040), so
        // orphan rows can no longer be created through it — they can only
        // pre-exist from an old database. Plant the orphan exactly the way an
        // old DB would look: task + attempt + event, then remove the task.
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let p = std::path::Path::new("/var/tmp").join(format!("ag-wf-orphan-{n}.db"));
        let _ = std::fs::remove_file(&p);
        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&p)
            .create_if_missing(true)
            .foreign_keys(false);
        let mut conn = sqlx::SqliteConnection::connect_with(&opts.clone())
            .await
            .unwrap();
        // Fresh file has no schema — run the migrations on it first.
        sqlx::migrate!("./migrations").run(&mut conn).await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query("INSERT INTO tasks (id, repository, prompt, adapter, status, created_at) VALUES ('t-orphan','r','p','mock','queued','2024-01-01T00:00:00Z')")
            .execute(&mut conn).await.unwrap();
        sqlx::query("INSERT INTO attempts (id, task_id, node_id, number, status, started_at) VALUES ('a-orphan','t-orphan','n-x',1,'running','2024-01-01T00:00:00Z')")
            .execute(&mut conn).await.unwrap();
        sqlx::query("INSERT INTO task_events (id, attempt_id, sequence, type, payload, created_at) VALUES ('e-orphan','a-orphan',1,'log','{}','2024-01-01T00:00:00Z')")
            .execute(&mut conn).await.unwrap();
        drop(conn);
        // Re-open through the app Store (FK on) and check the detector.
        let s2 = Store::open(p.to_str().unwrap()).await.unwrap();
        assert_eq!(
            s2.count_orphan_rows().await.unwrap(),
            0,
            "no orphans while parents exist"
        );
        // Remove the parent task out-of-band (again with FKs off).
        let mut conn = sqlx::SqliteConnection::connect_with(&opts).await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query("DELETE FROM tasks WHERE id = 't-orphan'")
            .execute(&mut conn)
            .await
            .unwrap();
        drop(conn);
        let orphans = s2.count_orphan_rows().await.unwrap();
        assert!(orphans >= 1, "detected orphaned attempt: {orphans}");
    }

    #[tokio::test]
    async fn audit_records_rejected_terminal_completion() {
        // Hardening P1 item 13: a late/stale completion for an attempt we
        // already finalized is rejected but audited (with the source state),
        // so a stale node redelivery is traceable.
        let s = temp_store().await;
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node_id = s
            .enroll_node(&EnrollRequest {
                token,
                name: "n".into(),
                adapters: vec!["mock".into()],
                repositories: vec![String::new()],
                max_concurrency: 2,
                agent_version: "test".into(),
                protocol_version: None,
                permission_interception: "wrapper".into(),
            })
            .await
            .unwrap()
            .unwrap()
            .node_id;
        let _task = s
            .create_task(&CreateTaskRequest {
                prompt: "p".into(),
                repository: String::new(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: Some(60),
                validation_command: None,
                base_commit: None,
                parent_acp_session_id: None,
                security_profile: None,
                network_mode: None,
            })
            .await
            .unwrap();
        let a = s.try_assign(&node_id).await.unwrap().unwrap();
        s.ack_attempt(&a.attempt_id).await.unwrap();
        // First completion succeeds (running -> succeeded).
        assert!(s
            .complete_attempt(&a.attempt_id, &CompleteAttemptRequest::default())
            .await
            .unwrap());
        // Second (late) completion is an idempotent Ok(true) but audited.
        assert!(s
            .complete_attempt(&a.attempt_id, &CompleteAttemptRequest::default())
            .await
            .unwrap());
        let audits = s.list_audit(None, 100).await.unwrap();
        let rejs: Vec<_> = audits
            .iter()
            .filter(|e| e.action == "complete.rejected_terminal")
            .collect();
        assert_eq!(rejs.len(), 1, "exactly one rejected-terminal audit");
        assert_eq!(rejs[0].actor_type, "attempt");
        assert_eq!(rejs[0].actor_id.as_deref(), Some(a.attempt_id.as_str()));
        assert_eq!(rejs[0].subject.as_deref(), Some("succeeded"));
    }

    #[tokio::test]
    async fn audit_records_rejected_nonterminal_retry() {
        // Hardening P1 item 13: a retry against a non-terminal task (queued)
        // is rejected and audited with the source state.
        let s = temp_store().await;
        let task = s
            .create_task(&CreateTaskRequest {
                prompt: "p".into(),
                repository: String::new(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: Some(60),
                validation_command: None,
                base_commit: None,
                parent_acp_session_id: None,
                security_profile: None,
                network_mode: None,
            })
            .await
            .unwrap();
        // The task is queued (never failed); retry must be rejected.
        assert!(!s.retry_task(&task.id).await.unwrap());
        let audits = s.list_audit(None, 100).await.unwrap();
        let rejs: Vec<_> = audits
            .iter()
            .filter(|e| e.action == "retry.rejected_nonterminal")
            .collect();
        assert_eq!(rejs.len(), 1, "exactly one rejected-retry audit");
        assert_eq!(rejs[0].actor_type, "task");
        assert_eq!(rejs[0].actor_id.as_deref(), Some(task.id.as_str()));
        assert_eq!(rejs[0].subject.as_deref(), Some("queued"));
    }
}
