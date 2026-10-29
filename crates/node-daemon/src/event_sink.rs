//! Event sink: buffered, durable, backpressured event streaming to the CP.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentgrid_adapters::{to_event_type, AdapterEvent};
use agentgrid_common::{
    AgentEventEnvelope, EventKind, EventType, IncomingEvent, IngestEventsRequest,
};
use reqwest::Client;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, Notify};

use crate::outbox;
use crate::polling::send_with_retry;
use crate::secret_redactor::StreamingRedactor;

/// Shared, bounded-ish event buffer that flushes to the control plane in
/// batches (every 200ms or when 50 events accumulate).
fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

pub struct EventSink {
    buf: Mutex<VecDeque<IncomingEvent>>,
    next: AtomicU64,
    notify: Notify,
    // Counts events that came from the adapter's stdout/stderr. Used to warn on a
    // silent agent that exits 0 but produced no output.
    adapter_events: AtomicU64,
    attempt_id: String,
    client: Client,
    server: String,
    /// Hardening P0 item 8: fencing token echoed on every event flush so the CP
    /// can reject a stale writer (reassigned/lost attempt) with 409.
    fence: String,
    /// Stage 2.1: durable JSONL outbox. Events are appended here before any
    // send attempt and removed only after the CP acks the batch, so a daemon
    // kill no longer drops the in-flight tail.
    outbox: Arc<outbox::EventOutbox>,
    /// Stage 2.1: approximate RAM bytes pending in `buf` (backpressure).
    buf_bytes: AtomicU64,
    /// Stage 2.1: per-attempt RAM buffer cap and `read_stream` line cap,
    /// captured from `AGENTGRID_EVENT_BUF_BYTES` / `AGENTGRID_MAX_LINE_BYTES`
    /// at construction. Held on the sink instead of re-read from the process
    /// env per event: env is process-global, so a test lowering it used to
    /// make an unrelated, concurrently starting attempt drop its events.
    buf_cap_bytes: AtomicU64,
    line_cap_bytes: AtomicU64,
    /// Stage 2.1: latched once an `output_truncated` notice has been emitted,
    // so a chatty agent produces one truncation notice, not one per dropped line.
    truncated_warned: AtomicBool,
    /// Stage 2.1: count of droppable events dropped due to backpressure
    // (for `output_truncated` metadata).
    dropped_count: AtomicU64,
    /// Stage 2.1: approximate bytes of droppable events dropped due to
    // backpressure (for `output_truncated` metadata).
    dropped_bytes: AtomicU64,
    /// Latched once the on-disk outbox hit its spool limit. Further `push`
    // calls become no-ops and `run_attempt` should fail the attempt with
    // `error_code=spool_full` (disk-full fail-closed).
    spool_full: AtomicBool,
    /// Stage 13 plan approval: the last `plan` event text the adapter emitted
    /// during this attempt; carried on the completion so an `expandable`
    /// architect step can pause its workflow run in `PlanReady`.
    plan: std::sync::Mutex<Option<String>>,
    /// Per-task cost ceilings (autopilot safety): cumulative adapter-reported
    /// usage. `set_budget` installs them at attempt start; `budget_breach`
    /// reports the first breached ceiling. Strict `>`, mirroring
    /// `WorkflowBudget::check`.
    tokens_total: AtomicU64,
    cost_total: AtomicU64,
    /// Last *cumulative* cost (cents) an ACP `usage_update` reported. ACP
    /// sends the running session total, not an increment, so `push` subtracts
    /// this to keep `cost_total` a true sum. See `agentgrid_acp`'s
    /// `normalize_usage_payload`.
    last_cumulative_cost_cents: AtomicU64,
    /// Last context-gauge reading (`used`) from an ACP `usage_update`. The
    /// gauge is occupancy, not spend: it rises as the conversation grows and
    /// falls on compaction. `push` counts only the positive movement, so the
    /// running token total is monotone and a compaction cannot refund budget.
    last_gauge_tokens: AtomicU64,
    budget: std::sync::Mutex<(Option<u64>, Option<u64>)>,
}

/// Split a `metric` payload into the `(tokens, cost_cents)` increment to add
/// to the running totals.
///
/// Wrapper adapters (`claude`, `codex`) emit per-event deltas already and take
/// the plain path. ACP `usage_update` instead reports running totals, flagged
/// by `agentgrid_acp::methods::normalize_usage_payload`, and each needs its own
/// baseline:
///
/// * `cost_cumulative` — a monotone session total; the increment is the
///   difference from the last reading.
/// * `tokens_gauge` — context occupancy, which can go **down** when the agent
///   compacts. Only positive movement counts, so a compaction neither subtracts
///   from the total nor re-adds the same tokens when the context grows again.
///
/// `saturating_sub` covers both: a reset (cost going backwards on an account
/// rotation, occupancy dropping on compaction) yields zero instead of a
/// negative or a double count. Under-counting a reset is the safe direction for
/// a spend ceiling.
fn metric_delta(
    payload: &Value,
    last_cumulative_cost: &AtomicU64,
    last_gauge_tokens: &AtomicU64,
) -> (u64, u64) {
    let tokens = payload.get("tokens").and_then(|v| v.as_u64()).unwrap_or(0);
    let cost = payload
        .get("cost_cents")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let tokens = if payload.get("tokens_gauge").and_then(|v| v.as_bool()) == Some(true) {
        let prev = last_gauge_tokens.swap(tokens, Ordering::Relaxed);
        tokens.saturating_sub(prev)
    } else {
        tokens
    };
    let cost = if payload.get("cost_cumulative").and_then(|v| v.as_bool()) == Some(true) {
        let prev = last_cumulative_cost.swap(cost, Ordering::Relaxed);
        cost.saturating_sub(prev)
    } else {
        cost
    };
    (tokens, cost)
}

