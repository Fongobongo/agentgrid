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

/// The daemon-wide HTTP client: bounded connect (10 s) + total-request
/// (120 s) timeouts. reqwest defaults to NO timeout, so a connection that
/// accepts but never answers parked the poll loop / heartbeat / event
/// flusher forever and `send_with_retry`'s attempt-count bound never fired.
/// Production paths build through this; tests keep their own plain clients
/// against dummy servers.
pub fn daemon_http_client() -> Result<Client> {
    Ok(Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120))
        .build()?)
}

/// Authed client routed through the proxy pool's current egress proxy, if
/// any pool entry is alive. When the current proxy later fails, callers
/// rebuild via this helper after `pool.mark_dead(url)` (failover).
pub fn authed_client(
    headers: HeaderMap,
    pool: &crate::proxy::ProxyPool,
) -> Result<(Client, Option<String>)> {
    let mut b = Client::builder()
        .default_headers(headers)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(120));
    let via = pool.current();
    if let Some(p) = &via {
        b = b.proxy(reqwest::Proxy::all(p)?);
    }
    Ok((b.build()?, via))
}

/// Transport entry point (plan 0.3 2.3): one-time startup (recovery,
/// heartbeat) runs once regardless of transport, then the node runs the
/// selected channel: long polling, WebSocket, or auto (WS with poll fallback).
pub async fn run_transport(cfg: Config, cred: crate::config::SavedCredential) -> Result<()> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", cred.credential))?,
    );
    let (client, _) = authed_client(headers.clone(), &cfg.proxies)?;
    // Proxy failover hook: rebuild the authed client against the pool's
    // current (next-alive) proxy after a transport failure rotated the pool.
    let mk_client = {
        let pool = cfg.proxies.clone();
        move || {
            authed_client(headers.clone(), &pool)
                .map(|(c, _)| c)
                .unwrap_or_else(|_| Client::new())
        }
    };
    let sem = Arc::new(Semaphore::new(cfg.max_concurrency as usize));

    // Startup recovery: redeliver durable completions, reap orphaned artifact
    // spool entries, and retry staged artifacts from a prior (killed) run.
    // Audit ND-9: this used to be awaited before the first heartbeat, but a
    // slow CP (send_with_retry backs off up to 20 rounds per pending
    // completion) kept the node silent past the 30s offline sweep, flapping
    // it offline during every long recovery. The work is best-effort and its
    // one unsafe interleaving is already guarded (ND-4 redelivers an
    // undelivered completion instead of re-running), so spawn it and let the
    // heartbeat / poll loops start immediately.
    {
        let rcfg = cfg.clone();
        let rclient = mk_client();
        tokio::spawn(async move { recovery::startup_recovery(&rcfg, &rclient).await });
    }

    // Heartbeat loop: publish status/load/capabilities periodically (runs on
    // every transport; the WS channel additionally carries slot heartbeats).
    heartbeat::spawn_heartbeat(
        cfg.clone(),
        client.clone(),
        sem.clone(),
        Some(Arc::new(mk_client.clone())),
    );

    // Proxy health prober: TCP-probes pool entries, revives reachable ones
    // early, keeps unreachable ones quarantined. 0 disables.
    let probe_secs: u64 = std::env::var("AGENTGRID_PROXY_PROBE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    if probe_secs > 0 {
        let pool = std::sync::Arc::clone(&cfg.proxies);
        tokio::spawn(crate::proxy::probe_loop(
            pool,
            std::time::Duration::from_secs(probe_secs),
        ));
    }

    match cfg.transport {
        Transport::Poll => poll_loop_inner(cfg, client, sem, cred.node_id, None, mk_client).await,
        Transport::Ws => crate::ws::ws_loop(cfg, cred, client, sem, mk_client).await,
        Transport::Auto => crate::ws::auto_loop(cfg, cred, client, sem, mk_client).await,
    }
}

