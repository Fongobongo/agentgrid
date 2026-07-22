//! Node daemon: long-polls the control plane, runs the adapter as a separate
//! process group in a per-attempt worktree, streams stdout/stderr as events,
//! and reports completion. Stage-1 version: in-memory, mock adapter only.

use std::collections::VecDeque;
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentgrid_acp::{
    map_session_update, InitializeParams, Message, SessionCancelParams, SessionNewParams,
    SessionPromptParams,
};
use agentgrid_adapters::{to_event_type, AdapterEvent, ExecutionBackend};
use agentgrid_common::{
    policy::{AutonomyLevel, BuiltinPolicyProvider, PolicyDecision},
    AdapterCapability, AgentEventEnvelope, ApprovalStatus, ApprovalView, Assignment, CancelState,
    CompleteAttemptRequest, ContextProvider, CreateAgentSessionRequest, EnrollRequest,
    EnrollResponse, EventKind, EventType, HeartbeatRequest, IncomingEvent, IngestEventsRequest,
    NodeStatus, NoopContextProvider, PollRequest, PollResponse, UploadArtifactRequest,
};
use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, Notify, Semaphore};

mod git;
mod outbox;
mod sandbox;

#[derive(Clone)]
struct Config {
    server: String,
    node_name: String,
    workspace_root: PathBuf,
    max_concurrency: u32,
    agent_version: String,
    adapters: Vec<AdapterSpec>,
    repositories: Vec<String>,
    heartbeat_secs: u64,
    enroll_token: Option<String>,
    credential_path: PathBuf,
    repository_root: PathBuf,
    /// Substrings masked to `***` in streamed logs (Stage 3.4).
    secrets: Vec<String>,
    /// Extra env vars forwarded to the adapter subprocess (e.g. API keys).
    adapter_env: Vec<(String, String)>,
    /// Agent isolation: wrap the spawned agent in a container (idea 5).
    sandbox: sandbox::SandboxKind,
    /// Stage 2.1: durable event/completion outbox root (survives daemon kill).
    outbox_root: PathBuf,
    /// Stage 2.1: a single durable completion spool (idempotent redelivery).
    completion_outbox: Arc<outbox::CompletionOutbox>,
    /// Stage 9.1: command-policy autonomy level driving the local
    /// short-circuit in `session/request_permission` (Allow/Deny) before the
    /// approval flow is reached.
    autonomy: AutonomyLevel,
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

/// How an adapter is driven: a legacy wrapper binary (stdout-parsed) or an
/// ACP-speaking agent (JSON-RPC 2.0 over stdio). Not a replacement — both
/// coexist in the registry (Stage 5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterProtocol {
    Wrapper,
    Acp,
}

impl AdapterProtocol {
    fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "acp" => AdapterProtocol::Acp,
            _ => AdapterProtocol::Wrapper,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AdapterSpec {
    pub id: String,
    pub protocol: AdapterProtocol,
}

/// Parse `AGENTGRID_ADAPTERS=mock,claude,opencode:acp` into specs. An entry
/// with no `:protocol` suffix defaults to `Wrapper` (backward compatible).
fn parse_adapters(s: &str) -> Vec<AdapterSpec> {
    s.split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once(':') {
            Some((id, proto)) => AdapterSpec {
                id: id.trim().to_string(),
                protocol: AdapterProtocol::parse(proto),
            },
            None => AdapterSpec {
                id: p.to_string(),
                protocol: AdapterProtocol::Wrapper,
            },
        })
        .collect()
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
        agent_version: std::env::var("AGENTGRID_AGENT_VERSION")
            .unwrap_or_else(|_| "0.1.0-dev".into()),
        adapters: parse_adapters(
            &std::env::var("AGENTGRID_ADAPTERS").unwrap_or_else(|_| "mock".into()),
        ),
        repositories: split_csv("AGENTGRID_REPOSITORIES", "*"),
        heartbeat_secs: std::env::var("AGENTGRID_HEARTBEAT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10),
        enroll_token: std::env::var("AGENTGRID_ENROLL_TOKEN").ok(),
        credential_path: PathBuf::from(&data_dir).join("credential.json"),
        repository_root: PathBuf::from(
            std::env::var("AGENTGRID_REPOSITORY_ROOT")
                .unwrap_or_else(|_| "./agentgrid-repos".into()),
        ),
        secrets: split_csv("AGENTGRID_SECRETS", ""),
        adapter_env: parse_env_pairs("AGENTGRID_ADAPTER_ENV"),
        sandbox: sandbox::SandboxKind::from_env(),
        outbox_root: PathBuf::from(&data_dir).join("outbox"),
        completion_outbox: Arc::new({
            let dir = PathBuf::from(&data_dir).join("outbox");
            outbox::CompletionOutbox::open(&dir).unwrap_or_else(|e| {
                tracing::warn!("completion outbox open failed: {e}; events may be lost on kill");
                outbox::CompletionOutbox::open(&std::env::temp_dir()).unwrap()
            })
        }),
        autonomy: parse_autonomy(std::env::var("AGENTGRID_AUTONOMY").ok()),
    }
}

