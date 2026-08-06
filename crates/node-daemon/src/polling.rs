//! Long-polling loop: startup recovery, heartbeat spawn, assignment dispatch.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use agentgrid_common::{PollRequest, PollResponse};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::{Client, StatusCode};
use tokio::sync::Semaphore;

use crate::artifact_spool;
use crate::config::Config;
use crate::heartbeat;
use crate::recovery;

/// Run the main polling loop: startup recovery, heartbeat spawn, then long-poll
/// for assignments and spawn attempt runners.
pub async fn poll_loop(cfg: Config, cred: crate::config::SavedCredential) -> Result<()> {
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

    // Heartbeat loop: publish status/load/capabilities periodically.
    heartbeat::spawn_heartbeat(cfg.clone(), client.clone(), sem.clone());
    let hb_node_id = cred.node_id.clone();

    loop {
        let poll_req = PollRequest {
            node_id: hb_node_id.clone(),
            name: cfg.node_name.clone(),
            adapters: cfg.adapters.iter().map(|s| s.id.clone()).collect(),
            repositories: cfg.repositories.clone(),
            max_concurrency: cfg.max_concurrency,
            protocol_version: Some(agentgrid_common::NODE_PROTOCOL_VERSION.into()),
        };
        let resp = client
            .post(format!("{}/v1/node/poll", cfg.server))
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
                if let Some(a) = pr.assignment {
                    let permit = match sem.clone().try_acquire_owned() {
                        Ok(p) => p,
                        Err(_) => {
                            // At capacity; the control plane will re-offer on next poll.
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                    };
                    let cfg2 = cfg.clone();
                    let client2 = client.clone();
                    tokio::spawn(async move {
                        if let Err(e) = crate::attempt_runner::run_attempt(cfg2, client2, a).await {
                            tracing::error!("attempt error: {e}");
                        }
                        drop(permit);
                    });
                } else {
                    // No assignment: pace the loop instead of hammering the CP.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
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
