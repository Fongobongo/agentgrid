//! Stage 2.1: a durable JSONL outbox so events/completions survive a node
//! daemon crash or kill. Per-attempt event file + a completions file. The CP
//! ingest is idempotent (`ON CONFLICT (attempt_id, sequence) DO NOTHING`) and
//! `complete_attempt` is idempotent on terminal attempts, so redelivery after a
//! restart is safe — we only need durability of the un-acked tail.
//!
//! Design (ponytail: zero new deps, append-only JSONL):
//! - Each event is one JSON line: `{"seq":N,"type":...,"payload":...}`.
//! - `push` appends a line; `drain_pending` reads pending lines and removes
//!   acked ones by rewriting the file under a Mutex.
//! - Completion: one line per attempt in `completions.jsonl`; redelivered
//!   completions are no-ops on the CP (idempotent terminal ack).
//!
//! Terminal events (Status, Tool, Artifact, Result, Error) have reserved
//! capacity in the spool (TERMINAL_RESERVED_BYTES) so they can always be
//! written even when the log event spool hits the limit.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use agentgrid_common::{CompleteAttemptRequest, IncomingEvent};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// Reserved capacity for terminal events beyond the spool limit (64 KiB).
// Allows terminal state transitions to be durably recorded even when
// the outbox is full of Stdout/Stderr/Metric events.
const TERMINAL_RESERVED_BYTES: u64 = 64 * 1024;
/// Plan 370: completion outbox compacts (markers folded, acked lines
/// dropped) once the file exceeds this many bytes. Default 1 MiB; tune via
/// `AGENTGRID_COMPLETION_COMPACT_BYTES`.
fn compact_threshold() -> u64 {
    std::env::var("AGENTGRID_COMPLETION_COMPACT_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024 * 1024)
}

#[derive(Serialize, Deserialize)]
struct EventLine {
    seq: u64,
    #[serde(rename = "type")]
    ty: serde_json::Value,
    payload: serde_json::Value,
    /// Enqueue timestamp (epoch ms) for outbox-age metrics. `#[serde(default)]`
    /// keeps pre-existing spool lines (written before this field) parseable;
    /// they report age 0 like before.
    #[serde(default)]
    enqueued_at: u64,
}

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0) as u64
}

/// A durable event spool for one attempt. Append-only JSONL file guarded by a
/// Mutex; acked events are dropped by rewriting the file with the survivors.
///
/// Spool limit: if the file grows past `spool_limit_bytes` (env
/// `AGENTGRID_OUTBOX_SPOOL_LIMIT_MB`, default 256 MiB; 0 = unlimited),
/// `push` returns `Err(push::Error::SpoolFull)` so the sink can fail-closed
/// (emit a terminal `spool_full` error + stop buffering) instead of filling
/// the disk when the control plane is unreachable for a long time.
pub struct EventOutbox {
    path: PathBuf,
    /// Hardening P0 item 10: outbox root, used for the global spool quota scan.
    root: PathBuf,
    file: Mutex<()>,
    /// Group-fsync bookkeeping: `push` fsyncs at most once per
    /// `fsync_interval` (see `open`); 0 restores fsync-per-event.
    last_fsync: Mutex<std::time::Instant>,
    fsync_interval: std::time::Duration,
    spool_limit_bytes: u64,
    /// Hardening P0 item 10: global cap across ALL per-attempt spools plus the
    /// completion file (env `AGENTGRID_OUTBOX_QUOTA_BYTES` / `_MB`; 0 = off).
    /// Best-effort ceiling — a single oversized event may overshoot.
    quota_bytes: u64,
}

/// Errors from [`EventOutbox::push`]. `SpoolFull` is recoverable: the caller
/// should stop accepting events and terminate the attempt with `spool_full`.
#[derive(Debug)]
pub enum PushError {
    SpoolFull,
    Other(anyhow::Error),
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushError::SpoolFull => write!(f, "outbox spool full (limit reached)"),
            PushError::Other(e) => write!(f, "outbox push failed: {e}"),
        }
    }
}

impl std::error::Error for PushError {}