/// Rewrite a `metric` payload so the event we store carries per-event
/// **increments**.
///
/// The control plane aggregates these events with a plain
/// `SUM(json_extract(payload, '$.tokens'))` / `'$.cost_cents'`
/// (`workflow_tokens_cost`, `total_tokens_cost`). Leaving a running total or a
/// gauge in those fields would be summed across updates and inflate the
/// workflow budget — and, for tokens, would grow with the context window
/// rather than with consumption. The raw readings are preserved under
/// `used` / `cost_cumulative_cents` for observability.
fn delta_payload(payload: Value, tokens: u64, cost: u64) -> Value {
    let mut map = match payload {
        Value::Object(map) => map,
        other => return other,
    };
    let gauge = map.remove("tokens_gauge").and_then(|v| v.as_bool()) == Some(true);
    let cumulative = map.remove("cost_cumulative").and_then(|v| v.as_bool()) == Some(true);
    if gauge {
        map.insert("tokens".to_string(), json!(tokens));
    }
    if cumulative {
        if let Some(raw) = map.get("cost_cents").cloned() {
            map.insert("cost_cumulative_cents".to_string(), raw);
        }
        map.insert("cost_cents".to_string(), json!(cost));
    }
    Value::Object(map)
}

impl EventSink {
    pub fn new(
        attempt_id: String,
        client: Client,
        server: String,
        fence: String,
        outbox: Arc<outbox::EventOutbox>,
    ) -> Arc<Self> {
        Arc::new(Self {
            buf: Mutex::new(VecDeque::new()),
            next: AtomicU64::new(1),
            notify: Notify::new(),
            adapter_events: AtomicU64::new(0),
            attempt_id,
            fence,
            client,
            server,
            outbox,
            buf_bytes: AtomicU64::new(0),
            buf_cap_bytes: AtomicU64::new(env_u64("AGENTGRID_EVENT_BUF_BYTES", 4 * 1024 * 1024)),
            line_cap_bytes: AtomicU64::new(env_u64("AGENTGRID_MAX_LINE_BYTES", 1024 * 1024)),
            truncated_warned: AtomicBool::new(false),
            dropped_count: AtomicU64::new(0),
            dropped_bytes: AtomicU64::new(0),
            spool_full: AtomicBool::new(false),
            plan: std::sync::Mutex::new(None),
            tokens_total: AtomicU64::new(0),
            cost_total: AtomicU64::new(0),
            last_cumulative_cost_cents: AtomicU64::new(0),
            last_gauge_tokens: AtomicU64::new(0),
            budget: std::sync::Mutex::new((None, None)),
        })
    }

    /// Override the RAM buffer cap; the operator knob is
    /// `AGENTGRID_EVENT_BUF_BYTES`, read at construction.
    #[cfg(test)]
    pub fn set_buf_cap_bytes(&self, cap: u64) {
        self.buf_cap_bytes.store(cap, Ordering::Relaxed);
    }

    /// Override the `read_stream` line cap; the operator knob is
    /// `AGENTGRID_MAX_LINE_BYTES`, read at construction.
    #[cfg(test)]
    pub fn set_line_cap_bytes(&self, cap: u64) {
        self.line_cap_bytes.store(cap, Ordering::Relaxed);
    }

    /// Stage 13 plan approval: remember the latest `plan` event text emitted
    /// by the adapter (last wins).
    pub fn note_plan(&self, text: String) {
        if let Ok(mut g) = self.plan.lock() {
            *g = Some(text);
        }
    }

    /// Stage 13 plan approval: take the captured plan (if any) so the
    /// completion can carry it to the control plane.
    pub fn take_plan(&self) -> Option<String> {
        self.plan.lock().ok().and_then(|mut g| g.take())
    }

    /// Per-task cost ceilings from the assignment (`None` = unbounded).
    /// Installed once at attempt start, before any adapter output flows.
    pub fn set_budget(&self, max_tokens: Option<u64>, max_cost_cents: Option<u64>) {
        if let Ok(mut g) = self.budget.lock() {
            *g = (max_tokens, max_cost_cents);
        }
    }

