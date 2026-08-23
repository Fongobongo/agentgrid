//! Startup recovery: redeliver durable completions and retry/reap staged
//! artifacts left by a prior (killed) daemon run.

use std::time::Duration;

use reqwest::Client;

use crate::artifact_spool;
use crate::config::Config;
use crate::polling::{send_with_retry, upload_if_exists};

/// Redeliver one durable completion record by attempt id (no-op when none is
/// pending). Shared by startup recovery and by `dispatch_batch` (audit ND-4):
/// an assignment arriving for an attempt whose completion is still
/// undelivered must redeliver that completion, never re-run the agent —
/// otherwise a CP outage longer than the retry budget (or a non-retryable
/// 4xx) turns the redelivery into a full duplicate execution.
pub async fn redeliver_completion(cfg: &Config, client: &Client, attempt_id: &str) {
    let Some(c) = cfg
        .completion_outbox
        .pending()
        .unwrap_or_default()
        .into_iter()
        .find(|c| c.attempt_id == attempt_id)
    else {
        return;
    };
    let req = c.to_request();
    tracing::info!(attempt_id = %c.attempt_id, "redelivering durable completion");
    let url = format!("{}/v1/node/attempts/{}/complete", cfg.server, c.attempt_id);
    let mut post = client.post(&url).json(&req);
    // Hardening P0 item 8: redeliver with the recorded fencing token so the
    // CP accepts (or 409-rejects) the stale writer just like a fresh send.
    if !c.fencing_token.is_empty() {
        post = post.header(agentgrid_common::FENCING_TOKEN_HEADER, &c.fencing_token);
    }
    match send_with_retry(post, 20).await {
        Ok(s) if s.is_success() => {
            let _ = cfg.completion_outbox.ack(&c.attempt_id);
        }
        Ok(s) => tracing::warn!("completion redelivery got {s} for {}", c.attempt_id),
        Err(e) => tracing::warn!("completion redelivery failed for {}: {e}", c.attempt_id),
    }
}

/// Stage 2.1 + Hardening P1 item 11: run crash-recovery once at daemon startup.
///
/// 1. Redeliver any completion records a prior (killed) run recorded but never
///    got a CP ack for (complete_attempt is idempotent on terminal attempts).
/// 2. Reap orphaned artifact-spool entries from abandoned attempts (default
///    max age 24h; `AGENTGRID_ARTIFACT_ORPHAN_MAX_AGE_HOURS` overrides).
/// 3. Retry artifacts staged by a prior run whose upload never completed.
///
/// All three are best-effort: failures just leave work for the next restart.
pub async fn startup_recovery(cfg: &Config, client: &Client) {
    // Stage 2.1: redeliver any completion records a prior (killed) run recorded
    // but never got a CP ack for. Runs with the node-credentialed client so the
    // /v1/node/attempts/{id}/complete route authenticates. complete_attempt is
    // idempotent on terminal attempts, so this is safe.
    for c in cfg.completion_outbox.pending().unwrap_or_default() {
        redeliver_completion(cfg, client, &c.attempt_id).await;
    }

    // Hardening P1 item 11: recover orphaned artifact spool entries from
    // abandoned attempts (cancelled, expired, or otherwise never completed).
    // Default max age 24 hours; override via AGENTGRID_ARTIFACT_ORPHAN_MAX_AGE_HOURS.
    let orphan_max_age_hours = std::env::var("AGENTGRID_ARTIFACT_ORPHAN_MAX_AGE_HOURS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(24);
    if let Ok(removed) = artifact_spool::recover_orphans(
        &cfg.artifact_spool_root,
        Duration::from_secs(orphan_max_age_hours * 3600),
    ) {
        if removed > 0 {
            tracing::info!(removed, "cleaned up orphaned artifact spool entries");
        }
    }

    // Hardening P1 item 11: retry artifacts staged by a prior (killed) run
    // whose upload never completed. Best-effort and idempotent on the CP
    // (upload keyed by attempt_id+name); a failure here just leaves the file
    // staged for the next restart.
    let spool_root = cfg.artifact_spool_root.clone();
    if let Ok(pending) = artifact_spool::pending(&spool_root) {
        for (attempt_id, name, path) in &pending {
            tracing::info!(
                attempt_id,
                artifact = %name,
                "retrying staged artifact from previous run"
            );
            // The fencing token for the old attempt is no longer known (it
            // lives in the assignment, not the spool); send without a token.
            // The CP's N/N-1 policy accepts a blank token only for attempts
            // with no token yet, so a stale attempt is safely rejected.
            upload_if_exists(
                client,
                &cfg.server,
                attempt_id,
                name,
                path,
                "",
                &spool_root,
                cfg.max_artifact_size,
            )
            .await;
        }
    }
}