/// Stage 9.1: parse the command-policy autonomy level from an env string
/// like `l0`..`l4` (case-insensitive). Unknown / missing → default (`L2`).
fn parse_autonomy(v: Option<String>) -> AutonomyLevel {
    let Some(v) = v else {
        return AutonomyLevel::default();
    };
    serde_json::from_value(serde_json::Value::String(v.to_lowercase())).unwrap_or_default()
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

/// Resolve `bin` to an executable file on `PATH` (or a literal path if it
/// contains `/`). No shell is involved, so a crafted adapter id cannot inject
/// commands (Stage 2.3). Adapter ids come from operator config, not tasks.
fn resolve_in_path(bin: &str) -> Option<std::path::PathBuf> {
    if bin.contains('/') {
        return if std::path::Path::new(bin).is_file() {
            Some(std::path::PathBuf::from(bin))
        } else {
            None
        };
    }
    let path = std::env::var("PATH").ok()?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(bin);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Map an adapter id (e.g. `mock`, `claude`) to its binary name (`adapter-mock`).
fn adapter_bin_name(adapter_id: &str) -> String {
    format!("adapter-{}", adapter_id.replace('_', "-"))
}

/// Resolve the adapter binary for `adapter_id` on PATH. Returns None when the
/// node cannot run that adapter, so the attempt can be failed as
/// `infrastructure_failed` (Stage 2.4).
fn resolve_adapter_bin(adapter_id: &str) -> Option<String> {
    let bin = adapter_bin_name(adapter_id);
    resolve_in_path(&bin).map(|_| bin)
}

/// Native ACP launcher: if `AGENTGRID_ACP_LAUNCH_<ID>` is set, the node runs
/// that command directly (e.g. `claude --acp`, `codex --acp`) over stdio
/// instead of spawning a `adapter-<id>` wrapper binary. This lets a new
/// native-ACP agent be added with one env var, no per-agent crate/parser.
/// Returns `(program, args)`. The value is split on whitespace; operator config
/// (not task input), so quoting is the operator's responsibility.
// ponytail: naive whitespace split; use shlex if args need spaces.
fn resolve_acp_launch(adapter_id: &str) -> Option<(String, Vec<String>)> {
    let key = format!(
        "AGENTGRID_ACP_LAUNCH_{}",
        adapter_id
            .to_ascii_uppercase()
            .replace(|c: char| !c.is_alphanumeric(), "_")
    );
    let val = std::env::var(&key).ok()?;
    let mut parts = val.split_whitespace();
    let program = parts.next()?.to_string();
    let args = parts.map(|s| s.to_string()).collect();
    Some((program, args))
}

/// Agent profile (idea: oh-my-agent SSOT): an optional system prompt for this
/// adapter. `AGENTGRID_AGENT_PROFILE_<ID>` is either a path to a `.md` file
/// (read) or inline text. Returns None when unset. Projected into the worktree
/// as `AGENTS.md` (cross-agent convention); per-agent native projection
/// (`CLAUDE.md`, `.kiro/`) is a follow-up mapping table.
// ponytail: single AGENTS.md projection; native per-agent files if an agent
// ignores AGENTS.md.
fn agent_profile(adapter_id: &str) -> Option<String> {
    let key = format!(
        "AGENTGRID_AGENT_PROFILE_{}",
        adapter_id
            .to_ascii_uppercase()
            .replace(|c: char| !c.is_alphanumeric(), "_")
    );
    let val = std::env::var(&key).ok()?;
    if val.trim().is_empty() {
        return None;
    }
    let p = std::path::Path::new(&val);
    if p.is_file() {
        std::fs::read_to_string(p).ok()
    } else {
        Some(val)
    }
}

/// Stage 9.2: discover skills in the worktree + user home, keep only the ones
/// the operator explicitly trusted on the control plane (fail-closed: an
/// untrusted/unknown skill is omitted), and render a short "Available skills"
/// block to append to the prompt. Returns an empty string on any error so the
/// task is never blocked by the trust lookup wiring (the skills are a hint, not
/// a hard dependency).
///
/// `ponytail:` fetches the whole trust ledger per attempt (O(skills) over HTTP,
/// small); if skill counts grow, switch to a per-skill lookup or a node-side
/// cache keyed by `(name, source)`.
async fn compose_skills_block(
    client: &reqwest::Client,
    server: &str,
    ws_path: &std::path::Path,
) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let roots = agentgrid_skills::standard_roots(ws_path, home.as_deref());
    let (discovered, _diags) = agentgrid_skills::discover(&roots);
    if discovered.is_empty() {
        return String::new();
    }
    // Fetch the full trust ledger; untrusted/absent entries are dropped.
    let trusted_name: std::collections::HashSet<(String, String)> =
        match client.get(format!("{server}/v1/skills")).send().await {
            Ok(r) if r.status().is_success() => {
                match r.json::<Vec<agentgrid_common::SkillTrustView>>().await {
                    Ok(rows) => rows
                        .into_iter()
                        .filter(|v| v.trusted)
                        .map(|v| (v.name, v.source))
                        .collect(),
                    Err(_) => return String::new(),
                }
            }
            _ => return String::new(), // skills are a hint; don't block the task
        };
    render_trusted_skills_block(&discovered, &trusted_name)
}

/// Stage 11 (CTX): build a context pack for the attempt's repo+base_commit
/// via the configured `ContextProvider` (default Noop → empty pack), append its
/// body to the prompt, and stream a `context_pack` status event with the
/// before/after bytes + cache-hit metrics. Any provider error is swallowed:
/// the agent simply proceeds without a context digest (graceful fallback).
///
/// `ponytail:` single provider instance, no on-disk cache yet; the cache key
/// is computed by the provider so a future CTX impl can consult a warm cache
/// on disk and skip re-indexing (Stage 11 exit criterion).
async fn compose_context_block(
    provider: &dyn ContextProvider,
    assignment: &Assignment,
    sink: &Arc<EventSink>,
) -> String {
    let repo = assignment.repository.as_str();
    let base = assignment.base_commit.as_deref().unwrap_or("");
    let pack = match provider.build(repo, base) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                provider = provider.id(),
                "context provider failed: {e}; falling back to no pack"
            );
            return String::new();
        }
    };
    if pack.is_empty() {
        return String::new();
    }
    // Metrics event: bytes_in/bytes_out/cache_hit/index_ms.
    sink.push(
        EventType::Status,
        json!({
            "kind": "context_pack",
            "provider": pack.provider,
            "repo": pack.repo,
            "base_commit": pack.base_commit,
            "cache_key": pack.cache_key,
            "cache_hit": pack.cache_hit,
            "bytes_in": pack.bytes_in,
            "bytes_out": pack.bytes_out,
            "index_ms": pack.index_ms,
        }),
    )
    .await;
    pack.body
}

/// Pure render of the trusted subset of discovered skills. Separated so it can
/// be unit-tested without HTTP.
fn render_trusted_skills_block(
    discovered: &[agentgrid_skills::DiscoveredSkill],
    trusted: &std::collections::HashSet<(String, String)>,
) -> String {
    let mut keep: Vec<&agentgrid_skills::DiscoveredSkill> = discovered
        .iter()
        .filter(|d| trusted.contains(&(d.skill.name.clone(), d.source.as_str().to_string())))
        .collect();
    if keep.is_empty() {
        return String::new();
    }
    keep.sort_by(|a, b| a.skill.name.cmp(&b.skill.name));
    let mut out = String::from("\n\nAvailable agent skills (operator-trusted):\n");
    for d in keep {
        out.push_str(&format!(
            "- {} ({}): {}\n",
            d.skill.name,
            d.source.as_str(),
            d.skill.description.lines().next().unwrap_or("").trim(),
        ));
    }
    out
}

