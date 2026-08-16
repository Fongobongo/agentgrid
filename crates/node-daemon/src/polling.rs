//! Long-polling loop: startup recovery, heartbeat spawn, assignment dispatch.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use agentgrid_common::{PollRequest, PollResponse};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::{Client, StatusCode};
use tokio::sync::Semaphore;

use crate::artifact_spool;
use crate::config::{Config, Transport};
use crate::heartbeat;
use crate::recovery;

/// Transport entry point (plan 0.3 2.3): one-time startup (recovery,
/// heartbeat) runs once regardless of transport, then the node runs the
/// selected channel: long polling, WebSocket, or auto (WS with poll fallback).
pub async fn run_transport(cfg: Config, cred: crate::config::SavedCredential) -> Result<()> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", cred.credential))?,
    );
    let client = Client::builder().default_headers(headers).build()?;
    let sem = Arc::new(Semaphore::new(cfg.max_concurrency as usize));

    // Startup recovery: redeliver durable completions, reap orphaned artifact
    // spool entries, and retry staged artifacts from a prior (killed) run.
    recovery::startup_recovery(&cfg, &client).await;

    // Heartbeat loop: publish status/load/capabilities periodically (runs on
    // every transport; the WS channel additionally carries slot heartbeats).
    heartbeat::spawn_heartbeat(cfg.clone(), client.clone(), sem.clone());

    match cfg.transport {
        Transport::Poll => poll_loop_inner(cfg, client, sem, cred.node_id, None).await,
        Transport::Ws => crate::ws::ws_loop(cfg, cred, client, sem).await,
        Transport::Auto => crate::ws::auto_loop(cfg, cred, client, sem).await,
    }
}

/// Long-poll for assignments until cancelled or (if `max_duration` is set)
/// the deadline elapses. The deadline is used by auto transport to bound the
/// poll fallback before retrying the WebSocket channel.
pub async fn poll_loop_inner(
    cfg: Config,
    client: Client,
    sem: Arc<Semaphore>,
    node_id: String,
    max_duration: Option<Duration>,
) -> Result<()> {
    let deadline = max_duration.map(|d| tokio::time::Instant::now() + d);

    loop {
        if let Some(d) = deadline {
            if tokio::time::Instant::now() >= d {
                tracing::info!("poll fallback window elapsed; retrying WebSocket transport");
                return Ok(());
            }
        }
        let poll_req = PollRequest {
            node_id: node_id.clone(),
            name: cfg.node_name.clone(),
            adapters: cfg.adapters.iter().map(|s| s.id.clone()).collect(),
            repositories: cfg.repositories.clone(),
            max_concurrency: cfg.max_concurrency,
            protocol_version: Some(agentgrid_common::NODE_PROTOCOL_VERSION.into()),
        };
        let resp = client
            .post(format!("{}/v1/node/poll", cfg.server))
            .header("x-agentgrid-max-batch", cfg.max_concurrency.to_string())
            .json(&poll_req)
            .send()
            .await;

        match resp {
            Ok(r) if r.status().is_success() => {
                let pr: PollResponse = match r.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("bad poll response: {e}");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                };
                // Plan 0.3 1.2: consume the whole batch; legacy CPs only fill
                // `assignment` (N/N-1 compat).
                let mut batch = pr.assignments;
                if batch.is_empty() {
                    if let Some(a) = pr.assignment {
                        batch.push(a);
                    }
                }
                if batch.is_empty() {
                    // No assignment: pace the loop instead of hammering the CP.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                dispatch_batch(&cfg, &client, &sem, batch).await?;
            }
            Ok(r) => {
                tracing::warn!("poll returned status {}", r.status());
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Err(e) => {
                tracing::warn!("poll failed: {e}; retrying in 3s");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

/// Attempts currently dispatched by this daemon. The CP's WS reconnect pull
/// can redeliver an assignment whose ack is still in flight (attempt still
/// `assigned` in the store); dispatching it twice would run two attempt
/// runners for one attempt, interleaving their event streams.
static IN_FLIGHT: std::sync::LazyLock<tokio::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(std::collections::HashSet::new()));

/// Dispatch a batch of assignments: one task per attempt, gated by the
/// concurrency semaphore. Shared by the poll and WS transports (plan 0.3 2.3);
/// waits (never drops) if a slot is briefly busy, so no assignment is lost.
/// Duplicate attempt ids (redelivery) are dropped — delivery is at-least-once,
/// dispatch is idempotent.
pub async fn dispatch_batch(
    cfg: &Config,
    client: &Client,
    sem: &Arc<Semaphore>,
    batch: Vec<agentgrid_common::Assignment>,
) -> Result<()> {
    for a in batch {
        {
            let mut in_flight = IN_FLIGHT.lock().await;
            if !in_flight.insert(a.attempt_id.clone()) {
                tracing::warn!(
                    attempt_id = %a.attempt_id,
                    "assignment redelivered while already in flight; dropping duplicate"
                );
                continue;
            }
        }
        let permit = match sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => sem.clone().acquire_owned().await?,
        };
        let cfg2 = cfg.clone();
        let client2 = client.clone();
        tokio::spawn(async move {
            let attempt_id = a.attempt_id.clone();
            if let Err(e) = crate::attempt_runner::run_attempt(cfg2, client2, a).await {
                tracing::error!("attempt error: {e}");
            }
            IN_FLIGHT.lock().await.remove(&attempt_id);
            drop(permit);
        });
    }
    Ok(())
}

/// Stage an artifact and upload it (idempotent; re-stages replace). On success
/// the staged copy is removed; on failure the staged file stays for the next
/// daemon startup retry (Hardening P1 item 11).
#[allow(clippy::too_many_arguments)]
pub async fn upload_if_exists(
    client: &reqwest::Client,
    server: &str,
    attempt_id: &str,
    name: &str,
    path: &std::path::Path,
    fence: &str,
    spool_root: &std::path::Path,
    max_artifact_size: u64,
) {
    // Stage into the durable spool first (idempotent; re-stages replace).
    let staged = match artifact_spool::stage(spool_root, attempt_id, name, path) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("artifact {name} stage failed for {attempt_id}: {e}");
            return;
        }
    };
    // Check artifact size before upload (Hardening P1 item 11: limit size).
    let metadata = match tokio::fs::metadata(&staged).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("artifact {name} metadata failed for {attempt_id}: {e}");
            return;
        }
    };
    if metadata.len() > max_artifact_size {
        tracing::warn!(
            "artifact {name} for {attempt_id} exceeds max size ({} > {} bytes); skipping",
            metadata.len(),
            max_artifact_size
        );
        let _ = artifact_spool::remove(spool_root, attempt_id, name);
        return;
    }
    // Upload from the spool copy so a mid-upload daemon kill leaves the staged
    // file intact for the startup retry.
    let bytes = match tokio::fs::read(&staged).await {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("artifact {name} spool read failed for {attempt_id}: {e}");
            return;
        }
    };
    let media = artifact_media_type(name);
    // Hardening P1 item 11: send the server-computed hash so the CP can verify
    // the artifact arrived intact (client hash mismatch → 422).
    let sha = sha256_hex_bytes(&bytes);
    let mut post = client
        .post(format!(
            "{server}/v1/node/attempts/{attempt_id}/artifacts/raw"
        ))
        .header("x-artifact-name", name)
        .header("x-artifact-media-type", media)
        .header("x-artifact-sha256", sha.as_str());
    if !fence.is_empty() {
        post = post.header("x-agentgrid-fencing-token", fence);
    }
    match send_with_retry(post.body(bytes), 10).await {
        Ok(s) if s.is_success() => {
            let _ = artifact_spool::remove(spool_root, attempt_id, name);
        }
        Ok(s) => {
            tracing::warn!("artifact {name} upload got {s} for {attempt_id} (staged for retry)")
        }
        Err(e) => {
            tracing::warn!("artifact {name} upload failed: {e} (staged for retry)");
        }
    }
}

