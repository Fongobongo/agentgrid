//! HA store helpers: epoch / replication-sequence bookkeeping and the in-place
//! snapshot restore used by a standby.
//!
//! A standby cannot replace its `SqlitePool` (AppState owns the store for the
//! process lifetime), so a snapshot is applied through the SQLite backup API
//! into the live database file. The caller (standby apply loop) guarantees the
//! node is not serving mutations while this runs.

use std::ffi::{c_int, CString};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};
use sqlx::Connection;

use super::Store;
use crate::ha::incremental;
use crate::store::WRITE_GATE;

/// Result of applying a replicated changeset batch on a standby.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied { seq: u64 },
    Duplicate,
    Rejected { applied_seq: u64, reason: String },
}

/// Result of validating a snapshot file before applying it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotCheck {
    pub schema_version: i64,
    /// Replication sequence the snapshot contains; 0 for files written before
    /// this column was read back.
    pub repl_seq: u64,
}

impl Store {
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Directory that also holds `artifacts/` and HA snapshot files.
    pub fn data_dir(&self) -> &Path {
        self.db_path.parent().unwrap_or_else(|| Path::new("."))
    }

    async fn ha_state_i64(&self, column: &'static str) -> Result<i64> {
        // audited: `column` is a compile-time literal, never user input
        let sql = format!("SELECT {column} FROM ha_cluster_state WHERE id = 1");
        let v: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.as_str()))
            .fetch_optional(&self.pool)
            .await?;
        Ok(v.unwrap_or(0))
    }

    pub async fn ha_epoch(&self) -> Result<u64> {
        Ok(self.ha_state_i64("epoch").await?.max(0) as u64)
    }

    pub async fn ha_repl_seq(&self) -> Result<u64> {
        Ok(self.ha_state_i64("repl_seq").await?.max(0) as u64)
    }

    pub async fn ha_term(&self) -> Result<u64> {
        Ok(self.ha_state_i64("term").await?.max(0) as u64)
    }

    /// Highest applied sqlx migration (the snapshot schema version).
    pub async fn schema_version(&self) -> Result<i64> {
        let v: Option<i64> = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(&self.pool)
            .await?;
        Ok(v.unwrap_or(0))
    }

    /// Leader-side promotion: persist the new epoch under an unfenced write
    /// transaction (the gate is flipped only after this commits). In snapshot
    /// mode it also reserves a fresh replication sequence for the snapshot the
    /// new leader will ship; incremental modes keep the current sequence so
    /// standbys do not see a gap.
    pub async fn promote_epoch(
        &self,
        epoch: u64,
        term: u64,
        leader_id: &str,
        bump_seq: bool,
    ) -> Result<u64> {
        let seq = self
            .promote_epoch_inner(epoch, term, leader_id, bump_seq)
            .await
            .context("persist ha_cluster_state epoch")?;
        Ok(seq)
    }

    async fn promote_epoch_inner(
        &self,
        epoch: u64,
        term: u64,
        leader_id: &str,
        bump_seq: bool,
    ) -> Result<u64> {
        let schema = self.schema_version().await?;
        let mut tx = super::begin_immediate(&self.pool).await?;
        let sql = if bump_seq {
            "UPDATE ha_cluster_state SET epoch = ?, term = ?, leader_id = ?, \
             schema_version = ?, repl_seq = repl_seq + 1, updated_at = ? WHERE id = 1"
        } else {
            "UPDATE ha_cluster_state SET epoch = ?, term = ?, leader_id = ?, \
             schema_version = ?, updated_at = ? WHERE id = 1"
        };
        sqlx::query(sql)
            .bind(epoch as i64)
            .bind(term as i64)
            .bind(leader_id)
            .bind(schema)
            .bind(super::now_iso())
            .execute(&mut *tx)
            .await?;
        let seq: i64 = sqlx::query_scalar("SELECT repl_seq FROM ha_cluster_state WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(seq.max(0) as u64)
    }

    /// Bump `repl_seq` and return the new value. Only the leader calls this,
    /// so it goes through the fenced write path.
    pub async fn next_repl_seq(&self) -> Result<u64> {
        let mut tx = self.write_txn().await?;
        sqlx::query(
            "UPDATE ha_cluster_state SET repl_seq = repl_seq + 1, updated_at = ? WHERE id = 1",
        )
        .bind(super::now_iso())
        .execute(&mut *tx)
        .await?;
        let seq: i64 = sqlx::query_scalar("SELECT repl_seq FROM ha_cluster_state WHERE id = 1")
            .fetch_one(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(seq.max(0) as u64)
    }

    /// Validate a downloaded snapshot: readable, `quick_check` clean, and a
    /// known schema. Never trusts the sender's size/hash alone.
    pub async fn verify_snapshot_file(path: &Path) -> Result<SnapshotCheck> {
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .read_only(true)
            .busy_timeout(Duration::from_secs(5));
        let mut conn = SqliteConnection::connect_with(&opts)
            .await
            .with_context(|| format!("open snapshot {}", path.display()))?;
        let integrity: String = sqlx::query_scalar("PRAGMA quick_check")
            .fetch_one(&mut conn)
            .await?;
        if !integrity.eq_ignore_ascii_case("ok") {
            conn.close().await.ok();
            anyhow::bail!("snapshot quick_check failed: {integrity}");
        }
        let schema_version: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
            .fetch_one(&mut conn)
            .await?;
        let repl_seq: i64 =
            sqlx::query_scalar("SELECT repl_seq FROM ha_cluster_state WHERE id = 1")
                .fetch_optional(&mut conn)
                .await?
                .unwrap_or(0);
        conn.close().await.ok();
        Ok(SnapshotCheck {
            schema_version,
            repl_seq: repl_seq.max(0) as u64,
        })
    }

    /// `VACUUM INTO` a snapshot and return the replication sequence it
    /// contains. The write gate stays held across both steps, so no commit can
    /// land between the sequence read and the copy.
    pub async fn snapshot_to(&self, name: &str) -> Result<u64> {
        let _permit = WRITE_GATE
            .acquire()
            .await
            .expect("write gate semaphore closed");
        let seq = self.ha_repl_seq().await?;
        self.backup_to(name).await?;
        Ok(seq)
    }

    /// Keep a consistent copy of the local database that may hold writes the
    /// cluster never acknowledged (a demoted leader). Best effort: a failed
    /// quarantine is logged, not fatal.
    pub async fn quarantine(&self, epoch: u64) -> Option<String> {
        let name = format!(
            "control-plane.db.stale-{epoch}-{}",
            chrono::Utc::now().timestamp()
        );
        match self.backup_to(&name).await {
            Ok(()) => {
                tracing::warn!(file = %name, "quarantined local database before snapshot restore");
                Some(name)
            }
            Err(e) => {
                tracing::warn!("database quarantine failed: {e}");
                None
            }
        }
    }

    /// Apply a replicated changeset batch (`first_seq..=last_seq`) on a
    /// standby. Commits with `synchronous=FULL` so an acknowledged sync write
    /// survives a standby crash. Any conflict aborts the whole batch; the
    /// caller then falls back to a snapshot.
    pub async fn apply_changeset_batch(
        &self,
        first_seq: u64,
        last_seq: u64,
        schema_version: i64,
        bytes: &[u8],
    ) -> Result<ApplyOutcome> {
        // Serialise applies (and re-read the watermark under the gate) so two
        // peer requests cannot both pass the gap check and apply the batch.
        let _permit = WRITE_GATE
            .acquire()
            .await
            .expect("write gate semaphore closed");
        let local_schema = self.schema_version().await?;
        let applied = self.ha_repl_seq().await?;
        if schema_version != local_schema {
            return Ok(ApplyOutcome::Rejected {
                applied_seq: applied,
                reason: format!("schema {schema_version} != local {local_schema}"),
            });
        }
        if last_seq <= applied {
            return Ok(ApplyOutcome::Duplicate);
        }
        if first_seq != applied + 1 {
            return Ok(ApplyOutcome::Rejected {
                applied_seq: applied,
                reason: format!("gap: expected {}, batch starts {first_seq}", applied + 1),
            });
        }
        let mut conn = self.pool.acquire().await?;
        // WAL + NORMAL is not durable across an OS crash; the apply path pays
        // the fsync so a data-quorum ack is never lost with the standby.
        sqlx::query("PRAGMA synchronous=FULL")
            .execute(&mut *conn)
            .await?;
        let mut tx = conn.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("PRAGMA defer_foreign_keys=ON")
            .execute(&mut *tx)
            .await?;
        let applied_raw = {
            let mut handle = tx.lock_handle().await?;
            let db = handle.as_raw_handle().as_ptr();
            // SAFETY: `db` is a live handle and the lock is held for the call.
            unsafe { incremental::apply_raw(db, bytes) }
        };
        if let Err(e) = applied_raw {
            tx.rollback().await?;
            sqlx::query("PRAGMA synchronous=NORMAL")
                .execute(&mut *conn)
                .await?;
            return Ok(ApplyOutcome::Rejected {
                applied_seq: applied,
                reason: format!("apply failed: {e}"),
            });
        }
        sqlx::query("UPDATE ha_cluster_state SET repl_seq = ?, updated_at = ? WHERE id = 1")
            .bind(last_seq as i64)
            .bind(super::now_iso())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        sqlx::query("PRAGMA synchronous=NORMAL")
            .execute(&mut *conn)
            .await?;
        Ok(ApplyOutcome::Applied { seq: last_seq })
    }

    /// Undo this leader's own un-replicated writes in one transaction.
    ///
    /// `inverted` holds the changesets above `target_seq`, reversed and already
    /// inverted, so replaying them walks the database back to `target_seq`.
    /// `from_seq` is the position the caller built that batch against; if the
    /// database moved (an in-flight commit that was still holding the write
    /// gate), the batch is stale and this refuses instead of undoing the wrong
    /// rows.
    pub async fn apply_rewind(
        &self,
        from_seq: u64,
        target_seq: u64,
        inverted: &[u8],
    ) -> Result<()> {
        let _permit = WRITE_GATE
            .acquire()
            .await
            .expect("write gate semaphore closed");
        let local = self.ha_repl_seq().await?;
        if local != from_seq {
            anyhow::bail!(
                "replication position moved from {from_seq} to {local} during the rewind"
            );
        }
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("PRAGMA defer_foreign_keys=ON")
            .execute(&mut *tx)
            .await?;
        let undone = {
            let mut handle = tx.lock_handle().await?;
            let db = handle.as_raw_handle().as_ptr();
            // SAFETY: `db` is a live handle and the lock is held for the call.
            unsafe { incremental::apply_raw(db, inverted) }
        };
        if let Err(e) = undone {
            tx.rollback().await?;
            anyhow::bail!("undoing the divergent changesets failed: {e}");
        }
        sqlx::query("UPDATE ha_cluster_state SET repl_seq = ?, updated_at = ? WHERE id = 1")
            .bind(target_seq as i64)
            .bind(super::now_iso())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Apply `snapshot` into the live database file via the SQLite backup API.
    /// Blocking FFI runs on the blocking pool.
    pub async fn restore_snapshot(&self, snapshot: std::path::PathBuf) -> Result<()> {
        // Close every pooled connection first: the restore checkpoints the
        // destination WAL and a concurrent idle reader would keep the copied
        // pages out of the main file. The pool reopens lazily afterwards.
        let max = self.pool.options().get_max_connections();
        for _ in 0..max {
            match self.pool.acquire().await {
                Ok(conn) => {
                    let _ = conn.close().await;
                }
                Err(_) => break,
            }
        }
        let dest = self.db_path.clone();
        tokio::task::spawn_blocking(move || restore_in_place(&snapshot, &dest))
            .await
            .context("snapshot restore task panicked")??;
        Ok(())
    }
}

fn cstr(path: &Path) -> Result<CString> {
    // `as_encoded_bytes` is the portable accessor (unix: raw bytes, Windows:
    // WTF-8); `std::os::unix::ffi` made this module unix-only, so the crate
    // could not even be type-checked on the Windows/macOS dev hosts.
    CString::new(path.as_os_str().as_encoded_bytes()).context("path contains NUL")
}

struct RawDb(*mut libsqlite3_sys::sqlite3);

impl Drop for RawDb {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: closed exactly once; the handle came from sqlite3_open_v2.
            unsafe { libsqlite3_sys::sqlite3_close(self.0) };
        }
    }
}