impl EventOutbox {
    pub fn open(dir: &Path, attempt_id: &str) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        // attempt_id is a UUID-ish token from the CP; sanitize defensively.
        let safe = attempt_id
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-')
            .collect::<String>();
        let spool_limit_bytes = std::env::var("AGENTGRID_OUTBOX_SPOOL_LIMIT_BYTES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| {
                std::env::var("AGENTGRID_OUTBOX_SPOOL_LIMIT_MB")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|mb| mb * 1024 * 1024)
                    .unwrap_or(256 * 1024 * 1024)
            });
        let quota_bytes = std::env::var("AGENTGRID_OUTBOX_QUOTA_BYTES")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or_else(|| {
                std::env::var("AGENTGRID_OUTBOX_QUOTA_MB")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .map(|mb| mb * 1024 * 1024)
                    .unwrap_or(1024 * 1024 * 1024)
            });
        // Group-fsync: per-event fsync measured ~3 ms on cloud disks and
        // capped a chatty adapter at ~300 events/s (tests/e2e/measure-flush.sh).
        // Batching the fsync to at most one per `fsync_interval` keeps the
        // same crash guarantees for process crashes (page cache survives) and
        // widens ONLY the machine-crash loss window to < interval.
        // AGENTGRID_OUTBOX_FSYNC_MS=0 restores the old fsync-per-event.
        let fsync_interval_ms = std::env::var("AGENTGRID_OUTBOX_FSYNC_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(100);
        Ok(Self {
            path: dir.join(format!("{safe}.jsonl")),
            root: dir.to_path_buf(),
            file: Mutex::new(()),
            last_fsync: Mutex::new(std::time::Instant::now()),
            fsync_interval: std::time::Duration::from_millis(fsync_interval_ms),
            spool_limit_bytes,
            quota_bytes,
        })
    }

    /// Append an event durably. Returns immediately after fsync. Returns
    /// `Err(PushError::SpoolFull)` when the on-disk spool exceeds the limit so
    /// the caller can fail-closed instead of filling the disk.
    pub fn push(&self, ev: &IncomingEvent) -> std::result::Result<(), PushError> {
        let _g = self.file.lock().unwrap();
        // Hardening P0 item 10: global quota across all spools. Best-effort:
        // a full recursive scan on every push is too hot, so we only check
        // when this attempt's own file is under its per-attempt limit (the
        // common case) and the scan is cheap for typical outbox sizes. The
        // per-attempt spool limit remains the primary guard; the quota is a
        // second ceiling for many concurrent attempts.
        if self.quota_bytes > 0 {
            if let Ok(total) = total_bytes(&self.root) {
                if total >= self.quota_bytes {
                    return Err(PushError::SpoolFull);
                }
            }
        }
        // Check the cap before appending: if already over, refuse. The limit
        // is a safety ceiling, not an exact bound (one event may overshoot).
        if self.spool_limit_bytes > 0 {
            if let Ok(meta) = std::fs::metadata(&self.path) {
                // Hardening P1 item 34: reserve capacity for terminal events
                // (Status, Tool, Artifact, Result, Error) so they can always
                // get through even when the spool is full of log events.
                // Non-terminal events (Stdout, Stderr, Metric) are blocked at
                // the hard limit. Terminal events can exceed the limit by
                // TERMINAL_RESERVED_BYTES.
                let is_terminal = matches!(
                    ev.r#type,
                    agentgrid_common::EventType::Status
                        | agentgrid_common::EventType::Tool
                        | agentgrid_common::EventType::Artifact
                        | agentgrid_common::EventType::Result
                        | agentgrid_common::EventType::Error
                );
                let effective_limit = if is_terminal {
                    self.spool_limit_bytes
                        .saturating_add(TERMINAL_RESERVED_BYTES)
                } else {
                    self.spool_limit_bytes
                };
                if meta.len() >= effective_limit {
                    return Err(PushError::SpoolFull);
                }
            }
        }
        let line = EventLine {
            seq: ev.sequence,
            ty: serde_json::to_value(ev.r#type).unwrap_or(serde_json::Value::Null),
            payload: ev.payload.clone(),
            enqueued_at: now_epoch_ms(),
        };
        let mut s = serde_json::to_string(&line)
            .context("encode outbox line")
            .map_err(PushError::Other)?;
        s.push('\n');
        // O_APPEND via OpenOptions ensures atomic appends for lines < PIPE_BUF.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open outbox {}", self.path.display()))
            .map_err(PushError::Other)?;
        f.write_all(s.as_bytes())
            .map_err(|e| PushError::Other(e.into()))?;
        let mut last = self.last_fsync.lock().unwrap();
        if self.fsync_interval.is_zero() || last.elapsed() >= self.fsync_interval {
            f.sync_data().map_err(|e| PushError::Other(e.into()))?;
            *last = std::time::Instant::now();
        }
        // ponytail: unsynced tail ≤ `fsync_interval`; ack()'s rewrite fsyncs,
        // so delivered events always re-anchor durability.
        Ok(())
    }

    /// Audit X-N1: drop the attempt's spool file once the attempt is terminal
    /// and its events were drained. Startup recovery never replays event
    /// outboxes of terminal attempts (attempt ids are unique per attempt — a
    /// retry gets a new id), so a surviving file is dead weight that still
    /// counts against the global quota scan; enough accumulated files push
    /// every future `push` into `SpoolFull` until an operator wipes history.
    pub fn discard(&self) {
        let _g = self.file.lock().unwrap();
        match std::fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(path = %self.path.display(), "outbox discard failed: {e}")
            }
        }
    }

    /// Read all currently-pending events (in sequence order). Hardening P0
    /// item 10: an unparseable middle line is moved to
    /// `<dir>/quarantine/<file>-<ts>` instead of silently dropped — a torn
    /// write never takes down the rest of the spool, and the damaged record
    /// stays inspectable for recovery.
    pub fn pending(&self) -> Result<VecDeque<IncomingEvent>> {
        let _g = self.file.lock().unwrap();
        let mut out = VecDeque::new();
        let content = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(VecDeque::new()),
            Err(e) => return Err(e.into()),
        };
        let mut quarantined = String::new();
        let mut clean = String::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<EventLine>(line) {
                Ok(l) => {
                    clean.push_str(line);
                    clean.push('\n');
                    let ty: agentgrid_common::EventType =
                        serde_json::from_value(l.ty).unwrap_or(agentgrid_common::EventType::Status);
                    out.push_back(IncomingEvent {
                        sequence: l.seq,
                        r#type: ty,
                        payload: l.payload,
                    });
                }
                Err(_) => {
                    quarantined.push_str(line);
                    quarantined.push('\n');
                }
            }
        }
        if !quarantined.is_empty() {
            quarantine_rewrite(&self.path, &clean, &quarantined)?;
        }
        Ok(out)
    }

    /// Drop acked sequences (those in `acked`) by rewriting the file with the
    /// survivors. Pending lines remain.
    pub fn ack(&self, acked: &[u64]) -> Result<()> {
        let _g = self.file.lock().unwrap();
        let content = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        // Hardening P1 item 34: O(1) membership via a set instead of the
        // O(n×m) `acked.contains` scan over every line.
        let acked_set: std::collections::HashSet<u64> = acked.iter().copied().collect();
        let mut survivors = String::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let l: EventLine = match serde_json::from_str(line) {
                Ok(l) => l,
                // Keep unparseable lines rather than dropping evidence.
                Err(_) => {
                    survivors.push_str(line);
                    survivors.push('\n');
                    continue;
                }
            };
            if !acked_set.contains(&l.seq) {
                survivors.push_str(line);
                survivors.push('\n');
            }
        }
        // Atomic replace: write tmp + fsync + rename (Hardening P1 item 11).
        let tmp = self.path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, &survivors)?;
        {
            let f = std::fs::OpenOptions::new().write(true).open(&tmp)?;
            f.sync_all()?;
        }
        fsync_parent(&tmp)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// One durable completion record per attempt (idempotent redelivery on the CP).
