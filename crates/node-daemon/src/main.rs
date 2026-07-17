//! Node daemon: long-polls the control plane, runs the adapter as a separate
//! process group in a per-attempt worktree, streams stdout/stderr as events,
//! and reports completion. Stage-1 version: in-memory, mock adapter only.

use std::collections::VecDeque;
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentgrid_adapters::{to_event_type, AdapterEvent};
use agentgrid_common::{
    Assignment, CancelState, CompleteAttemptRequest, EnrollRequest, EnrollResponse, EventType,
    HeartbeatRequest, IncomingEvent, IngestEventsRequest, NodeStatus, PollRequest, PollResponse,
    UploadArtifactRequest,
};
use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, Notify, Semaphore};

mod git;

#[derive(Clone)]
struct Config {
    server: String,
    node_name: String,
    workspace_root: PathBuf,
    max_concurrency: u32,
    adapter: String,
    agent_version: String,
    adapters: Vec<String>,
    repositories: Vec<String>,
    heartbeat_secs: u64,
    enroll_token: Option<String>,
    credential_path: PathBuf,
    repository_root: PathBuf,
    /// Substrings masked to `***` in streamed logs (Stage 3.4).
    secrets: Vec<String>,
    /// Extra env vars forwarded to the adapter subprocess (e.g. API keys).
    adapter_env: Vec<(String, String)>,
}

/// Node identity persisted to disk after enrollment (never re-sent in plaintext).
#[derive(Serialize, Deserialize)]
struct SavedCredential {
    node_id: String,
    credential: String,
}

fn split_csv(env: &str, default: &str) -> Vec<String> {
    std::env::var(env)
        .ok()
        .and_then(|v| {
            let items: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if items.is_empty() {
                None
            } else {
                Some(items)
            }
        })
        .unwrap_or_else(|| vec![default.to_string()])
}