unsafe fn open_db(path: &Path, flags: c_int) -> Result<RawDb> {
    let c = cstr(path)?;
    let mut db: *mut libsqlite3_sys::sqlite3 = std::ptr::null_mut();
    // SAFETY: `c` is a valid NUL-terminated path and `db` is a valid out ptr.
    let rc =
        unsafe { libsqlite3_sys::sqlite3_open_v2(c.as_ptr(), &mut db, flags, std::ptr::null()) };
    if rc != libsqlite3_sys::SQLITE_OK {
        let msg = if db.is_null() {
            format!("sqlite3_open_v2 rc={rc}")
        } else {
            // SAFETY: db is a live handle; errmsg returns a static string.
            let m = unsafe { std::ffi::CStr::from_ptr(libsqlite3_sys::sqlite3_errmsg(db)) }
                .to_string_lossy()
                .into_owned();
            // SAFETY: closing the handle we just opened.
            unsafe { libsqlite3_sys::sqlite3_close(db) };
            m
        };
        anyhow::bail!("open {}: {msg}", path.display());
    }
    Ok(RawDb(db))
}

fn errmsg(db: *mut libsqlite3_sys::sqlite3) -> String {
    if db.is_null() {
        return "null sqlite handle".into();
    }
    // SAFETY: errmsg returns a NUL-terminated static string for this handle.
    unsafe { std::ffi::CStr::from_ptr(libsqlite3_sys::sqlite3_errmsg(db)) }
        .to_string_lossy()
        .into_owned()
}