async fn probe_adapter(bin: &str) -> AdapterProbe {
    if resolve_in_path(bin).is_none() {
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
    // Counts events that came from the adapter's stdout/stderr. Used to warn on a
    // silent agent that exits 0 but produced no output.
    adapter_events: AtomicU64,
    attempt_id: String,
    client: reqwest::Client,
    server: String,
    /// Stage 2.1: durable JSONL outbox. Events are appended here before any
    // send attempt and removed only after the CP acks the batch, so a daemon
    // kill no longer drops the in-flight tail.
    outbox: Arc<outbox::EventOutbox>,
    /// Stage 2.1: approximate RAM bytes pending in `buf` (backpressure).
    buf_bytes: AtomicU64,
    /// Stage 2.1: latched once an `output_truncated` notice has been emitted,
    // so a chatty agent produces one truncation notice, not one per dropped line.
    truncated_warned: std::sync::atomic::AtomicBool,
}

impl EventSink {
    fn new(
        attempt_id: String,
        client: reqwest::Client,
        server: String,
        outbox: Arc<outbox::EventOutbox>,
    ) -> Arc<Self> {
        Arc::new(Self {
            buf: Mutex::new(VecDeque::new()),
            next: AtomicU64::new(1),
            notify: Notify::new(),
            adapter_events: AtomicU64::new(0),
            attempt_id,
            client,
            server,
            outbox,
            buf_bytes: AtomicU64::new(0),
            truncated_warned: std::sync::atomic::AtomicBool::new(false),
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
        // Stage 2.1 backpressure: ordinary log/usage events are dropped (with a
        // single `output_truncated` notice) once the RAM buffer exceeds the per-
        // attempt cap, so a chatty agent can't wedge the node. Terminal state
        // (status/result/error) and tool calls are never dropped.
        let droppable = matches!(
            ty,
            EventType::Stdout | EventType::Stderr | EventType::Metric
        );
        if droppable {
            let cap = std::env::var("AGENTGRID_EVENT_BUF_BYTES")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(4 * 1024 * 1024);
            let cur = self.buf_bytes.load(Ordering::Relaxed);
            if cur >= cap {
                if !self
                    .truncated_warned
                    .swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    tracing::warn!(
                        attempt_id = %self.attempt_id,
                        cap, "output truncated: event buffer over cap; dropping further logs"
                    );
                    self.emit_truncated_notice(cap).await;
                }
                return;
            }
        }
        let seq = self.next.fetch_add(1, Ordering::SeqCst);
        let approx_bytes = payload.to_string().len() as u64;
        let ev = IncomingEvent {
            sequence: seq,
            r#type: ty,
            payload,
        };
        // Stage 2.1: persist before buffering so a kill doesn't drop it. A
        // failed fsync is non-fatal (we still deliver from RAM this run); it
        // just means the disk tail isn't covered.
        if let Err(e) = self.outbox.push(&ev) {
            tracing::warn!(attempt_id = %self.attempt_id, "outbox push failed: {e}");
        }
        self.buf_bytes.fetch_add(approx_bytes, Ordering::Relaxed);
        self.buf.lock().await.push_back(ev);
        if self.buf.lock().await.len() >= 50 {
            self.notify.notify_one();
        }
    }

    async fn emit_truncated_notice(&self, cap: u64) {
        let seq = self.next.fetch_add(1, Ordering::SeqCst);
        let ev = IncomingEvent {
            sequence: seq,
            r#type: EventType::Status,
            payload: serde_json::json!({
                "event": "output_truncated",
                "reason": "event buffer over cap",
                "cap_bytes": cap,
            }),
        };
        if let Err(e) = self.outbox.push(&ev) {
            tracing::warn!(attempt_id = %self.attempt_id, "outbox push (truncation) failed: {e}");
        }
        self.buf.lock().await.push_back(ev);
        self.notify.notify_one();
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
        let seqs: Vec<u64> = batch.iter().map(|e| e.sequence).collect();
        // Approximate bytes for backpressure accounting. Released only on a
        // successful ack so a failed flush (batch pushed back) doesn't undercount.
        let freed: u64 = batch
            .iter()
            .map(|e| e.payload.to_string().len() as u64)
            .sum();
        let req = IngestEventsRequest { events: batch };
        // Stage 2.1: verify the HTTP status and retry transient/5xx failures.
        // On a still-non-2xx response the batch is returned to the front of the
        // buffer so the flusher loop keeps retrying while the daemon runs; the
        // durable outbox still holds them for redelivery after a restart.
        match send_with_retry(self.client.post(&url).json(&req), 10).await {
            Ok(s) if s.is_success() => {
                // CP acked: release the RAM budget and drop the lines from the
                // durable outbox.
                self.buf_bytes.fetch_sub(freed, Ordering::Relaxed);
                if let Err(e) = self.outbox.ack(&seqs) {
                    tracing::warn!(attempt_id = %self.attempt_id, "outbox ack failed: {e}");
                }
            }
            Ok(s) => {
                tracing::warn!(attempt_id = %self.attempt_id, "event flush got {s}; will retry");
                let mut buf = self.buf.lock().await;
                for e in req.events {
                    buf.push_front(e);
                }
            }
            Err(e) => {
                tracing::warn!(attempt_id = %self.attempt_id, "event flush error {e}; will retry");
                let mut buf = self.buf.lock().await;
                for e in req.events {
                    buf.push_front(e);
                }
            }
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

    /// Drain the RAM buffer with a single send attempt (no long retry). Used
    /// in the post-adapter path so a down CP doesn't block the completion
    /// recording for tens of seconds; the durable outbox retains the events
    /// and the flusher loop (while it lives) keeps retrying.
    async fn flush_quick(&self) {
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
        let seqs: Vec<u64> = batch.iter().map(|e| e.sequence).collect();
        let freed: u64 = batch
            .iter()
            .map(|e| e.payload.to_string().len() as u64)
            .sum();
        let req = IngestEventsRequest { events: batch };
        match send_with_retry(self.client.post(&url).json(&req), 1).await {
            Ok(s) if s.is_success() => {
                self.buf_bytes.fetch_sub(freed, Ordering::Relaxed);
                if let Err(e) = self.outbox.ack(&seqs) {
                    tracing::warn!(attempt_id = %self.attempt_id, "outbox ack failed: {e}");
                }
            }
            _ => {
                // Push back; the durable outbox still holds these lines.
                let mut buf = self.buf.lock().await;
                for e in req.events {
                    buf.push_front(e);
                }
            }
        }
    }

    /// Synchronously drain the RAM buffer to the CP (CP is up by the time this is
    /// called, after report_complete succeeded). Loops flush() with full retry
    /// until the buffer is empty or the deadline passes, so events buffered
    /// during a CP outage are not lost when the flusher is aborted.
    /// Drain directly from the durable outbox on disk, ignoring the RAM
    /// buffer. Ground-truth recovery path: events an aborted flusher dropped
    /// mid-flush (its local `req` is gone) are still on disk and get
    /// redelivered here. Loops until the outbox is empty or the deadline
    /// passes. The CP is up by the time this is called.
    async fn drain_outbox(&self, deadline: tokio::time::Instant) {
        let url = format!(
            "{}/v1/node/attempts/{}/events",
            self.server, self.attempt_id
        );
        loop {
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    attempt_id = %self.attempt_id,
                    "drain_outbox timed out; events remain on disk"
                );
                return;
            }
            let pending = match self.outbox.pending() {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(attempt_id = %self.attempt_id, "outbox read failed: {e}");
                    return;
                }
            };
            if pending.is_empty() {
                return;
            }
            let batch: Vec<IncomingEvent> = pending.into_iter().collect();
            let seqs: Vec<u64> = batch.iter().map(|e| e.sequence).collect();
            let req = IngestEventsRequest { events: batch };
            match send_with_retry(self.client.post(&url).json(&req), 10).await {
                Ok(s) if s.is_success() => {
                    if let Err(e) = self.outbox.ack(&seqs) {
                        tracing::warn!(attempt_id = %self.attempt_id, "outbox ack failed: {e}");
                    }
                }
                _ => return,
            }
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
        // Stage 3.1: accept the versioned envelope first; fall back to the
        // legacy `{type, payload}` adapter event; anything else is a raw log.
        // Unknown kinds are preserved (never fatal).
        if let Ok(env) = serde_json::from_str::<AgentEventEnvelope>(&masked) {
            sink.push(env.kind.to_event_type(), env.payload).await;
            sink.note_adapter_event();
            continue;
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
                sink.push(ty, json!({ "text": masked })).await;
                sink.note_adapter_event();
            }
        }
    }
}

/// Terminal outcome of an ACP-driven attempt.
struct AcpResult {
    success: bool,
    error_code: Option<String>,
    /// ACP session id from `session/new`, reported back so the control plane
    /// can resume it on a follow-up task (Stage 11.5).
    session_id: Option<String>,
}

/// Stage 5: drive an ACP agent over stdio (JSON-RPC 2.0). Spawns
/// `adapter-<id>`, runs initialize/new/prompt, forwards `session/update` into
/// the event sink, answers `session/request_permission` via the durable
/// approval flow, and returns the terminal outcome. Cancellation/timeout are
/// handled here (the wrapper path keeps those in run_attempt's select!).
async fn drive_acp_session(
    cfg: &Config,
    client: &reqwest::Client,
    assignment: &Assignment,
    ws_path: &std::path::Path,
    sink: Arc<EventSink>,
) -> Result<AcpResult> {
    // Native ACP launcher (direct CLI, e.g. `claude --acp`) takes priority
    // over the `adapter-<id>` wrapper binary.
    let (program, args) = match resolve_acp_launch(&assignment.adapter) {
        Some((program, args)) => (program, args),
        None => match resolve_adapter_bin(&assignment.adapter) {
            Some(b) => (b, vec![]),
            None => {
                tracing::error!(adapter = %assignment.adapter, "ACP adapter binary not found");
                return Ok(AcpResult {
                    success: false,
                    error_code: Some("infrastructure_failed".into()),
                    session_id: None,
                });
            }
        },
    };
    let (program, args) = sandbox::sandbox_command(cfg.sandbox, &program, &args, ws_path);
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true);
    for (k, v) in &cfg.adapter_env {
        cmd.env(k, v);
    }
    // Forward the agent profile as an env hint for agents that read it.
    if let Some(text) = agent_profile(&assignment.adapter) {
        cmd.env("AGENTGRID_SYSTEM_PROMPT", &text);
    }
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stdin = child.stdin.take().expect("piped stdin");
    let (acp, mut notif) = agentgrid_acp::new(stdout, stdin);
    let acp = std::sync::Arc::new(acp);

    let model = std::env::var("AGENTGRID_AGENT_VERSION").unwrap_or_else(|_| "default".into());
    if let Err(e) = acp
        .initialize(InitializeParams {
            protocol_version: "0.1".into(),
            agent: assignment.adapter.clone(),
            model,
            session_id: None,
            cwd: ws_path.to_string_lossy().into_owned(),
            capabilities: Value::Null,
            client: Value::Null,
        })
        .await
    {
        tracing::error!("ACP initialize failed: {e}");
        let _ = child.start_kill();
        return Ok(AcpResult {
            success: false,
            error_code: Some("infrastructure_failed".into()),
            session_id: None,
        });
    }
    let session_id = match acp
        .session_new(SessionNewParams {
            agent: assignment.adapter.clone(),
            model: None,
            cwd: ws_path.to_string_lossy().into_owned(),
            prompt: None,
            mcp: Value::Null,
            parent_session_id: assignment.parent_acp_session_id.clone(),
        })
        .await
    {
        Ok(r) => r.session_id,
        Err(e) => {
            tracing::error!("ACP session/new failed: {e}");
            let _ = child.start_kill();
            return Ok(AcpResult {
                success: false,
                error_code: Some("infrastructure_failed".into()),
                session_id: None,
            });
        }
    };

    let flusher = tokio::spawn(sink.clone().run_flusher());

    let sid = session_id.clone();
    let task_id = assignment.task_id.clone();
    let attempt_id = assignment.attempt_id.clone();
    let sink2 = sink.clone();
    let acp2 = acp.clone();
    let client2 = client.clone();
    let server2 = cfg.server.clone();
    let autonomy = cfg.autonomy;
    let stream_task = tokio::spawn(async move {
        while let Some(msg) = notif.recv().await {
            match msg {
                Message::Notification { params, .. } => {
                    let upd = params.get("update").unwrap_or(&params);
                    let env = map_session_update(&sid, upd);
                    sink2.push(env.kind.to_event_type(), env.payload).await;
                    sink2.note_adapter_event();
                }
                Message::Request { id, method, params }
                    if method == "session/request_permission" =>
                {
                    let allow = request_permission(
                        &client2,
                        &server2,
                        &task_id,
                        &attempt_id,
                        &sid,
                        &params,
                        autonomy,
                        &sink2,
                    )
                    .await;
                    let _ = acp2.respond(id, allow).await;
                }
                _ => {}
            }
        }
    });

    let acp3 = acp.clone();
    let mut prompt_text = assignment.prompt.clone();
    // Stage 11 (CTX): append a repo context pack (if a provider is configured).
    // Noop by default → empty body → agent proceeds without a digest.
    let ctx_provider = NoopContextProvider;
    prompt_text.push_str(&compose_context_block(&ctx_provider, assignment, &sink).await);
    // Stage 9.2: append the operator-trusted skills discovered in this worktree
    // (fail-closed: untrusted skills are omitted, any lookup error = no block).
    prompt_text.push_str(&compose_skills_block(client, &cfg.server, ws_path).await);
    let sid_prompt = session_id.clone();
    let mut prompt = tokio::spawn(async move {
        acp3.session_prompt(SessionPromptParams {
            session_id: sid_prompt,
            prompt: prompt_text,
        })
        .await
    });
    let cancel_client = client.clone();
    let cancel_url = format!(
        "{}/v1/node/attempts/{}/cancel",
        cfg.server, assignment.attempt_id
    );
    let pid = child.id().unwrap_or(0);
    let timeout = Duration::from_secs(assignment.timeout_secs.max(1));
    let outcome = tokio::select! {
        res = &mut prompt => match res {
            Ok(_) => AcpResult { success: true, error_code: None, session_id: Some(session_id.clone()) },
            Err(e) => AcpResult { success: false, error_code: Some(format!("agent_error: {e}")), session_id: Some(session_id.clone()) },
        },
        _ = wait_for_cancel(cancel_client, cancel_url) => {
            acp.session_cancel(SessionCancelParams { session_id: session_id.clone() }).await.ok();
            // Bound the reap for the same reason as the timeout branch.
            terminate_group(pid);
            let _ = tokio::time::timeout(
                Duration::from_secs(12),
                child.wait(),
            )
            .await;
            AcpResult { success: false, error_code: Some("cancelled".into()), session_id: Some(session_id.clone()) }
        }
        _ = tokio::time::sleep(timeout) => {
            terminate_group(pid);
            // Bound the reap so a child that ignores SIGTERM (or a pidfd that
            // never fires) can't park the session forever. terminate_group
            // escalates to SIGKILL after 10s, so allow a little slack.
            let _ = tokio::time::timeout(
                Duration::from_secs(15),
                child.wait(),
            )
            .await;
            AcpResult { success: false, error_code: Some("timeout".into()), session_id: Some(session_id.clone()) }
        }
    };
    stream_task.abort();
    flusher.abort();
    Ok(outcome)
}