pub struct CompletionOutbox {
    path: PathBuf,
    file: Mutex<()>,
}

/// Hardening P1 item 11: fsync the parent directory of `path` so a rename /
/// temp-file replace is durable on power loss. Without this, a successful
/// rename in the page cache can disappear after a crash even though the file
/// data is durable. No-op (warns) on platforms without `fdatasync`; best-effort
/// — errors are surfaced so the caller can decide, but never panic.
fn fsync_parent(path: &std::path::Path) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let dir = match std::fs::File::open(parent) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    // SAFETY: fdatasync(fd) on an open directory fd is a safe POSIX op.
    let rc = unsafe { libc_fdatasync(dir.as_raw_fd()) };
    drop(dir);
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// libc::fdatasync shim — kept here so the module compiles on unix only (the
/// daemon is Linux-only per ADR). Returns 0 on success, -1 with errno on error.
#[cfg(unix)]
unsafe fn libc_fdatasync(fd: std::os::unix::io::RawFd) -> i32 {
    libc::fdatasync(fd)
}

#[derive(Serialize, Deserialize)]
pub struct CompletionLine {
    pub attempt_id: String,
    pub exit_code: i32,
    pub commit_sha: Option<String>,
    pub error_code: Option<String>,
    pub acp_session_id: Option<String>,
    /// Hardening P0 item 8: fencing token echoed on the redelivered completion
    /// so the CP rejects a stale writer with 409. `#[serde(default)]` keeps old
    /// `completions.jsonl` files (pre-token) parseable — they redeliver with a
    /// blank token, which the CP accepts only if the attempt has no token yet.
    #[serde(default)]
    pub fencing_token: String,
    /// Hardening P0 item 10: the full completion payload is now durable — plan,
    /// provenance, resolved base and remote head snapshots survive a first-send
    /// failure and are re-sent on redelivery instead of being dropped. All
    /// `#[serde(default)]` so pre-0038 `completions.jsonl` files stay parseable
    /// (they redeliver those fields as `None`, matching the old behaviour).
    #[serde(default)]
    pub resolved_base_sha: Option<String>,
    #[serde(default)]
    pub remote_head_at_start: Option<String>,
    #[serde(default)]
    pub remote_head_at_finish: Option<String>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub provenance: Option<agentgrid_common::ProvenanceRecord>,
    #[serde(default)]
    pub pending_artifacts: Vec<String>,
    /// Competitor-gap feature (convergence metrics): feedback-loop rounds.
    /// `#[serde(default)]` keeps pre-0077 `completions.jsonl` lines parseable.
    #[serde(default)]
    pub validation_rounds: u32,
}

