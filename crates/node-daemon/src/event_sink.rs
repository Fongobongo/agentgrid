//! Event sink: buffered, durable, backpressured event streaming to the CP.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentgrid_adapters::{to_event_type, AdapterEvent};
use agentgrid_common::{AgentEventEnvelope, EventType, IncomingEvent, IngestEventsRequest};
use reqwest::Client;
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Mutex, Notify};

use crate::outbox;
use crate::polling::send_with_retry;
use crate::secret_redactor::StreamingRedactor;

/// Shared, bounded-ish event buffer that flushes to the control plane in
/// batches (every 200ms or when 50 events accumulate).
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
            truncated_warned: AtomicBool::new(false),
            dropped_count: AtomicU64::new(0),
            dropped_bytes: AtomicU64::new(0),
            spool_full: AtomicBool::new(false),
        })
    }

    /// Record that an event originated from the adapter output (not the
    /// daemon's own synthetic events).
    pub fn note_adapter_event(&self) {
        self.adapter_events.fetch_add(1, Ordering::SeqCst);
    }

    pub fn adapter_event_count(&self) -> u64 {
        self.adapter_events.load(Ordering::SeqCst)
    }

    pub async fn push(&self, ty: EventType, payload: Value) {
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
        if let Err(e) = self.outbox.push(&ev) {
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
        if let Err(e) = self.outbox.push(&ev) {
            tracing::warn!(attempt_id = %self.attempt_id, "outbox push (truncation) failed: {e}");
        }
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
                "error": "event outbox spool full; attempt failed to avoid data loss"
            }),
        };
        if let Err(e) = self.outbox.push(&ev) {
            tracing::warn!(attempt_id = %self.attempt_id, "outbox push (spool-full) failed: {e}");
        }
        self.buf.lock().await.push_back(ev);
        self.notify.notify_one();
    }

    pub fn spool_full(&self) -> bool {
        self.spool_full.load(Ordering::Relaxed)
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
        for chunk in split_batch(batch) {
            let (acked, chunk) = self.send_events(chunk, true).await;
            if !acked {
                let mut buf = self.buf.lock().await;
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
            post = post.header("x-agentgrid-fencing-token", &self.fence);
        }
        let max_attempts = if retry { 10 } else { 1 };
        match send_with_retry(post, max_attempts).await {
            Ok(s) if s.is_success() => {
                self.buf_bytes.fetch_sub(freed, Ordering::Relaxed);
                if let Err(e) = self.outbox.ack(&seqs) {
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
        for chunk in split_batch(batch) {
            let (acked, chunk) = self.send_events(chunk, false).await;
            if !acked {
                // Push back the failed chunk; the durable outbox still holds
                // every line for restart redelivery.
                let mut buf = self.buf.lock().await;
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
            // Hardening P1 item 34: never hold the whole pending spool in RAM
            // or POST it as one oversized batch — chunk at the CP batch cap and
            // deliver chunk-by-chunk, acking each.
            for chunk in split_batch(pending.into_iter().collect()) {
                let seqs: Vec<u64> = chunk.iter().map(|e| e.sequence).collect();
                let req = IngestEventsRequest { events: chunk };
                match send_with_retry(self.client.post(&url).json(&req), 10).await {
                    Ok(s) if s.is_success() => {
                        if let Err(e) = self.outbox.ack(&seqs) {
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
) {
    let mut reader = tokio::io::BufReader::new(reader);
    let mut chunk = vec![0u8; 4096];
    let min_len = std::env::var("AGENTGRID_REDACT_MIN_LEN")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(6);
    let line_cap: usize = std::env::var("AGENTGRID_MAX_LINE_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024 * 1024);
    let mut redactor = StreamingRedactor::new(secrets, min_len, line_cap);

    loop {
        let n = match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        for line in redactor.feed(&chunk[..n]) {
            emit_line_masked(&line, &sink, stream, &raw).await;
        }
    }

    // Flush any remaining partial line
    if let Some(line) = redactor.finish() {
        emit_line_masked(&line, &sink, stream, &raw).await;
    }
}

/// Emit a line that has already been masked by the streaming redactor.
async fn emit_line_masked(
    line: &[u8],
    sink: &Arc<EventSink>,
    stream: &str,
    raw: &Option<Arc<Mutex<tokio::fs::File>>>,
) {
    if let Some(f) = raw {
        let mut g = f.lock().await;
        let _ = g.write_all(line).await;
        let _ = g
            .write_all(
                b"
",
            )
            .await;
    }
    // Stage 3.1: accept the versioned envelope first; fall back to the
    // legacy `{type, payload}` adapter event; anything else is a raw log.
    // Unknown kinds are preserved (never fatal).
    let s = String::from_utf8_lossy(line).to_string();
    if let Ok(env) = serde_json::from_str::<AgentEventEnvelope>(&s) {
        sink.push(env.kind.to_event_type(), env.payload).await;
        sink.note_adapter_event();
        return;
    }
    match serde_json::from_str::<AdapterEvent>(&s) {
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
            sink.push(ty, json!({ "text": s })).await;
            sink.note_adapter_event();
        }
    }
}
