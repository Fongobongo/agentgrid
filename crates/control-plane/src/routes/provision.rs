//! `POST /v1/node-installs` — provision a remote host as a node from the panel.
//!
//! The control plane normally never dials a node: delivery is pull (the node
//! polls, the CP assigns). Onboarding is the one exception, and it is bounded
//! on purpose:
//!
//! - **off by default.** `AGENTGRID_PROVISION_ENABLED=1` is required. A control
//!   plane that can SSH into arbitrary hosts is a different threat model from
//!   one that cannot, and an operator should have to say yes.
//! - **no stored credentials.** A password or key path arrives in the request,
//!   is used for the length of the install, and is never written to the
//!   database, the audit trail or a log line. Key paths refer to files on the
//!   control-plane host; with no key at all, ssh falls back to its agent.
//! - **direct transport only.** The CLI's default reverse tunnel points at the
//!   machine running the command. Here that machine is the control plane, so a
//!   tunnel would be `ssh -R` from the CP to the host and back — pointless
//!   indirection. The install therefore requires a server URL the *node* can
//!   reach (the CP itself, or a peer's relay).
//! - **jobs, not a blocking request.** An install is scp + ssh and takes
//!   seconds; the handler returns a job id and the UI polls the steps.
//!
//! The provisioning itself lives in `agentgrid-provision`, shared with
//! `ag nodes install`, so host-key policy has one implementation.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agentgrid_provision::{Plan, Target, Transport};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Extension, Json,
};
use serde::{Deserialize, Serialize};

use crate::auth::AuthedUser;
use crate::AppState;

/// How many finished jobs to keep for the UI. A panel is not a job archive:
/// past a couple of dozen entries the oldest finished ones are dropped.
const KEPT_JOBS: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Succeeded,
    Failed,
}