fn exec(db: *mut libsqlite3_sys::sqlite3, sql: &str) -> Result<()> {
    let c = CString::new(sql).context("pragma contains NUL")?;
    // SAFETY: valid handle + NUL-terminated SQL.
    let rc = unsafe {
        libsqlite3_sys::sqlite3_exec(
            db,
            c.as_ptr(),
            None,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if rc != libsqlite3_sys::SQLITE_OK {
        anyhow::bail!("{sql}: {}", errmsg(db));
    }
    Ok(())
}

fn restore_in_place(snapshot: &Path, dest: &Path) -> Result<()> {
    let src = unsafe { open_db(snapshot, libsqlite3_sys::SQLITE_OPEN_READONLY)? };
    let dst = unsafe { open_db(dest, libsqlite3_sys::SQLITE_OPEN_READWRITE)? };
    exec(dst.0, "PRAGMA busy_timeout=5000")?;
    let main = c"main";
    // SAFETY: both handles are live for the duration of the backup.
    let backup =
        unsafe { libsqlite3_sys::sqlite3_backup_init(dst.0, main.as_ptr(), src.0, main.as_ptr()) };
    if backup.is_null() {
        anyhow::bail!("backup_init failed: {}", errmsg(dst.0));
    }
    let mut rc;
    let mut attempts = 0;
    loop {
        // SAFETY: `backup` is a valid backup handle.
        rc = unsafe { libsqlite3_sys::sqlite3_backup_step(backup, -1) };
        if rc == libsqlite3_sys::SQLITE_DONE || rc == libsqlite3_sys::SQLITE_OK {
            break;
        }
        if rc == libsqlite3_sys::SQLITE_BUSY || rc == libsqlite3_sys::SQLITE_LOCKED {
            attempts += 1;
            if attempts > 100 {
                // SAFETY: finishing the backup handle exactly once.
                unsafe { libsqlite3_sys::sqlite3_backup_finish(backup) };
                anyhow::bail!("backup step stayed busy after {attempts} retries");
            }
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        let msg = errmsg(dst.0);
        // SAFETY: finishing the backup handle exactly once.
        unsafe { libsqlite3_sys::sqlite3_backup_finish(backup) };
        anyhow::bail!("backup step rc={rc}: {msg}");
    }
    // SAFETY: finishing the backup handle exactly once.
    let finish = unsafe { libsqlite3_sys::sqlite3_backup_finish(backup) };
    if finish != libsqlite3_sys::SQLITE_OK {
        anyhow::bail!("backup_finish rc={finish}: {}", errmsg(dst.0));
    }
    // The destination is a WAL database while pooled connections hold it open,
    // so the copied pages sit in its WAL. Checkpoint them into the main file
    // before closing: a reopened connection would otherwise see a WAL whose
    // header counters came from the snapshot and discard the restored pages.
    exec(dst.0, "PRAGMA wal_checkpoint(TRUNCATE)")?;
    exec(dst.0, "PRAGMA journal_mode=WAL")?;
    exec(dst.0, "PRAGMA synchronous=NORMAL")?;
    Ok(())
}