/// Long-poll for assignments until cancelled or (if `max_duration` is set)
/// the deadline elapses. The deadline is used by auto transport to bound the
/// poll fallback before retrying the WebSocket channel.
pub async fn poll_loop_inner(
    cfg: Config,
    mut client: Client,
    sem: Arc<Semaphore>,
    node_id: String,
    max_duration: Option<Duration>,
    mk_client: impl Fn() -> Client,
) -> Result<()> {
    let deadline = max_duration.map(|d| tokio::time::Instant::now() + d);
    // Consecutive poll failures back off exponentially (3s, 6s, 12s, 24s cap)
    // so a down/unreachable CP is not hammered by every node every 3s.
    let mut fail_streak: u32 = 0;
    let backoff = |streak: u32| Duration::from_secs(3 * (1 << streak.min(3)));
    // Proxy the client is actually built against. Transport errors mark
    // THIS url dead (not whatever current() happens to return at that
    // instant — the prober may have rotated the pool meanwhile).
    let mut built_against: Option<String> = cfg.proxies.current();

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
                        let wait = backoff(fail_streak);
                        fail_streak = fail_streak.saturating_add(1);
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                };
                fail_streak = 0;
                // CP-managed proxy list (env AGENTGRID_PROXY_URLS wins if set).
                cfg.proxies.update_from_cp(pr.proxy_urls.clone());
                // Rebuild the client whenever the effective proxy changes:
                // first list delivery, failover AND recovery back to a
                // higher-priority revived proxy (prober/TTL revive).
                if let Some(now) = cfg.proxies.current() {
                    if built_against.as_deref() != Some(now.as_str()) {
                        tracing::info!("active egress proxy -> {now}");
                        client = mk_client();
                        built_against = Some(now);
                    }
                }
                // Plan 0.3 1.2: consume the whole batch; legacy CPs only fill
                // `assignment` (N/N-1 compat).
                *cfg.managed_adapter_env.lock().unwrap() = pr.adapter_env.clone();
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
                let wait = backoff(fail_streak);
                fail_streak = fail_streak.saturating_add(1);
                tracing::warn!("poll returned status {}; retrying in {wait:?}", r.status());
                tokio::time::sleep(wait).await;
            }
            Err(e) => {
                let wait = backoff(fail_streak);
                fail_streak = fail_streak.saturating_add(1);
                tracing::warn!("poll failed: {e}; retrying in {wait:?}");
                // Proxy failover: a connect/timeout error may be the egress
                // proxy dying (the CP being down produces the same symptom;
                // marking a healthy-direct pool empty is a no-op). Rotate to
                // the next alive proxy and rebuild the client.
                if e.is_connect() || e.is_timeout() {
                    if let Some(p) = built_against.clone() {
                        tracing::warn!("egress proxy {p} failed; rotating to next");
                        cfg.proxies.mark_dead(&p);
                        client = mk_client();
                        built_against = cfg.proxies.current();
                    }
                }
                tokio::time::sleep(wait).await;
            }
        }
    }
}

/// Attempts currently dispatched by this daemon. The CP's WS reconnect pull
/// can redeliver an assignment whose ack is still in flight (attempt still
/// `assigned` in the store); dispatching it twice would run two attempt
/// runners for one attempt, interleaving their event streams.
static IN_FLIGHT: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

/// Audit X-N2: RAII removal of the IN_FLIGHT entry. The entry used to be
/// dropped only at the normal end of the runner task, so the undelivered-
/// completion redelivery branch (which never spawns a runner) and a panicking
/// runner leaked the id forever — every later legitimate redelivery of that
/// attempt was then silently dropped as a "duplicate" until daemon restart.
struct InFlightGuard(String);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        IN_FLIGHT
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.0);
    }
}

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
    // Audit ND-4: an attempt with a durable completion still undelivered must
    // not re-run. Once run_attempt returned, the in-flight guard below is
    // gone, so a CP that never saw the completion redelivers the assignment
    // and would start a full second runner (fresh worktree, second agent
    // execution, interleaved event streams). Redeliver the recorded
    // completion instead — complete_attempt is idempotent on the CP side.
    let undelivered: std::collections::HashSet<String> = cfg
        .completion_outbox
        .pending()
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.attempt_id)
        .collect();
    for a in batch {
        {
            let dup = !IN_FLIGHT
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(a.attempt_id.clone());
            if dup {
                tracing::warn!(
                    attempt_id = %a.attempt_id,
                    "assignment redelivered while already in flight; dropping duplicate"
                );
                continue;
            }
        }
        if undelivered.contains(&a.attempt_id) {
            tracing::warn!(
                attempt_id = %a.attempt_id,
                "assignment redelivered but its durable completion is still undelivered; \
                 redelivering the completion instead of re-running the agent"
            );
            let cfg2 = cfg.clone();
            let client2 = client.clone();
            let aid = a.attempt_id.clone();
            // The guard releases the slot when the redelivery attempt ends —
            // success or not — so a still-undelivered completion can be
            // redelivered again on the next offer.
            let guard = InFlightGuard(aid.clone());
            tokio::spawn(async move {
                crate::recovery::redeliver_completion(&cfg2, &client2, &aid).await;
                drop(guard);
            });
            continue;
        }
        let permit = match sem.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => sem.clone().acquire_owned().await?,
        };
        let cfg2 = cfg.clone();
        let client2 = client.clone();
        let guard = InFlightGuard(a.attempt_id.clone());
        tokio::spawn(async move {
            if let Err(e) = crate::attempt_runner::run_attempt(cfg2, client2, a).await {
                tracing::error!("attempt error: {e}");
            }
            drop(guard);
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
        post = post.header(agentgrid_common::FENCING_TOKEN_HEADER, fence);
    }
    match send_with_retry(post.body(bytes), 10).await {
        Ok(s) if s.is_success() => {
            let _ = artifact_spool::remove(spool_root, attempt_id, name);
        }
        Ok(s)
            if s == reqwest::StatusCode::CONFLICT
                || s == reqwest::StatusCode::PRECONDITION_FAILED =>
        {
            // Audit ND-8: a fencing rejection is TERMINAL — this writer is
            // stale (the attempt was reverted/reassigned), and no retry can
            // ever succeed. Startup recovery would re-send it with an empty
            // token (guaranteed to fail again for token-bearing attempts)
            // once per restart until the 24h orphan reaper deletes it, with
            // pending_artifacts advertising it the whole time. Drop it now.
            tracing::warn!(
                "artifact {name} for {attempt_id} fenced off by the CP ({s}); dropping staged copy"
            );
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
    agentgrid_common::sha256_hex(bytes)
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