/// Best-effort content type for a known artifact name; unknown names fall back
/// to a binary-safe `application/octet-stream`.
fn artifact_media_type(name: &str) -> &'static str {
    match name {
        "changes.patch" => "text/x-diff",
        "validation.log" | "agent-raw-output.log" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Hex SHA-256 of a byte slice (hardening P1 item 11: artifact upload
/// verification header).
fn sha256_hex_bytes(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    let out = h.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether an HTTP status from the control plane is worth retrying from the
/// node: transient server errors and rate limiting. Client errors (4xx) are
/// not retried (Stage 2.1).
fn is_retryable_status(s: StatusCode) -> bool {
    s.is_server_error() || s == StatusCode::TOO_MANY_REQUESTS
}

/// Send a request, retrying on transport errors and retryable HTTP statuses
/// with exponential backoff (capped at 5s). Returns the final status, or the
/// last transport error. Bounded by `max_attempts` so a permanently
/// unavailable control plane cannot block the daemon forever (Stage 2.1).
pub async fn send_with_retry(
    builder: reqwest::RequestBuilder,
    max_attempts: usize,
) -> Result<StatusCode, reqwest::Error> {
    let mut backoff = Duration::from_millis(200);
    let mut attempt = 0;
    loop {
        attempt += 1;
        let send = match builder.try_clone() {
            Some(b) => b,
            None => return builder.send().await.map(|r| r.status()),
        };
        match send.send().await {
            Ok(r) => {
                let s = r.status();
                if attempt < max_attempts && is_retryable_status(s) {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                    continue;
                }
                return Ok(s);
            }
            Err(e) => {
                if attempt < max_attempts && (e.is_connect() || e.is_timeout() || e.is_request()) {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(5));
                    continue;
                }
                return Err(e);
            }
        }
    }
}
