//! Post-agent validation: run the operator's `validation_command` in the
//! worktree with a fresh process group, bounded timeout, and cancellation
//! support, streaming output as events.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agentgrid_common::EventType;
use anyhow::Result;
use reqwest::Client;
use serde_json::json;

use crate::completion::{terminate_group, wait_for_cancel};
use crate::event_sink::{read_stream, EventSink};
use crate::polling::send_with_retry;

/// Outcome of a validation run: the command's exit code plus whether it was
/// cut short by the per-attempt timeout or a user cancellation (both kill the
/// whole process tree). The caller maps these to distinct error codes
/// (`validation_failed` / `validation_timeout` / `validation_cancelled`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidationOutcome {
    pub code: i32,
    pub timed_out: bool,
    pub cancelled: bool,
}

/// Run the post-agent validation command in the worktree, streaming its output
/// as events and writing `validation.log`. The command is a trusted operator
/// shell string (explicitly marked as such — it is NOT adapter input), run
/// with a fresh process group, a bounded timeout, cancellation support and the
/// same line-cap/lossy-UTF-8 streaming as the agent output (Hardening P0 item
/// 12). Returns the outcome; the process tree is always terminated on
/// timeout/cancel.
#[allow(clippy::too_many_arguments)]
pub async fn run_validation(
    workdir: &Path,
    command: &str,
    timeout: Duration,
    cancel_url: String,
    client: Client,
    server: &str,
    attempt_id: &str,
    fence: &str,
    sink: &Arc<EventSink>,
    secrets: &[String],
) -> Result<ValidationOutcome> {
    sink.push(
        EventType::Status,
        json!({ "status": "validating", "phase": "validation" }),
    )
    .await;
    // Best-effort: flip the CP attempt/task to `validating`. A failure here
    // only means the status stays `running`; the outcome codes below still
    // drive the terminal transition correctly.
    if !fence.is_empty() {
        let post = client
            .post(format!(
                "{server}/v1/node/attempts/{attempt_id}/begin_validate"
            ))
            .header("x-agentgrid-fencing-token", fence);
        match send_with_retry(post, 2).await {
            Ok(s) if s.is_success() => {}
            Ok(s) => tracing::warn!(
                attempt_id,
                "begin_validate got {s}; validation proceeds with status=running"
            ),
            Err(e) => tracing::warn!(attempt_id, "begin_validate failed: {e}"),
        }
    }

    // Hardening P0 item 12: structured argv — never `format!("{command} 2>&1")`.
    // The shell string is an explicit operator-trusted command (repository
    // validation_command), so `sh -c` is the documented contract; stdout and
    // stderr are piped separately and merged by the same lossy/line-capped
    // streaming the agent output uses.
    // Hardening P2 item 424: apply the sandbox policy to validation too — when
    // the sandbox is Docker, the validation command runs inside the same
    // hardened container the agent uses (cap-drop, no-new-privileges, network
    // none, read-only + tmpfs, resource limits), not as a bare host `sh -c`.
    let kind = sandbox_kind();
    let (program, prefix_args) = crate::sandbox::sandbox_prefix(
        kind, workdir, "sh",
        None, // validation has no task network_mode; sandbox env default applies
    );
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(prefix_args)
        .arg("-c")
        .arg(command)
        .current_dir(workdir)
        .process_group(0)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // The validation command inherits the daemon env by default (it needs
    // PATH, repo state, etc.). The unsafe guard applies the same rule as the
    // agent path: unsandboxed runs do NOT get the unsafe bypass env.
    for k in crate::sandbox::unsafe_env_guard(sandbox_kind()) {
        cmd.env_remove(k);
    }
    let mut child = cmd.spawn()?;
    let pid = child.id().unwrap_or(0);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("validation stdout pipe unavailable for {attempt_id}"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("validation stderr pipe unavailable for {attempt_id}"))?;
    let raw = tokio::fs::File::create(workdir.join("validation.log"))
        .await
        .ok();
    let raw = raw.map(|f| Arc::new(tokio::sync::Mutex::new(f)));
    let s1 = sink.clone();
    let secrets_out = secrets.to_vec();
    let r1 = tokio::spawn(read_stream(stdout, s1, "stdout", secrets_out, raw.clone()));
    let s2 = sink.clone();
    let secrets_err = secrets.to_vec();
    let r2 = tokio::spawn(read_stream(stderr, s2, "stderr", secrets_err, raw.clone()));

    enum VOutcome {
        Exited(i32),
        Timeout,
        Cancel,
    }
    let verdict = tokio::select! {
        status = child.wait() => VOutcome::Exited(status?.code().unwrap_or(-1)),
        _ = tokio::time::sleep(timeout) => VOutcome::Timeout,
        _ = wait_for_cancel(attempt_id, client.clone(), cancel_url) => VOutcome::Cancel,
    };
    let (code, timed_out, cancelled) = match verdict {
        VOutcome::Exited(c) => (c, false, false),
        VOutcome::Timeout => {
            terminate_group(pid);
            let status = child.wait().await;
            (
                status.ok().and_then(|s| s.code()).unwrap_or(-1),
                true,
                false,
            )
        }
        VOutcome::Cancel => {
            sink.push(
                EventType::Status,
                json!({ "status": "cancelled", "phase": "validation", "reason": "user_requested" }),
            )
            .await;
            terminate_group(pid);
            let status = child.wait().await;
            (
                status.ok().and_then(|s| s.code()).unwrap_or(-1),
                false,
                true,
            )
        }
    };
    let _ = r1.await;
    let _ = r2.await;
    // read_stream mirrors straight into validation.log via the raw file handle.
    Ok(ValidationOutcome {
        code,
        timed_out,
        cancelled,
    })
}

/// Hardening P0 item 12: the sandbox kind the validation subprocess sees. This
/// mirrors how the agent path picks `sandbox_prefix`; validation keeps it
/// simpler (no container prefix) but still applies the unsafe env guard.
fn sandbox_kind() -> crate::sandbox::SandboxKind {
    crate::sandbox::SandboxKind::from_env()
}
