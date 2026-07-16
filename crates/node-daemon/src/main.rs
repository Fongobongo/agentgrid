//! Node daemon: long-polls the control plane, runs the adapter as a separate
//! process group in a per-attempt worktree, streams stdout/stderr as events,
//! and reports completion. Stage-1 version: in-memory, mock adapter only.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentgrid_adapters::{to_event_type, AdapterEvent};
use agentgrid_common::{
    Assignment, CompleteAttemptRequest, EventType, IncomingEvent, IngestEventsRequest, PollRequest,
    PollResponse,
};
use anyhow::Result;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::{Mutex, Notify, Semaphore};

#[derive(Clone)]
struct Config {
    server: String,
    node_id: String,
    node_name: String,
    workspace_root: PathBuf,
    max_concurrency: u32,
    adapter: String,
}

fn config_from_env() -> Config {
    let node_id =
        std::env::var("AGENTGRID_NODE_ID").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());
    let node_name = std::env::var("AGENTGRID_NODE_NAME")
        .unwrap_or_else(|_| hostname().unwrap_or_else(|| "node".into()));
    let workspace_root = PathBuf::from(
        std::env::var("AGENTGRID_WORKSPACE_ROOT")
            .unwrap_or_else(|_| "./agentgrid-workspace".into()),
    );
    Config {
        server: std::env::var("AGENTGRID_SERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:7800".into()),
        node_id,
        node_name,
        workspace_root,
        max_concurrency: std::env::var("AGENTGRID_MAX_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2),
        adapter: std::env::var("AGENTGRID_ADAPTER").unwrap_or_else(|_| "adapter-mock".into()),
    }
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Shared, bounded-ish event buffer that flushes to the control plane in
/// batches (every 200ms or when 50 events accumulate).
struct EventSink {
    buf: Mutex<VecDeque<IncomingEvent>>,
    next: AtomicU64,
    notify: Notify,
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
            attempt_id,
            client,
            server,
        })
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

async fn read_stream<R: AsyncRead + Unpin>(reader: R, sink: Arc<EventSink>, stream: &str) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        match serde_json::from_str::<AdapterEvent>(&line) {
            Ok(ae) => sink.push(to_event_type(&ae.r#type), ae.payload).await,
            Err(_) => {
                let ty = if stream == "stderr" {
                    EventType::Stderr
                } else {
                    EventType::Stdout
                };
                sink.push(ty, json!({ "text": line })).await;
            }
        }
    }
}

async fn run_attempt(cfg: Config, client: reqwest::Client, assignment: Assignment) -> Result<()> {
    let workdir = cfg.workspace_root.join(&assignment.attempt_id);
    tokio::fs::create_dir_all(&workdir).await?;
    tracing::info!(attempt_id = %assignment.attempt_id, "starting attempt");

    let mut cmd = tokio::process::Command::new(&cfg.adapter);
    cmd.arg("--prompt").arg(&assignment.prompt);
    cmd.current_dir(&workdir);
    cmd.env("AGENTGRID_ATTEMPT_ID", &assignment.attempt_id);
    // Separate process group so a cancel can SIGTERM the whole tree (Stage 2.7).
    cmd.process_group(0);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to spawn adapter {}: {e}", cfg.adapter);
            report_complete(&client, &cfg.server, &assignment.attempt_id, 127).await;
            return Ok(());
        }
    };

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let sink = EventSink::new(
        assignment.attempt_id.clone(),
        client.clone(),
        cfg.server.clone(),
    );
    let flusher = tokio::spawn(sink.clone().run_flusher());

    let r1 = tokio::spawn(read_stream(stdout, sink.clone(), "stdout"));
    let r2 = tokio::spawn(read_stream(stderr, sink.clone(), "stderr"));

    let status = child.wait().await?;
    let _ = r1.await;
    let _ = r2.await;
    sink.flush().await;
    flusher.abort();

    let code = status.code().unwrap_or(-1);
    tracing::info!(attempt_id = %assignment.attempt_id, exit_code = code, "attempt finished");
    report_complete(&client, &cfg.server, &assignment.attempt_id, code).await;
    Ok(())
}

async fn report_complete(client: &reqwest::Client, server: &str, attempt_id: &str, exit_code: i32) {
    let url = format!("{}/v1/node/attempts/{}/complete", server, attempt_id);
    let req = CompleteAttemptRequest { exit_code };
    if let Err(e) = client.post(&url).json(&req).send().await {
        tracing::warn!("complete report failed for {attempt_id}: {e}");
    }
}

async fn poll_loop(cfg: Config) -> Result<()> {
    let client = reqwest::Client::new();
    let sem = Arc::new(Semaphore::new(cfg.max_concurrency as usize));

    loop {
        let poll_req = PollRequest {
            node_id: cfg.node_id.clone(),
            name: cfg.node_name.clone(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
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
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = config_from_env();
    tokio::fs::create_dir_all(&cfg.workspace_root).await?;
    tracing::info!(
        node_id = %cfg.node_id,
        server = %cfg.server,
        adapter = %cfg.adapter,
        "node daemon starting"
    );
    poll_loop(cfg).await
}
