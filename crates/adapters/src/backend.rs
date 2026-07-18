//! Execution backend contract (Stage 3.2).
//!
//! An `ExecutionBackend` knows how to spawn one attempt's agent process. The
//! first and only implementation is [`ProcessBackend`] — the existing native
//! subprocess-in-a-worktree model. The trait exists so a conformance suite can
//! drive adapters through one normalized `spawn` call and so future backends
//! (container, ACP) can be dropped in without touching the node daemon.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

/// Inputs needed to spawn one attempt's agent process.
pub struct SpawnRequest {
    pub bin: String,
    pub prompt: String,
    pub workdir: PathBuf,
    pub attempt_id: String,
    pub timeout: Duration,
    pub env: Vec<(String, String)>,
}

/// A running agent process: its stdout/stderr streams (for event streaming)
/// plus enough to cancel (pid) and collect (child handle). The node daemon
/// owns the rest of the lifecycle (stream/collect/cancel) against this handle.
pub struct BackendProcess {
    pub child: Child,
    pub stdout: ChildStdout,
    pub stderr: ChildStderr,
    pub pid: u32,
    pub timeout: Duration,
}

/// Contract for spawning an attempt's agent process (Stage 3.2).
pub trait ExecutionBackend {
    /// Spawn the adapter binary. Returns the running process, or an `io::Error`
    /// if the binary cannot be started.
    fn spawn(&self, req: SpawnRequest) -> std::io::Result<BackendProcess>;
}

/// Native subprocess backend: launches `bin --prompt <prompt>` in `workdir` as
/// its own process group, forwarding `AGENTGRID_ATTEMPT_ID` and any env.
pub struct ProcessBackend;

impl ExecutionBackend for ProcessBackend {
    fn spawn(&self, req: SpawnRequest) -> std::io::Result<BackendProcess> {
        let mut cmd = Command::new(&req.bin);
        cmd.arg("--prompt").arg(&req.prompt);
        cmd.current_dir(&req.workdir);
        cmd.env("AGENTGRID_ATTEMPT_ID", &req.attempt_id);
        for (k, v) in &req.env {
            cmd.env(k, v);
        }
        // Separate process group so a cancel can SIGTERM the whole tree.
        cmd.process_group(0);
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let pid = child.id().unwrap_or(0);
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        Ok(BackendProcess {
            child,
            stdout,
            stderr,
            pid,
            timeout: req.timeout,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn req(bin: &str) -> SpawnRequest {
        SpawnRequest {
            bin: bin.into(),
            prompt: "ignored".into(),
            workdir: std::env::temp_dir(),
            attempt_id: "t".into(),
            timeout: Duration::from_secs(5),
            env: vec![],
        }
    }

    #[tokio::test]
    async fn spawn_runs_process_and_collects_exit() {
        let mut bp = ProcessBackend.spawn(req("true")).unwrap();
        assert!(bp.child.wait().await.unwrap().success());
    }

    #[tokio::test]
    async fn spawn_failure_is_reported() {
        let mut bp = ProcessBackend.spawn(req("false")).unwrap();
        assert!(!bp.child.wait().await.unwrap().success());
    }

    #[tokio::test]
    async fn spawn_missing_binary_errors() {
        assert!(ProcessBackend
            .spawn(req("/no/such/adapter-binary"))
            .is_err());
    }
}