#[derive(Clone, Debug, Serialize)]
pub struct InstallJob {
    pub id: String,
    pub host: String,
    pub name: String,
    pub state: JobState,
    /// One line per completed step, in order. Appended by the install thread.
    pub steps: Vec<String>,
    pub error: Option<String>,
    /// The `AGENTGRID_SERVER` the node was configured with (success only).
    pub server_url: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

/// In-memory install jobs plus the resolved node binary.
///
/// `std::sync::Mutex`, not a tokio one: the install runs on a blocking thread
/// (it spawns ssh/scp), so it must be able to append a step without a runtime
/// handle. Nothing under the lock awaits.
pub struct ProvisioningRuntime {
    pub enabled: bool,
    node_binary: Option<PathBuf>,
    jobs: std::sync::Mutex<HashMap<String, InstallJob>>,
}

impl ProvisioningRuntime {
    /// Read the environment once at startup. The binary is resolved here rather
    /// than per request so a missing distribution is visible in
    /// `GET /v1/provisioning` before anyone fills in a form.
    pub fn from_env() -> Self {
        let enabled = std::env::var("AGENTGRID_PROVISION_ENABLED")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        Self {
            enabled,
            node_binary: resolve_node_binary(),
            jobs: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn node_binary(&self) -> Option<&PathBuf> {
        self.node_binary.as_ref()
    }

    fn insert(&self, job: InstallJob) {
        let mut jobs = self.jobs.lock().unwrap();
        if jobs.len() >= KEPT_JOBS {
            // Drop the oldest *finished* job; a running one is never evicted.
            let victim = jobs
                .values()
                .filter(|j| j.state != JobState::Running)
                .min_by(|a, b| a.started_at.cmp(&b.started_at))
                .map(|j| j.id.clone());
            if let Some(id) = victim {
                jobs.remove(&id);
            }
        }
        jobs.insert(job.id.clone(), job);
    }

    fn push_step(&self, id: &str, message: &str) {
        if let Some(job) = self.jobs.lock().unwrap().get_mut(id) {
            job.steps.push(message.to_string());
        }
    }

    fn finish(&self, id: &str, outcome: Result<String, String>) {
        if let Some(job) = self.jobs.lock().unwrap().get_mut(id) {
            job.finished_at = Some(now_iso());
            match outcome {
                Ok(server_url) => {
                    job.state = JobState::Succeeded;
                    job.server_url = Some(server_url);
                }
                Err(e) => {
                    job.state = JobState::Failed;
                    job.error = Some(e);
                }
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<InstallJob> {
        self.jobs.lock().unwrap().get(id).cloned()
    }

    pub fn list(&self) -> Vec<InstallJob> {
        let jobs = self.jobs.lock().unwrap();
        let mut v: Vec<InstallJob> = jobs.values().cloned().collect();
        v.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        v
    }
}

fn resolve_node_binary() -> Option<PathBuf> {
    let dir = std::env::var("AGENTGRID_NODE_DIST")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        })?;
    ["agentgrid-node-daemon", "agentgrid-node-daemon.exe"]
        .iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
}

fn now_iso() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Body of `POST /v1/node-installs`.
///
/// Deliberately **not** `Debug`: it carries a password and a key path, and a
/// stray `{:?}` in a log line is exactly how those end up in a log file.
#[derive(Deserialize)]
pub struct InstallRequest {
    /// `user@host` or `user@host:port`.
    pub host: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub adapters: Option<String>,
    #[serde(default)]
    pub repositories: Option<String>,
    #[serde(default)]
    pub max_concurrency: Option<u32>,
    /// URL the node will dial. Defaults to `AGENTGRID_PUBLIC_URL`.
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub data_dir: Option<String>,
    /// Private key path **on the control-plane host**. None = ssh agent.
    #[serde(default)]
    pub ssh_key: Option<String>,
    /// OpenSSH certificate for `ssh_key` (short-lived, CA-signed). Requires
    /// `ssh_key`; mutually exclusive with `password`.
    #[serde(default)]
    pub ssh_cert: Option<String>,
    /// Password auth (needs `sshpass` or `expect` on the control-plane host).
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub accept_new_host_key: bool,
    #[serde(default)]
    pub host_key_fingerprint: Option<String>,
    #[serde(default)]
    pub allow_root: bool,
}

#[derive(Serialize)]
pub struct InstallAccepted {
    pub job_id: String,
    pub state: JobState,
}

#[derive(Serialize)]
pub struct ProvisioningInfo {
    pub enabled: bool,
    /// Absolute path of the daemon binary the control plane would copy.
    pub node_binary: Option<String>,
    /// Why installs cannot run, when they cannot. Rendered by the UI verbatim.
    pub reason: Option<String>,
}

#[derive(Serialize)]
pub struct ApiError {
    pub error: String,
}

type ApiResult<T> = Result<(StatusCode, Json<T>), (StatusCode, Json<ApiError>)>;

fn bad(code: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<ApiError>) {
    (code, Json(ApiError { error: msg.into() }))
}

/// `GET /v1/provisioning` — can the panel offer the Install button, and
/// if not, why? Read-only, so an operator (GET-only) may call it too.
pub async fn provisioning_info(State(state): State<Arc<AppState>>) -> Json<ProvisioningInfo> {
    let rt = &state.provisioning;
    let reason = if !rt.enabled {
        Some(
            "provisioning is disabled on this control plane; set AGENTGRID_PROVISION_ENABLED=1 \
             and restart it to install nodes from here"
                .to_string(),
        )
    } else if rt.node_binary().is_none() {
        Some(
            "the control plane cannot find agentgrid-node-daemon; point AGENTGRID_NODE_DIST at \
             the directory that holds it"
                .to_string(),
        )
    } else {
        None
    };
    Json(ProvisioningInfo {
        enabled: rt.enabled,
        node_binary: rt.node_binary().map(|p| p.display().to_string()),
        reason,
    })
}

/// `POST /v1/node-installs` — mint a one-shot token and provision the host in
/// the background. Returns 202 with a job id; poll
/// `GET /v1/node-installs/{id}` for the steps.
pub async fn start_install(
    State(state): State<Arc<AppState>>,
    Extension(auth): Extension<AuthedUser>,
    Json(req): Json<InstallRequest>,
) -> ApiResult<InstallAccepted> {
    if !state.provisioning.enabled {
        return Err(bad(
            StatusCode::FORBIDDEN,
            "provisioning is disabled on this control plane \
             (AGENTGRID_PROVISION_ENABLED=1 to enable)",
        ));
    }
    let Some(binary) = state.provisioning.node_binary().cloned() else {
        return Err(bad(
            StatusCode::CONFLICT,
            "agentgrid-node-daemon not found next to the control plane; \
             set AGENTGRID_NODE_DIST to the directory that holds it",
        ));
    };
    let server = req
        .server
        .clone()
        .or_else(|| std::env::var("AGENTGRID_PUBLIC_URL").ok())
        .filter(|s| !s.trim().is_empty());
    let Some(server) = server else {
        return Err(bad(
            StatusCode::BAD_REQUEST,
            "no server URL: send `server` (the address the node will dial) or set \
             AGENTGRID_PUBLIC_URL on the control plane",
        ));
    };

    let name = req
        .name
        .clone()
        .unwrap_or_else(|| "remote-node".to_string())
        .trim()
        .to_string();
    let plan = Plan {
        // A tunnel from the control plane to the host and back would only add a
        // hop: the node can dial the CP (or a peer's relay) directly.
        transport: Transport::Direct,
        name: name.clone(),
        repositories: req.repositories.clone().unwrap_or_else(|| "*".into()),
        adapters: req.adapters.clone().unwrap_or_else(|| "mock".into()),
        max_concurrency: req.max_concurrency.unwrap_or(2),
        data_dir: req
            .data_dir
            .clone()
            .unwrap_or_else(|| "/var/lib/agentgrid".into()),
        server: Some(server.clone()),
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        allow_root: req.allow_root,
        // Unused by `Transport::Direct`; kept at the CLI defaults.
        local_port: 7800,
        remote_port: 7800,
        binary,
    };
    let target = Target {
        host: req.host.trim().to_string(),
        ssh_key: req.ssh_key.clone(),
        ssh_cert: req.ssh_cert.clone(),
        password: req.password.clone(),
        accept_new_host_key: req.accept_new_host_key,
        host_key_fingerprint: req.host_key_fingerprint.clone(),
    };
    if let Err(e) = agentgrid_provision::validate(&target, &plan) {
        return Err(bad(StatusCode::BAD_REQUEST, e.to_string()));
    }

    let (token, _expires_at) = match state.store.create_enrollment_token().await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("install: minting an enrollment token failed: {e}");
            return Err(bad(
                StatusCode::SERVICE_UNAVAILABLE,
                "could not mint an enrollment token",
            ));
        }
    };

    let id = uuid::Uuid::new_v4().to_string();
    state.provisioning.insert(InstallJob {
        id: id.clone(),
        host: target.host.clone(),
        name: name.clone(),
        state: JobState::Running,
        steps: Vec::new(),
        error: None,
        server_url: None,
        started_at: now_iso(),
        finished_at: None,
    });

    // Security-sensitive mutation: record who asked for it before it runs, with
    // the host as the subject (there is no node id yet).
    let _ = state
        .store
        .audit(
            "user",
            Some(auth.username.as_str()),
            "node.install",
            Some(&target.host),
            Some(&format!("job={id} name={name}")),
        )
        .await;

    tracing::warn!(
        job = %id,
        host = %target.host,
        name = %name,
        actor = %auth.username,
        "provisioning a node over ssh from the control plane"
    );

    let rt = state.provisioning.clone();
    let job_id = id.clone();
    // ssh/scp are blocking child processes; keep them off the reactor.
    tokio::task::spawn_blocking(move || {
        let mut log = |m: &str| rt.push_step(&job_id, m);
        let outcome = agentgrid_provision::install(&target, &plan, &token, &mut log);
        rt.finish(
            &job_id,
            outcome.map(|o| o.server_url).map_err(|e| format!("{e:#}")),
        );
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(InstallAccepted {
            job_id: id,
            state: JobState::Running,
        }),
    ))
}

/// `GET /v1/node-installs/{id}` — steps so far, and the terminal state.
pub async fn get_install(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthedUser>,
    Path(id): Path<String>,
) -> ApiResult<InstallJob> {
    match state.provisioning.get(&id) {
        Some(job) => Ok((StatusCode::OK, Json(job))),
        None => Err(bad(StatusCode::NOT_FOUND, "no such install job")),
    }
}

/// `GET /v1/node-installs` — recent jobs, newest first (the panel's list).
pub async fn list_installs(
    State(state): State<Arc<AppState>>,
    Extension(_auth): Extension<AuthedUser>,
) -> Json<Vec<InstallJob>> {
    Json(state.provisioning.list())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime() -> ProvisioningRuntime {
        ProvisioningRuntime {
            enabled: true,
            node_binary: None,
            jobs: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn job(id: &str, started: &str, state: JobState) -> InstallJob {
        InstallJob {
            id: id.into(),
            host: "root@10.0.0.5".into(),
            name: "n".into(),
            state,
            steps: vec![],
            error: None,
            server_url: None,
            started_at: started.into(),
            finished_at: None,
        }
    }

    #[test]
    fn finishing_records_the_outcome() {
        let rt = runtime();
        rt.insert(job("a", "2026-01-01T00:00:00Z", JobState::Running));
        rt.push_step("a", "copying agentgrid-node-daemon");
        rt.finish("a", Ok("http://cp:7800".into()));
        let j = rt.get("a").unwrap();
        assert_eq!(j.state, JobState::Succeeded);
        assert_eq!(j.server_url.as_deref(), Some("http://cp:7800"));
        assert_eq!(j.steps, vec!["copying agentgrid-node-daemon".to_string()]);
        assert!(j.finished_at.is_some());
    }

    #[test]
    fn a_failed_job_keeps_the_error() {
        let rt = runtime();
        rt.insert(job("a", "2026-01-01T00:00:00Z", JobState::Running));
        rt.finish(
            "a",
            Err("ssh/scp step failed (prepare remote dir): exit 255".into()),
        );
        let j = rt.get("a").unwrap();
        assert_eq!(j.state, JobState::Failed);
        assert!(j.error.unwrap().contains("exit 255"));
        assert!(j.server_url.is_none());
    }

    /// The list is a panel, not a job archive: past `KEPT_JOBS` the oldest
    /// *finished* job goes. The newest one is always kept.
    #[test]
    fn oldest_finished_job_is_evicted_first() {
        let rt = runtime();
        for i in 0..KEPT_JOBS {
            let id = format!("j{i}");
            let started = format!("2026-01-01T00:00:{i:02}Z");
            rt.insert(job(&id, &started, JobState::Succeeded));
        }
        rt.insert(job("new", "2026-01-01T00:01:00Z", JobState::Running));
        let all = rt.list();
        assert_eq!(all.len(), KEPT_JOBS);
        assert!(rt.get("j0").is_none(), "oldest finished job should be gone");
        assert!(rt.get("new").is_some(), "the new job must survive");
    }

    /// An install in flight is never evicted, even when the cap is reached:
    /// the UI is polling that id, and dropping it would look like a lost job.
    #[test]
    fn running_jobs_are_never_evicted() {
        let rt = runtime();
        for i in 0..KEPT_JOBS {
            let id = format!("j{i}");
            let started = format!("2026-01-01T00:00:{i:02}Z");
            rt.insert(job(&id, &started, JobState::Running));
        }
        rt.insert(job("new", "2026-01-01T00:01:00Z", JobState::Running));
        assert_eq!(rt.list().len(), KEPT_JOBS + 1);
        assert!(rt.get("j0").is_some());
    }
}
