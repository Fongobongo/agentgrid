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

use std::collections::VecDeque;
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

/// Changeset batches applied on this host, and apply attempts that ended in a
/// rejection/gap. Process-wide (like the SQLite write-path counters) so
/// `/metrics` can show a standby that stopped making progress.
static BATCHES_APPLIED: AtomicU64 = AtomicU64::new(0);
static APPLY_FAILURES: AtomicU64 = AtomicU64::new(0);
/// Demotions repaired by undoing the divergent tail, and demotions that fell
/// back to a snapshot. The ratio is the answer to "is failover costing us a
/// database download?".
static REWINDS: AtomicU64 = AtomicU64::new(0);
static REWIND_FAILURES: AtomicU64 = AtomicU64::new(0);

/// `(batches applied, apply failures)` since process start.
pub fn changeset_stats() -> (u64, u64) {
    (
        BATCHES_APPLIED.load(Ordering::Relaxed),
        APPLY_FAILURES.load(Ordering::Relaxed),
    )
}

/// `(rewinds, fallbacks to a snapshot)` since process start.
pub fn rewind_stats() -> (u64, u64) {
    (
        REWINDS.load(Ordering::Relaxed),
        REWIND_FAILURES.load(Ordering::Relaxed),
    )
}

pub(crate) fn note_rewind(rewound: bool) {
    if rewound {
        REWINDS.fetch_add(1, Ordering::Relaxed);
    } else {
        REWIND_FAILURES.fetch_add(1, Ordering::Relaxed);
    }
}

pub(crate) fn note_batches(n: usize) {
    if n > 0 {
        BATCHES_APPLIED.fetch_add(n as u64, Ordering::Relaxed);
    }
}

pub(crate) fn note_apply_failure() {
    APPLY_FAILURES.fetch_add(1, Ordering::Relaxed);
}
use crate::store::{ApplyOutcome, Store};

/// Async mode coalesces commits into one peer request for at most this long.
const ASYNC_BATCH_WINDOW: Duration = Duration::from_millis(1000);
const MAX_BATCH_JOBS: usize = 256;
const JOB_QUEUE: usize = 1024;

/// A demoted leader undoes its own un-replicated tail instead of restoring a
/// whole snapshot, which needs the changesets it shipped to still be around.
/// Only those above the last data-quorum acknowledgement matter, so in steady
/// state the buffer holds one entry in sync mode and one async batch window in
/// async mode. The caps are two orders of magnitude above that and exist to
/// bound a pathological case, not to be reached.
const REWIND_MAX_ENTRIES: usize = 512;
const REWIND_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Changesets this node committed while leader, newest last, minus everything
/// the cluster already acknowledged.
#[derive(Default)]
struct RewindBuffer {
    entries: VecDeque<(u64, Arc<Vec<u8>>)>,
    bytes: usize,
    /// Lowest sequence dropped to stay under the caps. A rewind target below
    /// it is unreachable, because that changeset is gone.
    floor: Option<u64>,
}

impl RewindBuffer {
    fn push(&mut self, seq: u64, bytes: Arc<Vec<u8>>) {
        self.bytes += bytes.len();
        self.entries.push_back((seq, bytes));
        while self.entries.len() > REWIND_MAX_ENTRIES || self.bytes > REWIND_MAX_BYTES {
            let Some((seq, dropped)) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= dropped.len();
            self.floor = Some(seq);
        }
    }

    /// Forget what a data quorum already holds.
    fn trim(&mut self, replicated: u64) {
        while let Some((seq, _)) = self.entries.front() {
            if *seq > replicated {
                break;
            }
            let Some((_, dropped)) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= dropped.len();
        }
    }

    fn can_rewind_to(&self, target: u64) -> bool {
        match self.floor {
            Some(floor) => target >= floor,
            None => true,
        }
    }
}

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