impl CompletionOutbox {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            path: dir.join("completions.jsonl"),
            file: Mutex::new(()),
        })
    }

    /// Record a completion durably (idempotent: replaces any existing line
    /// for this attempt so the latest exit/error wins; the CP complete_attempt
    /// is idempotent on terminal state).
    pub fn record(
        &self,
        attempt_id: &str,
        req: &CompleteAttemptRequest,
        fencing_token: &str,
    ) -> Result<()> {
        let _g = self.file.lock().unwrap();
        let line = CompletionLine {
            attempt_id: attempt_id.to_string(),
            exit_code: req.exit_code,
            commit_sha: req.commit_sha.clone(),
            error_code: req.error_code.clone(),
            acp_session_id: req.acp_session_id.clone(),
            fencing_token: fencing_token.to_string(),
            resolved_base_sha: req.resolved_base_sha.clone(),
            remote_head_at_start: req.remote_head_at_start.clone(),
            remote_head_at_finish: req.remote_head_at_finish.clone(),
            plan: req.plan.clone(),
            provenance: req.provenance.clone(),
            pending_artifacts: req.pending_artifacts.clone(),
            validation_rounds: req.validation_rounds,
        };
        // Dedupe: drop any prior pending line for this attempt so we don't
        // redeliver a stale terminal state alongside the fresh one.
        let mut survivors = String::new();
        if let Ok(content) = std::fs::read_to_string(&self.path) {
            for l in content.lines() {
                if l.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<CompletionLine>(l) {
                    Ok(c) if c.attempt_id == attempt_id => continue,
                    _ => {
                        survivors.push_str(l);
                        survivors.push('\n');
                    }
                }
            }
        }
        let mut s = serde_json::to_string(&line)?;
        s.push('\n');
        use std::io::Write;
        // Hardening P1 item 11: NEVER truncate the durable completion file
        // in-place. Write to a sibling temp file, fsync its data, fsync the
        // parent dir, then atomically rename over the live file — so a kill /
        // power loss during the record leaves either the old file intact or
        // the new file fully durable, never a torn / empty file.
        let tmp = self.path.with_extension("jsonl.tmp-rec");
        {
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&tmp)?;
            f.write_all(survivors.as_bytes())?;
            f.write_all(s.as_bytes())?;
            f.sync_all()?;
        }
        fsync_parent(&tmp)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Drop a completion line once the CP has acked it (terminal state set).
    /// Plan 370: append-only `{"drop":"<attempt_id>"}` marker (O(1), no
    /// rewrite) instead of rewriting the whole file per ack. The file is
    /// compacted (markers folded, dropped lines removed) only when it grows
    /// past [`CompletionOutbox::compact`]'s threshold, so the common path
    /// stays O(1) and the durable file stays bounded.
    pub fn ack(&self, attempt_id: &str) -> Result<()> {
        let _g = self.file.lock().unwrap();
        let marker = format!("{{\"drop\":\"{attempt_id}\"}}\n");
        use std::io::Write;
        let mut f = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        f.write_all(marker.as_bytes())?;
        f.sync_all()?;
        fsync_parent(&self.path)?;
        // Threshold compaction: fold markers + drop acked lines in one pass
        // when the file grows large, keeping startup/reconciliation O(n) only
        // occasionally.
        if f.metadata()?.len() > compact_threshold() {
            drop(f);
            self.compact()?;
        }
        Ok(())
    }

    /// Rewrite the file keeping only un-acked completions and dropping stale
    /// markers. Called by [`CompletionOutbox::ack`] at the compaction
    /// threshold and by startup recovery when the marker set is large.
    fn compact(&self) -> Result<()> {
        let content = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let dropped = Self::dropped_attempts(&content);
        let mut survivors = String::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| {
                    v.get("drop")
                        .map(|d| d.as_str().unwrap_or_default().to_string())
                })
                .is_some()
            {
                continue; // drop markers never survive compaction
            }
            let l: CompletionLine = match serde_json::from_str(line) {
                Ok(l) => l,
                Err(_) => {
                    survivors.push_str(line);
                    survivors.push('\n');
                    continue;
                }
            };
            if !dropped.contains(&l.attempt_id) {
                survivors.push_str(line);
                survivors.push('\n');
            }
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, &survivors)?;
        {
            let f = std::fs::OpenOptions::new().write(true).open(&tmp)?;
            f.sync_all()?;
        }
        fsync_parent(&tmp)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Attempt ids recorded as dropped in `content` (from `{"drop": ...}`
    /// markers). Idempotent under repeated markers.
    fn dropped_attempts(content: &str) -> std::collections::HashSet<String> {
        content
            .lines()
            .filter_map(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .ok()
                    .and_then(|v| v.get("drop").and_then(|d| d.as_str().map(String::from)))
            })
            .collect()
    }

    /// All pending completion records (for startup reconciliation). Hardening
    /// P0 item 10: an unparseable line is quarantined (moved aside) rather than
    /// silently dropped, so a torn write never silently loses a completion.
    pub fn pending(&self) -> Result<Vec<CompletionLine>> {
        let _g = self.file.lock().unwrap();
        let content = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        // Plan 370: drop markers are folded into the effective pending set
        // (an attempt with a marker is acked, not pending).
        let dropped = Self::dropped_attempts(&content);
        let mut out = vec![];
        let mut quarantined = String::new();
        let mut clean = String::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            // Skip drop markers entirely.
            if serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .map(|v| v.get("drop").is_some())
                .unwrap_or(false)
            {
                continue;
            }
            match serde_json::from_str::<CompletionLine>(line) {
                Ok(l) => {
                    clean.push_str(line);
                    clean.push('\n');
                    if !dropped.contains(&l.attempt_id) {
                        out.push(l);
                    }
                }
                Err(_) => {
                    quarantined.push_str(line);
                    quarantined.push('\n');
                }
            }
        }
        if !quarantined.is_empty() {
            quarantine_rewrite(&self.path, &clean, &quarantined)?;
        }
        Ok(out)
    }
}

/// Hardening P0 item 10: total bytes of every `.jsonl` file under `dir`
/// (per-attempt spools + the shared completion file). Non-recursive by design —
/// the quarantine directory is excluded so quarantined corrupt records never
/// count against the live quota.
pub fn total_bytes(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_file()
            && entry
                .file_name()
                .to_str()
                .map(|n| n.ends_with(".jsonl"))
                .unwrap_or(false)
        {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

/// Audit follow-up: delete crash leftovers the quota accounting never sees —
/// orphaned `*.tmp` stage-file siblings (a crash between write and rename in
/// ack/record/compact) and quarantine entries older than 24 h. Without this
/// they accumulate forever: invisible to `total_bytes`, never swept. Best-
/// effort; called from startup recovery.
pub fn sweep_crash_leftovers(dir: &Path, max_age: Duration) -> (u64, u64) {
    let mut removed_files = 0u64;
    let mut removed_bytes = 0u64;
    let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d,
        Err(_) => return (0, 0),
    };
    let mut sweep_dir = |d: &Path, tmp_only: bool| {
        let Ok(entries) = std::fs::read_dir(d) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_file() {
                continue;
            }
            let name_ok = entry
                .file_name()
                .to_str()
                .map(|n| n.contains(".tmp") || n.ends_with(".jsonl.tmp"))
                .unwrap_or(false);
            if tmp_only && !name_ok {
                continue;
            }
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|m| now.saturating_sub(m) > max_age)
                .unwrap_or(false);
            if !stale {
                continue;
            }
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if std::fs::remove_file(entry.path()).is_ok() {
                removed_files += 1;
                removed_bytes += size;
            }
        }
    };
    // Top level: only .tmp stage siblings; live .jsonl spools are untouched.
    sweep_dir(dir, true);
    // Quarantine: age-cap every entry.
    sweep_dir(&dir.join("quarantine"), false);
    (removed_files, removed_bytes)
}

