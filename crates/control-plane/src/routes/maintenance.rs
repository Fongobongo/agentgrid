//! Maintenance routes: backup, storage GC, Prometheus metrics.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, Json};

use crate::AppState;

#[derive(serde::Deserialize)]
pub(crate) struct BackupRequest {
    path: String,
}

/// Hardening P1 item 15: `ag storage gc` — reconcile the artifact tree against
/// the metadata table. `dry_run=true` only reports drift
/// `{orphan_files, orphan_bytes, metadata_without_file, free_mb}`; `false`
/// deletes orphan files and prunes dangling metadata rows.
#[derive(serde::Deserialize)]
pub(crate) struct StorageGcRequest {
    #[serde(default)]
    dry_run: bool,
}

pub async fn admin_backup(
    State(state): State<Arc<AppState>>,
    Json(req): Json<BackupRequest>,
) -> StatusCode {
    // Reject non-confined paths as a client error, not a server one.
    if !crate::store::Store::is_valid_backup_name(&req.path) {
        return StatusCode::BAD_REQUEST;
    }
    match state.store.backup_to(&req.path).await {
        Ok(()) => StatusCode::OK,
        Err(e) => {
            tracing::error!("backup failed: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

pub async fn storage_gc_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<StorageGcRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    match state.store.storage_reconcile(req.dry_run).await {
        Ok((orphan_files, orphan_bytes, metadata_without_file)) => {
            let free = state.store.free_bytes() / (1024 * 1024);
            let _ = state
                .store
                .audit(
                    "system",
                    None,
                    "storage.gc",
                    None,
                    Some(
                        &serde_json::json!({
                            "dry_run": req.dry_run,
                            "orphan_files": orphan_files,
                            "orphan_bytes": orphan_bytes,
                            "metadata_without_file": metadata_without_file,
                        })
                        .to_string(),
                    ),
                )
                .await;
            Ok(Json(serde_json::json!({
                "dry_run": req.dry_run,
                "orphan_files": orphan_files,
                "orphan_bytes": orphan_bytes,
                "metadata_without_file": metadata_without_file,
                "free_mb": free,
            })))
        }
        Err(e) => {
            tracing::error!("storage gc failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Escape a label value for the Prometheus exposition format. Node-supplied
/// values (name, sandbox backend, network mode) must not be able to inject
/// fake metrics via quotes/newlines.
fn prom_label(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

pub async fn metrics(State(state): State<Arc<AppState>>) -> (StatusCode, axum::response::Response) {
    use axum::response::IntoResponse;
    let nodes = match state.store.list_nodes(None, None).await {
        Ok(n) => n,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "".into_response()),
    };
    // Audit X-C3: task gauges/counters used to be computed from
    // `list_tasks()` — the oldest 1000 rows only — so once the table grew
    // past the cap the outcome counters froze and running-task alerts went
    // blind to new tasks. Full-table GROUP BY for counts; newest-terminal
    // window for the duration histogram.
    let task_counts = match state.store.task_status_counts().await {
        Ok(t) => t,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "".into_response()),
    };
    let durations = match state.store.recent_terminal_task_seconds(1000).await {
        Ok(d) => d,
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "".into_response()),
    };
    let attempts = state.store.count_attempts().await.unwrap_or(0);

    let mut node_status = std::collections::HashMap::<String, u64>::new();
    for n in &nodes {
        *node_status.entry(format!("{}", n.status)).or_insert(0) += 1;
    }
    let mut task_status = std::collections::HashMap::<String, u64>::new();
    for (st, c) in &task_counts {
        *task_status.entry(st.clone()).or_insert(0) += *c as u64;
    }

    // Task duration histogram + terminal outcome counters (Stage 5.2).
    let mut buckets: [(u64, u64); 5] = [(60, 0), (300, 0), (1800, 0), (3600, 0), (u64::MAX, 0)];
    let mut dur_sum = 0u64;
    let mut dur_count = 0u64;
    for secs in &durations {
        let secs = *secs as u64;
        dur_sum += secs;
        dur_count += 1;
        for b in buckets.iter_mut() {
            if secs <= b.0 {
                b.1 += 1;
            }
        }
    }
    let mut outcome: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for (st, c) in &task_counts {
        if st == "succeeded" || st == "failed" || st == "cancelled" {
            outcome.insert(st.clone(), *c as u64);
        }
    }

    let mut s = String::new();
    s.push_str("# HELP agentgrid_nodes Nodes by status.\n");
    s.push_str("# TYPE agentgrid_nodes gauge\n");
    for (st, c) in &node_status {
        s.push_str(&format!("agentgrid_nodes{{status=\"{st}\"}} {c}\n"));
    }
    s.push_str("# HELP agentgrid_tasks Tasks by status.\n");
    s.push_str("# TYPE agentgrid_tasks gauge\n");
    for (st, c) in &task_status {
        s.push_str(&format!("agentgrid_tasks{{status=\"{st}\"}} {c}\n"));
    }
    s.push_str("# HELP agentgrid_attempts_total Total attempts.\n");
    s.push_str("# TYPE agentgrid_attempts_total counter\n");
    s.push_str(&format!("agentgrid_attempts_total {attempts}\n"));

    s.push_str("# HELP agentgrid_task_duration_seconds Task duration (finished tasks).\n");
    s.push_str("# TYPE agentgrid_task_duration_seconds histogram\n");
    for (le, c) in &buckets {
        let le_s = if *le == u64::MAX {
            "+Inf".to_string()
        } else {
            le.to_string()
        };
        s.push_str(&format!(
            "agentgrid_task_duration_seconds_bucket{{le=\"{le_s}\"}} {c}\n"
        ));
    }
    s.push_str(&format!("agentgrid_task_duration_seconds_sum {dur_sum}\n"));
    s.push_str(&format!(
        "agentgrid_task_duration_seconds_count {dur_count}\n"
    ));

    s.push_str("# HELP agentgrid_tasks_total Terminal task outcomes (cumulative).\n");
    s.push_str("# TYPE agentgrid_tasks_total counter\n");
    for (st, c) in &outcome {
        s.push_str(&format!("agentgrid_tasks_total{{status=\"{st}\"}} {c}\n"));
    }

    s.push_str("# HELP agentgrid_node_free_disk_mb Free disk reported via heartbeat.\n");
    s.push_str("# TYPE agentgrid_node_free_disk_mb gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_free_disk_mb{{node=\"{}\"}} {}\n",
            prom_label(&n.name),
            n.free_disk_mb
        ));
    }
    s.push_str("# HELP agentgrid_node_mem_available_mb Host MemAvailable reported via heartbeat.\n");
    s.push_str("# TYPE agentgrid_node_mem_available_mb gauge\n");
    for n in &nodes {
        if n.mem_available_mb > 0 {
            s.push_str(&format!(
                "agentgrid_node_mem_available_mb{{node=\"{}\"}} {}\n",
                prom_label(&n.name),
                n.mem_available_mb
            ));
        }
    }
    s.push_str("# HELP agentgrid_node_load_avg Load average reported via heartbeat.\n");
    s.push_str("# TYPE agentgrid_node_load_avg gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_load_avg{{node=\"{}\"}} {}\n",
            prom_label(&n.name),
            n.load_avg
        ));
    }

    s.push_str("# HELP agentgrid_sqlite_db_bytes Main database file size in bytes.\n");
    s.push_str("# TYPE agentgrid_sqlite_db_bytes gauge\n");
    let db_path_c = state.db_path.clone();
    let (db_bytes, wal_bytes) = tokio::task::spawn_blocking(move || {
        let db = std::fs::metadata(&db_path_c).map(|m| m.len()).unwrap_or(0);
        let wal = std::fs::metadata(format!("{db_path_c}-wal"))
            .map(|m| m.len())
            .unwrap_or(0);
        (db, wal)
    })
    .await
    .unwrap_or((0, 0));
    s.push_str(&format!("agentgrid_sqlite_db_bytes {db_bytes}\n"));
    s.push_str("# HELP agentgrid_sqlite_wal_bytes WAL file size in bytes.\n");
    s.push_str("# TYPE agentgrid_sqlite_wal_bytes gauge\n");
    s.push_str(&format!("agentgrid_sqlite_wal_bytes {wal_bytes}\n"));

    s.push_str(
        "# HELP agentgrid_scheduler_latency_ms Last scheduler latency: queued→assigned in ms.\n",
    );
    s.push_str("# TYPE agentgrid_scheduler_latency_ms gauge\n");
    s.push_str(&format!(
        "agentgrid_scheduler_latency_ms {}\n",
        state
            .store
            .scheduler_latency_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(
        "# HELP agentgrid_scheduler_assignments_total Total assignments made by the scheduler.\n",
    );
    s.push_str("# TYPE agentgrid_scheduler_assignments_total counter\n");
    s.push_str(&format!(
        "agentgrid_scheduler_assignments_total {}\n",
        state
            .store
            .scheduler_assignments
            .load(std::sync::atomic::Ordering::Relaxed)
    ));

    s.push_str(
        "# HELP agentgrid_sqlite_checkpoint_ms Last wal_checkpoint(TRUNCATE) duration in ms.\n",
    );
    s.push_str("# TYPE agentgrid_sqlite_checkpoint_ms gauge\n");
    s.push_str(&format!(
        "agentgrid_sqlite_checkpoint_ms {}\n",
        state
            .store
            .checkpoint_ms
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(
        "# HELP agentgrid_sqlite_busy_total Cumulative SQLITE_BUSY/locked-class failures.\n",
    );
    s.push_str("# TYPE agentgrid_sqlite_busy_total counter\n");
    s.push_str(&format!(
        "agentgrid_sqlite_busy_total {}\n",
        state
            .store
            .sqlite_busy
            .load(std::sync::atomic::Ordering::Relaxed)
    ));

    // Plan 0.3 stage 0: write-path + poll observability under load.
    let (write_txns, write_lock_failures) = crate::store::write_txn_stats();
    s.push_str(
        "# HELP agentgrid_sqlite_write_txns_total BEGIN IMMEDIATE write transactions begun.\n",
    );
    s.push_str("# TYPE agentgrid_sqlite_write_txns_total counter\n");
    s.push_str(&format!("agentgrid_sqlite_write_txns_total {write_txns}\n"));
    s.push_str(
        "# HELP agentgrid_sqlite_write_lock_failures_total Write-lock acquisitions still busy after busy_timeout.\n",
    );
    s.push_str("# TYPE agentgrid_sqlite_write_lock_failures_total counter\n");
    s.push_str(&format!(
        "agentgrid_sqlite_write_lock_failures_total {write_lock_failures}\n"
    ));

    let (poll_requests, poll_ms) = crate::routes::events::poll_stats();
    s.push_str("# HELP agentgrid_poll_requests_total Node poll requests served.\n");
    s.push_str("# TYPE agentgrid_poll_requests_total counter\n");
    s.push_str(&format!("agentgrid_poll_requests_total {poll_requests}\n"));
    s.push_str("# HELP agentgrid_poll_duration_ms_sum Cumulative poll handler time in ms.\n");
    s.push_str("# TYPE agentgrid_poll_duration_ms_sum counter\n");
    s.push_str(&format!("agentgrid_poll_duration_ms_sum {poll_ms}\n"));

    // Plan 0.3 2.2/2.5: transport mix visibility (a silent fallback of WS
    // nodes to poll must be observable).
    let ws_nodes = state.ws_registry.connection_count().await;
    s.push_str("# HELP agentgrid_node_transport_connections Connected nodes by transport.\n");
    s.push_str("# TYPE agentgrid_node_transport_connections gauge\n");
    s.push_str(&format!(
        "agentgrid_node_transport_connections{{transport=\"ws\"}} {ws_nodes}\n"
    ));
    s.push_str("# HELP agentgrid_ws_assignment_pushes_total Assignment batches pushed over WS.\n");
    s.push_str("# TYPE agentgrid_ws_assignment_pushes_total counter\n");
    s.push_str(&format!(
        "agentgrid_ws_assignment_pushes_total {}\n",
        crate::ws::ws_pushes()
    ));

    let oldest_queued = state.store.oldest_queued_age_secs().await.unwrap_or(None);
    s.push_str("# HELP agentgrid_oldest_queued_task_seconds Age of the oldest queued task.\n");
    s.push_str("# TYPE agentgrid_oldest_queued_task_seconds gauge\n");
    s.push_str(&format!(
        "agentgrid_oldest_queued_task_seconds {}\n",
        oldest_queued
            .map(|a| format!("{a:.3}"))
            .unwrap_or_else(|| "0".into())
    ));

    // Plan 0.2 item 4.2: automatic-backup observability.
    let last_backup_at = state
        .store
        .last_backup_at
        .load(std::sync::atomic::Ordering::Relaxed);
    s.push_str("# HELP agentgrid_last_backup_at_unix Unix timestamp of the last successful automatic backup (0 = never).\n");
    s.push_str("# TYPE agentgrid_last_backup_at_unix gauge\n");
    s.push_str(&format!("agentgrid_last_backup_at_unix {last_backup_at}\n"));
    let backup_age = if last_backup_at > 0 {
        chrono::Utc::now()
            .timestamp()
            .saturating_sub(last_backup_at)
    } else {
        0
    };
    s.push_str("# HELP agentgrid_last_backup_age_seconds Seconds since the last successful automatic backup (0 = never).\n");
    s.push_str("# TYPE agentgrid_last_backup_age_seconds gauge\n");
    s.push_str(&format!("agentgrid_last_backup_age_seconds {backup_age}\n"));
    s.push_str("# HELP agentgrid_backup_errors_total Cumulative failed automatic backups.\n");
    s.push_str("# TYPE agentgrid_backup_errors_total counter\n");
    s.push_str(&format!(
        "agentgrid_backup_errors_total {}\n",
        state
            .store
            .backup_errors
            .load(std::sync::atomic::Ordering::Relaxed)
    ));

    // Hardening P2 item 35: security-observability counters.
    s.push_str(
        "# HELP agentgrid_cross_node_rejects_total Cross-node mutation/read attempts rejected (wrong owner).
",
    );
    s.push_str("# TYPE agentgrid_cross_node_rejects_total counter\n");
    s.push_str(&format!(
        "agentgrid_cross_node_rejects_total {}\n",
        state
            .cross_node_rejects
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(
        "# HELP agentgrid_stale_fencing_tokens_total Mutations rejected for a stale fencing token.
",
    );
    s.push_str("# TYPE agentgrid_stale_fencing_tokens_total counter\n");
    s.push_str(&format!(
        "agentgrid_stale_fencing_tokens_total {}\n",
        state
            .stale_fencing_tokens
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(
        "# HELP agentgrid_event_rejections_total Event batches rejected (terminal attempt / too large / count cap).
",
    );
    s.push_str("# TYPE agentgrid_event_rejections_total counter\n");
    s.push_str(&format!(
        "agentgrid_event_rejections_total {}\n",
        state
            .event_rejections
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    // Hardening P1 item 14: event-sequence gaps a batch introduced (max
    // sequence in the batch exceeded the contiguous prefix). Monotonic across
    // batches; the durable outbox still redrives the missing sequences.
    s.push_str(
        "# HELP agentgrid_event_gaps_total Event-sequence gaps introduced by a batch (max seq > contiguous prefix).",
    );
    s.push_str("\n# TYPE agentgrid_event_gaps_total counter\n");
    s.push_str(&format!(
        "agentgrid_event_gaps_total {}\n",
        state.event_gaps.load(std::sync::atomic::Ordering::Relaxed)
    ));
    // Hardening P2 item 35: lease-expiry reverts (the lease/ACK race path).
    s.push_str(
        "# HELP agentgrid_lease_reverts_total Expired-lease assignments re-queued by the sweep.",
    );
    s.push_str("\n# TYPE agentgrid_lease_reverts_total counter\n");
    s.push_str(&format!(
        "agentgrid_lease_reverts_total {}\n",
        state
            .store
            .lease_reverts
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(
        "# HELP agentgrid_active_attempt_drift_total Drifted active_attempts counters repaired by reconcile.",
    );
    s.push_str("\n# TYPE agentgrid_active_attempt_drift_total counter\n");
    s.push_str(&format!(
        "agentgrid_active_attempt_drift_total {}\n",
        state
            .store
            .active_attempt_drift
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(
        "# HELP agentgrid_artifact_cleanup_bytes_total Cumulative bytes reclaimed by artifact retention.",
    );
    s.push_str("\n# TYPE agentgrid_artifact_cleanup_bytes_total counter\n");
    s.push_str(&format!(
        "agentgrid_artifact_cleanup_bytes_total {}\n",
        state
            .store
            .artifact_cleanup_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str("# HELP agentgrid_artifact_cleanup_runs_total Total artifact cleanup runs.");
    s.push_str("\n# TYPE agentgrid_artifact_cleanup_runs_total counter\n");
    s.push_str(&format!(
        "agentgrid_artifact_cleanup_runs_total {}\n",
        state
            .store
            .artifact_cleanup_runs
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str("# HELP agentgrid_artifact_cleanup_failures_total Total artifact cleanup failures.");
    s.push_str("\n# TYPE agentgrid_artifact_cleanup_failures_total counter\n");
    s.push_str(&format!(
        "agentgrid_artifact_cleanup_failures_total {}\n",
        state
            .store
            .artifact_cleanup_failures
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(
        "# HELP agentgrid_artifact_cleanup_duration_seconds_total Total artifact cleanup duration in seconds.",
    );
    s.push_str("\n# TYPE agentgrid_artifact_cleanup_duration_seconds_total counter\n");
    s.push_str(&format!(
        "agentgrid_artifact_cleanup_duration_seconds_total {}\n",
        state
            .store
            .artifact_cleanup_duration_secs
            .load(std::sync::atomic::Ordering::Relaxed)
    ));

    // Hardening P2 item 35: validation duration histogram + outcomes.
    s.push_str(
        "# HELP agentgrid_validation_duration_ms Validation duration (validating-state window).\n",
    );
    s.push_str("# TYPE agentgrid_validation_duration_ms histogram\n");
    s.push_str(&format!(
        "agentgrid_validation_duration_ms_sum {}\n",
        state
            .store
            .validation_duration_sum
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str(&format!(
        "agentgrid_validation_duration_ms_count {}\n",
        state
            .store
            .validation_duration_count
            .load(std::sync::atomic::Ordering::Relaxed)
    ));
    s.push_str("# HELP agentgrid_validation_outcomes_total Validation outcomes.\n");
    s.push_str("# TYPE agentgrid_validation_outcomes_total counter\n");
    for (k, v) in state
        .store
        .validation_outcomes
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
    {
        s.push_str(&format!(
            "agentgrid_validation_outcomes_total{{outcome=\"{}\"}} {v}\n",
            prom_label(k)
        ));
    }
    s.push_str(
        "# HELP agentgrid_attempts_by_security_profile_total Attempts by security profile.\n",
    );
    s.push_str("# TYPE agentgrid_attempts_by_security_profile_total counter\n");
    for (k, v) in state
        .store
        .security_profile_attempts
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
    {
        s.push_str(&format!(
            "agentgrid_attempts_by_security_profile_total{{profile=\"{}\"}} {v}\n",
            prom_label(k)
        ));
    }

    // Hardening P2 items 10/35: per-node storage & lock gauges from heartbeat.
    s.push_str("# HELP agentgrid_node_outbox_bytes Bytes buffered in the node's durable outbox.\n");
    s.push_str("# TYPE agentgrid_node_outbox_bytes gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_outbox_bytes{{node=\"{}\"}} {}\n",
            prom_label(&n.name),
            n.outbox_bytes
        ));
    }
    s.push_str("# HELP agentgrid_node_outbox_rows Pending outbox rows on the node.\n");
    s.push_str("# TYPE agentgrid_node_outbox_rows gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_outbox_rows{{node=\"{}\"}} {}\n",
            prom_label(&n.name),
            n.outbox_rows
        ));
    }
    s.push_str("# HELP agentgrid_node_outbox_oldest_pending_age_ms Age of the oldest unacked outbox event.\n");
    s.push_str("# TYPE agentgrid_node_outbox_oldest_pending_age_ms gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_outbox_oldest_pending_age_ms{{node=\"{}\"}} {}\n",
            prom_label(&n.name),
            n.outbox_oldest_pending_age_ms
        ));
    }
    s.push_str("# HELP agentgrid_node_outbox_corruption_total Quarantined corrupt outbox records on the node.\n");
    s.push_str("# TYPE agentgrid_node_outbox_corruption_total gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_outbox_corruption_total{{node=\"{}\"}} {}\n",
            prom_label(&n.name),
            n.outbox_corruption_count
        ));
    }
    s.push_str(
        "# HELP agentgrid_node_outbox_completion_rows Pending completion records on the node.\n",
    );
    s.push_str("# TYPE agentgrid_node_outbox_completion_rows gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_outbox_completion_rows{{node=\"{}\"}} {}\n",
            prom_label(&n.name),
            n.outbox_completion_rows
        ));
    }
    s.push_str(
        "# HELP agentgrid_node_artifact_spool_bytes Bytes staged in the node's artifact spool.\n",
    );
    s.push_str("# TYPE agentgrid_node_artifact_spool_bytes gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_artifact_spool_bytes{{node=\"{}\"}} {}\n",
            prom_label(&n.name),
            n.artifact_spool_bytes
        ));
    }
    s.push_str(
        "# HELP agentgrid_node_repo_lock_wait_ms Cumulative repository-lock wait on the node.\n",
    );
    s.push_str("# TYPE agentgrid_node_repo_lock_wait_ms gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_repo_lock_wait_ms{{node=\"{}\"}} {}\n",
            prom_label(&n.name),
            n.repo_lock_wait_ms
        ));
    }
    s.push_str("# HELP agentgrid_node_sandbox_backend Sandbox backend kind per node.\n");
    s.push_str("# TYPE agentgrid_node_sandbox_backend gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_sandbox_backend{{node=\"{}\",backend=\"{}\"}} 1\n",
            prom_label(&n.name),
            prom_label(&n.sandbox_backend)
        ));
    }
    s.push_str("# HELP agentgrid_node_enforced_limits Whether sandbox enforces resource limits.\n");
    s.push_str("# TYPE agentgrid_node_enforced_limits gauge\n");
    for n in &nodes {
        s.push_str(&format!(
            "agentgrid_node_enforced_limits{{node=\"{}\"}} {}\n",
            prom_label(&n.name),
            if n.enforced_limits { 1 } else { 0 }
        ));
    }
    s.push_str("# HELP agentgrid_node_network_mode Network mode per node.\n");
    s.push_str("# TYPE agentgrid_node_network_mode gauge\n");
    for n in &nodes {
        let mode = match n.network_mode.as_str() {
            "none" => 0,
            "restricted" => 1,
            "unrestricted" => 2,
            _ => 0,
        };
        let labels = format!(
            "node=\"{}\",mode=\"{}\"",
            prom_label(&n.name),
            prom_label(&n.network_mode)
        );
        s.push_str(&format!(
            "agentgrid_node_network_mode{{{}}} {}\n",
            labels, mode
        ));
    }

    (
        StatusCode::OK,
        (
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; version=0.0.4",
            )],
            s,
        )
            .into_response(),
    )
}
