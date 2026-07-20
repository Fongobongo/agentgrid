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

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agentgrid_common::{CompleteAttemptRequest, IncomingEvent};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct EventLine {
    seq: u64,
    #[serde(rename = "type")]
    ty: serde_json::Value,
    payload: serde_json::Value,
}

/// A durable event spool for one attempt. Append-only JSONL file guarded by a
/// Mutex; acked events are dropped by rewriting the file with the survivors.
pub struct EventOutbox {
    path: PathBuf,
    file: Mutex<()>,
}

impl EventOutbox {
    pub fn open(dir: &Path, attempt_id: &str) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        // attempt_id is a UUID-ish token from the CP; sanitize defensively.
        let safe = attempt_id
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-')
            .collect::<String>();
        Ok(Self {
            path: dir.join(format!("{safe}.jsonl")),
            file: Mutex::new(()),
        })
    }

    /// Append an event durably. Returns immediately after fsync.
    pub fn push(&self, ev: &IncomingEvent) -> Result<()> {
        let _g = self.file.lock().unwrap();
        let line = EventLine {
            seq: ev.sequence,
            ty: serde_json::to_value(ev.r#type).unwrap_or(serde_json::Value::Null),
            payload: ev.payload.clone(),
        };
        let mut s = serde_json::to_string(&line).context("encode outbox line")?;
        s.push('\n');
        // O_APPEND via OpenOptions ensures atomic appends for lines < PIPE_BUF.
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open outbox {}", self.path.display()))?;
        f.write_all(s.as_bytes())?;
        f.sync_data()?;
        Ok(())
    }

    /// Read all currently-pending events (in sequence order).
    pub fn pending(&self) -> Result<VecDeque<IncomingEvent>> {
        let _g = self.file.lock().unwrap();
        let mut out = VecDeque::new();
        let content = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(VecDeque::new()),
            Err(e) => return Err(e.into()),
        };
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let l: EventLine = serde_json::from_str(line).context("decode outbox line")?;
            let ty: agentgrid_common::EventType =
                serde_json::from_value(l.ty).unwrap_or(agentgrid_common::EventType::Status);
            out.push_back(IncomingEvent {
                sequence: l.seq,
                r#type: ty,
                payload: l.payload,
            });
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
            if !acked.contains(&l.seq) {
                survivors.push_str(line);
                survivors.push('\n');
            }
        }
        // Atomic replace: write tmp + rename.
        let tmp = self.path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, &survivors)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// One durable completion record per attempt (idempotent redelivery on the CP).
pub struct CompletionOutbox {
    path: PathBuf,
    file: Mutex<()>,
}

#[derive(Serialize, Deserialize)]
pub struct CompletionLine {
    pub attempt_id: String,
    pub exit_code: i32,
    pub commit_sha: Option<String>,
    pub error_code: Option<String>,
    pub acp_session_id: Option<String>,
}

impl CompletionOutbox {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            path: dir.join("completions.jsonl"),
            file: Mutex::new(()),
        })
    }

    /// Record a completion durably (idempotent: the latest line for an
    /// attempt wins; the CP complete_attempt is idempotent on terminal state).
    pub fn record(&self, attempt_id: &str, req: &CompleteAttemptRequest) -> Result<()> {
        let _g = self.file.lock().unwrap();
        let line = CompletionLine {
            attempt_id: attempt_id.to_string(),
            exit_code: req.exit_code,
            commit_sha: req.commit_sha.clone(),
            error_code: req.error_code.clone(),
            acp_session_id: req.acp_session_id.clone(),
        };
        let mut s = serde_json::to_string(&line)?;
        s.push('\n');
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        f.write_all(s.as_bytes())?;
        f.sync_data()?;
        Ok(())
    }

    /// Drop a completion line once the CP has acked it (terminal state set).
    pub fn ack(&self, attempt_id: &str) -> Result<()> {
        let _g = self.file.lock().unwrap();
        let content = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let mut survivors = String::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let l: CompletionLine = match serde_json::from_str(line) {
                Ok(l) => l,
                Err(_) => {
                    survivors.push_str(line);
                    survivors.push('\n');
                    continue;
                }
            };
            if l.attempt_id != attempt_id {
                survivors.push_str(line);
                survivors.push('\n');
            }
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        std::fs::write(&tmp, &survivors)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// All pending completion records (for startup reconciliation).
    pub fn pending(&self) -> Result<Vec<CompletionLine>> {
        let _g = self.file.lock().unwrap();
        let content = match std::fs::read_to_string(&self.path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut out = vec![];
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(l) = serde_json::from_str::<CompletionLine>(line) {
                out.push(l);
            }
        }
        Ok(out)
    }
}

impl CompletionLine {
    pub fn to_request(&self) -> CompleteAttemptRequest {
        CompleteAttemptRequest {
            exit_code: self.exit_code,
            commit_sha: self.commit_sha.clone(),
            error_code: self.error_code.clone(),
            acp_session_id: self.acp_session_id.clone(),
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
            acp_session_id: Some("sess-1".into()),
        };
        co.record("att-9", &req).unwrap();
        // Reopen (fresh process) — record survives.
        let co2 = CompletionOutbox::open(&dir).unwrap();
        let pending = co2.pending().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].attempt_id, "att-9");
        assert_eq!(pending[0].commit_sha.as_deref(), Some("abc"));
        let r = pending[0].to_request();
        assert_eq!(r.exit_code, 0);
        assert_eq!(r.acp_session_id.as_deref(), Some("sess-1"));
        co2.ack("att-9").unwrap();
        assert!(co2.pending().unwrap().is_empty(), "acked completion gone");
        std::fs::remove_dir_all(&dir).ok();
    }
}