/// Hardening P0 item 10: total pending event rows across all per-attempt spools.
/// Excludes the shared completions.jsonl file and quarantine directory.
pub fn pending_rows(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_file() {
            let name = entry.file_name();
            let name_str = name.to_str().unwrap_or("");
            if name_str.ends_with(".jsonl") && name_str != "completions.jsonl" {
                let content = std::fs::read_to_string(entry.path())?;
                total += content.lines().filter(|l| !l.trim().is_empty()).count() as u64;
            }
        }
    }
    Ok(total)
}

/// Hardening P0 item 10: age in milliseconds of the oldest unacked event
/// across all per-attempt spools. Returns 0 if no pending events.
pub fn oldest_pending_age_ms(dir: &Path) -> std::io::Result<u64> {
    let mut oldest_ms: Option<u64> = None;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0) as u64;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_file() {
            let name = entry.file_name();
            let name_str = name.to_str().unwrap_or("");
            if name_str.ends_with(".jsonl") && name_str != "completions.jsonl" {
                let content = std::fs::read_to_string(entry.path())?;
                for line in content.lines() {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Ok(ev) = serde_json::from_str::<EventLine>(line) {
                        // Prefer the enqueue timestamp; legacy spool lines
                        // (pre-enqueued_at) fall back to payload.created_at,
                        // which most events lack (age 0, as before).
                        let event_ms = if ev.enqueued_at > 0 {
                            Some(ev.enqueued_at)
                        } else {
                            ev.payload
                                .get("created_at")
                                .and_then(|v| v.as_str())
                                .and_then(|created_at| {
                                    chrono::DateTime::parse_from_rfc3339(created_at)
                                        .ok()
                                        .map(|dt| dt.timestamp_millis().max(0) as u64)
                                })
                        };
                        if let Some(event_ms) = event_ms {
                            if event_ms <= now {
                                let age = now.saturating_sub(event_ms);
                                oldest_ms = Some(oldest_ms.map_or(age, |o| o.max(age)));
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(oldest_ms.unwrap_or(0))
}

/// Hardening P0 item 10: total quarantined corrupt records in the outbox
/// quarantine directory.
pub fn corruption_count(dir: &Path) -> std::io::Result<u64> {
    let quarantine_dir = dir.join("quarantine");
    if !quarantine_dir.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in std::fs::read_dir(quarantine_dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_file() {
            let content = std::fs::read_to_string(entry.path())?;
            total += content.lines().filter(|l| !l.trim().is_empty()).count() as u64;
        }
    }
    Ok(total)
}

/// Hardening P0 item 10: pending completion records in completions.jsonl.
pub fn completion_rows(dir: &Path) -> std::io::Result<u64> {
    let path = dir.join("completions.jsonl");
    if !path.exists() {
        return Ok(0);
    }
    let content = std::fs::read_to_string(path)?;
    // Audit X-B19: ack `{"drop":...}` markers are not pending rows, and a
    // record whose id was dropped must not be counted either.
    let mut dropped = std::collections::HashSet::new();
    for l in content.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(l.trim()) {
            if let Some(d) = v.get("drop").and_then(|d| d.as_str()) {
                dropped.insert(d.to_string());
            }
        }
    }
    Ok(content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<CompletionLine>(l).ok())
        .filter(|c| !dropped.contains(&c.attempt_id))
        .count() as u64)
}

/// Hardening P0 item 10: atomically rewrite `path` with only `clean` lines and
/// append `damaged` lines to `<parent>/quarantine/<file>-<unix-ts>` so corrupt
/// records are preserved for inspection instead of lost. The rewrite uses the
/// same tmp+fsync+rename discipline as `ack`.
fn quarantine_rewrite(path: &Path, clean: &str, damaged: &str) -> Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("outbox path has no parent"))?;
    let quarantine_dir = parent.join("quarantine");
    std::fs::create_dir_all(&quarantine_dir)?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("outbox");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let qpath = quarantine_dir.join(format!("{file_name}-{ts}"));
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&qpath)?;
        f.write_all(damaged.as_bytes())?;
        f.sync_all()?;
    }
    fsync_parent(&qpath)?;
    let tmp = path.with_extension("jsonl.tmp-quarantine");
    {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        f.write_all(clean.as_bytes())?;
        f.sync_all()?;
    }
    fsync_parent(&tmp)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