fn config_from_env() -> Config {
    let data_dir =
        std::env::var("AGENTGRID_DATA_DIR").unwrap_or_else(|_| "./agentgrid-data".into());
    Config {
        server: std::env::var("AGENTGRID_SERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:7800".into()),
        node_name: std::env::var("AGENTGRID_NODE_NAME")
            .unwrap_or_else(|_| hostname().unwrap_or_else(|| "node".into())),
        workspace_root: PathBuf::from(
            std::env::var("AGENTGRID_WORKSPACE_ROOT")
                .unwrap_or_else(|_| "./agentgrid-workspace".into()),
        ),
        max_concurrency: std::env::var("AGENTGRID_MAX_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2),
        adapter: std::env::var("AGENTGRID_ADAPTER").unwrap_or_else(|_| "adapter-mock".into()),
        agent_version: std::env::var("AGENTGRID_AGENT_VERSION")
            .unwrap_or_else(|_| "0.1.0-dev".into()),
        adapters: split_csv("AGENTGRID_ADAPTERS", "mock"),
        repositories: split_csv("AGENTGRID_REPOSITORIES", "*"),
        heartbeat_secs: std::env::var("AGENTGRID_HEARTBEAT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10),
        enroll_token: std::env::var("AGENTGRID_ENROLL_TOKEN").ok(),
        credential_path: PathBuf::from(data_dir).join("credential.json"),
        repository_root: PathBuf::from(
            std::env::var("AGENTGRID_REPOSITORY_ROOT")
                .unwrap_or_else(|_| "./agentgrid-repos".into()),
        ),
        secrets: split_csv("AGENTGRID_SECRETS", ""),
        adapter_env: parse_env_pairs("AGENTGRID_ADAPTER_ENV"),
    }
}

/// Parse `KEY=VALUE` pairs from an env var (space/newline/comma separated).
/// Used to forward secrets/API keys to the adapter subprocess (Stage 3.1).
fn parse_env_pairs(env: &str) -> Vec<(String, String)> {
    std::env::var(env)
        .ok()
        .map(|v| {
            v.split([' ', ',', '\n'])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .filter_map(|s| {
                    let (k, val) = s.split_once('=')?;
                    Some((k.trim().to_string(), val.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Stage 3.1 capability discovery: resolve the adapter binary in `PATH` and
/// capture its `--version` (best-effort). A missing binary means the node
/// should report `degraded` so the scheduler excludes it.
struct AdapterProbe {
    found: bool,
    version: Option<String>,
}

async fn probe_adapter(bin: &str) -> AdapterProbe {
    let which = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v '{bin}'"))
        .output()
        .await;
    let found = match which {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout);
            o.status.success() && !out.trim().is_empty()
        }
        Err(_) => false,
    };
    if !found {
        return AdapterProbe {
            found: false,
            version: None,
        };
    }
    let ver = tokio::process::Command::new(bin)
        .arg("--version")
        .output()
        .await;
    let version = ver.ok().filter(|o| o.status.success()).map(|o| {
        String::from_utf8_lossy(&o.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string()
    });
    AdapterProbe {
        found: true,
        version,
    }
}

/// Shared, bounded-ish event buffer that flushes to the control plane in
/// batches (every 200ms or when 50 events accumulate).
struct EventSink {
    buf: Mutex<VecDeque<IncomingEvent>>,
    next: AtomicU64,
    notify: Notify,
    // Counts events that came from the adapter's stdout/stderr (excludes the
    // daemon's own synthetic "attempt started" event). Used to warn on a
    // silent agent that exits 0 but produced no output.
    adapter_events: AtomicU64,
    attempt_id: String,
    client: reqwest::Client,
    server: String,
}

impl EventSink {
    fn new(attempt_id: String, client: reqwest::Client, server: String) -> Arc<Self> {
        Arc::new(Self {
            buf: Mutex::new(VecDeque::new()),
            next: AtomicU64::new(1),
            notify: Notify::new(),
            adapter_events: AtomicU64::new(0),
            attempt_id,
            client,
            server,
        })
    }

    /// Record that an event originated from the adapter output (not the
    /// daemon's own synthetic events).
    fn note_adapter_event(&self) {
        self.adapter_events.fetch_add(1, Ordering::SeqCst);
    }

    fn adapter_event_count(&self) -> u64 {
        self.adapter_events.load(Ordering::SeqCst)
    }

    async fn push(&self, ty: EventType, payload: serde_json::Value) {
        let seq = self.next.fetch_add(1, Ordering::SeqCst);
        self.buf.lock().await.push_back(IncomingEvent {
            sequence: seq,
            r#type: ty,
            payload,
        });
        if self.buf.lock().await.len() >= 50 {
            self.notify.notify_one();
        }
    }

    async fn flush(&self) {
        let batch: Vec<IncomingEvent> = std::mem::take(&mut *self.buf.lock().await)
            .into_iter()
            .collect();
        if batch.is_empty() {
            return;
        }
        let url = format!(
            "{}/v1/node/attempts/{}/events",
            self.server, self.attempt_id
        );
        let req = IngestEventsRequest { events: batch };
        if let Err(e) = self.client.post(&url).json(&req).send().await {
            tracing::warn!("event flush failed: {e}");
        }
    }

    async fn run_flusher(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = self.notify.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
            self.flush().await;
        }
    }
}

async fn read_stream<R: AsyncRead + Unpin>(
    reader: R,
    sink: Arc<EventSink>,
    stream: &str,
    secrets: Vec<String>,
    raw: Option<Arc<Mutex<tokio::fs::File>>>,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let masked = mask_secrets(&line, &secrets);
        if let Some(f) = &raw {
            let mut g = f.lock().await;
            let _ = g.write_all(masked.as_bytes()).await;
            let _ = g.write_all(b"\n").await;
        }
        match serde_json::from_str::<AdapterEvent>(&masked) {
            Ok(ae) => {
                sink.push(to_event_type(&ae.r#type), ae.payload).await;
                sink.note_adapter_event();
            }
            Err(_) => {
                let ty = if stream == "stderr" {
                    EventType::Stderr
                } else {
                    EventType::Stdout
                };
                sink.push(ty, json!({ "text": line })).await;
                sink.note_adapter_event();
            }
        }
    }
}

async fn run_attempt(cfg: Config, client: reqwest::Client, assignment: Assignment) -> Result<()> {
    let repo_root = cfg.repository_root.clone();
    let ws_root = cfg.workspace_root.clone();
    let prep_assignment = assignment.clone();
    let ws = tokio::task::spawn_blocking(move || {
        git::prepare_workspace(&repo_root, &ws_root, &prep_assignment)
    })
    .await??;
    tracing::info!(attempt_id = %assignment.attempt_id, git = ws.is_git, "starting attempt");

    // Raw adapter output is mirrored to disk as a safety net against CLI
    // output-format changes (Stage 3.1): the structured events may be lossy,
    // but the raw log is always preserved as an artifact.
    let raw_path = ws.path.join("agent-raw-output.log");
    let raw_file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&raw_path)
        .await
        .ok()
        .map(|f| Arc::new(Mutex::new(f)));

    let mut cmd = tokio::process::Command::new(&cfg.adapter);
    cmd.arg("--prompt").arg(&assignment.prompt);
    cmd.current_dir(&ws.path);
    cmd.env("AGENTGRID_ATTEMPT_ID", &assignment.attempt_id);
    // Forward configured env (e.g. API keys) to the adapter subprocess (Stage 3.1).
    for (k, v) in &cfg.adapter_env {
        cmd.env(k, v);
    }
    // Separate process group so a cancel can SIGTERM the whole tree (Stage 2.7).
    cmd.process_group(0);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to spawn adapter {}: {e}", cfg.adapter);
            report_complete(
                &client,
                &cfg.server,
                &assignment.attempt_id,
                127,
                None,
                None,
            )
            .await;
            return Ok(());
        }
    };

    let pid = child.id().unwrap_or(0);
    let cancel_url = format!(
        "{}/v1/node/attempts/{}/cancel",
        cfg.server, assignment.attempt_id
    );
    let cancel_client = client.clone();
    let timeout = Duration::from_secs(assignment.timeout_secs.max(1));

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let sink = EventSink::new(
        assignment.attempt_id.clone(),
        client.clone(),
        cfg.server.clone(),
    );
    // Confirm liveness at once so the assignment lease (store.rs ASSIGNMENT_LEASE_SECS)
    // cannot expire before a slow agent emits its first event. Without this a silent
    // agent that starts but takes >lease seconds to produce output loses the
    // assignment and the task is reassigned (double-attempt). The first ingested
    // event flips the attempt to 'running', after which the lease revert is a no-op.
    sink.push(EventType::Metric, json!({ "text": "attempt started" }))
        .await;
    let flusher = tokio::spawn(sink.clone().run_flusher());

    let r1 = tokio::spawn(read_stream(
        stdout,
        sink.clone(),
        "stdout",
        cfg.secrets.clone(),
        raw_file.clone(),
    ));
    let r2 = tokio::spawn(read_stream(
        stderr,
        sink.clone(),
        "stderr",
        cfg.secrets.clone(),
        raw_file.clone(),
    ));

    enum Outcome {
        Exited(i32),
        Timeout,
        Cancel,
    }
    let outcome = tokio::select! {
        status = child.wait() => Outcome::Exited(status?.code().unwrap_or(-1)),
        _ = tokio::time::sleep(timeout) => Outcome::Timeout,
        _ = wait_for_cancel(cancel_client, cancel_url) => Outcome::Cancel,
    };
    let (code, kill_reason) = match outcome {
        Outcome::Exited(c) => (c, None),
        Outcome::Timeout => {
            terminate_group(pid);
            let status = child.wait().await?;
            (status.code().unwrap_or(-1), Some("timeout"))
        }
        Outcome::Cancel => {
            terminate_group(pid);
            let status = child.wait().await?;
            (status.code().unwrap_or(-1), None)
        }
    };
    let _ = r1.await;
    let _ = r2.await;
    sink.flush().await;
    // A silent agent that exits 0 without producing any output yields a task
    // that looks "succeeded" but is empty (e.g. opencode emitted nothing for a
    // run). Surface it so ops can notice the missing output.
    if code == 0 && sink.adapter_event_count() == 0 {
        tracing::warn!(
            attempt_id = %assignment.attempt_id,
            "adapter exited 0 but produced no stdout/stderr events; task output may be empty (silent agent?)"
        );
    }
    flusher.abort();

    let node_name = cfg.node_name.clone();
    let workdir = ws.path.clone();
    let patch_path = workdir.join("changes.patch");
    let validation_log = workdir.join("validation.log");
    let commit_sha =
        tokio::task::spawn_blocking(move || git::finalize_workspace(ws, node_name.as_str()))
            .await??;

    // Validation runs only when the agent itself succeeded (Stage 3.3); the
    // diff is already committed so it survives a validation failure.
    let mut error_code: Option<String> = if code == 0 {
        None
    } else {
        // A killed attempt reports why: timeout is distinct from a generic
        // agent failure (so dashboards/queries can tell them apart).
        Some(kill_reason.unwrap_or("agent_failed").into())
    };
    if code == 0 {
        if let Some(cmd) = &assignment.validation_command {
            match run_validation(&workdir, cmd, &sink).await {
                Ok(vcode) if vcode != 0 => error_code = Some("validation_failed".into()),
                Err(e) => {
                    tracing::error!("validation failed to run: {e}");
                    error_code = Some("validation_failed".into());
                }
                _ => {}
            }
        }
    }

    // Upload produced artifacts (changes.patch for git tasks; validation.log;
    // raw adapter output as a format-change safety net, Stage 3.1).
    upload_if_exists(
        &client,
        &cfg.server,
        &assignment.attempt_id,
        "changes.patch",
        &patch_path,
    )
    .await;
    upload_if_exists(
        &client,
        &cfg.server,
        &assignment.attempt_id,
        "validation.log",
        &validation_log,
    )
    .await;
    upload_if_exists(
        &client,
        &cfg.server,
        &assignment.attempt_id,
        "agent-raw-output.log",
        &raw_path,
    )
    .await;

    tracing::info!(attempt_id = %assignment.attempt_id, exit_code = code, "attempt finished");
    report_complete(
        &client,
        &cfg.server,
        &assignment.attempt_id,
        code,
        commit_sha,
        error_code,
    )
    .await;
    Ok(())
}

/// Run the post-agent validation command in the worktree, streaming its output
/// as events and writing `validation.log`. Returns the command exit code.
async fn run_validation(workdir: &std::path::Path, command: &str, sink: &EventSink) -> Result<i32> {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!("{command} 2>&1"))
        .current_dir(workdir)
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let mut log = String::new();
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await? {
        sink.push(EventType::Stdout, json!({ "text": line })).await;
        log.push_str(&line);
        log.push('\n');
    }
    let status = child.wait().await?;
    let code = status.code().unwrap_or(-1);
    tokio::fs::write(workdir.join("validation.log"), &log).await?;
    Ok(code)
}

/// Upload a local file as an artifact if it exists (idempotent per name).
async fn upload_if_exists(
    client: &reqwest::Client,
    server: &str,
    attempt_id: &str,
    name: &str,
    path: &std::path::Path,
) {
    if let Ok(content) = tokio::fs::read_to_string(path).await {
        let req = UploadArtifactRequest {
            name: name.to_string(),
            content,
        };
        if let Err(e) = client
            .post(format!("{server}/v1/node/attempts/{attempt_id}/artifacts"))
            .json(&req)
            .send()
            .await
        {
            tracing::warn!("artifact {name} upload failed: {e}");
        }
    }
}

/// Poll the control plane until cancellation is requested for this attempt.
/// Replace any known secret substring with `***` (Stage 3.4).
fn mask_secrets(line: &str, secrets: &Vec<String>) -> String {
    let mut s = line.to_string();
    for sec in secrets {
        if !sec.is_empty() {
            s = s.replace(sec, "***");
        }
    }
    s
}

async fn wait_for_cancel(client: reqwest::Client, url: String) {
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
fn terminate_group(pid: u32) {
    if pid == 0 {
        return;
    }
    unsafe {
        // SAFETY: pid is a valid process-group id from our spawned child; SIGTERM is safe.
        libc::killpg(pid as i32, libc::SIGTERM);
    }
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(10));
        unsafe {
            // SAFETY: same process group; SIGKILL after grace period is safe.
            libc::killpg(pid as i32, libc::SIGKILL);
        }
    });
}

async fn report_complete(
    client: &reqwest::Client,
    server: &str,
    attempt_id: &str,
    exit_code: i32,
    commit_sha: Option<String>,
    error_code: Option<String>,
) {
    let url = format!("{}/v1/node/attempts/{}/complete", server, attempt_id);
    let req = CompleteAttemptRequest {
        exit_code,
        commit_sha,
        error_code,
    };
    if let Err(e) = client.post(&url).json(&req).send().await {
        tracing::warn!("complete report failed for {attempt_id}: {e}");
    }
}

/// Load a previously enrolled credential, or enroll a fresh one with the
/// configured token and persist it for future starts.
async fn load_or_enroll(cfg: &Config) -> Result<SavedCredential> {
    if let Ok(s) = tokio::fs::read_to_string(&cfg.credential_path).await {
        if let Ok(c) = serde_json::from_str::<SavedCredential>(&s) {
            return Ok(c);
        }
    }
    let token = cfg
        .enroll_token
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no saved credential and AGENTGRID_ENROLL_TOKEN unset"))?;
    let client = reqwest::Client::new();
    let req = EnrollRequest {
        token,
        name: cfg.node_name.clone(),
        adapters: cfg.adapters.clone(),
        repositories: cfg.repositories.clone(),
        max_concurrency: cfg.max_concurrency,
        agent_version: cfg.agent_version.clone(),
    };
    let resp = client
        .post(format!("{}/v1/node/enroll", cfg.server))
        .json(&req)
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("enroll failed: {}", resp.status());
    }
    let er: EnrollResponse = resp.json().await?;
    let saved = SavedCredential {
        node_id: er.node_id,
        credential: er.credential,
    };
    if let Some(parent) = cfg.credential_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&cfg.credential_path, serde_json::to_string(&saved)?).await?;
    Ok(saved)
}

