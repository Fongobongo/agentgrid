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
/// cut short (`timeout` / `cancelled` / `resource_limit:<reason>` /
/// `budget_exceeded` / `None` = natural exit).
#[derive(Debug, Clone)]
pub struct SupervisedRun {
    pub code: i32,
    pub kill_reason: Option<String>,
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
    // Plan 6.3 (#534): sample the children high-water mark before spawn so
    // the attempt's agent/build peak is attributable at reap time.
    let rss_before = crate::completion::child_peak_rss_kb();
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

    let (code, kill_reason): (i32, Option<String>) = {
        // Per-task cost ceilings: race the bounded wait against a 1s budget
        // poll. Without a configured budget the future pends forever (zero
        // overhead, no behaviour change); on breach the run is killed like a
        // cancel, but reported as `budget_exceeded` (failed, not cancelled).
        let budget_hit = async {
            if !sink.has_budget() {
                std::future::pending::<()>().await;
            }
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                if sink.budget_breach().is_some() {
                    return;
                }
            }
        };
        tokio::pin!(budget_hit);
        tokio::select! {
            exit = wait_bounded(&mut child, timeout, attempt_id, cancel_client, cancel_url) => {
                match exit?
                {
            BoundedExit::Exited(c) => {
                // Stage 12 / ADR 0003: a sandboxed attempt may have been
                // OOM-killed by the container runtime — the exit code alone
                // (137) is indistinguishable from an agent crash. Ask the
                // runtime; on OOM upgrade the outcome to the first-class
                // `resource_limit:<reason>` error code so the CP can treat
                // "the profile ceiling was hit" differently from "the agent
                // died" (e.g. never auto-retry an OOM).
                let oom = crate::sandbox::inspect_container_oom(attempt_id).await;
                if oom.is_some() {
                    crate::sandbox::remove_sandbox_container(attempt_id).await;
                }
                (c, oom.map(|reason| format!("resource_limit:{reason}")))
            }
            BoundedExit::TimedOut => {
                terminate_group(pid);
                // Audit ND-6: killing the `docker run` client leaves the
                // container itself running — remove it by per-attempt name.
                crate::sandbox::remove_sandbox_container(attempt_id).await;
                // Bound the reap: a child in uninterruptible disk sleep
                // survives SIGKILL and would park run_attempt forever,
                // leaking the concurrency slot (same bound as the ACP path).
                let status = tokio::time::timeout(std::time::Duration::from_secs(15), child.wait())
                    .await
                    .ok()
                    .and_then(|r| r.ok());
                (
                    status.and_then(|s| s.code()).unwrap_or(-1),
                    Some("timeout".to_string()),
                )
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
                let status = tokio::time::timeout(std::time::Duration::from_secs(15), child.wait())
                    .await
                    .ok()
                    .and_then(|r| r.ok());
                (
                    status.and_then(|s| s.code()).unwrap_or(-1),
                    Some("cancelled".to_string()),
                )
            }
            }
            },
            _ = &mut budget_hit => {
                // Per-task cost ceiling tripped mid-run: terminal error event
                // first (so the trail shows why), then the same kill +
                // bounded reap as a cancel. Reported as `budget_exceeded`
                // (failed), never `cancelled`.
                let field = sink.budget_breach().unwrap_or("max_tokens");
                sink.emit_budget_exceeded_error(field).await;
                tracing::warn!(
                    attempt_id,
                    field,
                    "per-task cost ceiling breached; killing attempt"
                );
                terminate_group(pid);
                crate::sandbox::remove_sandbox_container(attempt_id).await;
                let status = tokio::time::timeout(std::time::Duration::from_secs(15), child.wait())
                    .await
                    .ok()
                    .and_then(|r| r.ok());
                (
                    status.and_then(|s| s.code()).unwrap_or(-1),
                    Some("budget_exceeded".to_string()),
                )
            }
        }
    };

    let _ = r1.await;
    let _ = r2.await;
    // The in-run budget poll only fires when the adapter reports usage *during*
    // the run. A short single-step run (opencode emits one `step_finish`, at the
    // end) finishes before the first metric lands, so the breach only becomes
    // visible after the streams are drained — here. The spend did exceed the
    // ceiling, and reporting a natural exit would tell the operator the budget
    // held when it did not.
    let mut kill_reason = kill_reason;
    if kill_reason.is_none() {
        if let Some(field) = sink.budget_breach() {
            sink.emit_budget_exceeded_error(field).await;
            tracing::warn!(
                attempt_id,
                field,
                "per-task cost ceiling breached after the agent exited; marking the run over budget"
            );
            kill_reason = Some("budget_exceeded".to_string());
        }
    }
    // Plan 6.3 (#534): report the agent subprocess peak RSS (daemon RSS is
    // already in the heartbeat as `active_rss_mib`). Best-effort: a missing
    // sample only skips the event.
    if let Some(payload) = crate::completion::child_peak_event(rss_before) {
        tracing::info!(
            attempt_id,
            child_peak_rss_kb = payload.get("child_peak_rss_kb").and_then(|v| v.as_u64()),
            "agent subprocess peak RSS"
        );
        sink.push(EventKind::Log.to_event_type(), payload).await;
    }
    Ok(SupervisedRun { code, kill_reason })
}
