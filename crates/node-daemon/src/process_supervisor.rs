//! Process supervision: spawn the adapter process through the ExecutionBackend
//! contract and supervise it — bounded timeout, cancellation, process-group
//! kill, event streaming.

use std::sync::Arc;

use agentgrid_adapters::{ExecutionBackend, ProcessBackend, SpawnRequest};
use agentgrid_common::EventKind;
use anyhow::Result;
use reqwest::Client;
use serde_json::json;
use tokio::sync::Mutex;

use crate::command_guard::CommandGuard;
use crate::completion::{terminate_group, wait_bounded, BoundedExit};
use crate::event_sink::{read_stream, EventSink};

/// Outcome of a supervised adapter run: the process exit code plus why it was
/// cut short (`timeout` / `cancelled` / `None` = natural exit).
#[derive(Debug, Clone, Copy)]
pub struct SupervisedRun {
    pub code: i32,
    pub kill_reason: Option<&'static str>,
}

/// Spawn the adapter through `ProcessBackend`, stream stdout/stderr as events
/// (masking secrets), and supervise until natural exit, per-attempt timeout, or
/// user cancellation — terminating the whole process group on the latter two.
/// The caller is responsible for the durable early-completion record and the
/// feedback/validation loop; this is the process-lifetime supervision only.
///
/// Returns `Err` only when the process could not be spawned at all.
#[allow(clippy::too_many_arguments)]
pub async fn supervise_adapter(
    req: SpawnRequest,
    cancel_url: String,
    cancel_client: Client,
    sink: Arc<EventSink>,
    secrets: Vec<String>,
    raw_file: Option<Arc<Mutex<tokio::fs::File>>>,
    attempt_id: &str,
    guard: Arc<CommandGuard>,
) -> Result<SupervisedRun> {
    let bp = ProcessBackend.spawn(req)?;
    let pid = bp.pid;
    let timeout = bp.timeout;
    let stdout = bp.stdout;
    let stderr = bp.stderr;
    let mut child = bp.child;
    let cancel_client = cancel_client;

    let g1 = guard.clone();
    let g2 = guard.clone();
    let r1 = tokio::spawn(read_stream(
        stdout,
        sink.clone(),
        "stdout",
        secrets.clone(),
        raw_file.clone(),
        g1.clone(),
    ));
    let r2 = tokio::spawn(read_stream(
        stderr,
        sink.clone(),
        "stderr",
        secrets,
        raw_file,
        g2,
    ));

    let (code, kill_reason): (i32, Option<&'static str>) = {
        match wait_bounded(&mut child, timeout, attempt_id, cancel_client, cancel_url).await? {
            BoundedExit::Exited(c) => (c, None),
            BoundedExit::TimedOut => {
                terminate_group(pid);
                // Audit ND-6: killing the `docker run` client leaves the
                // container itself running — remove it by per-attempt name.
                crate::sandbox::remove_sandbox_container(attempt_id).await;
                let status = child.wait().await?;
                (status.code().unwrap_or(-1), Some("timeout"))
            }
            BoundedExit::Cancelled => {
                sink.push(
                    EventKind::Cancel.to_event_type(),
                    json!({
                        "kind": "cancel",
                        "reason": "user_requested",
                        "attempt_id": attempt_id
                    }),
                )
                .await;
                terminate_group(pid);
                crate::sandbox::remove_sandbox_container(attempt_id).await;
                let status = child.wait().await?;
                (status.code().unwrap_or(-1), Some("cancelled"))
            }
        }
    };

    let _ = r1.await;
    let _ = r2.await;
    Ok(SupervisedRun { code, kill_reason })
}
