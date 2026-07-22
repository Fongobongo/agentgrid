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
    /// Stage 12: optional resource limits the backend should apply if it can.
    /// A backend that cannot enforce a limit (e.g. `ProcessBackend` without a
    /// cgroup scope) reports [`BackendProcess::enforced_limits`] = `false` so
    /// profiles honestly reflect the isolation level.
    pub limits: ResourceLimits,
}

/// Resource ceiling an attempt's agent must not exceed (Stage 12). Maps to
/// cgroups v2 / systemd transient scope knobs on Linux; other backends apply
/// what they can and claim only that.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceLimits {
    /// Max RSS in bytes (systemd `MemoryMax=` / cgroup `memory.max`).
    pub memory_max: Option<u64>,
    /// CPU quota as a percentage of one core (systemd `CPUQuota=`; 200 = 2 cores).
    pub cpu_quota_percent: Option<u32>,
    /// Max tasks (PIDs) in the cgroup (systemd `TasksMax=`).
    pub tasks_max: Option<u32>,
}

/// Why an attempt's agent terminated, as classified by the backend (Stage 12).
/// Lets the node map an exit to an `error_code` (`resource_limit` for a hit
/// ceiling) without each backend re-implementing the heuristic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendOutcome {
    /// Exited normally (success is the caller's job).
    Exited { code: Option<i32> },
    /// Killed by `signal` (e.g. SIGTERM/SIGKILL from cancel or timeout).
    Killed { signal: i32 },
    /// A resource ceiling was hit (OOM, task limit). Node → `resource_limit`.
    ResourceLimit { reason: String },
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
    /// Whether the backend actually enforced [`SpawnRequest::limits`]. A native
    /// `ProcessBackend` reports `false` (no cgroup); a cgroup/container backend
    /// reports `true`. Profiles use this to refuse a strict run on a backend
    /// that can't enforce.
    pub enforced_limits: bool,
}

/// Classify a child's raw exit status into a [`BackendOutcome`]. Used by every
/// backend; Linux backends additionally set `ResourceLimit` when the cgroup
/// reports an OOM kill, but the signal path is shared.
pub fn classify_exit(status: std::process::ExitStatus) -> BackendOutcome {
    if let Some(code) = status.code() {
        return BackendOutcome::Exited { code: Some(code) };
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            // SIGKILL is the common OOM-killer signal; a cgroup-aware backend
            // upgrades this to ResourceLimit when it can prove the limit fired.
            return BackendOutcome::Killed { signal: sig };
        }
    }
    BackendOutcome::Exited { code: None }
}

impl BackendOutcome {
    /// Map an outcome to the control-plane `error_code` string. `None` = no
    /// error (success path); a backend that proved a resource ceiling hit yields
    /// `resource_limit` (Stage 12).
    pub fn error_code(&self) -> Option<String> {
        match self {
            BackendOutcome::Exited { code: Some(0) } => None,
            BackendOutcome::Exited { code: Some(c) } => Some(format!("agent_failed:exit {c}")),
            BackendOutcome::Exited { code: None } => Some("agent_failed".into()),
            BackendOutcome::Killed { signal } => Some(format!("agent_failed:killed by {signal}")),
            BackendOutcome::ResourceLimit { reason } => Some(format!("resource_limit:{reason}")),
        }
    }
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
            enforced_limits: false,
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
            limits: ResourceLimits::default(),
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

    #[tokio::test]
    async fn process_backend_does_not_enforce_limits() {
        // ProcessBackend has no cgroup; it must admit enforced_limits=false so
        // profiles honestly refuse a strict run (Stage 12 capability-honesty).
        let mut bp = ProcessBackend.spawn(req("true")).unwrap();
        assert!(!bp.enforced_limits);
        let _ = bp.child.wait().await;
    }

    #[test]
    fn classify_exit_maps_cleanup() {
        // Normal exit → Exited with the code.
        let s = std::process::Command::new("true").status().unwrap();
        assert!(matches!(
            classify_exit(s),
            BackendOutcome::Exited { code: Some(0) }
        ));
        // Non-zero exit is still Exited (not Killed).
        let s = std::process::Command::new("false").status().unwrap();
        assert!(matches!(classify_exit(s), BackendOutcome::Exited { code: Some(c) } if c != 0));
    }

    #[test]
    fn outcome_error_code_distinguishes_resource_limit() {
        // Success → no error code.
        assert_eq!(BackendOutcome::Exited { code: Some(0) }.error_code(), None);
        // Resource limit → error_code starts with resource_limit (:reason).
        let out = BackendOutcome::ResourceLimit {
            reason: "oom".into(),
        }
        .error_code();
        assert!(out.as_deref().unwrap().starts_with("resource_limit"));
        // A plain crash stays agent_failed, not resource_limit.
        assert!(BackendOutcome::Exited { code: Some(127) }
            .error_code()
            .as_deref()
            .unwrap()
            .contains("agent_failed"));
    }
}