/// Stage 5/9.1: answer `session/request_permission`. First the builtin
/// command-policy provider classifies the requested command; an `Allow`
/// short-circuits (no operator round-trip), `Deny` is rejected outright, and
/// only `Ask` falls through to the durable operator approval flow below.
/// Fail-closed: any error or timeout denies.
///
/// The provider handles only Bash-style shell commands (the common ACP case,
/// `permission = {tool:"Bash", input:"<cmd>"}`); other tools always reach the
/// approval flow — see `enforcement_boundary` doc: a wrapper adapter without
/// structured tool calls cannot be fully intercepted.
#[allow(clippy::too_many_arguments)]
async fn request_permission(
    client: &reqwest::Client,
    server: &str,
    task_id: &str,
    attempt_id: &str,
    session_id: &str,
    permission: &Value,
    autonomy: AutonomyLevel,
    sink: &Arc<EventSink>,
) -> bool {
    // Stage 9.1 local short-circuit for Bash commands.
    if let Some(decision) = policy_decision(permission, autonomy) {
        sink.push(
            EventType::Status,
            json!({
                "kind": "permission_decision",
                "decision": decision.0,
                "risk_class": decision.1,
                "reason": decision.2,
                "source": "local_policy",
                "autonomy": autonomy,
            }),
        )
        .await;
        return decision.0 == PolicyDecision::Allow;
    }
    // Fall through: ask the operator via the durable approval flow.
    let create = client
        .post(format!("{server}/v1/tasks/{task_id}/approvals"))
        .json(&json!({ "attempt_id": attempt_id, "session_id": session_id, "permission": permission }))
        .send()
        .await;
    let id = match create {
        Ok(r) if r.status().is_success() => match r.json::<Value>().await {
            Ok(v) => v
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            Err(_) => return false,
        },
        _ => return false,
    };
    if id.is_empty() {
        return false;
    }
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        match client
            .get(format!("{server}/v1/approvals/{id}"))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => match resp.json::<ApprovalView>().await {
                Ok(av) => match av.status {
                    ApprovalStatus::Allowed => return true,
                    ApprovalStatus::Pending => continue,
                    _ => return false,
                },
                Err(_) => return false,
            },
            _ => return false,
        }
    }
    false
}

