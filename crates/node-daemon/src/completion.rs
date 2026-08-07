//! Attempt completion: reporting, acking, session creation, cancel handling.

use std::time::Duration;

use agentgrid_common::{CancelState, CompleteAttemptRequest, CreateAgentSessionRequest};
use reqwest::Client;

use crate::outbox;
use crate::polling::send_with_retry;

/// Wait for the control plane to request cancellation of this attempt.
pub async fn wait_for_cancel(client: Client, url: String) {
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => {
                if let Ok(cs) = r.json::<CancelState>().await {
                    if cs.cancel_requested {
                        return;
                    }
                }
            }
            _ => {}
        }
    }
}

/// SIGTERM the whole process group, then SIGKILL after a 10s grace period.
pub fn terminate_group(pid: u32) {
    if pid == 0 {
        return;
    }
    unsafe {
        // SAFETY: pid is a valid process-group id from our spawned child; SIGTERM is safe.
        libc::killpg(pid as i32, libc::SIGTERM);
    }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(10)).await;
        unsafe {
            // SAFETY: same process group; SIGKILL after grace period is safe.
            libc::killpg(pid as i32, libc::SIGKILL);
        }
    });
}

#[allow(clippy::too_many_arguments)]
pub async fn report_complete(
    client: &Client,
    server: &str,
    attempt_id: &str,
    exit_code: i32,
    commit_sha: Option<String>,
    error_code: Option<String>,
    acp_session_id: Option<String>,
    plan: Option<String>,
    resolved_base_sha: Option<String>,
    remote_head_at_start: Option<String>,
    remote_head_at_finish: Option<String>,
    provenance: Option<agentgrid_common::ProvenanceRecord>,
    pending_artifacts: Vec<String>,
    completion_outbox: &outbox::CompletionOutbox,
    fence: &str,
) {
    let url = format!("{}/v1/node/attempts/{}/complete", server, attempt_id);
    let req = CompleteAttemptRequest {
        exit_code,
        commit_sha,
        error_code,
        resolved_base_sha,
        remote_head_at_start,
        remote_head_at_finish,
        acp_session_id,
        plan,
        provenance,
        pending_artifacts,
    };
    // Stage 2.1: persist the completion durably so a daemon kill before the CP
    // acks it is redelivered on the next startup (complete_attempt is
    // idempotent on terminal attempts).
    if let Err(e) = completion_outbox.record(attempt_id, &req, fence) {
        tracing::warn!("completion outbox record failed for {attempt_id}: {e}");
    }
    // Completion is terminal and must be delivered; retry transient and 5xx
    // failures with backoff. The durable outbox also covers the daemon-kill gap.
    let mut post = client.post(&url).json(&req);
    if !fence.is_empty() {
        post = post.header("x-agentgrid-fencing-token", fence);
    }
    match send_with_retry(post, 20).await {
        Ok(s) if s.is_success() => {
            if let Err(e) = completion_outbox.ack(attempt_id) {
                tracing::warn!("completion outbox ack failed for {attempt_id}: {e}");
            }
        }
        Ok(s) => tracing::error!("complete report got {s} for {attempt_id}; not retrying"),
        Err(e) => tracing::error!("complete report failed for {attempt_id}: {e}"),
    }
}

/// Explicit assignment acknowledgement (Stage 1.3): tell the control plane the
/// agent actually started so the assignment is not reverted by the ack deadline.
pub async fn ack_attempt(client: &Client, server: &str, attempt_id: &str, fence: &str) {
    let url = format!("{}/v1/node/attempts/{}/ack", server, attempt_id);
    let mut req = client.post(&url);
    if !fence.is_empty() {
        req = req.header("x-agentgrid-fencing-token", fence);
    }
    if let Err(e) = req.send().await {
        tracing::warn!("ack failed for {attempt_id}: {e}");
    }
}

/// Stage 3.2: open an agent session for this attempt (best-effort; a failed
/// CP call only warns, it must not block the attempt).
pub async fn create_agent_session(
    client: &Client,
    server: &str,
    attempt_id: &str,
    adapter: &str,
    fence: &str,
) {
    let url = format!("{}/v1/node/attempts/{}/session", server, attempt_id);
    let body = CreateAgentSessionRequest {
        adapter: adapter.to_string(),
    };
    let mut req = client.post(&url).json(&body);
    if !fence.is_empty() {
        req = req.header("x-agentgrid-fencing-token", fence);
    }
    if let Err(e) = req.send().await {
        tracing::warn!("agent session create failed for {attempt_id}: {e}");
    }
}
