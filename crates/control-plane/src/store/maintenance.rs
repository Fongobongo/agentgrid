//! Background maintenance: lease reversion, offline detection, WAL
//! checkpoints, backups, artifact retention, disk accounting.
//! Extracted from `store.rs`.

use super::{iso_plus_secs, mark_offline_nodes, now_iso, revert_expired_leases, Store};
use anyhow::Result;
use sqlx::Row;
use std::time::Duration;

impl Store {
    /// Background maintenance: revert unconfirmed assignments (lease expired)
    /// and mark silent nodes offline. Runs one tick (the background loop calls
    /// this repeatedly; also exposed for tests/ops).
    pub async fn tick_maintenance(&self) -> Result<()> {
        let now = now_iso();
        let reverted = revert_expired_leases(&self.pool, &now).await?;
        if reverted > 0 {
            self.lease_reverts
                .fetch_add(reverted as u64, std::sync::atomic::Ordering::Relaxed);
        }
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
        // Plan 0.2 item 4.2: periodic automatic backup with rotation.
        let backup_every = std::env::var("AGENTGRID_BACKUP_EVERY_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(86400);
        let backup_keep = std::env::var("AGENTGRID_BACKUP_KEEP")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(5)
            .max(1);
        tokio::spawn(async move {
            // Tick every 15s: node-staleness is 30s, so a 15s cadence still
            // marks a dead node offline within ~45s of its last heartbeat.
            // Run the WAL checkpoint only every 4th tick (~60s): a checkpoint
            // takes the writer briefly (TRUNCATE) and serializes against user
            // BEGIN IMMEDIATE writes — running it every tick caused
            // `database is locked` (SQLITE_BUSY) on retry_task under load.
            let mut tick = 0u32;
            let mut last_backup = std::time::Instant::now();
            loop {
                tokio::time::sleep(Duration::from_secs(15)).await;
                let now = now_iso();
                match revert_expired_leases(&store.pool, &now).await {
                    Ok(n) if n > 0 => {
                        store
                            .lease_reverts
                            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(e) => tracing::warn!("lease maintenance failed: {e}"),
                    _ => {}
                }
                if let Err(e) = mark_offline_nodes(&store.pool, &now).await {
                    tracing::warn!("node maintenance failed: {e}");
                }
                // Approval expiry + scheduled-workflow triggers must fire on
                // the background cadence too; tick_maintenance only runs at
                // startup, so without these scheduled workflows would never
                // fire and approvals would never expire in production.
                if let Err(e) = store.tick_approval_expiry().await {
                    tracing::warn!("approval expiry failed: {e}");
                }
                if let Err(e) = store
                    .tick_workflow_schedules(chrono::Utc::now().timestamp())
                    .await
                {
                    tracing::warn!("workflow schedule tick failed: {e}");
                }
                let _ = store.cleanup_artifacts(168).await;
                tick = tick.wrapping_add(1);
                if tick % 4 == 0 {
                    let _ = store.wal_checkpoint().await;
                }
                // Plan 0.2 item 4.2: automatic backup + rotation. A failed
                // backup is counted and retried on the next interval; the
                // timestamp only advances on success.
                if last_backup.elapsed() >= Duration::from_secs(backup_every) {
                    let ts = chrono::Utc::now().timestamp();
                    let name = format!("auto-backup-{ts}.db");
                    match store.backup_to(&name).await {
                        Ok(()) => {
                            store
                                .last_backup_at
                                .store(ts, std::sync::atomic::Ordering::Relaxed);
                            if let Err(e) = store.rotate_backups(backup_keep).await {
                                tracing::warn!("backup rotation failed: {e}");
                            }
                            tracing::info!(file = %name, "automatic backup complete");
                        }
                        Err(e) => {
                            store
                                .backup_errors
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            tracing::warn!("automatic backup failed: {e}");
                        }
                    }
                    last_backup = std::time::Instant::now();
                }
            }
        });
    }

    /// Plan 0.2 item 4.2: keep only the newest `keep` automatic backup files
    /// (`auto-backup-<unix-ts>.db`) in the data directory; unlink the rest.
    /// Timestamps in the name make lexicographic order == newest first.
    async fn rotate_backups(&self, keep: usize) -> Result<()> {
        let dir = self
            .artifact_root
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        let mut names: Vec<String> = Vec::new();
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Some(ent) = rd.next_entry().await? {
            let name = ent.file_name().to_string_lossy().to_string();
            if name.starts_with("auto-backup-") && name.ends_with(".db") {
                names.push(name);
            }
        }
        names.sort_unstable_by(|a, b| b.cmp(a));
        for old in names.iter().skip(keep) {
            let _ = tokio::fs::remove_file(dir.join(old)).await;
        }
        Ok(())
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

    /// Plan 2.1 (#18): background agent-heartbeat ticker — every interval,
    /// spawn the scheduled task (agent's `prompt`) for each agent whose
    /// heartbeat is due. Budget enforcement runs inside
    /// `create_agent_task` (a `budget_exceeded` trail row is written and the
    /// fire is skipped); the heartbeat timestamp advances regardless so an
    /// exhausted agent stops being retried each tick. Best-effort: one bad
    /// agent does not stall the loop.
    pub fn start_agent_heartbeat_ticker(&self) {
        let store = self.clone();
        let secs = std::env::var("AGENTGRID_AGENT_TICK_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(5);
        tokio::spawn(async move {
            loop {
                let now_unix = chrono::Utc::now().timestamp();
                let due = match store.due_agents(now_unix).await {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!("agent heartbeat ticker listing due agents failed: {e}");
                        Vec::new()
                    }
                };
                for a in due {
                    let req = agentgrid_common::CreateTaskRequest {
                        repository: "*".into(),
                        prompt: a.prompt.clone(),
                        adapter: "mock".into(),
                        requested_node_id: None,
                        timeout_secs: None,
                        base_commit: None,
                        parent_acp_session_id: None,
                        network_mode: None,
                        validation_command: None,
                        security_profile: None,
                        group_id: None,
                        agent_id: Some(a.id.clone()),
                        consensus_group_id: None,
                        consensus_member: None,
                        opencode_override: None,
                    };
                    if let Err(e) = store.create_agent_task(&a.id, &req).await {
                        tracing::warn!("agent {} heartbeat spawn failed: {e}", a.id);
                    }
                    if let Err(e) = store.record_agent_heartbeat(&a.id).await {
                        tracing::warn!("agent {} heartbeat record failed: {e}", a.id);
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
        // Hardening P1 item 22: reconcile the denormalized active_attempts
        // cache against the authoritative attempt rows so a crash mid-assign
        // or a partial write cannot leave a node over/under-counted.
        let drift = self.reconcile_active_attempts().await?;
        if drift > 0 {
            tracing::warn!(drift, "active_attempts cache reconciled on startup");
        }
        // Hardening P1 item 21: detect orphan rows (events/artifacts pointing
        // at missing attempts, attempts pointing at missing tasks). These
        // should be impossible with FKs but currently only logged so the
        // operator notices data-integrity drift early.
        let orphans = self.count_orphan_rows().await?;
        if orphans > 0 {
            tracing::warn!(orphans, "orphan rows detected on startup");
        }
        let _ = self
            .audit(
                "system",
                None,
                "startup_reconcile",
                None,
                Some(&format!(
                    "in_flight={inflight} active_attempts_drift={drift}"
                )),
            )
            .await;
        tracing::info!("startup reconcile complete");
        Ok(())
    }

    /// Recompute each node's `active_attempts` from the attempt table and apply
    /// it where it differs. Returns the number of nodes whose counter changed.
    pub async fn reconcile_active_attempts(&self) -> Result<u64> {
        let res = sqlx::query(
            "UPDATE nodes SET active_attempts = (\
               SELECT COUNT(*) FROM attempts \
               WHERE attempts.node_id = nodes.id \
                 AND attempts.status IN ('assigned','running','validating')) \
             WHERE active_attempts <> (\
               SELECT COUNT(*) FROM attempts \
               WHERE attempts.node_id = nodes.id \
                 AND attempts.status IN ('assigned','running','validating'))",
        )
        .execute(&self.pool)
        .await?;
        // Hardening P2 item 35: surface how many nodes had a drifted counter we
        // repaired (a non-zero value here indicates a scheduler/counter bug).
        let repaired = res.rows_affected();
        if repaired > 0 {
            self.active_attempt_drift
                .fetch_add(repaired, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(repaired)
    }

    /// Count orphan rows: attempts whose task_id no longer exists, events whose
    /// attempt no longer exists, and artifacts whose attempt no longer exists.
    /// Returns the sum; 0 on a healthy DB. (Detection only — no auto-repair.)
    pub async fn count_orphan_rows(&self) -> Result<i64> {
        let attempts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM attempts a LEFT JOIN tasks t ON a.task_id = t.id \
             WHERE t.id IS NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        let events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM task_events e LEFT JOIN attempts a ON e.attempt_id = a.id \
             WHERE a.id IS NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        let artifacts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM artifacts ar LEFT JOIN attempts a ON ar.attempt_id = a.id \
             WHERE a.id IS NULL",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(attempts + events + artifacts)
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

    /// Valid backup target name: a single plain file name (no separators,
    /// not absolute, no NUL). The backup always lands in the data directory.
    pub fn is_valid_backup_name(path: &str) -> bool {
        let p = std::path::Path::new(path);
        !path.is_empty() && !path.contains('\0') && !p.is_absolute() && p.components().count() == 1
    }

    /// Compact copy of the database for backup/restore rehearsal (Stage 2.5 ops).
    /// Only a plain file name is accepted; the backup always lands in the data
    /// directory (parent of the artifact root) so the admin endpoint cannot be
    /// used for arbitrary file writes. `VACUUM INTO` refuses to overwrite an
    /// existing file.
    pub async fn backup_to(&self, path: &str) -> Result<()> {
        if !Self::is_valid_backup_name(path) {
            return Err(anyhow::anyhow!(
                "backup path must be a plain file name, got: {path}"
            ));
        }
        let full = self
            .artifact_root
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(path);
        let stmt = format!(
            "VACUUM INTO '{}'",
            full.to_string_lossy().replace('\'', "''")
        );
        // audited: stmt is code-generated DDL, not user input
        sqlx::query(sqlx::AssertSqlSafe(&stmt))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete artifact metadata older than `retention_hours` (default 168).
    /// Files on disk are left for an operator cleanup job (metadata only here).
    pub async fn cleanup_artifacts(&self, retention_hours: i64) -> Result<u64> {
        let start = std::time::Instant::now();
        let cutoff = iso_plus_secs(-(retention_hours * 3600));
        // Hardening P1 item 15: delete the backing file alongside the metadata
        // row, so retention does not leave orphan files on disk. Collect the
        // (attempt_id, name) pairs first, unlink each, then drop the rows.
        let rows = sqlx::query("SELECT attempt_id, name FROM artifacts WHERE stored_at < ?")
            .bind(&cutoff)
            .fetch_all(&self.pool)
            .await?;
        for r in &rows {
            let attempt_id: String = r.try_get("attempt_id")?;
            let name: String = r.try_get("name")?;
            if let Ok(path) = self.artifact_path(&attempt_id, &name) {
                // Best-effort: a missing file is not an error (already gone).
                // Hardening P2 item 35: tally reclaimed bytes before unlink.
                if let Ok(meta) = tokio::fs::metadata(&path).await {
                    let bytes = meta.len();
                    if bytes > 0 {
                        self.artifact_cleanup_bytes
                            .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
        // Hardening P1 item 15: drop now-empty attempt dirs so the artifact
        // root does not accumulate stale directories. Also sweep orphan
        // `.tmp.upload` crash leftovers per-dir (never counted as artifact).
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for r in &rows {
            let attempt_id: String = r.try_get("attempt_id")?;
            if seen.insert(attempt_id.clone()) {
                let dir = self.artifact_root.join(&attempt_id);
                // Sweep tmp crash leftovers before testing emptiness.
                if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
                    while let Ok(Some(ent)) = rd.next_entry().await {
                        let name = ent.file_name().to_string_lossy().to_string();
                        if name.ends_with(".tmp.upload") || name.ends_with(".tmp") {
                            let _ = tokio::fs::remove_file(ent.path()).await;
                        }
                    }
                }
                let _ = tokio::fs::remove_dir(&dir).await;
            }
        }
        let res = sqlx::query("DELETE FROM artifacts WHERE stored_at < ?")
            .bind(&cutoff)
            .execute(&self.pool)
            .await?;
        let duration_secs = start.elapsed().as_secs();
        self.artifact_cleanup_runs
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.artifact_cleanup_duration_secs
            .fetch_add(duration_secs, std::sync::atomic::Ordering::Relaxed);
        Ok(res.rows_affected())
    }

    /// Hardening P1 item 15: scan the artifact root for drift between the
    /// metadata table and the on-disk tree.
    ///
    /// - **Orphan files** — a file under `<artifact_root>/<attempt_id>/<name>`
    ///   with no `artifacts` row. In `dry_run` mode they are reported only;
    ///   otherwise they are unlinked (their metadata was already deleted by
    ///   retention, so the bytes are unreachable garbage).
    /// - **Metadata without files** — an `artifacts` row whose backing file is
    ///   missing (crash between unlink and row delete, or external cleanup).
    ///   These rows are pruned so the table never points at nothing.
    ///
    /// Returns `(orphan_files, orphan_bytes, metadata_without_file)`.
    #[allow(clippy::type_complexity)]
    pub async fn storage_reconcile(&self, dry_run: bool) -> Result<(u64, u64, u64)> {
        // Load every live artifact path from metadata.
        let rows = sqlx::query("SELECT attempt_id, name FROM artifacts")
            .fetch_all(&self.pool)
            .await?;
        let mut live: std::collections::HashSet<(String, String)> =
            std::collections::HashSet::new();
        for r in &rows {
            let attempt_id: String = r.try_get("attempt_id")?;
            let name: String = r.try_get("name")?;
            live.insert((attempt_id, name));
        }

        // Walk the artifact root, never following symlinks. Entire walk
        // runs on the blocking pool so a large artifact tree does not stall
        // the async executor.
        let root_clone = self.artifact_root.clone();
        let live_for_scan = live.clone();
        let (orphans_scanned, orphan_bytes_scanned, orphan_paths) =
            tokio::task::spawn_blocking(move || {
                let mut orphans = 0u64;
                let mut orphan_bytes = 0u64;
                let mut paths: Vec<std::path::PathBuf> = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&root_clone) {
                    for entry in entries.flatten() {
                        let ft = match entry.file_type() {
                            Ok(ft) => ft,
                            Err(_) => continue,
                        };
                        let attempt_id = entry.file_name().to_string_lossy().to_string();
                        if !ft.is_dir() {
                            continue;
                        }
                        let dir_path = entry.path();
                        if std::fs::symlink_metadata(&dir_path)
                            .map(|m| m.file_type().is_symlink())
                            .unwrap_or(false)
                        {
                            continue;
                        }
                        if let Ok(files) = std::fs::read_dir(&dir_path) {
                            for f in files.flatten() {
                                if !f.file_type().map(|t| t.is_file()).unwrap_or(false) {
                                    continue;
                                }
                                let name = f.file_name().to_string_lossy().to_string();
                                if name.ends_with(".tmp.upload") || name.ends_with(".tmp") {
                                    continue;
                                }
                                let key = (attempt_id.clone(), name.clone());
                                if live_for_scan.contains(&key) {
                                    continue;
                                }
                                orphans += 1;
                                if let Ok(meta) = f.metadata() {
                                    orphan_bytes += meta.len();
                                }
                                paths.push(f.path());
                            }
                        }
                    }
                }
                (orphans, orphan_bytes, paths)
            })
            .await
            .unwrap_or((0, 0, Vec::new()));
        let orphans = orphans_scanned;
        let orphan_bytes = orphan_bytes_scanned;
        let mut metadata_without_file = 0u64;
        if !dry_run {
            for p in orphan_paths {
                let _ = tokio::fs::remove_file(&p).await;
            }
        }

        // Metadata rows whose backing file is missing (async check to avoid
        // blocking the executor on stat calls over many rows).
        for (attempt_id, name) in &live {
            if let Ok(path) = self.artifact_path(attempt_id, name) {
                if tokio::fs::symlink_metadata(&path).await.is_err() {
                    metadata_without_file += 1;
                    if !dry_run {
                        sqlx::query("DELETE FROM artifacts WHERE attempt_id = ? AND name = ?")
                            .bind(attempt_id)
                            .bind(name)
                            .execute(&self.pool)
                            .await?;
                    }
                }
            }
        }

        Ok((orphans, orphan_bytes, metadata_without_file))
    }

    /// Free bytes on the artifact volume (statvfs). Exposed for `ag storage`.
    pub fn artifact_root(&self) -> &std::path::Path {
        &self.artifact_root
    }

    /// Hardening P1 item 15: free bytes on the artifact root's filesystem
    /// (statvfs). Used by the critical-disk watermark to stop new assignments.
    /// Cached briefly: this runs on every try_assign (async hot path) and the
    /// syscall must not stall an executor thread each time.
    pub fn free_bytes(&self) -> u64 {
        if let Ok(cache) = self.free_bytes_cache.lock() {
            if let Some((at, v)) = *cache {
                if at.elapsed() < std::time::Duration::from_secs(5) {
                    return v;
                }
            }
        }
        let v = self.statvfs_free_bytes();
        if let Ok(mut cache) = self.free_bytes_cache.lock() {
            *cache = Some((std::time::Instant::now(), v));
        }
        v
    }

    fn statvfs_free_bytes(&self) -> u64 {
        let path = std::path::Path::new(&self.artifact_root);
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        let cpath = match std::ffi::CString::new(path.to_string_lossy().as_bytes()) {
            Ok(c) => c,
            Err(_) => {
                tracing::warn!("free_bytes: invalid artifact_root path, assuming disk not full");
                return u64::MAX;
            }
        };
        // SAFETY: cpath is a valid NUL-terminated path; the statvfs struct is
        // zeroed and written by the kernel.
        let rc = unsafe { libc::statvfs(cpath.as_ptr(), &mut s) };
        if rc != 0 {
            tracing::warn!("free_bytes: statvfs failed, assuming disk not full");
            return u64::MAX;
        }
        s.f_bavail.saturating_mul(s.f_frsize)
    }

    /// Hardening P1 item 15: total bytes currently stored as artifacts
    /// (sum of `size_bytes` across metadata rows). Used by the artifact
    /// storage quota to refuse uploads past `AGENTGRID_ARTIFACT_QUOTA_MB`.
    pub async fn artifact_storage_bytes(&self) -> Result<u64> {
        let row = sqlx::query("SELECT COALESCE(SUM(size_bytes), 0) AS total FROM artifacts")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<i64, _>("total").unwrap_or(0) as u64)
    }
}
