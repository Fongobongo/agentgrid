//! Incremental replication over SQLite session changesets (RPO async/sync).
//!
//! The leader records every write transaction in a `sqlite3_session` attached
//! to the user tables — never `ha_cluster_state`, whose epoch/term rows differ
//! between hosts by design. At commit it extracts the changeset and ships it to
//! the data replicas, which apply it with `sqlite3changeset_apply` and then
//! record the sequence.
//!
//! Changesets are self-delimiting, so a batch is a plain concatenation and the
//! batch envelope carries `[first_seq, last_seq]`. Applying a batch executes
//! ordinary DML, so FTS triggers fire exactly as they did on the leader.
//!
//! The bundled SQLite is compiled with `SQLITE_ENABLE_SESSION` (see
//! `.cargo/config.toml`); the pre-generated `libsqlite3-sys` bindings expose
//! the API, so no bindgen/libclang toolchain is required.

use std::ffi::{c_int, c_void, CString};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Result};
use sqlx::sqlite::{SqliteConnection, SqlitePool};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::config::RpoMode;
use super::quorum::VoterSet;
use super::role::{mono_ms, HaRuntime};
use super::transport::{self, ChangesetAck};

/// Async mode coalesces commits into one peer request for at most this long.
const ASYNC_BATCH_WINDOW: Duration = Duration::from_millis(1000);
const MAX_BATCH_JOBS: usize = 256;
const JOB_QUEUE: usize = 1024;

/// A `sqlite3_session` owned by one write transaction.
pub struct Session {
    raw: *mut libsqlite3_sys::sqlite3_session,
}

// SAFETY: the session belongs to the transaction's connection and is only
// touched while that transaction owns the connection exclusively (the write
// gate serialises writers), even if the future migrates between workers.
unsafe impl Send for Session {}

impl Session {
    /// Create a session on `conn` and attach the named tables. Must be called
    /// immediately after `BEGIN IMMEDIATE`, before the transaction writes.
    pub async fn attach(conn: &mut SqliteConnection, tables: &[String]) -> Result<Self> {
        let mut handle = conn.lock_handle().await?;
        let db = handle.as_raw_handle().as_ptr();
        let mut raw: *mut libsqlite3_sys::sqlite3_session = std::ptr::null_mut();
        // SAFETY: `db` is a live connection; `raw` is a valid out pointer.
        let rc = unsafe { libsqlite3_sys::sqlite3session_create(db, c"main".as_ptr(), &mut raw) };
        if rc != libsqlite3_sys::SQLITE_OK || raw.is_null() {
            bail!(
                "sqlite3session_create failed (rc={rc}); SQLite was built without \
                 SQLITE_ENABLE_SESSION (check LIBSQLITE3_FLAGS)"
            );
        }
        for table in tables {
            let Ok(name) = CString::new(table.as_str()) else {
                continue;
            };
            // SAFETY: `raw` is a live session; the name is NUL-terminated.
            let rc = unsafe { libsqlite3_sys::sqlite3session_attach(raw, name.as_ptr()) };
            if rc != libsqlite3_sys::SQLITE_OK {
                // SAFETY: `raw` came from sqlite3session_create and is deleted once.
                unsafe { libsqlite3_sys::sqlite3session_delete(raw) };
                bail!("sqlite3session_attach({table}) failed (rc={rc})");
            }
        }
        Ok(Self { raw })
    }