    /// The first breached ceiling (`"max_tokens"` / `"max_cost_cents"`), or
    /// `None` when unbounded or within budget. Lock-free read of the
    /// accumulated totals against the installed ceilings.
    pub fn budget_breach(&self) -> Option<&'static str> {
        let (max_tokens, max_cost) = self.budget.lock().ok().map(|g| *g).unwrap_or((None, None));
        if let Some(limit) = max_tokens {
            if self.tokens_total.load(Ordering::Relaxed) > limit {
                return Some("max_tokens");
            }
        }
        if let Some(limit) = max_cost {
            if self.cost_total.load(Ordering::Relaxed) > limit {
                return Some("max_cost_cents");
            }
        }
        None
    }

    pub fn has_budget(&self) -> bool {
        self.budget
            .lock()
            .ok()
            .map(|g| g.0.is_some() || g.1.is_some())
            .unwrap_or(false)
    }

    /// Record that an event originated from the adapter output (not the
    /// daemon's own synthetic events).
    pub fn note_adapter_event(&self) {
        self.adapter_events.fetch_add(1, Ordering::SeqCst);
    }

    /// Re-enqueue undelivered outbox events left by a prior daemon run at the
    /// buffer head (sequence order preserved) and advance the sequence
    /// counter past them. Without the seed, fresh events restart at 1 and
    /// collide with pending sequences — the CP's idempotent ingest
    /// (`ON CONFLICT (attempt_id, sequence) DO NOTHING`) would then silently
    /// drop the NEW events. Bytes are accounted in `buf_bytes` like `push`
    /// so the ack-path `fetch_sub` stays balanced.
    pub async fn requeue(&self, pending: std::collections::VecDeque<IncomingEvent>) {
        if pending.is_empty() {
            return;
        }
        if let Some(max_seq) = pending.iter().map(|e| e.sequence).max() {
            self.next.fetch_max(max_seq + 1, Ordering::SeqCst);
        }
        let bytes: u64 = pending
            .iter()
            .map(|e| e.payload.to_string().len() as u64)
            .sum();
        self.buf_bytes.fetch_add(bytes, Ordering::Relaxed);
        let mut buf = self.buf.lock().await;
        for e in pending.into_iter().rev() {
            buf.push_front(e);
        }
        self.notify.notify_one();
    }

    pub fn adapter_event_count(&self) -> u64 {
        self.adapter_events.load(Ordering::SeqCst)
    }

    /// Every EventOutbox file operation (append + interval fsync, ack's full
    /// rewrite, pending's re-read) runs on the blocking pool. They used to
    /// execute inline on the Tokio worker — the one spot the project's own
    /// spawn_blocking discipline (heartbeat scans, all git work) was missed —
    /// so a struggling disk stalled every concurrent flush, the WS control
    /// channel and unrelated attempts behind one fsync.
    async fn outbox_push(&self, ev: &IncomingEvent) -> std::result::Result<(), outbox::PushError> {
        let outbox = self.outbox.clone();
        let ev = ev.clone();
        match tokio::task::spawn_blocking(move || outbox.push(&ev)).await {
            Ok(r) => r,
            Err(join) => Err(outbox::PushError::Other(anyhow::anyhow!(
                "outbox push blocking task failed: {join}"
            ))),
        }
    }

    async fn outbox_ack(&self, seqs: &[u64]) -> anyhow::Result<()> {
        let outbox = self.outbox.clone();
        let seqs = seqs.to_vec();
        match tokio::task::spawn_blocking(move || outbox.ack(&seqs)).await {
            Ok(r) => r,
            Err(join) => Err(anyhow::anyhow!("outbox ack blocking task failed: {join}")),
        }
    }

    async fn outbox_pending(&self) -> anyhow::Result<std::collections::VecDeque<IncomingEvent>> {
        let outbox = self.outbox.clone();
        match tokio::task::spawn_blocking(move || outbox.pending()).await {
            Ok(r) => r,
            Err(join) => Err(anyhow::anyhow!(
                "outbox pending blocking task failed: {join}"
            )),
        }
    }

    pub async fn push(&self, ty: EventType, payload: Value) {
        // Per-task cost ceilings: accumulate adapter-reported usage FIRST,
        // before the backpressure drop below — a dropped metric event is
        // still real spend, and the ceiling must count it (fail-closed
        // direction: count early, never under-count).
        let mut payload = payload;
        if ty == EventType::Metric {
            let (tokens, cost) = metric_delta(
                &payload,
                &self.last_cumulative_cost_cents,
                &self.last_gauge_tokens,
            );
            // Store the increment, not the running total: the control plane
            // sums these fields across events.
            payload = delta_payload(payload, tokens, cost);
            if tokens > 0 {
                self.tokens_total.fetch_add(tokens, Ordering::Relaxed);
            }
            if cost > 0 {
                self.cost_total.fetch_add(cost, Ordering::Relaxed);
            }
        }
        // Stage 2.1 backpressure: ordinary log/usage events are dropped (with a
        // single `output_truncated` notice) once the RAM buffer exceeds the per-
        // attempt cap, so a chatty agent can't wedge the node. Terminal state
        // (status/result/error) and tool calls are never dropped.
        let droppable = matches!(
            ty,
            EventType::Stdout | EventType::Stderr | EventType::Metric
        );
        if droppable {
            let cap = self.buf_cap_bytes.load(Ordering::Relaxed);
            let cur = self.buf_bytes.load(Ordering::Relaxed);
            if cur >= cap {
                // Hardening P1 item 34: track dropped events/bytes for
                // output_truncated metadata (bytes dropped/range).
                let approx_bytes = payload.to_string().len() as u64;
                self.dropped_count.fetch_add(1, Ordering::Relaxed);
                self.dropped_bytes
                    .fetch_add(approx_bytes, Ordering::Relaxed);
                if !self.truncated_warned.swap(true, Ordering::Relaxed) {
                    self.emit_truncated_notice(cap).await;
                }
                return;
            }
        }
        if self.spool_full.load(Ordering::Relaxed) {
            return;
        }
        let seq = self.next.fetch_add(1, Ordering::SeqCst);
        let ev = IncomingEvent {
            sequence: seq,
            r#type: ty,
            payload,
        };
        // Stage 2.1: persist before buffering so a kill doesn't drop it. A
        // failed fsync is non-fatal (we still deliver from RAM this run); it
        // just means the disk tail isn't covered. SpoolFull is NOT non-fatal:
        // the on-disk outbox hit its ceiling, so further events can't be made
        // durable. Latch `spool_full` (once) and emit a terminal `error` event
        // so the operator sees why the attempt is being failed.
        if let Err(e) = self.outbox_push(&ev).await {
            if matches!(e, outbox::PushError::SpoolFull) {
                if !self.spool_full.swap(true, Ordering::SeqCst) {
                    tracing::error!(
                        attempt_id = %self.attempt_id,
                        "outbox spool limit reached; failing attempt to protect the disk"
                    );
                    self.emit_spool_full_error().await;
                }
                return;
            }
            tracing::warn!(attempt_id = %self.attempt_id, "outbox push failed: {e}");
        }
        let approx_bytes = ev.payload.to_string().len() as u64;
        self.buf_bytes.fetch_add(approx_bytes, Ordering::Relaxed);
        self.buf.lock().await.push_back(ev);
        if self.buf.lock().await.len() >= 50 {
            self.notify.notify_one();
        }
    }

    async fn emit_truncated_notice(&self, cap: u64) {
        let seq = self.next.fetch_add(1, Ordering::SeqCst);
        let dropped_count = self.dropped_count.load(Ordering::Relaxed);
        let dropped_bytes = self.dropped_bytes.load(Ordering::Relaxed);
        let ev = IncomingEvent {
            sequence: seq,
            r#type: EventType::Status,
            payload: json!({
                "event": "output_truncated",
                "reason": "event buffer over cap",
                "cap_bytes": cap,
                "dropped_count": dropped_count,
                "dropped_bytes": dropped_bytes,
            }),
        };
        if let Err(e) = self.outbox_push(&ev).await {
            tracing::warn!(attempt_id = %self.attempt_id, "outbox push (truncation) failed: {e}");
        }
        // Account bytes like `push` so the ack-path fetch_sub stays balanced.
        let approx_bytes = ev.payload.to_string().len() as u64;
        self.buf_bytes.fetch_add(approx_bytes, Ordering::Relaxed);
        self.buf.lock().await.push_back(ev);
        self.notify.notify_one();
    }

    /// Emit the single terminal `spool_full` error event. Best-effort: if the
    /// outbox is full it can't be made durable, but the RAM buffer path still
    /// delivers it (or the flusher retries it).
    async fn emit_spool_full_error(&self) {
        let seq = self.next.fetch_add(1, Ordering::SeqCst);
        let ev = IncomingEvent {
            sequence: seq,
            r#type: EventType::Error,
            payload: json!({
                // Machine-readable marker (matches the completion's
                // error_code; the run-disk-full e2e asserts on this key).
                "error_code": "spool_full",
                "error": "event outbox spool full; attempt failed to avoid data loss"
            }),
        };
        if let Err(e) = self.outbox_push(&ev).await {
            tracing::warn!(attempt_id = %self.attempt_id, "outbox push (spool-full) failed: {e}");
        }
        // Account bytes like `push` so the ack-path fetch_sub stays balanced.
        let approx_bytes = ev.payload.to_string().len() as u64;
        self.buf_bytes.fetch_add(approx_bytes, Ordering::Relaxed);
        self.buf.lock().await.push_back(ev);
        self.notify.notify_one();
    }

    pub fn spool_full(&self) -> bool {
        self.spool_full.load(Ordering::Relaxed)
    }

    /// Emit the terminal `budget_exceeded` error event when a per-task cost
    /// ceiling trips. Best-effort like `emit_spool_full_error`; the
    /// completion carries the same code (callers map the kill reason).
    pub async fn emit_budget_exceeded_error(&self, field: &str) {
        let seq = self.next.fetch_add(1, Ordering::SeqCst);
        let ev = IncomingEvent {
            sequence: seq,
            r#type: EventType::Error,
            payload: json!({
                "error_code": "budget_exceeded",
                "field": field,
                "tokens_total": self.tokens_total.load(Ordering::Relaxed),
                "cost_total": self.cost_total.load(Ordering::Relaxed),
                "error": format!("per-task cost ceiling breached ({field}); attempt killed"),
            }),
        };
        if let Err(e) = self.outbox_push(&ev).await {
            tracing::warn!(attempt_id = %self.attempt_id, "outbox push (budget) failed: {e}");
        }
        let approx_bytes = ev.payload.to_string().len() as u64;
        self.buf_bytes.fetch_add(approx_bytes, Ordering::Relaxed);
        self.buf.lock().await.push_back(ev);
        self.notify.notify_one();
    }

    /// Flush the RAM buffer to the CP; on partial failure the unacked tail is
    /// pushed back to the buffer front for the next flush.
    pub async fn flush(&self) {
        let batch: Vec<IncomingEvent> = std::mem::take(&mut *self.buf.lock().await)
            .into_iter()
            .collect();
        if batch.is_empty() {
            return;
        }
        let mut chunks = split_batch(batch);
        for i in 0..chunks.len() {
            let chunk = std::mem::take(&mut chunks[i]);
            let (acked, chunk) = self.send_events(chunk, true).await;
            if !acked {
                let mut buf = self.buf.lock().await;
                // Push back the failed chunk AND every unsent chunk after it
                // (front-order preserved), not just the failed one.
                for rest in chunks.drain(i + 1..).rev() {
                    for e in rest.into_iter().rev() {
                        buf.push_front(e);
                    }
                }
                for e in chunk.into_iter().rev() {
                    buf.push_front(e);
                }
                return;
            }
        }
    }

    /// POST one bounded event batch; `retry` decides whether transient/5xx
    /// failures are retried with backoff (true for the flusher loop, false for
    /// the quick post-adapter drain). Returns `(acked, batch)`: `acked` false
    /// hands the caller the batch back so it can be pushed to the buffer front
    /// for retry.
    async fn send_events(
        &self,
        batch: Vec<IncomingEvent>,
        retry: bool,
    ) -> (bool, Vec<IncomingEvent>) {
        if batch.is_empty() {
            return (true, batch);
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
        // Serialize by reference so `batch` stays owned by the caller for the
        // push-back path on failure.
        let req = IngestEventsRequest {
            events: batch.clone(),
        };
        let mut post = self.client.post(&url).json(&req);
        if !self.fence.is_empty() {
            post = post.header(agentgrid_common::FENCING_TOKEN_HEADER, &self.fence);
        }
        let max_attempts = if retry { 10 } else { 1 };
        match send_with_retry(post, max_attempts).await {
            Ok(s) if s.is_success() => {
                self.buf_bytes.fetch_sub(freed, Ordering::Relaxed);
                if let Err(e) = self.outbox_ack(&seqs).await {
                    tracing::warn!(attempt_id = %self.attempt_id, "outbox ack failed: {e}");
                }
                (true, batch)
            }
            Ok(s) => {
                tracing::warn!(attempt_id = %self.attempt_id, "event flush got {s}; will retry");
                (false, batch)
            }
            Err(e) => {
                tracing::warn!(attempt_id = %self.attempt_id, "event flush error {e}; will retry");
                (false, batch)
            }
        }
    }

    pub async fn run_flusher(self: Arc<Self>) {
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
    pub async fn flush_quick(&self) {
        let batch: Vec<IncomingEvent> = std::mem::take(&mut *self.buf.lock().await)
            .into_iter()
            .collect();
        if batch.is_empty() {
            return;
        }
        // Hardening P1 item 34: bounded chunks so a large post-adapter flush is
        // not rejected by the CP batch cap and RAM stays bounded.
        let mut chunks = split_batch(batch);
        for i in 0..chunks.len() {
            let chunk = std::mem::take(&mut chunks[i]);
            let (acked, chunk) = self.send_events(chunk, false).await;
            if !acked {
                // Push back the failed chunk AND every unsent chunk after it;
                // the durable outbox still holds every line for restart
                // redelivery.
                let mut buf = self.buf.lock().await;
                for rest in chunks.drain(i + 1..).rev() {
                    for e in rest.into_iter().rev() {
                        buf.push_front(e);
                    }
                }
                for e in chunk.into_iter().rev() {
                    buf.push_front(e);
                }
                return;
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
    /// Test accessor: snapshot of the current RAM buffer events.
    #[cfg(test)]
    pub async fn buffered_events(&self) -> Vec<IncomingEvent> {
        self.buf.lock().await.iter().cloned().collect()
    }

    pub async fn drain_outbox(&self, deadline: tokio::time::Instant) {
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
            let pending = match self.outbox_pending().await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(attempt_id = %self.attempt_id, "outbox read failed: {e}");
                    return;
                }
            };
            if pending.is_empty() {
                return;
            }
            // Hardening P1 item 34: never hold the whole pending spool in RAM
            // or POST it as one oversized batch — chunk at the CP batch cap and
            // deliver chunk-by-chunk, acking each.
            for chunk in split_batch(pending.into_iter().collect()) {
                let seqs: Vec<u64> = chunk.iter().map(|e| e.sequence).collect();
                let req = IngestEventsRequest { events: chunk };
                let mut post = self.client.post(&url).json(&req);
                if !self.fence.is_empty() {
                    post = post.header(agentgrid_common::FENCING_TOKEN_HEADER, &self.fence);
                }
                // Audit X-B14: the drain deadline is only checked between
                // chunks, so the full 10-attempt retry budget (~26 s of
                // backoff) used to overshoot it per stuck chunk. A drain is
                // best-effort by design — the durable outbox retains whatever
                // this pass could not deliver.
                match send_with_retry(post, 2).await {
                    Ok(s) if s.is_success() => {
                        if let Err(e) = self.outbox_ack(&seqs).await {
                            tracing::warn!(attempt_id = %self.attempt_id, "outbox ack failed: {e}");
                        }
                    }
                    _ => return,
                }
                if tokio::time::Instant::now() >= deadline {
                    tracing::warn!(
                        attempt_id = %self.attempt_id,
                        "drain_outbox timed out; events remain on disk"
                    );
                    return;
                }
            }
        }
    }
}

/// Hardening P1 item 34: split an event buffer into bounded chunks matching the
/// CP's ingest caps (`AGENTGRID_MAX_EVENT_BATCH` events / `_KB` bytes, default
/// 500 / 4 MiB). Slightly conservative on the byte cap so a single oversized
/// payload never trips the server's 413.
pub fn split_batch(events: Vec<IncomingEvent>) -> Vec<Vec<IncomingEvent>> {
    let max_events = std::env::var("AGENTGRID_MAX_EVENT_BATCH")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(500);
    let max_bytes = std::env::var("AGENTGRID_MAX_EVENT_BATCH_KB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(4 * 1024 * 1024);
    let max_bytes = (max_bytes as f64 * 0.9) as usize; // leave headroom
    let mut chunks: Vec<Vec<IncomingEvent>> = Vec::new();
    let mut cur: Vec<IncomingEvent> = Vec::new();
    let mut cur_bytes = 0usize;
    for e in events {
        let sz = e.payload.to_string().len() + 64;
        if (!cur.is_empty() && cur.len() >= max_events)
            || (!cur.is_empty() && cur_bytes + sz > max_bytes)
        {
            chunks.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
        cur_bytes += sz;
        cur.push(e);
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
}

/// Read a subprocess stream, mask secrets, and emit each line as an event.
/// Manual line reading preserves a partial tail on EOF (crashed adapter's last
/// half-event is kept rather than dropped). Bounded line length so an adapter
/// that never emits a newline cannot grow the buffer without limit.
pub async fn read_stream<R: AsyncRead + Unpin>(
    reader: R,
    sink: Arc<EventSink>,
    stream: &str,
    secrets: Vec<String>,
    raw: Option<Arc<Mutex<tokio::fs::File>>>,
    guard: Arc<crate::command_guard::CommandGuard>,
) {
    let mut reader = tokio::io::BufReader::new(reader);
    let mut chunk = vec![0u8; 4096];
    let min_len = std::env::var("AGENTGRID_REDACT_MIN_LEN")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(6);
    let line_cap = sink.line_cap_bytes.load(Ordering::Relaxed) as usize;
    let mut redactor = StreamingRedactor::new(secrets, min_len, line_cap);

    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        for line in redactor.feed(&chunk[..n]) {
            emit_line_masked(&line, &sink, stream, &raw, &guard).await;
        }
    }

    // Flush any remaining partial line
    if let Some(line) = redactor.finish() {
        emit_line_masked(&line, &sink, stream, &raw, &guard).await;
    }

    // Drain the raw mirror before returning: `write_all` lands in the tokio
    // fs pool, and a reader (a test, or a crash-path tail reader) may observe
    // the file before the pool flushes. Without this, the last raw lines can
    // be missing from disk when this function returns.
    if let Some(f) = raw {
        let mut g = f.lock().await;
        let _ = g.flush().await;
    }
}

/// Emit a line that has already been masked by the streaming redactor.
async fn emit_line_masked(
    line: &[u8],
    sink: &Arc<EventSink>,
    stream: &str,
    raw: &Option<Arc<Mutex<tokio::fs::File>>>,
    guard: &crate::command_guard::CommandGuard,
) {
    if let Some(f) = raw {
        let mut g = f.lock().await;
        // Disk-full (or any I/O error) used to silently truncate the raw
        // mirror: the CP-visible event stream is unaffected, but whoever
        // debugs from the worktree never learned the tail was missing.
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line);
        buf.push(b'\n');
        if let Err(e) = g.write_all(&buf).await {
            tracing::warn!("raw output mirror write failed: {e}");
        }
    }
    // Stage 3.1: accept the versioned envelope first; fall back to the
    // legacy `{type, payload}` adapter event; anything else is a raw log.
    // Unknown kinds are preserved (never fatal).
    let s = String::from_utf8_lossy(line).to_string();
    if let Ok(env) = serde_json::from_str::<AgentEventEnvelope>(&s) {
        if env.kind == EventKind::Plan {
            if let Some(text) = env.payload.get("text").and_then(|x| x.as_str()) {
                sink.note_plan(text.to_string());
            }
        }
        // Plan 1.2: command guard — deny-listed tool calls are replaced with
        // a `guard_blocked` error event so the operator/audit trail sees the
        // block instead of silently forwarding the dangerous call.
        if matches!(env.kind, EventKind::ToolCall) {
            if let Some(cmd) = env
                .payload
                .get("input")
                .and_then(|v| v.as_str())
                .or_else(|| env.payload.get("tool").and_then(|v| v.as_str()))
            {
                use crate::command_guard::GuardDecision::*;
                match guard.decide(cmd) {
                    Allowed => {}
                    DeniedMatch | DeniedNotAllowlisted => {
                        sink.push(
                            EventType::Error,
                            json!({
                                "kind": "guard_blocked",
                                "command": cmd,
                                "reason": "denylist match",
                            }),
                        )
                        .await;
                        sink.note_adapter_event();
                        return;
                    }
                }
            }
        }
        sink.push(env.kind.to_event_type(), env.payload).await;
        sink.note_adapter_event();
        return;
    }
    match serde_json::from_str::<AdapterEvent>(&s) {
        Ok(ae) => {
            if ae.r#type == "plan" {
                if let Some(text) = ae.payload.get("text").and_then(|x| x.as_str()) {
                    sink.note_plan(text.to_string());
                }
            }
            // Plan 1.2: same guard for legacy wrapper adapters.
            if ae.r#type == "tool_call" {
                if let Some(cmd) = ae
                    .payload
                    .get("input")
                    .and_then(|v| v.as_str())
                    .or_else(|| ae.payload.get("tool").and_then(|v| v.as_str()))
                {
                    use crate::command_guard::GuardDecision::*;
                    match guard.decide(cmd) {
                        Allowed => {}
                        DeniedMatch | DeniedNotAllowlisted => {
                            sink.push(
                                EventType::Error,
                                json!({
                                    "kind": "guard_blocked",
                                    "command": cmd,
                                    "reason": "denylist match",
                                }),
                            )
                            .await;
                            sink.note_adapter_event();
                            return;
                        }
                    }
                }
            }
            sink.push(to_event_type(&ae.r#type), ae.payload).await;
            sink.note_adapter_event();
        }
        Err(_) => {
            let ty = if stream == "stderr" {
                EventType::Stderr
            } else {
                EventType::Stdout
            };
            sink.push(ty, json!({ "text": s })).await;
            sink.note_adapter_event();
        }
    }
}

#[cfg(test)]
mod plan_capture_tests {
    use super::*;
    use crate::outbox::EventOutbox;

    fn test_sink(id: &str) -> Arc<EventSink> {
        let dir = std::env::temp_dir().join(format!("ag-sink-plan-{}-{}", std::process::id(), id));
        let ob = EventOutbox::open(&dir, id).expect("open test outbox");
        EventSink::new(
            id.into(),
            Client::new(),
            "http://unused".into(),
            String::new(),
            Arc::new(ob),
        )
    }

    #[tokio::test]
    async fn plan_event_captured_last_wins() {
        let sink = test_sink("plan-capture");
        let guard = crate::command_guard::CommandGuard::new(vec![], vec![]);
        emit_line_masked(
            br#"{"type":"plan","payload":{"text":"plan-a"}}"#,
            &sink,
            "stdout",
            &None,
            &guard,
        )
        .await;
        emit_line_masked(
            br#"{"type":"log","payload":{"text":"hi"}}"#,
            &sink,
            "stdout",
            &None,
            &guard,
        )
        .await;
        emit_line_masked(
            br#"{"type":"plan","payload":{"text":"plan-b"}}"#,
            &sink,
            "stdout",
            &None,
            &guard,
        )
        .await;
        assert_eq!(sink.take_plan().as_deref(), Some("plan-b"));
        assert!(sink.take_plan().is_none(), "take is consuming");
    }

    #[tokio::test]
    async fn budget_accumulates_metric_usage_and_trips_strictly_over() {
        let sink = test_sink("plan-budget");
        assert!(!sink.has_budget());
        assert_eq!(sink.budget_breach(), None);
        sink.set_budget(Some(100), Some(5));
        assert!(sink.has_budget());
        // Under the ceilings: no breach (strict `>`, equality passes).
        sink.push(EventType::Metric, json!({ "tokens": 100, "cost_cents": 5 }))
            .await;
        assert_eq!(sink.budget_breach(), None);
        // One token over trips max_tokens first.
        sink.push(EventType::Metric, json!({ "tokens": 1, "cost_cents": 0 }))
            .await;
        assert_eq!(sink.budget_breach(), Some("max_tokens"));
        // Cost ceiling trips independently.
        let sink2 = test_sink("plan-budget-cost");
        sink2.set_budget(None, Some(5));
        sink2
            .push(EventType::Metric, json!({ "tokens": 999, "cost_cents": 6 }))
            .await;
        assert_eq!(sink2.budget_breach(), Some("max_cost_cents"));
        // Non-metric events never feed the totals.
        let sink3 = test_sink("plan-budget-noise");
        sink3.set_budget(Some(0), Some(0));
        sink3.push(EventType::Stdout, json!({ "text": "hi" })).await;
        assert_eq!(sink3.budget_breach(), None);
        // The terminal error event carries the code + observed totals.
        sink.emit_budget_exceeded_error("max_tokens").await;
        let buf = sink.buffered_events().await;
        let err = buf
            .iter()
            .find(|e| e.r#type == EventType::Error)
            .expect("budget error event emitted");
        assert_eq!(
            err.payload.get("error_code").and_then(|v| v.as_str()),
            Some("budget_exceeded")
        );
        assert_eq!(
            err.payload.get("field").and_then(|v| v.as_str()),
            Some("max_tokens")
        );
    }

    #[tokio::test]
    async fn cumulative_acp_cost_is_deltaed_not_summed() {
        // ACP `usage_update` reports the running session total. Summing it on
        // every update would report 5 + 12 + 20 = 37 cents for a 20-cent
        // session — and trip ceilings that never fired in reality.
        let sink = test_sink("plan-cumulative");
        sink.set_budget(None, Some(30));
        for cumulative in [5u64, 12, 20] {
            sink.push(
                EventType::Metric,
                json!({ "cost_cents": cumulative, "cost_cumulative": true }),
            )
            .await;
        }
        assert_eq!(
            sink.budget_breach(),
            None,
            "true spend is 20 cents; a naive sum (37) would trip a 30-cent ceiling"
        );

        // A session reset (account rotation) sends a lower total: the delta is
        // zero, so it neither subtracts from nor re-adds to the running total.
        let sink2 = test_sink("plan-cumulative-reset");
        sink2.set_budget(None, Some(21));
        sink2
            .push(
                EventType::Metric,
                json!({ "cost_cents": 20, "cost_cumulative": true }),
            )
            .await;
        sink2
            .push(
                EventType::Metric,
                json!({ "cost_cents": 3, "cost_cumulative": true }),
            )
            .await;
        assert_eq!(
            sink2.budget_breach(),
            None,
            "a reset must not push the total past the ceiling"
        );

        // Per-event deltas (wrapper adapters) carry no flag and stay additive.
        let sink3 = test_sink("plan-deltas");
        sink3.set_budget(None, Some(5));
        sink3
            .push(EventType::Metric, json!({ "cost_cents": 6 }))
            .await;
        assert_eq!(sink3.budget_breach(), Some("max_cost_cents"));
    }

    #[tokio::test]
    async fn gauge_tokens_become_monotone_increments() {
        // ACP reports context occupancy, not spend: 1000 → 1500 → 3000 means
        // 3000 tokens entered the context, not 5500. Summing the gauge would
        // grow with the window rather than with consumption.
        let sink = test_sink("plan-gauge-tokens");
        for used in [1000u64, 1500, 3000] {
            sink.push(
                EventType::Metric,
                json!({ "tokens": used, "tokens_gauge": true, "used": used }),
            )
            .await;
        }
        assert_eq!(sink.tokens_total.load(Ordering::Relaxed), 3000);

        // Compaction drops occupancy to 800: no refund, and the baseline moves
        // down so the next growth is counted from 800, not from 3000.
        sink.push(
            EventType::Metric,
            json!({ "tokens": 800, "tokens_gauge": true, "used": 800 }),
        )
        .await;
        assert_eq!(
            sink.tokens_total.load(Ordering::Relaxed),
            3000,
            "a compaction must not subtract from tokens already spent"
        );

        sink.push(
            EventType::Metric,
            json!({ "tokens": 1200, "tokens_gauge": true, "used": 1200 }),
        )
        .await;
        assert_eq!(
            sink.tokens_total.load(Ordering::Relaxed),
            3400,
            "growth after compaction counts from the lowered baseline"
        );
    }

    #[tokio::test]
    async fn gauge_tokens_trip_the_token_ceiling() {
        let sink = test_sink("plan-gauge-ceiling");
        sink.set_budget(Some(3000), None);
        for used in [1000u64, 1500, 3000] {
            sink.push(
                EventType::Metric,
                json!({ "tokens": used, "tokens_gauge": true }),
            )
            .await;
        }
        assert_eq!(
            sink.budget_breach(),
            None,
            "3000 spent against a 3000 ceiling is not a breach"
        );

        sink.push(
            EventType::Metric,
            json!({ "tokens": 3400, "tokens_gauge": true }),
        )
        .await;
        assert_eq!(sink.budget_breach(), Some("max_tokens"));
    }

    #[test]
    fn forwarded_metric_carries_increments_not_totals() {
        // The control plane sums `$.tokens` / `$.cost_cents` across events, so
        // the stored payload must hold the increment while the raw readings
        // survive under their own keys.
        let out = delta_payload(
            json!({
                "tokens": 1200,
                "tokens_gauge": true,
                "used": 1200,
                "cost_cents": 5,
                "cost_cumulative": true,
            }),
            300,
            3,
        );
        assert_eq!(out.get("tokens"), Some(&json!(300)));
        assert_eq!(out.get("cost_cents"), Some(&json!(3)));
        assert_eq!(out.get("cost_cumulative_cents"), Some(&json!(5)));
        assert_eq!(out.get("used"), Some(&json!(1200)));
        assert!(
            out.get("tokens_gauge").is_none(),
            "the marker describes the raw field, which is gone"
        );
    }

    #[test]
    fn plain_deltas_pass_through_untouched() {
        let out = delta_payload(json!({ "tokens": 42, "cost_cents": 7 }), 42, 7);
        assert_eq!(out.get("tokens"), Some(&json!(42)));
        assert_eq!(out.get("cost_cents"), Some(&json!(7)));
        assert!(out.get("cost_cumulative_cents").is_none());
    }
}
