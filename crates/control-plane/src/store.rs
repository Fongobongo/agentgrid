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

mod agents;
mod approvals;
mod artifacts;
mod attempts;
mod conversations;
mod events;
mod learnings;
mod maintenance;
mod nodes;
pub(crate) mod opencode_profiles;
mod profiles;
mod repositories;
mod scheduler;
mod shared_context;
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
    /// Write transactions begun by THIS store (per-instance counterpart of
    /// the process-wide `write_txn_stats` counter; tests that assert an exact
    /// transaction count need per-store isolation because the lib suites run
    /// tests in parallel).
    pub(crate) write_txn_count: std::sync::Arc<std::sync::atomic::AtomicU64>,
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

/// Audit X-D3: shared keyset-pagination scaffolding. Six list queries used to
/// carry copy-pasted copies of the cursor predicate / order clause / page
/// cap, which had already drifted (only tasks.rs carried the binding-order
/// comment). A cursor-semantics fix now lands once.
pub(super) const KEYSET_PREDICATE: &str = " AND (created_at > ? OR (created_at = ? AND id > ?))";
pub(super) const KEYSET_ORDER: &str = " ORDER BY created_at ASC, id ASC LIMIT ?";

/// Server-side page cap + default for every keyset list (hardening P2 item 20).
pub(super) fn page_limit(limit: Option<u64>) -> i64 {
    limit.unwrap_or(100).min(1000) as i64
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

/// Plan 0.3 stage 0: write-path counters for load observability. `busy` is
/// incremented when a `BEGIN IMMEDIATE` still cannot take the write lock
/// after the 5s `busy_timeout` — a direct signal of writer contention.
static WRITE_TXNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static WRITE_BUSY_FAILURES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Plan 0.3 item 1.1: single-writer gate. SQLite allows one writer at a
/// time; letting many tasks race `BEGIN IMMEDIATE` burns time on lock
/// waits (`busy_timeout` backoff) under load. Holding one permit across
/// the whole transaction serializes writers in FIFO order, so the lock is
/// never contended and `busy_timeout` is only a safety net.
static WRITE_GATE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// (write transactions begun, write-lock failures) since process start.
pub fn write_txn_stats() -> (u64, u64) {
    (
        WRITE_TXNS.load(std::sync::atomic::Ordering::Relaxed),
        WRITE_BUSY_FAILURES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// A write transaction holding the single-writer gate permit (plan 0.3 1.1).
/// Derefs to `SqliteConnection` (same as `sqlx::Transaction` does), so query
/// call sites work unchanged; the gate permit is released when the
/// transaction commits, rolls back, or is dropped. Never acquire two of
/// these in the same task (the gate is non-reentrant and would deadlock).
pub struct WriteTxn<'a> {
    tx: sqlx::Transaction<'a, sqlx::Sqlite>,
    _permit: tokio::sync::SemaphorePermit<'static>,
}

impl std::ops::Deref for WriteTxn<'_> {
    type Target = sqlx::SqliteConnection;
    fn deref(&self) -> &Self::Target {
        &self.tx
    }
}

impl std::ops::DerefMut for WriteTxn<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tx
    }
}

impl<'a> WriteTxn<'a> {
    pub async fn commit(self) -> Result<()> {
        Ok(self.tx.commit().await?)
    }

    pub async fn rollback(self) -> Result<()> {
        Ok(self.tx.rollback().await?)
    }
}

/// Begin a `BEGIN IMMEDIATE` write transaction. The default `pool.begin()` uses
/// a deferred BEGIN that takes the write lock only at the first UPDATE, so two
/// readers can both pass a `SELECT ... WHERE status=...` guard and then race
/// on the flip. `BEGIN IMMEDIATE` takes the RESERVED lock up front, serializing
/// the flip and making compare-and-set guards sound (hardening P0 item 7).
/// The returned [`WriteTxn`] holds the process-wide write gate (plan 0.3 1.1),
/// so writers queue in FIFO instead of contending on the SQLite lock.
async fn begin_immediate(pool: &sqlx::SqlitePool) -> Result<WriteTxn<'static>> {
    let permit = WRITE_GATE
        .acquire()
        .await
        .expect("write gate semaphore closed");
    match pool.begin_with("BEGIN IMMEDIATE").await {
        Ok(tx) => {
            WRITE_TXNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(WriteTxn {
                tx,
                _permit: permit,
            })
        }
        Err(e) => {
            let msg = format!("{e}").to_ascii_lowercase();
            if msg.contains("busy") || msg.contains("locked") {
                WRITE_BUSY_FAILURES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e.into())
        }
    }
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
#[path = "store/tests.rs"]
mod store_tests;
fn sha256_hex(s: &str) -> String {
    agentgrid_common::sha256_hex(s.as_bytes())
}

