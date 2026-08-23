//! Attempt completion: reporting, acking, session creation, cancel handling.

use std::time::Duration;

use agentgrid_common::{CancelState, CompleteAttemptRequest, CreateAgentSessionRequest};
use reqwest::Client;
use tokio::sync::Notify;

use crate::outbox;
use crate::polling::send_with_retry;

/// Cancel notifiers for in-flight attempts: the WS transport signals a
/// `Cancel` message here so supervisors wake instantly instead of waiting for
/// the next 1s HTTP probe (plan 0.3 2.3 / ADR 0009).
static CANCEL_NOTIFIERS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<Notify>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// RAII guard: registers a cancel notifier for `attempt_id` on creation and
/// removes it on drop. Hold for the lifetime of the attempt.
pub struct CancelGuard {
    attempt_id: String,
}

impl CancelGuard {
    pub fn new(attempt_id: &str) -> Self {
        let n = std::sync::Arc::new(Notify::new());
        CANCEL_NOTIFIERS
            .lock()
            .unwrap()
            .insert(attempt_id.to_string(), n);
        Self {
            attempt_id: attempt_id.to_string(),
        }
    }
}

impl Drop for CancelGuard {
    fn drop(&mut self) {
        CANCEL_NOTIFIERS.lock().unwrap().remove(&self.attempt_id);
    }
}

/// Trigger the registered notifier for `attempt_id` (WS Cancel message).
pub async fn notify_cancel(attempt_id: &str) {
    if let Some(n) = CANCEL_NOTIFIERS.lock().unwrap().get(attempt_id) {
        n.notify_waiters();
    }
}

/// Wait for the control plane to request cancellation of this attempt. Wakes
/// early if the WS channel pushes a `Cancel` (notifier), otherwise falls back
/// to the 1s HTTP probe loop (poll transport).
pub async fn wait_for_cancel(attempt_id: &str, client: Client, url: String) {
    let notifier = CANCEL_NOTIFIERS.lock().unwrap().get(attempt_id).cloned();
    // Build the notified() future up front so a Cancel that lands between
    // attempt start and the first HTTP probe is not lost.
    let notified_fut = async {
        match notifier.as_ref() {
            Some(n) => n.notified().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(notified_fut);
    loop {
        tokio::select! {
            _ = &mut notified_fut => return,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
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
        post = post.header(agentgrid_common::FENCING_TOKEN_HEADER, fence);
    }
    match send_with_retry(post, 20).await {
        Ok(s) if s.is_success() => {
            if let Err(e) = completion_outbox.ack(attempt_id) {
                tracing::warn!("completion outbox ack failed for {attempt_id}: {e}");
            }
        }
        // Audit X-B12: a definitive rejection can never succeed on redelivery
        // (fenced-off writer, attempt reaped/cancelled, malformed payload).
        // The record used to stay durable and be re-sent with a doomed token
        // once per restart. Mirror the artifact policy: drop it now.
        Ok(s) if matches!(s.as_u16(), 400 | 401 | 404 | 409 | 412 | 413 | 422) => {
            tracing::error!(
                "complete report got {s} for {attempt_id}; dropping durable record \
                 (definitive rejection)"
            );
            let _ = completion_outbox.ack(attempt_id);
        }
        Ok(s) => tracing::error!("complete report got {s} for {attempt_id}; not retrying"),
        Err(e) => tracing::error!("complete report failed for {attempt_id}: {e}"),
    }
}

/// Audit X-D4: shared bounded-wait core for supervised subprocesses (adapter
/// runs and validation). The three-way select was duplicated verbatim; only
/// the post-exit handling differs. On `TimedOut`/`Cancelled` the caller owns
/// group-kill + container cleanup, then reaps with a final `child.wait()`.
pub enum BoundedExit {
    Exited(i32),
    TimedOut,
    Cancelled,
}

pub async fn wait_bounded(
    child: &mut tokio::process::Child,
    timeout: std::time::Duration,
    attempt_id: &str,
    cancel_client: reqwest::Client,
    cancel_url: String,
) -> anyhow::Result<BoundedExit> {
    tokio::select! {
        status = child.wait() => {
            let code = status?.code().unwrap_or(-1);
            Ok(BoundedExit::Exited(code))
        }
        _ = tokio::time::sleep(timeout) => Ok(BoundedExit::TimedOut),
        _ = wait_for_cancel(attempt_id, cancel_client, cancel_url) => Ok(BoundedExit::Cancelled),
    }
}

/// Outcome of the assignment ack. `Rejected` means the CP definitively
/// refused the lease (404/409 — reverted by the ack-deadline reaper,
/// cancelled, or fenced off): the assignment must be dropped, never run,
/// or the stale holder would execute the task alongside the new one.
/// `Unreachable` keeps the historical best-effort semantics (network error
/// / 5xx): the ack is lost, the runner proceeds, and the lease race is
/// settled by the reaper plus the redelivery in-flight guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    Accepted,
    Rejected,
    Unreachable,
}

/// Explicit assignment acknowledgement (Stage 1.3): tell the control plane the
/// agent actually started so the assignment is not reverted by the ack deadline.
pub async fn ack_attempt(
    client: &Client,
    server: &str,
    attempt_id: &str,
    fence: &str,
) -> AckOutcome {
    let url = format!("{}/v1/node/attempts/{}/ack", server, attempt_id);
    let mut req = client.post(&url);
    if !fence.is_empty() {
        req = req.header(agentgrid_common::FENCING_TOKEN_HEADER, fence);
    }
    match req.send().await {
        Err(e) => {
            tracing::warn!("ack failed for {attempt_id}: {e}");
            AckOutcome::Unreachable
        }
        Ok(resp) if resp.status().is_success() => AckOutcome::Accepted,
        Ok(resp) => {
            let status = resp.status();
            tracing::warn!("ack rejected for {attempt_id}: HTTP {status}");
            if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::CONFLICT {
                AckOutcome::Rejected
            } else {
                AckOutcome::Unreachable
            }
        }
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
        req = req.header(agentgrid_common::FENCING_TOKEN_HEADER, fence);
    }
    if let Err(e) = req.send().await {
        tracing::warn!("agent session create failed for {attempt_id}: {e}");
    }
}