    /// Extract the pending changeset. `None` means the transaction wrote
    /// nothing (or only excluded tables).
    pub async fn take_changeset(&mut self, conn: &mut SqliteConnection) -> Result<Option<Vec<u8>>> {
        // Hold the connection lock so the session cannot be read while a write
        // is in flight on it.
        let _handle = conn.lock_handle().await?;
        let mut n: c_int = 0;
        let mut ptr: *mut c_void = std::ptr::null_mut();
        // SAFETY: live session; `n`/`ptr` are valid out pointers.
        let rc = unsafe { libsqlite3_sys::sqlite3session_changeset(self.raw, &mut n, &mut ptr) };
        if rc != libsqlite3_sys::SQLITE_OK {
            bail!("sqlite3session_changeset failed (rc={rc})");
        }
        if ptr.is_null() || n <= 0 {
            if !ptr.is_null() {
                // SAFETY: allocated by SQLite for this call.
                unsafe { libsqlite3_sys::sqlite3_free(ptr) };
            }
            return Ok(None);
        }
        // SAFETY: SQLite allocated `n` readable bytes at `ptr`.
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, n as usize) }.to_vec();
        // SAFETY: the buffer is no longer used after the copy.
        unsafe { libsqlite3_sys::sqlite3_free(ptr) };
        Ok(Some(bytes))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.raw.is_null() {
            // SAFETY: deleted exactly once; the pointer came from create.
            unsafe { libsqlite3_sys::sqlite3session_delete(self.raw) };
        }
    }
}

#[derive(Default)]
struct ApplyCtx {
    conflicts: u32,
}

unsafe extern "C" fn abort_on_conflict(
    ctx: *mut c_void,
    _conflict: c_int,
    _iter: *mut libsqlite3_sys::sqlite3_changeset_iter,
) -> c_int {
    // SAFETY: `ctx` points to the live ApplyCtx owned by `apply_raw`.
    let ctx = unsafe { &mut *(ctx as *mut ApplyCtx) };
    ctx.conflicts += 1;
    libsqlite3_sys::SQLITE_CHANGESET_ABORT
}

/// Apply a changeset (or concatenated batch) to the connection behind `db`.
/// The caller holds the connection's lock handle. Any conflict aborts the
/// whole apply, which makes the caller fall back to a snapshot.
///
/// # Safety
/// `db` must be a live sqlite3 handle and the caller must hold its lock.
pub unsafe fn apply_raw(db: *mut libsqlite3_sys::sqlite3, bytes: &[u8]) -> Result<u32> {
    let mut ctx = ApplyCtx::default();
    // SAFETY: valid handle; `bytes` outlives the call; the context is live.
    let rc = unsafe {
        libsqlite3_sys::sqlite3changeset_apply(
            db,
            bytes.len() as c_int,
            bytes.as_ptr() as *mut c_void,
            None,
            Some(abort_on_conflict),
            &mut ctx as *mut ApplyCtx as *mut c_void,
        )
    };
    if rc != libsqlite3_sys::SQLITE_OK {
        bail!(
            "sqlite3changeset_apply failed (rc={rc}, conflicts={})",
            ctx.conflicts
        );
    }
    Ok(ctx.conflicts)
}

/// Fail at startup, not on the first write, when SQLite lacks the session
/// extension (the C flags live in `.cargo/config.toml`).
async fn probe_session_support(pool: &SqlitePool) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let session = Session::attach(&mut tx, &[]).await?;
    drop(session);
    tx.rollback().await?;
    Ok(())
}

struct Job {
    seq: u64,
    bytes: Vec<u8>,
    ack: Option<oneshot::Sender<Result<(), String>>>,
}

/// Leader-side incremental replicator: owns the per-commit session table list
/// and the peer push worker.
pub struct Replicator {
    rt: Arc<HaRuntime>,
    tables: Vec<String>,
    schema_version: i64,
    tx: mpsc::Sender<Job>,
    rx: Mutex<Option<mpsc::Receiver<Job>>>,
    last_remote_ack_ms: AtomicU64,
}