/// SHA-256 of raw bytes as lowercase hex (used for artifact integrity).
fn sha256_bytes_hex(b: &[u8]) -> String {
    agentgrid_common::sha256_hex(b)
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
    /// Begin a write transaction on this store, counting it in the
    /// per-instance [`Self::write_txn_count`] (see the field doc: exact-count
    /// test assertions need per-store isolation under parallel test runs).
    pub(crate) async fn write_txn(&self) -> Result<WriteTxn<'static>> {
        let tx = begin_immediate(&self.pool).await?;
        self.write_txn_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(tx)
    }

    /// Write transactions begun by this store instance (tests/ops).
    pub fn write_txn_count(&self) -> u64 {
        self.write_txn_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

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
            write_txn_count: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
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
        // audited: clauses are compile-time constants; values are bound
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.as_str()));
        if let Some((created_at, id)) = &after {
            q = q.bind(created_at).bind(created_at).bind(id);
        }
        let rows = q.bind(limit).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(row_to_node_view).collect())
    }

    /// Single node by id (`GET /v1/nodes/{id}`, read by `ag node doctor`).
    /// `None` when the id is unknown.
    pub async fn get_node(&self, node_id: &str) -> Result<Option<NodeView>> {
        let row = sqlx::query(
            "SELECT id, name, status, adapters, repositories, max_concurrency, active_attempts, last_heartbeat_at, agent_version, load_avg, free_disk_mb, unsafe_active, permission_interception, outbox_bytes, artifact_spool_bytes, outbox_rows, outbox_oldest_pending_age_ms, outbox_corruption_count, outbox_completion_rows, repo_lock_wait_ms, sandbox_backend, enforced_limits, drained, created_at \
             FROM nodes WHERE id = ?",
        )
        .bind(node_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_node_view))
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
        use uuid::Uuid;
        let attempt_id: String = r.try_get("id")?;
        let task_id: String = r.try_get("task_id")?;
        let node_id: String = r.try_get("node_id")?;
        // Fencing: rotate the token together with the cancel so any later
        // mutation from the stale holder (artifact/event uploads still
        // presenting the old token) is rejected with 409 instead of being
        // attributed to the reverted attempt.
        let stale_fence = Uuid::new_v4().to_string();
        let moved = sqlx::query(
            "UPDATE attempts SET status = 'cancelled', finished_at = ?, fencing_token = ? \
             WHERE id = ? AND status = 'assigned'",
        )
        .bind(now)
        .bind(&stale_fence)
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
async fn lose_node_attempts(tx: &mut sqlx::SqliteConnection, node_id: &str) -> Result<()> {
    let now = now_iso();
    let rows = sqlx::query(
        "SELECT id, task_id FROM attempts WHERE node_id = ? AND status IN ('assigned', 'running', 'validating')",
    )
    .bind(node_id)
    .fetch_all(&mut *tx)
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
            .execute(&mut *tx)
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
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE nodes SET active_attempts = MAX(0, active_attempts - ?) WHERE id = ?")
        .bind(count)
        .bind(node_id)
        .execute(&mut *tx)
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
        group_id: r.try_get("group_id").unwrap_or_default(),
        agent_id: r.try_get("agent_id").unwrap_or_default(),
        // Plan 2.9 (#20): consensus-run tag (None when this task is not part
        // of a consensus batch — older rows never saw the columns).
        consensus_group_id: r
            .try_get::<Option<String>, _>("consensus_group_id")
            .ok()
            .flatten(),
        consensus_member: r
            .try_get::<Option<String>, _>("consensus_member")
            .ok()
            .flatten(),
        // Feature "opencode profiles": stored as JSON text; parse failures
        // are tolerated as None (a dashboard cannot render a malformed blob,
        // but the assignment path re-reads it fresh).
        opencode_override: r
            .try_get::<Option<String>, _>("opencode_override")
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<agentgrid_common::OpencodeOverride>(&s).ok()),
        // Competitor-gap feature (GitHub write-back): informational echo.
        github_repo: r.try_get::<Option<String>, _>("github_repo").ok().flatten(),
        github_issue: r.try_get::<Option<i64>, _>("github_issue").ok().flatten(),
        github_base_ref: r
            .try_get::<Option<String>, _>("github_base_ref")
            .ok()
            .flatten(),
        // Competitor-gap feature (convergence metrics): the attempt this task
        // was reworked from, if any.
        rework_of: r.try_get::<Option<String>, _>("rework_of").ok().flatten(),
        // Competitor-gap feature (task-level auto-retry): total attempts
        // allowed (1 = no auto-retry; older rows carry the column default).
        max_attempts: r
            .try_get::<Option<i64>, _>("max_attempts")
            .ok()
            .flatten()
            .unwrap_or(1) as u32,
        // Competitor-gap feature (consensus patch review): solve|review mode
        // and the reviewed task id (both absent on older rows).
        consensus_mode: r
            .try_get::<Option<String>, _>("consensus_mode")
            .ok()
            .flatten()
            .filter(|m| m != "solve"),
        review_of: r.try_get::<Option<String>, _>("review_of").ok().flatten(),
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

#[derive(Debug, Clone, serde::Serialize)]
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