impl CompletionLine {
    pub fn to_request(&self) -> CompleteAttemptRequest {
        CompleteAttemptRequest {
            exit_code: self.exit_code,
            commit_sha: self.commit_sha.clone(),
            error_code: self.error_code.clone(),
            // Hardening P0 item 10: the durable line now carries the full
            // payload, so redelivery re-sends everything the first send had —
            // plan/provenance/base/heads are no longer dropped on retry.
            resolved_base_sha: self.resolved_base_sha.clone(),
            remote_head_at_start: self.remote_head_at_start.clone(),
            remote_head_at_finish: self.remote_head_at_finish.clone(),
            acp_session_id: self.acp_session_id.clone(),
            plan: self.plan.clone(),
            provenance: self.provenance.clone(),
            pending_artifacts: self.pending_artifacts.clone(),
            validation_rounds: self.validation_rounds,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentgrid_common::{EventType, IncomingEvent};
    use serde_json::json;

    fn tmpdir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ag-obx-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn event_outbox_persists_and_acks() {
        let dir = tmpdir("ev");
        let ob = EventOutbox::open(&dir, "att-1").unwrap();
        let ev = IncomingEvent {
            sequence: 7,
            r#type: EventType::Stdout,
            payload: json!({ "text": "hi" }),
        };
        ob.push(&ev).unwrap();
        // Survives a "reopen" (new handle = fresh process).
        let ob2 = EventOutbox::open(&dir, "att-1").unwrap();
        let pending = ob2.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].sequence, 7);
        ob2.ack(&[7]).unwrap();
        assert!(
            ob2.pending().unwrap().is_empty(),
            "acked event must be gone"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn event_outbox_keeps_unacked_after_partial_ack() {
        let dir = tmpdir("evp");
        let ob = EventOutbox::open(&dir, "att-2").unwrap();
        for s in [1u64, 2, 3] {
            ob.push(&IncomingEvent {
                sequence: s,
                r#type: EventType::Stdout,
                payload: json!({ "seq": s }),
            })
            .unwrap();
        }
        ob.ack(&[2]).unwrap();
        let pending = ob.pending().unwrap();
        assert_eq!(
            pending.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            vec![1, 3],
            "only acked seq removed"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn completion_outbox_record_and_ack() {
        let dir = tmpdir("comp");
        let co = CompletionOutbox::open(&dir).unwrap();
        let req = CompleteAttemptRequest {
            exit_code: 0,
            commit_sha: Some("abc".into()),
            error_code: None,
            resolved_base_sha: None,
            remote_head_at_start: None,
            remote_head_at_finish: None,
            acp_session_id: Some("sess-1".into()),
            plan: None,
            provenance: None,
            pending_artifacts: vec![],
            validation_rounds: 0,
        };
        co.record("att-9", &req, "fence-1").unwrap();
        // Reopen (fresh process) — record survives.
        let co2 = CompletionOutbox::open(&dir).unwrap();
        let pending = co2.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].attempt_id, "att-9");
        assert_eq!(pending[0].commit_sha.as_deref(), Some("abc"));
        assert_eq!(pending[0].fencing_token, "fence-1");
        let r = pending[0].to_request();
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.acp_session_id.as_deref(), Some("sess-1"));
        co2.ack("att-9").unwrap();
        assert!(co2.pending().unwrap().is_empty(), "acked completion gone");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Plan 370: ack is append-only (drop marker) and pending() folds the
    /// markers; compaction rewrites only at the size threshold.
    #[test]
    fn completion_outbox_ack_appends_marker_and_compacts_at_threshold() {
        let dir = tmpdir("comp370");
        let co = CompletionOutbox::open(&dir).unwrap();
        let req = CompleteAttemptRequest {
            exit_code: 0,
            commit_sha: Some("abc".into()),
            error_code: None,
            resolved_base_sha: None,
            remote_head_at_start: None,
            remote_head_at_finish: None,
            acp_session_id: None,
            plan: None,
            provenance: None,
            pending_artifacts: vec![],
            validation_rounds: 0,
        };
        co.record("att-1", &req, "f").unwrap();
        co.record("att-2", &req, "f").unwrap();
        // ack appends a marker; both lines stay on disk until compaction.
        co.ack("att-1").unwrap();
        let content = std::fs::read_to_string(co.path.clone()).unwrap();
        assert!(
            content.contains("\"drop\":\"att-1\""),
            "ack must append a drop marker, not rewrite: {content}"
        );
        assert!(
            content.contains("att-2"),
            "un-acked completion line must survive an ack (no full rewrite)"
        );
        // pending() folds the marker: att-1 gone, att-2 still pending.
        let pending = co.pending().unwrap();
        let ids: Vec<&str> = pending.iter().map(|l| l.attempt_id.as_str()).collect();
        assert_eq!(ids, vec!["att-2"]);
        // Forced tiny threshold -> ack triggers compaction: markers folded,
        // acked line physically gone.
        std::env::set_var("AGENTGRID_COMPLETION_COMPACT_BYTES", "1");
        co.ack("att-2").unwrap();
        std::env::remove_var("AGENTGRID_COMPLETION_COMPACT_BYTES");
        let content = std::fs::read_to_string(co.path.clone()).unwrap();
        assert!(
            !content.contains("\"drop\""),
            "compaction must fold markers: {content}"
        );
        assert!(
            !content.contains("att-1") && !content.contains("att-2"),
            "compaction must physically drop acked lines: {content}"
        );
        assert!(co.pending().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Hardening P1 item 11: record() never truncates the live completion
    /// file in-place — it writes a sibling temp file and atomically renames
    /// over the live file. After a successful record the temp file must be
    /// gone, the live file must contain both the surviving and new lines, and
    /// a second record that dedupes the first collapses to exactly one line
    /// (latest-wins) without corruption.
    #[test]
    fn completion_outbox_record_is_atomic_no_truncate() {
        let dir = tmpdir("comp-atomic");
        let co = CompletionOutbox::open(&dir).unwrap();
        let path = co.path.clone();
        let req = CompleteAttemptRequest {
            exit_code: 0,
            commit_sha: Some("abc".into()),
            ..Default::default()
        };
        co.record("att-keep", &req, "f-1").unwrap();
        // No leftover temp file from record().
        assert!(
            std::fs::read_dir(&dir)
                .unwrap()
                .flatten()
                .all(|e| !e.file_name().to_string_lossy().ends_with(".tmp-rec")),
            "record must not leave a temp file behind"
        );
        // record again with a different attempt → file has 2 lines, live file
        // is intact (atomic replace preserved the prior line).
        let req2 = CompleteAttemptRequest {
            exit_code: 1,
            commit_sha: Some("def".into()),
            ..Default::default()
        };
        co.record("att-2", &req2, "f-2").unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            content.lines().filter(|l| !l.trim().is_empty()).count(),
            2,
            "two distinct attempts preserved atomically"
        );
        // Redeliver the same attempt: dedup → latest wins, file keeps att-2
        // and the fresh att-keep line.
        let req3 = CompleteAttemptRequest {
            exit_code: 7,
            commit_sha: Some("upd".into()),
            ..Default::default()
        };
        co.record("att-keep", &req3, "f-1b").unwrap();
        let pending = co.pending().unwrap();
        assert_eq!(pending.len(), 2, "dedup: still exactly two completion rows");
        let keep = pending
            .iter()
            .find(|p| p.attempt_id == "att-keep")
            .expect("att-keep survived");
        assert_eq!(keep.exit_code, 7, "latest record wins the dedup");
        assert_eq!(keep.fencing_token, "f-1b", "latest fence wins the dedup");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// When the on-disk spool exceeds the configured limit, `push` must return
    /// `SpoolFull` so the sink can fail-closed (disk-full protection) instead
    /// of growing the file until the host disk fills.
    #[test]
    fn event_outbox_push_fails_when_spool_limit_reached() {
        let dir = tmpdir("spool");
        // Tiny limit: 1 event's line (~40 bytes) overshoots it on the next push.
        std::env::set_var("AGENTGRID_OUTBOX_SPOOL_LIMIT_MB", "0");
        // 0 = unlimited; use 1 MiB-style integer? No — env is in MiB, so 0 is
        // the unlimited sentinel. Use a 1-byte limit by setting MiB=0 and
        // then patching the struct directly.
        std::env::remove_var("AGENTGRID_OUTBOX_SPOOL_LIMIT_MB");
        let ob = EventOutbox::open(&dir, "att-sp").unwrap();
        // Override the limit to 1 byte so the first push lands and the second
        // is refused (the file is now > 1 byte).
        let ob = EventOutbox {
            path: ob.path.clone(),
            root: ob.root.clone(),
            file: Mutex::new(()),
            last_fsync: Mutex::new(std::time::Instant::now()),
            fsync_interval: std::time::Duration::ZERO,
            spool_limit_bytes: 1,
            quota_bytes: 0,
        };
        let ev = IncomingEvent {
            sequence: 1,
            r#type: EventType::Stdout,
            payload: json!({ "text": "x" }),
        };
        // First push: file is empty (len 0 < 1) → succeeds, file now > 1 byte.
        ob.push(&ev).unwrap();
        // Second push: file len > 1 → SpoolFull.
        match ob.push(&IncomingEvent {
            sequence: 2,
            r#type: EventType::Stdout,
            payload: json!({ "text": "y" }),
        }) {
            Err(PushError::SpoolFull) => {}
            other => panic!("expected SpoolFull, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Hardening P1 item 34: terminal events can exceed the spool limit by
    /// TERMINAL_RESERVED_BYTES, while non-terminal (Stdout/Stderr/Metric)
    /// events are blocked at the hard limit.
    #[test]
    fn event_outbox_terminal_reserved_capacity() {
        let dir = tmpdir("spool-terminal");
        std::env::remove_var("AGENTGRID_OUTBOX_SPOOL_LIMIT_MB");
        let ob = EventOutbox::open(&dir, "att-term").unwrap();
        // Set a small limit: 200 bytes.
        let ob = EventOutbox {
            path: ob.path.clone(),
            root: ob.root.clone(),
            file: Mutex::new(()),
            last_fsync: Mutex::new(std::time::Instant::now()),
            fsync_interval: std::time::Duration::ZERO,
            spool_limit_bytes: 200,
            quota_bytes: 0,
        };
        // Push several Stdout events until we're near the limit.
        // Each event is roughly 70-80 bytes (incl. the enqueued_at field).
        // 3 events should put us near 200.
        for i in 1..=3 {
            let ev = IncomingEvent {
                sequence: i,
                r#type: EventType::Stdout,
                payload: json!({ "t": "x".repeat(10) }),
            };
            ob.push(&ev)
                .unwrap_or_else(|e| panic!("push {} failed: {e}", i));
        }
        // Next Stdout should fail (SpoolFull).
        let stdout_ev = IncomingEvent {
            sequence: 5,
            r#type: EventType::Stdout,
            payload: json!({ "t": "y" }),
        };
        match ob.push(&stdout_ev) {
            Err(PushError::SpoolFull) => {}
            other => panic!("expected SpoolFull for Stdout, got {other:?}"),
        }
        // But a terminal event (Result) should still succeed due to reserved capacity.
        let term_ev = IncomingEvent {
            sequence: 6,
            r#type: EventType::Result,
            payload: json!({ "exit_code": 0 }),
        };
        ob.push(&term_ev)
            .unwrap_or_else(|e| panic!("terminal push failed: {e}"));
        // Another terminal should also succeed (still within reserved).
        let term_ev2 = IncomingEvent {
            sequence: 7,
            r#type: EventType::Error,
            payload: json!({ "message": "oops" }),
        };
        ob.push(&term_ev2)
            .unwrap_or_else(|e| panic!("second terminal push failed: {e}"));
        // Eventually even terminal events hit the extended limit.
        // Fill up the reserved capacity with more terminal events.
        for i in 8..=800 {
            let term_ev = IncomingEvent {
                sequence: i,
                r#type: EventType::Status,
                payload: json!({ "msg": "x".repeat(200) }),
            };
            let _ = ob.push(&term_ev); // may succeed or fail
        }
        // Final terminal should fail (exceeded reserved capacity).
        let term_ev_final = IncomingEvent {
            sequence: 999,
            r#type: EventType::Result,
            payload: json!({ "exit_code": 1 }),
        };
        match ob.push(&term_ev_final) {
            Err(PushError::SpoolFull) => {}
            other => panic!("expected SpoolFull after reserved exhausted, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Hardening P0 item 10: the durable completion line now carries the full
    /// payload (plan, provenance, resolved base, remote heads, pending
    /// artifacts) and `to_request` re-sends all of it on redelivery — a first
    /// send failure no longer drops those fields.
    #[test]
    fn completion_line_preserves_full_payload_on_redelivery() {
        let dir = tmpdir("comp-full");
        let co = CompletionOutbox::open(&dir).unwrap();
        let req = CompleteAttemptRequest {
            exit_code: 3,
            commit_sha: Some("abc".into()),
            error_code: Some("validation_failed".into()),
            resolved_base_sha: Some("base-sha".into()),
            remote_head_at_start: Some("head-a".into()),
            remote_head_at_finish: Some("head-b".into()),
            acp_session_id: Some("sess-1".into()),
            plan: Some("steps:\n  - run: test".into()),
            provenance: Some(agentgrid_common::ProvenanceRecord {
                originator: "ci".into(),
                external_id: "job-9".into(),
                label: Some("nightly".into()),
                security_profile: None,
            }),
            pending_artifacts: vec!["changes.patch".into(), "validation.log".into()],
            validation_rounds: 0,
        };
        co.record("att-full", &req, "f-1").unwrap();
        let pending = co.pending().unwrap();
        assert_eq!(pending.len(), 1);
        let redelivered = pending[0].to_request();
        assert_eq!(redelivered.resolved_base_sha.as_deref(), Some("base-sha"));
        assert_eq!(redelivered.remote_head_at_start.as_deref(), Some("head-a"));
        assert_eq!(redelivered.remote_head_at_finish.as_deref(), Some("head-b"));
        assert_eq!(redelivered.plan.as_deref(), Some("steps:\n  - run: test"));
        assert_eq!(
            redelivered.provenance.as_ref().unwrap().external_id,
            "job-9"
        );
        assert_eq!(redelivered.pending_artifacts.len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Hardening P0 item 10: a corrupt line in the completion spool is moved
    /// to `<dir>/quarantine/` and the remaining valid completions survive.
    #[test]
    fn completion_outbox_quarantines_corrupt_line() {
        let dir = tmpdir("comp-quarantine");
        let co = CompletionOutbox::open(&dir).unwrap();
        let req = CompleteAttemptRequest {
            exit_code: 0,
            ..Default::default()
        };
        co.record("att-ok", &req, "f-1").unwrap();
        // Append a torn/corrupt line.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(co.path.clone())
            .unwrap();
        writeln!(f, "{{ this is not json").unwrap();
        drop(f);
        let pending = co.pending().unwrap();
        assert_eq!(pending.len(), 1, "valid completion survives");
        assert_eq!(pending[0].attempt_id, "att-ok");
        let quarantine_dir = dir.join("quarantine");
        assert!(
            quarantine_dir.exists(),
            "quarantine dir created for corrupt line"
        );
        let q_files: Vec<_> = std::fs::read_dir(&quarantine_dir)
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(q_files.len(), 1, "corrupt line quarantined");
        let qcontent = std::fs::read_to_string(q_files[0].path()).unwrap();
        assert!(qcontent.contains("this is not json"));
        // The live file no longer contains the corrupt line.
        let live = std::fs::read_to_string(&co.path).unwrap();
        assert!(!live.contains("not json"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Hardening P0 item 10: the global outbox quota refuses new events once
    /// all spool files combined exceed the configured cap.
    #[test]
    fn event_outbox_global_quota_blocks_pushes() {
        let dir = tmpdir("quota");
        let ob = EventOutbox::open(&dir, "att-q").unwrap();
        let ob = EventOutbox {
            path: ob.path.clone(),
            root: ob.root.clone(),
            file: Mutex::new(()),
            last_fsync: Mutex::new(std::time::Instant::now()),
            fsync_interval: std::time::Duration::ZERO,
            spool_limit_bytes: 0, // unlimited per-attempt: quota is the only gate
            quota_bytes: 1,
        };
        let ev = IncomingEvent {
            sequence: 1,
            r#type: EventType::Stdout,
            payload: json!({ "text": "x" }),
        };
        ob.push(&ev).unwrap();
        // Second push: total_bytes >= 1 → SpoolFull.
        match ob.push(&IncomingEvent {
            sequence: 2,
            r#type: EventType::Stdout,
            payload: json!({ "text": "y" }),
        }) {
            Err(PushError::SpoolFull) => {}
            other => panic!("expected SpoolFull from global quota, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