/// Stage 9.1: evaluate a `session/request_permission` against the builtin
/// command policy. Returns `Some((decision, risk_class, reason))` only for a
/// definitive local `Allow`/`Deny` of a Bash-style shell command
/// (`{tool:"Bash", input:"<cmd>"}`); `Ask` and non-Bash tools return `None`
/// (→ approval flow, fail-closed to the operator).
fn policy_decision(
    permission: &Value,
    autonomy: AutonomyLevel,
) -> Option<(PolicyDecision, String, String)> {
    let tool = permission.get("tool").and_then(|v| v.as_str())?;
    if !tool.eq_ignore_ascii_case("bash") {
        return None;
    }
    let cmd = permission.get("input").and_then(|v| v.as_str())?;
    let verdict = BuiltinPolicyProvider::new()
        .evaluate_with(autonomy, cmd, "")
        .ok()?;
    // `Ask` is not a local decision: fall through to the operator approval flow.
    if verdict.decision == PolicyDecision::Ask {
        return None;
    }
    Some((
        verdict.decision,
        format!("{:?}", verdict.risk_class),
        verdict.reason,
    ))
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

    // Stage 2.1: a durable event outbox for this attempt, so a daemon kill no
    // longer drops the in-flight event tail (redelivered on next startup;
    // CP ingest is idempotent on (attempt_id, sequence)).
    let outbox = Arc::new(outbox::EventOutbox::open(
        &cfg.outbox_root,
        &assignment.attempt_id,
    )?);
    // If a prior run left undelivered events for this attempt, re-queue them so
    // they go out before new ones (sequence order preserved by pending()).
    {
        let pending = outbox.pending().unwrap_or_default();
        if !pending.is_empty() {
            tracing::info!(attempt_id = %assignment.attempt_id, count = pending.len(), "requeueing undelivered outbox events");
        }
    }

    // Agent profile (idea 6): an optional system prompt for this adapter,
    // projected into the worktree as AGENTS.md before the agent runs. Sourced
    // from AGENTGRID_AGENT_PROFILE_<ID> (a path to a .md file, or inline text).
    if let Some(text) = agent_profile(&assignment.adapter) {
        let p = ws.path.join("AGENTS.md");
        let _ = tokio::fs::write(&p, &text).await;
    }

    // Stage 5: ACP adapters are driven over JSON-RPC 2.0 (stdio), not stdout
    // parsing. Everything below that point lives in drive_acp_session.
    if cfg
        .adapters
        .iter()
        .find(|s| s.id == assignment.adapter)
        .map(|s| s.protocol)
        == Some(AdapterProtocol::Acp)
    {
        let sink = EventSink::new(
            assignment.attempt_id.clone(),
            client.clone(),
            cfg.server.clone(),
            outbox.clone(),
        );
        ack_attempt(&client, &cfg.server, &assignment.attempt_id).await;
        create_agent_session(
            &client,
            &cfg.server,
            &assignment.attempt_id,
            &assignment.adapter,
        )
        .await;
        let res = drive_acp_session(&cfg, &client, &assignment, &ws.path, sink.clone()).await?;
        // Stage 2.3: keep the per-attempt repo_dir/branch for cleanup after finalize takes ws.
        let workdir = ws.path.clone();
        let cleanup_repo = ws.repo_dir.clone();
        let cleanup_branch =
            (ws.is_git && ws.branch.is_some()).then(|| ws.branch.clone().unwrap_or_default());
        let cleanup_path = ws.path.clone();
        let node_name = cfg.node_name.clone();
        let commit_sha =
            tokio::task::spawn_blocking(move || git::finalize_workspace(ws, node_name.as_str()))
                .await??;
        // Run the optional validation command — the ACP path used to skip it,
        // silently leaving validation_command unenforced for ACP agents. The
        // diff is already committed so it survives a validation failure.
        // Stage 11.4.
        let mut exit_code = if res.success { 0 } else { 1 };
        let mut error_code = res.error_code;
        if exit_code == 0 {
            if let Some(cmd) = &assignment.validation_command {
                match run_validation(&workdir, cmd, &sink).await {
                    Ok(vcode) if vcode != 0 => {
                        exit_code = vcode;
                        error_code = Some("validation_failed".into());
                    }
                    Err(e) => {
                        tracing::error!("ACP validation failed to run: {e}");
                        error_code = Some("validation_failed".into());
                    }
                    _ => {}
                }
            }
        }
        report_complete(
            &client,
            &cfg.server,
            &assignment.attempt_id,
            exit_code,
            commit_sha,
            error_code,
            res.session_id.clone(),
            &cfg.completion_outbox,
        )
        .await;
        // Stage 2.3: reclaim the per-attempt worktree and branch now the attempt
        // is terminal (prevents long-lived worktree/branch retention leaking disk).
        tokio::task::spawn_blocking(move || {
            git::cleanup_workspace(
                &cleanup_path,
                cleanup_repo.as_deref(),
                cleanup_branch.as_deref(),
            )
        })
        .await
        .ok();
        return Ok(());
    }

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

    // Stage 2.4: run strictly the adapter the control plane assigned; an
    // unknown or missing adapter binary is an infrastructure failure, not a
    // silent fallback to whatever binary happens to be configured.
    let bin = match resolve_adapter_bin(&assignment.adapter) {
        Some(b) => b,
        None => {
            tracing::error!(
                attempt_id = %assignment.attempt_id,
                adapter = %assignment.adapter,
                "adapter binary not found; reporting infrastructure_failed"
            );
            report_complete(
                &client,
                &cfg.server,
                &assignment.attempt_id,
                127,
                None,
                Some("infrastructure_failed".into()),
                None,
                &cfg.completion_outbox,
            )
            .await;
            return Ok(());
        }
    };
    // Stage 3.2: spawn through the ExecutionBackend contract (native process).
    // ponytail: sandbox not applied here (legacy wrapper path); the ACP path
    // is sandboxed. Wire Sandbox into ExecutionBackend if legacy isolation is
    // needed.
    //
    // Feedback loop (Stage 11.4): when a validation_command is configured and
    // the agent exits 0 but validation fails, re-spawn the agent with the
    // validation error appended to the prompt (same worktree) so it can fix
    // its own output. Bounded by AGENTGRID_FEEDBACK_RETRIES (default 0 = off,
    // backward compatible). Each round reuses the same sink/flusher so all
    // events stay under one attempt; the worktree accumulates the agent's
    // fixes and is committed once at the end.
    let retries: usize = std::env::var("AGENTGRID_FEEDBACK_RETRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let cancel_url = format!(
        "{}/v1/node/attempts/{}/cancel",
        cfg.server, assignment.attempt_id
    );
    let sink = EventSink::new(
        assignment.attempt_id.clone(),
        client.clone(),
        cfg.server.clone(),
        outbox.clone(),
    );
    let workdir = ws.path.clone();
    let validation_log = workdir.join("validation.log");
    let mut prompt = assignment.prompt.clone();
    let mut last_code: i32;
    let mut last_kill_reason: Option<&'static str> = None;

    // Ack once; the attempt is `running` for its whole (multi-round) lifetime.
    ack_attempt(&client, &cfg.server, &assignment.attempt_id).await;
    create_agent_session(
        &client,
        &cfg.server,
        &assignment.attempt_id,
        &assignment.adapter,
    )
    .await;
    let flusher = tokio::spawn(sink.clone().run_flusher());

    let mut round = 0usize;
    let validation_passed = loop {
        let req = agentgrid_adapters::SpawnRequest {
            bin: bin.clone(),
            prompt: prompt.clone(),
            workdir: ws.path.clone(),
            attempt_id: assignment.attempt_id.clone(),
            timeout: Duration::from_secs(assignment.timeout_secs.max(1)),
            env: cfg.adapter_env.clone(),
        };
        let bp = match agentgrid_adapters::ProcessBackend.spawn(req) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("failed to spawn adapter: {e}");
                last_code = 127;
                break false;
            }
        };
        let pid = bp.pid;
        let timeout = bp.timeout;
        let stdout = bp.stdout;
        let stderr = bp.stderr;
        let mut child = bp.child;
        let cancel_client = client.clone();

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
        let (code, kill_reason): (i32, Option<&'static str>) = {
            let outcome = tokio::select! {
                status = child.wait() => Outcome::Exited(status?.code().unwrap_or(-1)),
                _ = tokio::time::sleep(timeout) => Outcome::Timeout,
                _ = wait_for_cancel(cancel_client, cancel_url.clone()) => Outcome::Cancel,
            };
            match outcome {
                Outcome::Exited(c) => (c, None),
                Outcome::Timeout => {
                    terminate_group(pid);
                    let status = child.wait().await?;
                    (status.code().unwrap_or(-1), Some("timeout"))
                }
                Outcome::Cancel => {
                    sink.push(
                        EventKind::Cancel.to_event_type(),
                        json!({
                            "kind": "cancel",
                            "reason": "user_requested",
                            "attempt_id": assignment.attempt_id
                        }),
                    )
                    .await;
                    terminate_group(pid);
                    let status = child.wait().await?;
                    (status.code().unwrap_or(-1), None)
                }
            }
        };

        let _ = r1.await;
        let _ = r2.await;
        // Stage 2.1: record the terminal completion BEFORE the post-adapter
        // sends so a daemon kill during the (possibly blocking) flush/upload
        // window still redelivers the completion on the next startup. The exit
        // code is known here; commit_sha/validation verdict are refined later
        // (record() replaces the prior line, latest wins).
        let early_req = CompleteAttemptRequest {
            exit_code: code,
            commit_sha: None,
            error_code: kill_reason.map(|k| k.to_string()),
            acp_session_id: None,
        };
        if let Err(e) = cfg
            .completion_outbox
            .record(&assignment.attempt_id, &early_req)
        {
            tracing::warn!(attempt_id = %assignment.attempt_id, "early completion record failed: {e}");
        }
        // Single-shot drain: don't block for tens of seconds on a down CP; the
        // flusher loop + durable outbox cover redelivery.
        sink.flush_quick().await;
        if code == 0 && sink.adapter_event_count() == 0 {
            tracing::warn!(
                attempt_id = %assignment.attempt_id,
                "adapter exited 0 but produced no stdout/stderr events; task output may be empty (silent agent?)"
            );
        }
        last_code = code;
        last_kill_reason = kill_reason;

        // Agent failed (non-zero exit): no fixable validation to feed back; stop.
        if code != 0 {
            break false;
        }
        // Validate; if it passes we're done. If it fails and a retry is left,
        // feed the validation error back into the prompt and re-spawn.
        if let Some(cmd) = &assignment.validation_command {
            let v = run_validation(&workdir, cmd, &sink).await;
            let fail = match &v {
                Ok(vcode) => *vcode != 0,
                Err(e) => {
                    tracing::error!("validation failed to run: {e}");
                    true
                }
            };
            if fail {
                if round < retries {
                    let log = tokio::fs::read_to_string(&validation_log)
                        .await
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| "(no validation output)".into());
                    tracing::info!(attempt_id = %assignment.attempt_id, round, "validation failed; feeding error back to agent");
                    sink.push(
                        EventKind::Log.to_event_type(),
                        json!({ "kind": "feedback", "round": round, "retrying": true }),
                    )
                    .await;
                    prompt = format!(
                        "{orig}\n\nValidation failed (round {round}):\n```\n{log}\n```\nFix the code so the validation passes.",
                        orig = assignment.prompt
                    );
                    round += 1;
                    continue;
                }
                break false;
            }
        }
        break true;
    };
    // ponytail: flusher kept alive through finalize/artifacts/report_complete so
    // events buffered during a CP outage keep being retried and are delivered
    // once the CP recovers (the durable outbox also retains them). Aborted
    // after report_complete so a terminal attempt doesn't leak the task.
    // (was: flusher.abort() here, before the post-adapter sends.)

    let node_name = cfg.node_name.clone();
    let patch_path = workdir.join("changes.patch");
    // Stage 2.3: keep the per-attempt repo_dir/branch so the worktree and its
    // ref can be reclaimed after the attempt is terminal (finalize takes ws).
    let cleanup_repo = ws.repo_dir.clone();
    let cleanup_branch =
        (ws.is_git && ws.branch.is_some()).then(|| ws.branch.clone().unwrap_or_default());
    let cleanup_path = ws.path.clone();
    let commit_sha =
        tokio::task::spawn_blocking(move || git::finalize_workspace(ws, node_name.as_str()))
            .await??;

    let code = last_code;
    let error_code: Option<String> = if code == 0 {
        if validation_passed {
            None
        } else {
            Some("validation_failed".into())
        }
    } else {
        Some(last_kill_reason.unwrap_or("agent_failed").into())
    };

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
    // Stage 2.1: drain all pending events from the durable outbox BEFORE the
    // completion so the CP sees the full event stream before marking the task
    // terminal. Read from disk (ground truth), not RAM — an aborted flusher's
    // in-flight batch is gone from RAM but still on disk here.
    sink.drain_outbox(tokio::time::Instant::now() + Duration::from_secs(60))
        .await;
    report_complete(
        &client,
        &cfg.server,
        &assignment.attempt_id,
        code,
        commit_sha,
        error_code,
        None,
        &cfg.completion_outbox,
    )
    .await;
    // Ground-truth redelivery: any events still on disk (e.g. the CP flapped
    // again, or the pre-completion drain couldn't send) are delivered now.
    // The CP is up (report_complete succeeded).
    sink.drain_outbox(tokio::time::Instant::now() + Duration::from_secs(15))
        .await;
    flusher.abort();
    // Stage 2.3: reclaim the per-attempt worktree and branch now the attempt
    // is terminal. Best-effort in a spawn_blocking so a stuck worktree never
    // turns a successful attempt terminal.
    tokio::task::spawn_blocking(move || {
        git::cleanup_workspace(
            &cleanup_path,
            cleanup_repo.as_deref(),
            cleanup_branch.as_deref(),
        )
    })
    .await
    .ok();
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
        // Stage 2.1: check the response status and retry transient failures;
        // the upload is idempotent per (attempt_id, name) on the control plane.
        match send_with_retry(
            client
                .post(format!("{server}/v1/node/attempts/{attempt_id}/artifacts"))
                .json(&req),
            10,
        )
        .await
        {
            Ok(s) if s.is_success() => {}
            Ok(s) => tracing::warn!("artifact {name} upload got {s} for {attempt_id}"),
            Err(e) => tracing::warn!("artifact {name} upload failed: {e}"),
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
async fn send_with_retry(
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

#[allow(clippy::too_many_arguments)]
async fn report_complete(
    client: &reqwest::Client,
    server: &str,
    attempt_id: &str,
    exit_code: i32,
    commit_sha: Option<String>,
    error_code: Option<String>,
    acp_session_id: Option<String>,
    completion_outbox: &outbox::CompletionOutbox,
) {
    let url = format!("{}/v1/node/attempts/{}/complete", server, attempt_id);
    let req = CompleteAttemptRequest {
        exit_code,
        commit_sha,
        error_code,
        acp_session_id,
    };
    // Stage 2.1: persist the completion durably so a daemon kill before the CP
    // acks it is redelivered on the next startup (complete_attempt is
    // idempotent on terminal attempts).
    if let Err(e) = completion_outbox.record(attempt_id, &req) {
        tracing::warn!("completion outbox record failed for {attempt_id}: {e}");
    }
    // Completion is terminal and must be delivered; retry transient and 5xx
    // failures with backoff. The durable outbox also covers the daemon-kill gap.
    match send_with_retry(client.post(&url).json(&req), 20).await {
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
async fn ack_attempt(client: &reqwest::Client, server: &str, attempt_id: &str) {
    let url = format!("{}/v1/node/attempts/{}/ack", server, attempt_id);
    if let Err(e) = client.post(&url).send().await {
        tracing::warn!("ack failed for {attempt_id}: {e}");
    }
}

/// Stage 3.2: open an agent session for this attempt (best-effort; a failed
/// CP call only warns, it must not block the attempt).
async fn create_agent_session(
    client: &reqwest::Client,
    server: &str,
    attempt_id: &str,
    adapter: &str,
) {
    let url = format!("{}/v1/node/attempts/{}/session", server, attempt_id);
    let req = CreateAgentSessionRequest {
        adapter: adapter.to_string(),
    };
    if let Err(e) = client.post(&url).json(&req).send().await {
        tracing::warn!("agent session create failed for {attempt_id}: {e}");
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
        adapters: cfg.adapters.iter().map(|s| s.id.clone()).collect(),
        repositories: cfg.repositories.clone(),
        max_concurrency: cfg.max_concurrency,
        agent_version: cfg.agent_version.clone(),
        protocol_version: Some(agentgrid_common::NODE_PROTOCOL_VERSION.into()),
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

    // Stage 2.1: redeliver any completion records a prior (killed) run recorded
    // but never got a CP ack for. Runs with the node-credentialed client so the
    // /v1/node/attempts/{id}/complete route authenticates. complete_attempt is
    // idempotent on terminal attempts, so this is safe.
    for c in cfg.completion_outbox.pending().unwrap_or_default() {
        let req = c.to_request();
        tracing::info!(attempt_id = %c.attempt_id, "redelivering durable completion");
        let url = format!("{}/v1/node/attempts/{}/complete", cfg.server, c.attempt_id);
        match send_with_retry(client.post(&url).json(&req), 20).await {
            Ok(s) if s.is_success() => {
                let _ = cfg.completion_outbox.ack(&c.attempt_id);
            }
            Ok(s) => tracing::warn!("completion redelivery got {s} for {}", c.attempt_id),
            Err(e) => tracing::warn!("completion redelivery failed for {}: {e}", c.attempt_id),
        }
    }

    // Heartbeat loop: publish status/load/capabilities periodically.
    let hb_sem = sem.clone();
    let hb_cfg = cfg.clone();
    let hb_client = client.clone();
    let hb_node_id = cred.node_id.clone();
    tokio::spawn(async move {
        loop {
            // Stage 2.4: only advertise as Online when every configured adapter
            // binary is present; a missing one degrades the node. Stage 3.2:
            // report per-adapter capabilities (version + readiness) each beat.
            // Stage 2.5 ops: flag the node Degraded when free disk falls below
            // AGENTGRID_DISK_LOW_MB (default 1 GB) so it shows up in `ag nodes
            // list` and the scheduler can avoid stacking work onto a full host.
            let free_disk = read_free_disk_mb(&hb_cfg.workspace_root);
            let disk_low_mb = std::env::var("AGENTGRID_DISK_LOW_MB")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1024);
            let disk_low = free_disk < disk_low_mb;
            if disk_low {
                tracing::warn!(
                    "free disk low on node {}: {} MB < {} MB threshold; marking degraded",
                    hb_cfg.node_name,
                    free_disk,
                    disk_low_mb
                );
            }
            let mut capabilities = Vec::new();
            let all_ok = {
                let mut ok = true;
                for a in &hb_cfg.adapters {
                    let probe = if resolve_acp_launch(&a.id).is_some() {
                        AdapterProbe {
                            found: true,
                            version: None,
                        }
                    } else {
                        let bin = adapter_bin_name(&a.id);
                        probe_adapter(&bin).await
                    };
                    if !probe.found {
                        ok = false;
                    }
                    capabilities.push(AdapterCapability {
                        id: a.id.clone(),
                        version: probe.version,
                        ready: probe.found,
                    });
                }
                ok
            } && !disk_low;
            let status = if all_ok {
                NodeStatus::Online
            } else {
                NodeStatus::Degraded
            };
            tokio::time::sleep(Duration::from_secs(hb_cfg.heartbeat_secs)).await;
            let active = hb_cfg.max_concurrency - hb_sem.available_permits() as u32;
            let req = HeartbeatRequest {
                status: Some(status),
                name: hb_cfg.node_name.clone(),
                adapters: hb_cfg.adapters.iter().map(|s| s.id.clone()).collect(),
                repositories: hb_cfg.repositories.clone(),
                max_concurrency: hb_cfg.max_concurrency,
                agent_version: hb_cfg.agent_version.clone(),
                load_avg: read_load_avg(),
                free_disk_mb: free_disk,
                active_attempts: active,
                capabilities,
                protocol_version: Some(agentgrid_common::NODE_PROTOCOL_VERSION.into()),
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
    for a in &cfg.adapters {
        let bin = format!("adapter-{}", a.id.replace('_', "-"));
        let probe = probe_adapter(&bin).await;
        if probe.found {
            tracing::info!(adapter = %a.id, version = ?probe.version, "adapter detected");
        } else {
            tracing::warn!(
                adapter = %a.id,
                "adapter binary {bin} not found in PATH; node will report degraded until installed"
            );
        }
    }
    tokio::fs::create_dir_all(&cfg.workspace_root).await?;
    // Stage 2.3: reclaim workspace dirs + worktree gitlinks a prior (killed)
    // run left behind. Default 24h retention; tune with
    // AGENTGRID_WORKSPACE_RETENTION_HOURS (0 disables pruning).
    let retention_h: u64 = std::env::var("AGENTGRID_WORKSPACE_RETENTION_HOURS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    if retention_h > 0 {
        tokio::task::spawn_blocking({
            let ws = cfg.workspace_root.clone();
            let repos = cfg.repository_root.clone();
            move || {
                git::prune_stale_workspaces(
                    &ws,
                    &repos,
                    std::time::Duration::from_secs(retention_h * 3600),
                )
            }
        })
        .await
        .ok();
    }
    let cred = load_or_enroll(&cfg).await?;
    tracing::info!(
        node_id = %cred.node_id,
        server = %cfg.server,
        adapters = ?cfg.adapters,
        "node daemon starting"
    );
    poll_loop(cfg, cred).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stage 9.1: a Bash `cat` at default L2 is auto-allowed; `rm -rf` is
    /// auto-denied; `git push` yields `Ask` (None) → falls to approval flow.
    #[test]
    fn policy_decision_short_circuits_bash() {
        let allow = policy_decision(
            &json!({ "tool": "Bash", "input": "cat README.md" }),
            AutonomyLevel::L2,
        )
        .unwrap();
        assert_eq!(allow.0, PolicyDecision::Allow);

        let deny = policy_decision(
            &json!({ "tool": "Bash", "input": "rm -rf /tmp/x" }),
            AutonomyLevel::L2,
        )
        .unwrap();
        assert_eq!(deny.0, PolicyDecision::Deny);

        assert_eq!(
            policy_decision(
                &json!({ "tool": "Bash", "input": "git push" }),
                AutonomyLevel::L2,
            ),
            None,
            "Ask (git push @ L2) must fall through to the approval flow"
        );
    }

    #[test]
    fn policy_decision_non_bash_is_none() {
        // Non-Bash tools are never short-circuited locally → operator decides.
        assert_eq!(
            policy_decision(
                &json!({ "tool": "WebFetch", "input": "x" }),
                AutonomyLevel::L4
            ),
            None
        );
        assert_eq!(
            policy_decision(&json!({ "tool": "Bash" }), AutonomyLevel::L4),
            None,
            "missing input → no short-circuit"
        );
    }

    /// Stage 9.2: only trusted `(name, source)` skills are listed, sorted by
    /// name; untrusted/absent entries are omitted; an empty trusted set yields
    /// an empty block (fail-closed).
    #[test]
    fn render_trusted_skills_block_filters_and_sorts() {
        use agentgrid_skills::{DiscoveredSkill, Skill, SkillSource};
        use std::collections::HashMap;
        let mk = |name: &str, src: SkillSource, desc: &str| DiscoveredSkill {
            skill: Skill {
                name: name.into(),
                description: desc.into(),
                license: None,
                compatibility: None,
                allowed_tools: vec![],
                metadata: HashMap::new(),
                body: String::new(),
            },
            source: src,
            path: std::path::PathBuf::from(format!("/x/{name}/SKILL.md")),
        };
        let discovered = vec![
            mk("zebra", SkillSource::User, "last"),
            mk("alpha", SkillSource::Project, "first multi\nline desc"),
            mk("untrusted-one", SkillSource::Project, "x"),
        ];
        let mut trusted = std::collections::HashSet::new();
        trusted.insert(("alpha".to_string(), "project".to_string()));
        trusted.insert(("zebra".to_string(), "user".to_string()));
        let out = render_trusted_skills_block(&discovered, &trusted);
        assert!(out.contains("Available agent skills (operator-trusted)"));
        assert!(
            out.contains("- alpha (project): first"),
            "alpha trusted + rendered with first line of description"
        );
        assert!(out.contains("- zebra (user): last"));
        assert!(
            !out.contains("untrusted-one"),
            "untrusted skill must be omitted (fail-closed)"
        );
        assert!(out.find("alpha").unwrap() < out.find("zebra").unwrap());
        assert_eq!(
            render_trusted_skills_block(&discovered, &std::collections::HashSet::new()),
            ""
        );
    }

    /// A temporary EventOutbox for a given attempt, isolated per test run.
    fn test_outbox(attempt_id: &str) -> Arc<outbox::EventOutbox> {
        let dir = std::env::temp_dir().join(format!("ag-outbox-test-{}", uuid::Uuid::new_v4()));
        Arc::new(outbox::EventOutbox::open(&dir, attempt_id).unwrap())
    }

    #[tokio::test]
    async fn event_sink_drops_logs_over_cap_but_keeps_terminal_state() {
        // Stage 2.1: backpressure. A chatty agent's stdout/stderr are dropped
        // once the RAM buffer exceeds the cap; status/result/error are never
        // dropped, and exactly one `output_truncated` notice is emitted.
        std::env::set_var("AGENTGRID_EVENT_BUF_BYTES", "64");
        let sink = EventSink::new(
            "a1".into(),
            reqwest::Client::new(),
            "http://x".into(),
            test_outbox("a1"),
        );
        // Each line ~ 100 bytes; 64-byte cap overflows after the first.
        for _ in 0..50 {
            sink.push(EventType::Stdout, serde_json::json!({ "text": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" }))
                .await;
        }
        // A terminal-state event must still be accepted despite the overflow.
        sink.push(EventType::Result, serde_json::json!({ "ok": true }))
            .await;
        let buf = sink.buf.lock().await;
        let has_result = buf.iter().any(|e| e.r#type == EventType::Result);
        let truncation_notices = buf
            .iter()
            .filter(|e| e.payload.get("event").and_then(|v| v.as_str()) == Some("output_truncated"))
            .count();
        assert!(has_result, "terminal-state event must survive truncation");
        assert_eq!(truncation_notices, 1, "exactly one output_truncated notice");
        std::env::remove_var("AGENTGRID_EVENT_BUF_BYTES");
    }

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
        let sink = EventSink::new(
            "a1".into(),
            reqwest::Client::new(),
            "http://x".into(),
            test_outbox("a1"),
        );
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
        let sink = EventSink::new(
            "a1".into(),
            reqwest::Client::new(),
            "http://x".into(),
            test_outbox("a1"),
        );
        read_stream(reader, sink, "stdout", vec![], Some(raw.clone())).await;
        let got = tokio::fs::read_to_string(&raw_path).await.unwrap();
        assert!(got.contains("hello"), "structured line mirrored: {got}");
        assert!(got.contains("not json"), "unparsed line mirrored: {got}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn mask_secrets_replaces_known_substring() {
        assert_eq!(
            mask_secrets("token=sk-12345 and more", &vec!["sk-12345".to_string()]),
            "token=*** and more"
        );
        // No secrets configured -> unchanged.
        assert_eq!(mask_secrets("nothing", &vec![]), "nothing");
    }

    #[test]
    fn adapter_bin_name_maps_id() {
        assert_eq!(adapter_bin_name("mock"), "adapter-mock");
        assert_eq!(adapter_bin_name("claude"), "adapter-claude");
        assert_eq!(adapter_bin_name("my_adapter"), "adapter-my-adapter");
    }

    #[test]
    fn resolve_adapter_bin_rejects_missing() {
        assert!(resolve_adapter_bin("definitely-not-an-adapter-xyz").is_none());
    }

    #[test]
    fn retryable_status_codes() {
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_retryable_status(StatusCode::GATEWAY_TIMEOUT));
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(!is_retryable_status(StatusCode::OK));
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
    }

    /// Accept anything on a port and answer 200 OK, so the daemon's event sink
    /// flushes without retry/backoff noise during the test.
    async fn dummy_ingest_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .await;
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn drive_acp_session_runs_fake_agent_and_streams_events() {
        // Make the test-only ACP agent discoverable on PATH. It is built into
        // the same target dir; locate it relative to CARGO_MANIFEST_DIR.
        let manifest = env!("CARGO_MANIFEST_DIR");
        let fake = [
            "../../target/debug/adapter-fake-acp",
            "../../target/release/adapter-fake-acp",
        ]
        .iter()
        .map(|p| std::path::Path::new(manifest).join(p))
        .find(|p| p.is_file())
        .expect("fake ACP agent built");
        let bin_dir = fake.parent().unwrap();
        let orig = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{orig}", bin_dir.display()));

        let server = dummy_ingest_server().await;
        let cfg = Config {
            server: server.clone(),
            node_name: "test".into(),
            workspace_root: std::env::temp_dir().join("ag-acp-ws"),
            max_concurrency: 2,
            agent_version: "0.1.0".into(),
            adapters: vec![AdapterSpec {
                id: "fake-acp".into(),
                protocol: AdapterProtocol::Acp,
            }],
            repositories: vec!["*".into()],
            heartbeat_secs: 10,
            enroll_token: None,
            credential_path: std::env::temp_dir().join("ag-acp-cred.json"),
            repository_root: std::env::temp_dir().join("ag-acp-repos"),
            secrets: vec![],
            sandbox: sandbox::SandboxKind::None,
            adapter_env: vec![],
            outbox_root: std::env::temp_dir()
                .join(format!("ag-acp-outbox-{}", uuid::Uuid::new_v4())),
            completion_outbox: Arc::new(
                outbox::CompletionOutbox::open(
                    &std::env::temp_dir().join(format!("ag-acp-comp-{}", uuid::Uuid::new_v4())),
                )
                .unwrap(),
            ),
            autonomy: AutonomyLevel::default(),
        };
        let ws = std::env::temp_dir().join(format!(
            "ag-acp-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&ws).unwrap();
        let assignment = Assignment {
            attempt_id: format!("att-{}", uuid::Uuid::new_v4()),
            task_id: "t1".into(),
            repository: "*".into(),
            prompt: "do the thing".into(),
            adapter: "fake-acp".into(),
            number: 1,
            timeout_secs: 30,
            git_url: String::new(),
            default_branch: String::new(),
            validation_command: None,
            base_commit: None,
            parent_acp_session_id: None,
        };
        let sink = EventSink::new(
            assignment.attempt_id.clone(),
            reqwest::Client::new(),
            cfg.server.clone(),
            Arc::new(outbox::EventOutbox::open(&cfg.outbox_root, &assignment.attempt_id).unwrap()),
        );
        let res = drive_acp_session(
            &cfg,
            &reqwest::Client::new(),
            &assignment,
            &ws,
            sink.clone(),
        )
        .await
        .unwrap();
        assert!(res.success, "ACP session should succeed");
        assert_eq!(res.error_code, None);
        assert_eq!(
            res.session_id.as_deref(),
            Some("sess-fake-1"),
            "session_id from session/new is reported back (Stage 11.5)"
        );
        assert!(
            sink.adapter_event_count() >= 2,
            "two session/update events should stream; got {}",
            sink.adapter_event_count()
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    /// Stage 5: an ACP subprocess that hangs mid-frame (writes a truncated
    /// JSON line then blocks forever) must be torn down by the session
    /// timeout — the attempt fails with `timeout`, no hang.
    #[tokio::test]
    async fn drive_acp_session_hang_mid_frame_times_out() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let fake = [
            "../../target/debug/adapter-fake-acp",
            "../../target/release/adapter-fake-acp",
        ]
        .iter()
        .map(|p| std::path::Path::new(manifest).join(p))
        .find(|p| p.is_file())
        .expect("fake ACP agent built");
        let bin_dir = fake.parent().unwrap();
        let orig = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{orig}", bin_dir.display()));
        // Fake agent: write a truncated JSON-RPC line then block forever.
        std::env::set_var("PATH", format!("{}:{orig}", bin_dir.display()));
        let server = dummy_ingest_server().await;
        // Fake agent: write a truncated JSON-RPC line then block forever.
        // Pass via adapter_env so it only reaches THIS child (no env cross-talk
        // with parallel ACP tests in the same process).
        let cfg = Config {
            server: server.clone(),
            node_name: "test".into(),
            workspace_root: std::env::temp_dir().join("ag-acp-ws-hang"),
            max_concurrency: 2,
            agent_version: "0.1.0".into(),
            adapters: vec![AdapterSpec {
                id: "fake-acp".into(),
                protocol: AdapterProtocol::Acp,
            }],
            repositories: vec!["*".into()],
            heartbeat_secs: 10,
            enroll_token: None,
            credential_path: std::env::temp_dir().join("ag-acp-cred-hang.json"),
            repository_root: std::env::temp_dir().join("ag-acp-repos-hang"),
            secrets: vec![],
            sandbox: sandbox::SandboxKind::None,
            adapter_env: vec![("AG_FAKE_HANG".into(), "1".into())],
            outbox_root: std::env::temp_dir()
                .join(format!("ag-acp-outbox-hang-{}", uuid::Uuid::new_v4())),
            completion_outbox: Arc::new(
                outbox::CompletionOutbox::open(
                    &std::env::temp_dir()
                        .join(format!("ag-acp-comp-hang-{}", uuid::Uuid::new_v4())),
                )
                .unwrap(),
            ),
            autonomy: AutonomyLevel::default(),
        };
        let ws = std::env::temp_dir().join(format!(
            "ag-acp-hang-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&ws).unwrap();
        let assignment = Assignment {
            attempt_id: format!("att-hang-{}", uuid::Uuid::new_v4()),
            task_id: "t1".into(),
            repository: "*".into(),
            prompt: "do the thing".into(),
            adapter: "fake-acp".into(),
            number: 1,
            timeout_secs: 3,
            git_url: String::new(),
            default_branch: String::new(),
            validation_command: None,
            base_commit: None,
            parent_acp_session_id: None,
        };
        let sink = EventSink::new(
            assignment.attempt_id.clone(),
            reqwest::Client::new(),
            cfg.server.clone(),
            Arc::new(outbox::EventOutbox::open(&cfg.outbox_root, &assignment.attempt_id).unwrap()),
        );
        let res = tokio::time::timeout(
            Duration::from_secs(20),
            drive_acp_session(
                &cfg,
                &reqwest::Client::new(),
                &assignment,
                &ws,
                sink.clone(),
            ),
        )
        .await
        .expect("drive_acp_session must not hang on a mid-frame ACP death")
        .unwrap();
        assert!(!res.success, "hung ACP session should not succeed");
        assert_eq!(
            res.error_code.as_deref(),
            Some("timeout"),
            "expected timeout error_code, got {:?}",
            res.error_code
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    #[test]
    fn agent_profile_reads_inline_and_none() {
        std::env::set_var("AGENTGRID_AGENT_PROFILE_TESTAG", "be brief");
        assert_eq!(agent_profile("testag"), Some("be brief".into()));
        std::env::set_var("AGENTGRID_AGENT_PROFILE_TESTAG", "");
        assert_eq!(agent_profile("testag"), None);
        std::env::remove_var("AGENTGRID_AGENT_PROFILE_TESTAG");
    }
}