/// Reverse a changeset: `INSERT` becomes `DELETE`, `DELETE` becomes `INSERT`,
/// and `UPDATE` keeps its primary key with the old/new images swapped. Applying
/// the result to the database the forward changeset produced restores the rows
/// exactly as they were before it.
pub fn invert_changeset(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut n: c_int = 0;
    let mut ptr: *mut c_void = std::ptr::null_mut();
    // SAFETY: `bytes` is a well-formed changeset; `n`/`ptr` are out pointers.
    let rc = unsafe {
        libsqlite3_sys::sqlite3changeset_invert(
            bytes.len() as c_int,
            bytes.as_ptr() as *const c_void,
            &mut n,
            &mut ptr,
        )
    };
    if rc != libsqlite3_sys::SQLITE_OK || ptr.is_null() {
        bail!("sqlite3changeset_invert failed (rc={rc})");
    }
    // SAFETY: SQLite allocated `n` readable bytes at `ptr`.
    let out = unsafe { std::slice::from_raw_parts(ptr as *const u8, n as usize) }.to_vec();
    // SAFETY: freed exactly once, after the copy.
    unsafe { libsqlite3_sys::sqlite3_free(ptr) };
    Ok(out)
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
    bytes: Arc<Vec<u8>>,
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
    /// Un-replicated changesets, kept so a demotion can undo them instead of
    /// restoring a snapshot (see [`Replicator::rewind_to`]).
    rewind: Mutex<RewindBuffer>,
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
            rewind: Mutex::new(RewindBuffer::default()),
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
        if self.rt.config.rpo_mode == RpoMode::Snapshot {
            return Ok(());
        }
        // Keep the changeset until a data quorum has it: if this node is
        // demoted before then, the write exists nowhere else and has to be
        // undone locally (`rewind_to`). `Arc` so the worker and the buffer
        // share one copy instead of duplicating every write.
        let bytes = Arc::new(bytes);
        self.lock_rewind().push(seq, bytes.clone());
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

    fn lock_rewind(&self) -> std::sync::MutexGuard<'_, RewindBuffer> {
        self.rewind.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A data quorum holds `seq`: it can no longer be lost, so it no longer has
    /// to be undoable. Both the watermark and the trim happen under the buffer
    /// lock, so a concurrent `rewind_to` never sees a watermark whose
    /// changesets are already gone.
    fn note_replicated(&self, seq: u64) {
        let mut buf = self.lock_rewind();
        buf.trim(seq);
        self.rt.shared.advance_last_replicated_seq(seq);
    }

    /// Undo this node's own un-replicated writes, so its database is a valid
    /// prefix of the cluster history again.
    ///
    /// This is the cheap alternative to restoring a snapshot after a demotion.
    /// The leader is the only writer, so the divergent suffix is exactly the
    /// changesets above the last data-quorum acknowledgement — the ones the
    /// rewind buffer still holds. They are inverted and replayed newest-first in
    /// one transaction: when it commits, `repl_seq` is `target` again and the
    /// rows are what they were before this leadership admitted them, which
    /// makes the node immediately replicable *and* electable instead of
    /// condemned until a snapshot lands.
    ///
    /// Returns the sequence it rewound to. Any doubt — a changeset evicted from
    /// the buffer, a gap, a conflicting apply, a position that moved under us —
    /// is an error, and the caller falls back to the full snapshot.
    pub async fn rewind_to(&self, store: &Store, hint: u64) -> Result<u64> {
        let local = store.ha_repl_seq().await?;
        let (target, inverted) = {
            let buf = self.lock_rewind();
            // Re-read the watermark under the lock: a quorum acknowledgement
            // that landed while this demotion was in flight moved it up, and
            // its changeset is gone from the buffer by now.
            let target = hint.max(self.rt.shared.last_replicated_seq());
            if local <= target {
                return Ok(local);
            }
            if !buf.can_rewind_to(target) {
                bail!(
                    "changeset {} was evicted from the rewind buffer",
                    buf.floor.unwrap_or(target)
                );
            }
            let mut inverted = Vec::new();
            let mut expect = local;
            for (seq, bytes) in buf.entries.iter().rev() {
                if *seq <= target {
                    break;
                }
                if *seq != expect {
                    bail!("changeset {expect} is not retained");
                }
                inverted.extend_from_slice(&invert_changeset(bytes)?);
                expect -= 1;
            }
            if expect != target {
                bail!("changeset {expect} is not retained");
            }
            (target, inverted)
        };
        store.apply_rewind(local, target, &inverted).await?;
        tracing::info!(
            from = local,
            to = target,
            undone = local - target,
            "undid un-replicated writes from the demoted leadership"
        );
        Ok(target)
    }

    async fn run_sync(self: Arc<Self>, mut rx: mpsc::Receiver<Job>) {
        while let Some(job) = rx.recv().await {
            let outcome = match self.push_batch(job.seq, job.seq, &job.bytes).await {
                Ok(acks) if acks >= self.data_majority() => {
                    self.note_replicated(job.seq);
                    Ok(())
                }
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
            Ok(acks) => {
                if acks >= self.data_majority() {
                    self.note_replicated(last_seq);
                }
                if acks > 1 {
                    self.last_remote_ack_ms.store(mono_ms(), Ordering::Release);
                } else {
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
            }
            Err(e) => tracing::debug!(seq = last_seq, "async replication push failed: {e}"),
        }
    }

    /// Push one changeset batch to every data peer; returns the number of
    /// replicas that have it (self included). In S3 backend mode one
    /// successful PUT is the remote durable acknowledgement: self + object
    /// store = 2 data acks, which is the majority for 2 and 3 data replicas.
    async fn push_batch(&self, first_seq: u64, last_seq: u64, bytes: &[u8]) -> Result<usize> {
        let epoch = self.rt.gate.epoch();
        if let Some(s3) = &self.rt.s3 {
            s3.put_changeset(epoch, self.schema_version, first_seq, last_seq, bytes)
                .await?;
            return Ok(2);
        }
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

/// Standby in S3 backend mode: apply committed changeset objects in order.
/// Returns the number of batches applied. A gap, schema mismatch or rejected
/// batch stops the pass; the snapshot loop then restores `latest.json`.
pub async fn apply_s3_pending(rt: &HaRuntime, store: &Store) -> Result<usize> {
    let Some(s3) = &rt.s3 else {
        return Ok(0);
    };
    if rt.is_leader() || rt.shared.need_full_snapshot() {
        return Ok(0);
    }
    let schema_version = store.schema_version().await?;
    let mut applied = 0usize;
    // Start the LIST after what we already have: the previous version listed
    // the whole `changesets/` prefix on every 2 s tick, so an idle standby's
    // poll cost grew linearly with the cluster's write history.
    let start_after = store.ha_repl_seq().await?.max(rt.shared.applied_seq());
    for key in s3.list_changesets_after(start_after).await? {
        let Some((first_seq, last_seq)) = super::backend::S3Client::changeset_range(&key) else {
            continue;
        };
        let local = store.ha_repl_seq().await?.max(rt.shared.applied_seq());
        if last_seq <= local {
            continue;
        }
        if first_seq > local + 1 {
            tracing::info!(
                first_seq,
                local,
                "object-storage changeset gap; waiting for a snapshot"
            );
            note_apply_failure();
            break;
        }
        let Some(obj) = s3.get_changeset(&key).await? else {
            continue;
        };
        if obj.first_seq != first_seq || obj.last_seq != last_seq {
            anyhow::bail!("changeset object {key} header/range mismatch");
        }
        if obj.schema_version != schema_version {
            tracing::warn!(
                key,
                schema = obj.schema_version,
                local = schema_version,
                "changeset schema mismatch; waiting for a snapshot"
            );
            break;
        }
        if obj.epoch < rt.gate.epoch() {
            continue;
        }
        if obj.epoch > rt.gate.epoch() {
            rt.gate.set_epoch(obj.epoch);
        }
        let _apply = rt.shared.apply_lock.lock().await;
        match store
            .apply_changeset_batch(obj.first_seq, obj.last_seq, obj.schema_version, &obj.bytes)
            .await?
        {
            ApplyOutcome::Applied { seq } => {
                rt.shared.set_applied_seq(seq);
                applied += 1;
            }
            ApplyOutcome::Duplicate => {
                if rt.shared.applied_seq() < obj.last_seq {
                    rt.shared.set_applied_seq(obj.last_seq);
                }
            }
            ApplyOutcome::Rejected {
                applied_seq,
                reason,
            } => {
                tracing::warn!(key, applied_seq, reason, "object changeset rejected");
                note_apply_failure();
                break;
            }
        }
    }
    note_batches(applied);
    Ok(applied)
}