fn read_load_avg() -> f64 {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(0.0)
}

fn read_free_disk_mb(path: &std::path::Path) -> u64 {
    let cpath = match CString::new(path.to_string_lossy().as_bytes().to_vec()) {
        Ok(p) => p,
        Err(_) => return 0,
    };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: stat is a valid, zeroed statvfs; cpath is a valid NUL-terminated path.
    let free = unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) };
    if free != 0 || stat.f_frsize == 0 {
        return 0;
    }
    (stat.f_bavail as u64 * stat.f_frsize as u64) / (1024 * 1024)
}

async fn poll_loop(cfg: Config, cred: SavedCredential) -> Result<()> {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", cred.credential))?,
    );
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .build()?;
    let sem = Arc::new(Semaphore::new(cfg.max_concurrency as usize));

    // Capability discovery: re-probe the adapter each heartbeat (Stage 3.1).
    // A missing binary => degraded, so the scheduler excludes this node.
    let adapter_ok = Arc::new(AtomicBool::new(true));

    // Heartbeat loop: publish status/load/capabilities periodically.
    let hb_sem = sem.clone();
    let hb_cfg = cfg.clone();
    let hb_client = client.clone();
    let hb_node_id = cred.node_id.clone();
    let hb_adapter_ok = adapter_ok.clone();
    tokio::spawn(async move {
        loop {
            let probe = probe_adapter(&hb_cfg.adapter).await;
            hb_adapter_ok.store(probe.found, Ordering::Relaxed);
            let status = if probe.found {
                NodeStatus::Online
            } else {
                NodeStatus::Degraded
            };
            tokio::time::sleep(Duration::from_secs(hb_cfg.heartbeat_secs)).await;
            let active = hb_cfg.max_concurrency - hb_sem.available_permits() as u32;
            let req = HeartbeatRequest {
                status: Some(status),
                name: hb_cfg.node_name.clone(),
                adapters: hb_cfg.adapters.clone(),
                repositories: hb_cfg.repositories.clone(),
                max_concurrency: hb_cfg.max_concurrency,
                agent_version: hb_cfg.agent_version.clone(),
                load_avg: read_load_avg(),
                free_disk_mb: read_free_disk_mb(&hb_cfg.workspace_root),
                active_attempts: active,
            };
            if let Err(e) = hb_client
                .post(format!("{}/v1/node/heartbeat", hb_cfg.server))
                .json(&req)
                .send()
                .await
            {
                tracing::warn!("heartbeat failed: {e}");
            }
        }
    });

    loop {
        let poll_req = PollRequest {
            node_id: hb_node_id.clone(),
            name: cfg.node_name.clone(),
            adapters: cfg.adapters.clone(),
            repositories: cfg.repositories.clone(),
            max_concurrency: cfg.max_concurrency,
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
                        if let Err(e) = run_attempt(cfg2, client2, a).await {
                            tracing::error!("attempt error: {e}");
                        }
                        drop(permit);
                    });
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Stage 5.1: refuse to run as root unless explicitly allowed.
    if unsafe { libc::getuid() } == 0 && std::env::var_os("AGENTGRID_ALLOW_ROOT").is_none() {
        anyhow::bail!("refusing to run as root; set AGENTGRID_ALLOW_ROOT=1 to override");
    }

    let cfg = config_from_env();
    let probe = probe_adapter(&cfg.adapter).await;
    if probe.found {
        tracing::info!(adapter = %cfg.adapter, version = ?probe.version, "adapter detected");
    } else {
        tracing::warn!(
            adapter = %cfg.adapter,
            "adapter binary not found in PATH; node will report degraded until it is installed"
        );
    }
    tokio::fs::create_dir_all(&cfg.workspace_root).await?;
    let cred = load_or_enroll(&cfg).await?;
    tracing::info!(
        node_id = %cred.node_id,
        server = %cfg.server,
        adapter = %cfg.adapter,
        "node daemon starting"
    );
    poll_loop(cfg, cred).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_secrets_replaces_known() {
        assert_eq!(
            mask_secrets("token=abc123", &vec!["abc123".to_string()]),
            "token=***"
        );
        assert_eq!(mask_secrets("noop", &vec!["abc123".to_string()]), "noop");
        assert_eq!(
            mask_secrets("a secret b", &vec!["secret".to_string()]),
            "a *** b"
        );
    }

    #[tokio::test]
    async fn validation_command_reports_exit_and_log() {
        let dir = std::env::temp_dir().join(format!("ag-val-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sink = EventSink::new("a1".into(), reqwest::Client::new(), "http://x".into());
        let code = run_validation(&dir, "echo hi; exit 2", &sink)
            .await
            .unwrap();
        assert_eq!(code, 2);
        let log = std::fs::read_to_string(dir.join("validation.log")).unwrap();
        assert!(log.contains("hi"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn probe_adapter_finds_real_binary_and_reports_missing() {
        let good = probe_adapter("sh").await;
        assert!(good.found, "sh must exist on PATH");
        let bad = probe_adapter("definitely-not-an-agentgrid-adapter-xyz").await;
        assert!(!bad.found);
        assert!(bad.version.is_none());
    }

    #[tokio::test]
    async fn read_stream_mirrors_raw_output() {
        let dir = std::env::temp_dir().join(format!("ag-raw-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let raw_path = std::path::Path::new(&dir).join("raw.log");
        let f = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&raw_path)
            .await
            .unwrap();
        let raw = Arc::new(Mutex::new(f));
        let input = b"{\"type\":\"log\",\"payload\":{\"text\":\"hello\"}}\nnot json\n".to_vec();
        let reader = tokio::io::BufReader::new(std::io::Cursor::new(input));
        let sink = EventSink::new("a1".into(), reqwest::Client::new(), "http://x".into());
        read_stream(reader, sink, "stdout", vec![], Some(raw.clone())).await;
        let got = tokio::fs::read_to_string(&raw_path).await.unwrap();
        assert!(got.contains("hello"), "structured line mirrored: {got}");
        assert!(got.contains("not json"), "unparsed line mirrored: {got}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