impl Replicator {
    pub async fn new(rt: Arc<HaRuntime>, pool: &SqlitePool) -> Result<Arc<Self>> {
        probe_session_support(pool).await?;
        // `pragma_table_list` reports virtual and shadow tables distinctly; FTS
        // internals are maintained by triggers on the base tables, which fire
        // again when a changeset is applied.
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM pragma_table_list WHERE schema = 'main' AND type = 'table' \
             AND name NOT IN ('ha_cluster_state', '_sqlx_migrations', 'sqlite_sequence', \
                               'sqlite_stat1') ORDER BY name",
        )
        .fetch_all(pool)
        .await?;
        let schema_version: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(pool)
            .await?;
        let (tx, rx) = mpsc::channel(JOB_QUEUE);
        Ok(Arc::new(Self {
            rt,
            tables,
            schema_version,
            tx,
            rx: Mutex::new(Some(rx)),
            last_remote_ack_ms: AtomicU64::new(mono_ms()),
        }))
    }

    pub fn tables(&self) -> &[String] {
        &self.tables
    }

    pub fn schema_version(&self) -> i64 {
        self.schema_version
    }

    pub fn max_changeset_bytes(&self) -> u64 {
        self.rt.config.max_changeset_bytes
    }

    /// Ask the leader to cut a snapshot (one write exceeded the changeset cap
    /// or a peer reported a gap).
    pub fn request_snapshot(&self) {
        self.rt.shared.request_snapshot();
    }

    /// Start the push worker (idempotent; the receiver is taken once).
    pub fn start(self: &Arc<Self>) {
        let mut guard = self.rx.lock().unwrap_or_else(|e| e.into_inner());
        let Some(rx) = guard.take() else {
            return;
        };
        drop(guard);
        let this = self.clone();
        tokio::spawn(async move {
            if this.rt.config.rpo_mode == RpoMode::Sync {
                this.run_sync(rx).await;
            } else {
                this.run_async(rx).await;
            }
        });
    }

    /// Publish one committed changeset. In `sync` mode this waits for a data
    /// quorum acknowledgement; in `async` mode it returns after enqueueing.
    pub async fn publish(&self, seq: u64, bytes: Vec<u8>) -> Result<()> {
        match self.rt.config.rpo_mode {
            RpoMode::Snapshot => Ok(()),
            RpoMode::Async => {
                self.tx
                    .send(Job {
                        seq,
                        bytes,
                        ack: None,
                    })
                    .await
                    .map_err(|_| anyhow::anyhow!("HA replication worker stopped"))?;
                self.rt.shared.set_applied_seq(seq);
                Ok(())
            }
            RpoMode::Sync => {
                let (ack_tx, ack_rx) = oneshot::channel();
                self.tx
                    .send(Job {
                        seq,
                        bytes,
                        ack: Some(ack_tx),
                    })
                    .await
                    .map_err(|_| anyhow::anyhow!("HA replication worker stopped"))?;
                let wait = (self.rt.config.lease_ttl / 2).max(Duration::from_secs(1));
                match tokio::time::timeout(wait, ack_rx).await {
                    Ok(Ok(Ok(()))) => {
                        self.rt.shared.set_applied_seq(seq);
                        Ok(())
                    }
                    Ok(Ok(Err(e))) => {
                        self.rt.shared.set_replication_lost();
                        bail!("sync replication failed: {e}");
                    }
                    Ok(Err(_)) => {
                        self.rt.shared.set_replication_lost();
                        bail!("sync replication worker dropped the acknowledgement");
                    }
                    Err(_) => {
                        self.rt.shared.set_replication_lost();
                        bail!("sync replication ack exceeded {wait:?}");
                    }
                }
            }
        }
    }

    fn data_majority(&self) -> usize {
        VoterSet::new(self.rt.config.data_replicas, self.rt.config.witnesses_count).data_majority()
    }

    async fn run_sync(self: Arc<Self>, mut rx: mpsc::Receiver<Job>) {
        while let Some(job) = rx.recv().await {
            let outcome = match self.push_batch(job.seq, job.seq, &job.bytes).await {
                Ok(acks) if acks >= self.data_majority() => Ok(()),
                Ok(acks) => {
                    tracing::warn!(
                        seq = job.seq,
                        acks,
                        need = self.data_majority(),
                        "sync write missed the data quorum"
                    );
                    self.rt.shared.set_replication_lost();
                    Err(format!(
                        "changeset {} acknowledged by {acks} data replicas, need {}",
                        job.seq,
                        self.data_majority()
                    ))
                }
                Err(e) => {
                    tracing::warn!(seq = job.seq, "sync write replication failed: {e}");
                    self.rt.shared.set_replication_lost();
                    Err(e.to_string())
                }
            };
            if let Some(ack) = job.ack {
                let _ = ack.send(outcome);
            }
        }
    }

    async fn run_async(self: Arc<Self>, mut rx: mpsc::Receiver<Job>) {
        let lag = self.rt.config.async_max_lag;
        let window = ASYNC_BATCH_WINDOW.min(lag);
        let max = self.rt.config.max_changeset_bytes as usize;
        loop {
            let Some(first) = rx.recv().await else {
                return;
            };
            let deadline = Instant::now() + window;
            let mut jobs = vec![first];
            while jobs.len() < MAX_BATCH_JOBS {
                match tokio::time::timeout_at(deadline, rx.recv()).await {
                    Ok(Some(job)) => jobs.push(job),
                    _ => break,
                }
            }
            let mut chunk: Vec<u8> = Vec::new();
            let mut first_seq = 0u64;
            let mut last_seq = 0u64;
            for job in jobs {
                if !chunk.is_empty() && chunk.len() + job.bytes.len() > max {
                    self.flush_async(first_seq, last_seq, &chunk, lag).await;
                    chunk.clear();
                }
                if chunk.is_empty() {
                    first_seq = job.seq;
                }
                last_seq = job.seq;
                chunk.extend_from_slice(&job.bytes);
            }
            if !chunk.is_empty() {
                self.flush_async(first_seq, last_seq, &chunk, lag).await;
            }
        }
    }

    async fn flush_async(&self, first_seq: u64, last_seq: u64, bytes: &[u8], lag: Duration) {
        let acks = self.push_batch(first_seq, last_seq, bytes).await;
        match acks {
            Ok(acks) if acks > 1 => {
                self.last_remote_ack_ms.store(mono_ms(), Ordering::Release);
            }
            Ok(_) => {
                let behind =
                    mono_ms().saturating_sub(self.last_remote_ack_ms.load(Ordering::Acquire));
                if behind > lag.as_millis() as u64 {
                    tracing::warn!(
                        seq = last_seq,
                        behind_ms = behind,
                        "no data replica acknowledged within the async lag budget"
                    );
                    self.rt.shared.set_replication_lost();
                }
            }
            Err(e) => tracing::debug!(seq = last_seq, "async replication push failed: {e}"),
        }
    }

    /// Push one changeset batch to every data peer; returns the number of
    /// replicas that have it (self included).
    async fn push_batch(&self, first_seq: u64, last_seq: u64, bytes: &[u8]) -> Result<usize> {
        let epoch = self.rt.gate.epoch();
        let mut acks = 1;
        for peer in &self.rt.config.peers {
            match transport::push_changeset(
                &self.rt,
                &peer.url,
                epoch,
                first_seq,
                last_seq,
                self.schema_version,
                bytes,
            )
            .await
            {
                Ok(ChangesetAck::Applied | ChangesetAck::Duplicate) => acks += 1,
                Ok(ChangesetAck::Rejected {
                    applied_seq,
                    reason,
                }) => {
                    tracing::warn!(
                        peer = %peer.id,
                        seq = last_seq,
                        applied_seq,
                        reason,
                        "peer rejected changeset; requesting a snapshot"
                    );
                    self.rt.shared.request_snapshot();
                }
                Err(e) => tracing::debug!(peer = %peer.id, "changeset push failed: {e}"),
            }
        }
        Ok(acks)
    }
}
