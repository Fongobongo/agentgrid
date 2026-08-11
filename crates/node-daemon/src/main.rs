//! Node daemon: long-polls the control plane, runs the adapter as a separate
//! process group in a per-attempt worktree, streams stdout/stderr as events,
//! and reports completion. Stage-1 version: in-memory, mock adapter only.

use std::sync::Arc;
use std::time::Duration;

use agentgrid_acp::{
    map_session_update, InitializeParams, Message, SessionCancelParams, SessionNewParams,
    SessionPromptParams,
};
use agentgrid_common::{
    policy::{AutonomyLevel, BuiltinPolicyProvider, CommandPolicyProvider, PolicyDecision},
    ApprovalStatus, ApprovalView, Assignment, EventKind, EventType, NoopContextProvider,
};
use anyhow::Result;

use serde_json::{json, Value};

mod account_usage;
mod artifact_spool;
mod attempt_runner;
mod capabilities;
mod command_guard;
mod completion;
mod config;
mod enrollment;
pub mod evals;
mod event_sink;
mod git;
mod heartbeat;
mod mcp;
mod outbox;
mod polling;
mod process_supervisor;
mod profiles;
mod recovery;
mod sandbox;
mod secret_redactor;
mod skills;
mod validation;
mod ws;

use capabilities::{probe_adapter, probe_cluster_adapter, resolve_acp_launch, resolve_adapter_bin};
use completion::{terminate_group, wait_for_cancel};
use config::{config_from_env, Config};
use enrollment::load_or_enroll;
use event_sink::EventSink;
use mcp::mcp_servers_payload;
use profiles::{
    agent_profile, check_adapter_compatibility, check_profile_secrets, effective_autonomy,
    fetch_agent_profile,
};
use skills::{compose_context_block, compose_skills_block};

#[cfg(test)]
use config::AdapterSpec;
#[cfg(test)]
use enrollment::scrub_token_from_file;

/// Terminal outcome of an ACP-driven attempt.
struct AcpResult {
    success: bool,
    error_code: Option<String>,
    /// ACP session id from `session/new`, reported back so the control plane
    /// can resume it on a follow-up task (Stage 11.5).
    session_id: Option<String>,
    /// Plan 1.8 (#15): the provider answered 429 / rate limit — the attempt
    /// should be retried on the next account in the pool (rotate the token).
    rate_limited: bool,
}

/// Stage 5: drive an ACP agent over stdio (JSON-RPC 2.0). Spawns
/// `adapter-<id>`, runs initialize/new/prompt, forwards `session/update` into
/// the event sink, answers `session/request_permission` via the durable
/// approval flow, and returns the terminal outcome. Cancellation/timeout are
/// handled here (the wrapper path keeps those in run_attempt's select!).
async fn drive_acp_session(
    cfg: &Config,
    client: &reqwest::Client,
    assignment: &Assignment,
    ws_path: &std::path::Path,
    sink: Arc<EventSink>,
) -> Result<AcpResult> {
    // Native ACP launcher (direct CLI, e.g. `claude --acp`) takes priority
    // over the `adapter-<id>` wrapper binary.
    let (program, args) = match resolve_acp_launch(&assignment.adapter) {
        Some((program, args)) => (program, args),
        None => match resolve_adapter_bin(&assignment.adapter) {
            Some(b) => (b, vec![]),
            None => {
                tracing::error!(adapter = %assignment.adapter, "ACP adapter binary not found");
                return Ok(AcpResult {
                    success: false,
                    error_code: Some("infrastructure_failed".into()),
                    session_id: None,
                    rate_limited: false,
                });
            }
        },
    };
    let (program, args) = sandbox::sandbox_command(
        cfg.sandbox,
        &program,
        &args,
        ws_path,
        assignment.network_mode.as_deref(),
        assignment.read_only,
    );
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        // Own process group so terminate_group's killpg reaches the whole
        // agent tree on cancel/timeout (matches validation.rs).
        .process_group(0);
    // Hardening P0/P1 item 5: never let an unsandboxed agent run unsafe-unattended
    // unless the operator opted in — strip the bypass env the adapter otherwise
    // inherits from the daemon's parent process.
    for k in sandbox::unsafe_env_guard(cfg.sandbox) {
        cmd.env_remove(k);
    }
    // Hardening P1 item 27: do not inherit the daemon's full environment
    // (node credentials / secrets); start from PATH + HOME + the explicit
    // allowlist so the ACP child never sees daemon secrets.
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    if let Ok(home) = std::env::var("HOME") {
        cmd.env("HOME", home);
    }
    for (k, v) in &cfg.adapter_env {
        cmd.env(k, v);
    }
    // Forward the agent profile as an env hint for agents that read it.
    // Stage 13: prefer the control-plane active profile; fall back to env.
    let cp_profile = fetch_agent_profile(client, &cfg.server, &assignment.adapter).await;
    let profile_text = cp_profile
        .as_ref()
        .map(|p| p.system_prompt.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| agent_profile(&assignment.adapter));
    if let Some(text) = &profile_text {
        cmd.env("AGENTGRID_SYSTEM_PROMPT", text);
    }
    // Hardening P1 item 27: profile-declared secrets live in the daemon env but
    // the child no longer inherits it — forward the ones the profile requires.
    if let Some(p) = &cp_profile {
        for req in &p.secret_requirements {
            if let Some(v) = std::env::var_os(&req.env) {
                cmd.env(&req.env, v);
            }
        }
    }
    // Stage 13 secret-ref sync: required secrets must be set in the node env
    // before the agent starts. A missing required secret is fail-closed —
    // refuse to run (infrastructure_failed) rather than launch an agent that
    // will silently fail its first tool call. Optional secrets only warn.
    if let Some(code) = check_profile_secrets(cp_profile.as_ref()) {
        return Ok(AcpResult {
            success: false,
            error_code: Some(code),
            session_id: None,
            rate_limited: false,
        });
    }
    // Plan 1.12 (#7): shared-context group id — the agent gets AG_GROUP_ID so
    // it can `ag ctx set/get` against its group's shared notes.
    if let Some(gid) = &assignment.group_id {
        cmd.env("AG_GROUP_ID", gid);
    }
    // Stage 13 capability check: the profile's declared adapter_version must
    // be compatible with the installed adapter (cached probe from startup).
    if let Some(code) = check_adapter_compatibility(
        cp_profile.as_ref(),
        cfg.adapter_versions
            .get(&assignment.adapter)
            .and_then(|v| v.as_deref()),
    ) {
        return Ok(AcpResult {
            success: false,
            error_code: Some(code),
            session_id: None,
            rate_limited: false,
        });
    }
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stdin = child.stdin.take().expect("piped stdin");
    let (acp, mut notif) = agentgrid_acp::new(stdout, stdin);
    let acp = std::sync::Arc::new(acp);

    let model = std::env::var("AGENTGRID_AGENT_VERSION").unwrap_or_else(|_| "default".into());
    if let Err(e) = acp
        .initialize(InitializeParams {
            protocol_version: "0.1".into(),
            agent: assignment.adapter.clone(),
            model,
            session_id: None,
            cwd: ws_path.to_string_lossy().into_owned(),
            capabilities: Value::Null,
            client: Value::Null,
        })
        .await
    {
        tracing::error!("ACP initialize failed: {e}");
        let _ = child.start_kill();
        return Ok(AcpResult {
            success: false,
            error_code: Some("infrastructure_failed".into()),
            session_id: None,
            rate_limited: false,
        });
    }
    let mcp_subset = cp_profile
        .as_ref()
        .map(|p| p.mcp_server_ids.clone())
        .unwrap_or_default();
    let mcp_payload = mcp_servers_payload(client, &cfg.server, &mcp_subset).await;
    let session_id = match acp
        .session_new(SessionNewParams {
            agent: assignment.adapter.clone(),
            model: None,
            cwd: ws_path.to_string_lossy().into_owned(),
            prompt: None,
            mcp: mcp_payload,
            parent_session_id: assignment.parent_acp_session_id.clone(),
        })
        .await
    {
        Ok(r) => r.session_id,
        Err(e) => {
            tracing::error!("ACP session/new failed: {e}");
            let _ = child.start_kill();
            return Ok(AcpResult {
                success: false,
                error_code: Some("infrastructure_failed".into()),
                session_id: None,
                rate_limited: false,
            });
        }
    };

    let flusher = tokio::spawn(sink.clone().run_flusher());

    let sid = session_id.clone();
    let task_id = assignment.task_id.clone();
    let attempt_id = assignment.attempt_id.clone();
    let sink2 = sink.clone();
    let acp2 = acp.clone();
    let client2 = client.clone();
    let server2 = cfg.server.clone();
    let autonomy = effective_autonomy(cfg.autonomy, cp_profile.as_ref());
    // Plan 1.8 (#15): set by the stream task when an error event carries a
    // rate-limit marker; read at outcome time to decide account rotation.
    let rate_limited = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let rate_limited2 = rate_limited.clone();
    let stream_task = tokio::spawn(async move {
        while let Some(msg) = notif.recv().await {
            match msg {
                Message::Notification { params, .. } => {
                    let upd = params.get("update").unwrap_or(&params);
                    let env = map_session_update(&sid, upd);
                    // Plan 1.8 (#15): sniff provider rate-limit errors in the
                    // event stream (type=error payload with 429 / rate limit /
                    // too many requests / overloaded / quota) — the ACP
                    // session may survive them, but the attempt must rotate.
                    if env.kind == EventKind::Error {
                        let text = env.payload.to_string();
                        let l = text.to_ascii_lowercase();
                        if l.contains("429")
                            || l.contains("rate limit")
                            || l.contains("too many requests")
                            || l.contains("overloaded")
                            || l.contains("quota")
                        {
                            rate_limited2.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    sink2.push(env.kind.to_event_type(), env.payload).await;
                    sink2.note_adapter_event();
                }
                Message::Request { id, method, params }
                    if method == "session/request_permission" =>
                {
                    let allow = request_permission(
                        &client2,
                        &server2,
                        &task_id,
                        &attempt_id,
                        &sid,
                        &params,
                        autonomy,
                        &sink2,
                    )
                    .await;
                    let _ = acp2.respond(id, allow).await;
                }
                _ => {}
            }
        }
    });

    let acp3 = acp.clone();
    let mut prompt_text = assignment.prompt.clone();
    // Stage 11 (CTX): append a repo context pack (if a provider is configured).
    // Noop by default → empty body → agent proceeds without a digest.
    let ctx_provider = NoopContextProvider;
    prompt_text.push_str(&compose_context_block(&ctx_provider, assignment, &sink).await);
    // Stage 9.2: append the operator-trusted skills discovered in this worktree
    // (fail-closed: untrusted skills are omitted, any lookup error = no block).
    prompt_text.push_str(&compose_skills_block(client, &cfg.server, ws_path).await);
    let sid_prompt = session_id.clone();
    let mut prompt = tokio::spawn(async move {
        acp3.session_prompt(SessionPromptParams {
            session_id: sid_prompt,
            prompt: prompt_text,
        })
        .await
    });
    let cancel_client = client.clone();
    let cancel_url = format!(
        "{}/v1/node/attempts/{}/cancel",
        cfg.server, assignment.attempt_id
    );
    let pid = child.id().unwrap_or(0);
    let timeout = Duration::from_secs(assignment.timeout_secs.max(1));
    let outcome = tokio::select! {
        res = &mut prompt => match res {
            Ok(_) => {
                let rl = rate_limited.load(std::sync::atomic::Ordering::Relaxed);
                AcpResult { success: true, error_code: None, session_id: Some(session_id.clone()), rate_limited: rl }
            }
            Err(e) => {
                let rl = rate_limited.load(std::sync::atomic::Ordering::Relaxed);
                AcpResult { success: false, error_code: Some(format!("agent_error: {e}")), session_id: Some(session_id.clone()), rate_limited: rl }
            }
        },
        _ = wait_for_cancel(&assignment.attempt_id, cancel_client, cancel_url) => {
            // Ponytail: bound the session_cancel RPC so a process already
            // tearing down (or one that ignores session/cancel) can't park
            // drive_acp_session forever. The reap below still enforces
            // termination via signals.
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                acp.session_cancel(SessionCancelParams { session_id: session_id.clone() }),
            )
            .await;
            // Bound the reap for the same reason as the timeout branch.
            terminate_group(pid);
            let _ = tokio::time::timeout(
                Duration::from_secs(12),
                child.wait(),
            )
            .await;
            AcpResult { success: false, error_code: Some("cancelled".into()), session_id: Some(session_id.clone()), rate_limited: false }
        }
        _ = tokio::time::sleep(timeout) => {
            terminate_group(pid);
            // Bound the reap so a child that ignores SIGTERM (or a pidfd that
            // never fires) can't park the session forever. terminate_group
            // escalates to SIGKILL after 10s, so allow a little slack.
            let _ = tokio::time::timeout(
                Duration::from_secs(15),
                child.wait(),
            )
            .await;
            AcpResult { success: false, error_code: Some("timeout".into()), session_id: Some(session_id.clone()), rate_limited: false }
        }
    };
    stream_task.abort();
    flusher.abort();
    Ok(outcome)
}

/// Stage 5/9.1: answer `session/request_permission`. First the builtin
/// command-policy provider classifies the requested command; an `Allow`
/// short-circuits (no operator round-trip), `Deny` is rejected outright, and
/// only `Ask` falls through to the durable operator approval flow below.
/// Fail-closed: any error or timeout denies.
///
/// The provider handles only Bash-style shell commands (the common ACP case,
/// `permission = {tool:"Bash", input:"<cmd>"}`); other tools always reach the
/// approval flow — see `enforcement_boundary` doc: a wrapper adapter without
/// structured tool calls cannot be fully intercepted.
#[allow(clippy::too_many_arguments)]
async fn request_permission(
    client: &reqwest::Client,
    server: &str,
    task_id: &str,
    attempt_id: &str,
    session_id: &str,
    permission: &Value,
    autonomy: AutonomyLevel,
    sink: &Arc<EventSink>,
) -> bool {
    // Stage 9.1 local short-circuit for Bash commands.
    if let Some(decision) = policy_decision(permission, autonomy) {
        sink.push(
            EventType::Status,
            json!({
                "kind": "permission_decision",
                "decision": decision.0,
                "risk_class": decision.1,
                "reason": decision.2,
                "source": "local_policy",
                "autonomy": autonomy,
            }),
        )
        .await;
        return decision.0 == PolicyDecision::Allow;
    }
    // Fall through: ask the operator via the durable approval flow.
    let create = client
        .post(format!("{server}/v1/tasks/{task_id}/approvals"))
        .json(&json!({ "attempt_id": attempt_id, "session_id": session_id, "permission": permission }))
        .send()
        .await;
    let id = match create {
        Ok(r) if r.status().is_success() => match r.json::<Value>().await {
            Ok(v) => v
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            Err(_) => return false,
        },
        _ => return false,
    };
    if id.is_empty() {
        return false;
    }
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        match client
            .get(format!("{server}/v1/approvals/{id}"))
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => match resp.json::<ApprovalView>().await {
                Ok(av) => match av.status {
                    ApprovalStatus::Allowed => return true,
                    ApprovalStatus::Pending => continue,
                    _ => return false,
                },
                Err(_) => return false,
            },
            _ => return false,
        }
    }
    false
}

/// Stage 9.1: evaluate a `session/request_permission` against the builtin
/// command policy. Returns `Some((decision, risk_class, reason))` only for a
/// definitive local `Allow`/`Deny` of a Bash-style shell command
/// (`{tool:"Bash", input:"<cmd>"}`); `Ask` and non-Bash tools return `None`
/// (→ approval flow, fail-closed to the operator).
fn policy_decision(
    permission: &Value,
    autonomy: AutonomyLevel,
) -> Option<(PolicyDecision, String, String)> {
    let tool = permission.get("tool").and_then(|v| v.as_str())?;
    if !tool.eq_ignore_ascii_case("bash") {
        return None;
    }
    let cmd = permission.get("input").and_then(|v| v.as_str())?;
    // Stage 9.1: an external provider (CodeAlive bash-guard / DCG) takes
    // precedence when AGENTGRID_POLICY_BINARY is set; otherwise the builtin.
    // Both are fail-closed to `Ask` on error → falls through to approval flow.
    let verdict = match std::env::var("AGENTGRID_POLICY_BINARY") {
        Ok(bin) => {
            let version = std::env::var("AGENTGRID_POLICY_VERSION").unwrap_or_default();
            let p = agentgrid_common::policy::ExternalPolicyProvider::new(bin, version);
            p.evaluate(cmd, "").ok()?
        }
        Err(_) => BuiltinPolicyProvider::new()
            .evaluate_with(autonomy, cmd, "")
            .ok()?,
    };
    // `Ask` is not a local decision: fall through to the operator approval flow.
    if verdict.decision == PolicyDecision::Ask {
        return None;
    }
    Some((
        verdict.decision,
        format!("{:?}", verdict.risk_class),
        verdict.reason,
    ))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Stage 5.1: refuse to run as root unless explicitly allowed.
    if unsafe { libc::getuid() } == 0 && std::env::var_os("AGENTGRID_ALLOW_ROOT").is_none() {
        anyhow::bail!("refusing to run as root; set AGENTGRID_ALLOW_ROOT=1 to override");
    }

    // Fail-closed gate on unsafe unattended mode: it disables the sandbox and
    // bypasses permission interception, so it must be explicitly acknowledged
    // or the daemon refuses to start.
    if agentgrid_adapters::unsafe_unattended_from_env() {
        if !agentgrid_adapters::unsafe_ack_from_env() {
            anyhow::bail!(
                "AGENTGRID_UNSAFE_UNATTENDED is set but AGENTGRID_I_UNDERSTAND_UNSAFE=1 is missing; \
                 unsafe mode runs without a sandbox and bypasses permissions — refusing to start"
            );
        }
        tracing::warn!(
            "UNSAFE UNATTENDED MODE ACTIVE: no sandbox, permissions bypassed; \
             this node is flagged in the control plane"
        );
    }

    let mut cfg = config_from_env();
    for a in &cfg.adapters {
        let probe = if a.id == "zeroshot" {
            probe_cluster_adapter("zeroshot", "docker").await
        } else {
            let bin = format!("adapter-{}", a.id.replace('_', "-"));
            probe_adapter(&bin).await
        };
        if probe.found {
            cfg.adapter_versions
                .insert(a.id.clone(), probe.version.clone());
            tracing::info!(adapter = %a.id, version = ?probe.version, "adapter detected");
        } else {
            cfg.adapter_versions.insert(a.id.clone(), None);
            tracing::warn!(
                adapter = %a.id,
                "adapter for {} not ready (missing runtime/binary or version mismatch); node will report degraded",
                a.id
            );
        }
        // Plan §25: smoke-test the adapter inside the sandbox image too — the
        // host probe proves nothing about what the container ships. Report
        // degraded (not fatal) when the image lacks the adapter.
        if matches!(cfg.sandbox, sandbox::SandboxKind::Docker) && a.id != "zeroshot" {
            let bin = format!("adapter-{}", a.id.replace('_', "-"));
            match sandbox::probe_adapter_in_sandbox(&bin).await {
                Ok(true) => tracing::info!(adapter = %a.id, "adapter present in sandbox image"),
                Ok(false) => {
                    cfg.adapter_versions.insert(a.id.clone(), None);
                    tracing::warn!(
                        adapter = %a.id, bin = %bin,
                        "adapter missing in sandbox image; node will report degraded"
                    );
                }
                Err(e) => tracing::warn!(
                    adapter = %a.id, error = %e,
                    "sandbox smoke test failed (runtime/image issue)"
                ),
            }
        }
    }
    tokio::fs::create_dir_all(&cfg.workspace_root).await?;
    // Plan §25: verify the container runtime (version + daemon reachable)
    // when the sandbox is configured. Fails loud at startup so a broken
    // Docker/Podman setup never silently degrades into unsandboxed runs.
    if matches!(cfg.sandbox, sandbox::SandboxKind::Docker) {
        // Fail-closed: an unenforceable/malformed egress allowlist must stop
        // the daemon, never silently run with full egress.
        let net = std::env::var("AGENTGRID_SANDBOX_NETWORK").unwrap_or_else(|_| "none".to_string());
        sandbox::validate_network_mode(&net)?;
        match sandbox::probe_runtime_version().await {
            Ok(Some(v)) => tracing::info!(runtime = %v, "container runtime ready"),
            Ok(None) => tracing::warn!(
                "container runtime configured but unreachable (binary missing or daemon down); sandboxed runs will fail at spawn"
            ),
            Err(e) => tracing::warn!(error = %e, "container runtime probe failed"),
        }
    }
    // Stage 2.3: reclaim workspace dirs + worktree gitlinks a prior (killed)
    // run left behind. Default 24h retention; tune with
    // AGENTGRID_WORKSPACE_RETENTION_HOURS (0 disables pruning).
    let retention_h: u64 = std::env::var("AGENTGRID_WORKSPACE_RETENTION_HOURS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(24);
    if retention_h > 0 {
        let stats = tokio::task::spawn_blocking({
            let ws = cfg.workspace_root.clone();
            let repos = cfg.repository_root.clone();
            move || {
                git::prune_stale_workspaces(
                    &ws,
                    &repos,
                    std::time::Duration::from_secs(retention_h * 3600),
                )
            }
        })
        .await
        .ok();
        // Hardening P1 item 33/35: cleanup observability — log the prune
        // verdict so operators can see how much was reclaimed/quarantined.
        if let Some(stats) = stats {
            tracing::info!(
                pruned = stats.pruned,
                quarantined = stats.quarantined,
                worktrees_pruned = stats.worktrees_pruned,
                "stale workspace prune complete"
            );
        }
    }
    let cred = load_or_enroll(&cfg).await?;
    sandbox::set_node_id(&cred.node_id);
    // Plan §25: a hard-crashed daemon can strand attached containers; clean
    // this daemon's orphans before polling so slots aren't leaked.
    if matches!(cfg.sandbox, sandbox::SandboxKind::Docker) {
        sandbox::cleanup_orphan_containers().await;
    }
    tracing::info!(
        node_id = %cred.node_id,
        server = %cfg.server,
        adapters = ?cfg.adapters,
        "node daemon starting"
    );
    polling::run_transport(cfg, cred).await
}

#[cfg(test)]
mod tests {
    use super::scrub_token_from_file;
    use agentgrid_common::IncomingEvent;
    use config::AdapterProtocol;
    use event_sink::{read_stream, split_batch, EventSink};
    use skills::render_trusted_skills_block;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Mutex;
    use validation::run_validation;

    /// Hardening P0 (safe node install): after a successful enroll the one-time
    /// `AGENTGRID_ENROLL_TOKEN` line must be removed from the env file so it
    /// can't be reused/leaked off disk; other vars are preserved; the file is
    /// rewritten atomically at 0600.
    #[tokio::test]
    async fn scrub_removes_only_enroll_token_line() {
        let dir = std::env::temp_dir().join(format!(
            "ag-scrub-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let path = dir.join("enroll.env");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            "AGENTGRID_SERVER='http://cp:7800'\nAGENTGRID_ENROLL_TOKEN='secret-tok'\nAGENTGRID_NODE_NAME='n1'\n",
        )
        .unwrap();

        scrub_token_from_file(&path).await;

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            !after.contains("ENROLL_TOKEN"),
            "token must be scrubbed: {after}"
        );
        assert!(
            after.contains("AGENTGRID_SERVER='http://cp:7800'"),
            "other vars kept: {after}"
        );
        assert!(
            after.contains("AGENTGRID_NODE_NAME='n1'"),
            "other vars kept: {after}"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "file must be 0600 after scrub");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn scrub_missing_file_is_noop() {
        let p = std::env::temp_dir().join("ag-scrub-missing-noop");
        let _ = std::fs::remove_file(&p);
        scrub_token_from_file(&p).await; // must not panic
    }
    use super::*;

    /// Stage 9.1: a Bash `cat` at default L2 is auto-allowed; `rm -rf` is
    /// auto-denied; `git push` yields `Ask` (None) → falls to approval flow.
    #[test]
    fn policy_decision_short_circuits_bash() {
        let allow = policy_decision(
            &json!({ "tool": "Bash", "input": "cat README.md" }),
            AutonomyLevel::L2,
        )
        .unwrap();
        assert_eq!(allow.0, PolicyDecision::Allow);

        let deny = policy_decision(
            &json!({ "tool": "Bash", "input": "rm -rf /tmp/x" }),
            AutonomyLevel::L2,
        )
        .unwrap();
        assert_eq!(deny.0, PolicyDecision::Deny);

        assert_eq!(
            policy_decision(
                &json!({ "tool": "Bash", "input": "git push" }),
                AutonomyLevel::L2,
            ),
            None,
            "Ask (git push @ L2) must fall through to the approval flow"
        );
    }

    #[test]
    fn policy_decision_non_bash_is_none() {
        // Non-Bash tools are never short-circuited locally → operator decides.
        assert_eq!(
            policy_decision(
                &json!({ "tool": "WebFetch", "input": "x" }),
                AutonomyLevel::L4
            ),
            None
        );
        assert_eq!(
            policy_decision(&json!({ "tool": "Bash" }), AutonomyLevel::L4),
            None,
            "missing input → no short-circuit"
        );
    }

    /// Stage 9.2: only trusted `(name, source)` skills are listed, sorted by
    /// name; untrusted/absent entries are omitted; an empty trusted set yields
    /// an empty block (fail-closed).
    #[test]
    fn render_trusted_skills_block_filters_and_sorts() {
        use agentgrid_skills::{DiscoveredSkill, Skill, SkillSource};
        use std::collections::HashMap;
        let mk = |name: &str, src: SkillSource, desc: &str| DiscoveredSkill {
            skill: Skill {
                name: name.into(),
                description: desc.into(),
                license: None,
                compatibility: None,
                allowed_tools: vec![],
                metadata: HashMap::new(),
                body: String::new(),
            },
            source: src,
            path: std::path::PathBuf::from(format!("/x/{name}/SKILL.md")),
        };
        let discovered = vec![
            mk("zebra", SkillSource::User, "last"),
            mk("alpha", SkillSource::Project, "first multi\nline desc"),
            mk("untrusted-one", SkillSource::Project, "x"),
        ];
        let mut trusted = std::collections::HashSet::new();
        trusted.insert(("alpha".to_string(), "project".to_string()));
        trusted.insert(("zebra".to_string(), "user".to_string()));
        let out = render_trusted_skills_block(&discovered, &trusted);
        assert!(out.contains("Available agent skills (operator-trusted)"));
        assert!(
            out.contains("- alpha (project): first"),
            "alpha trusted + rendered with first line of description"
        );
        assert!(out.contains("- zebra (user): last"));
        assert!(
            !out.contains("untrusted-one"),
            "untrusted skill must be omitted (fail-closed)"
        );
        assert!(out.find("alpha").unwrap() < out.find("zebra").unwrap());
        assert_eq!(
            render_trusted_skills_block(&discovered, &std::collections::HashSet::new()),
            ""
        );
    }

    /// A temporary EventOutbox for a given attempt, isolated per test run.
    fn test_outbox(attempt_id: &str) -> Arc<outbox::EventOutbox> {
        let dir = std::env::temp_dir().join(format!("ag-outbox-test-{}", uuid::Uuid::new_v4()));
        Arc::new(outbox::EventOutbox::open(&dir, attempt_id).unwrap())
    }

    #[tokio::test]
    async fn event_sink_drops_logs_over_cap_but_keeps_terminal_state() {
        // Stage 2.1: backpressure. A chatty agent's stdout/stderr are dropped
        // once the RAM buffer exceeds the cap; status/result/error are never
        // dropped, and exactly one `output_truncated` notice is emitted.
        std::env::set_var("AGENTGRID_EVENT_BUF_BYTES", "64");
        let sink = EventSink::new(
            "a1".into(),
            reqwest::Client::new(),
            "http://x".into(),
            String::new(),
            test_outbox("a1"),
        );
        // Each line ~ 100 bytes; 64-byte cap overflows after the first.
        for _ in 0..50 {
            sink.push(EventType::Stdout, serde_json::json!({ "text": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" }))
                .await;
        }
        // A terminal-state event must still be accepted despite the overflow.
        sink.push(EventType::Result, serde_json::json!({ "ok": true }))
            .await;
        let buf = sink.buffered_events().await;
        let has_result = buf.iter().any(|e| e.r#type == EventType::Result);
        let truncation_notices = buf
            .iter()
            .filter(|e| e.payload.get("event").and_then(|v| v.as_str()) == Some("output_truncated"))
            .count();
        assert!(has_result, "terminal-state event must survive truncation");
        assert_eq!(truncation_notices, 1, "exactly one output_truncated notice");
        std::env::remove_var("AGENTGRID_EVENT_BUF_BYTES");
    }

    #[tokio::test]
    async fn validation_command_reports_exit_and_log() {
        let dir = std::env::temp_dir().join(format!("ag-val-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sink = EventSink::new(
            "a1".into(),
            reqwest::Client::new(),
            "http://x".into(),
            String::new(),
            test_outbox("a1"),
        );
        let out = run_validation(
            &dir,
            "echo hi; exit 2",
            std::time::Duration::from_secs(30),
            "http://x/v1/node/attempts/a1/cancel".into(),
            reqwest::Client::new(),
            "http://x",
            "a1",
            "",
            &sink,
            &[],
        )
        .await
        .unwrap();
        assert_eq!(out.code, 2);
        assert!(!out.timed_out && !out.cancelled);
        let log = std::fs::read_to_string(dir.join("validation.log")).unwrap();
        assert!(log.contains("hi"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn validation_command_masks_secrets_in_output_and_log() {
        // Audit 22.1.1: a secret that appears in validation stdout must be
        // masked in BOTH the streamed events and the validation.log artifact.
        let dir = std::env::temp_dir().join(format!("ag-valmsk-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let sink = EventSink::new(
            "a2".into(),
            reqwest::Client::new(),
            "http://x".into(),
            String::new(),
            test_outbox("a2"),
        );
        let secrets = vec!["sk-LEAK-12345".to_string()];
        let cmd = "printf 'token=sk-LEAK-12345 line\n'; exit 0";
        let out = run_validation(
            &dir,
            cmd,
            std::time::Duration::from_secs(30),
            "http://x/v1/node/attempts/a2/cancel".into(),
            reqwest::Client::new(),
            "http://x",
            "a2",
            "",
            &sink,
            &secrets,
        )
        .await
        .unwrap();
        assert_eq!(out.code, 0);
        let log = std::fs::read_to_string(dir.join("validation.log")).unwrap();
        assert!(
            !log.contains("sk-LEAK-12345"),
            "secret leaked into validation.log: {log}"
        );
        assert!(log.contains("***"), "masked marker missing: {log}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Hardening P0 item 12 / plan tests: a validation timeout must tear down
    /// the WHOLE process tree — a forked child that ignores the parent exit is
    /// killed with the process group, not orphaned.
    #[tokio::test]
    async fn validation_timeout_kills_forked_child_tree() {
        let dir = std::env::temp_dir().join(format!("ag-valto-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // The sleepers run through a uniquely-named wrapper script so the
        // leak check can pgrep for the test's own uuid instead of matching
        // every unrelated `sleep` on the machine.
        let marker = uuid::Uuid::new_v4().to_string();
        let sleeper = dir.join(format!("sleeper-{marker}.sh"));
        std::fs::write(&sleeper, "#!/bin/sh\nsleep 60\n").unwrap();
        std::fs::set_permissions(
            &sleeper,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        // Shell that starts a background sleeper and then sleeps itself: the
        // sleeper is in the same process group, so terminate_group(pid) must
        // reap it too.
        let run_cmd = format!("{s} & {s}", s = sleeper.display());
        let sink = EventSink::new(
            "a-valto".into(),
            reqwest::Client::new(),
            "http://x".into(),
            String::new(),
            test_outbox("a-valto"),
        );
        let out = run_validation(
            &dir,
            &run_cmd,
            std::time::Duration::from_millis(300),
            "http://x/v1/node/attempts/a-valto/cancel".into(),
            reqwest::Client::new(),
            "http://x",
            "a-valto",
            "",
            &sink,
            &[],
        )
        .await
        .unwrap();
        assert!(out.timed_out, "short timeout must fire: {out:?}");
        // Give terminate_group's SIGKILL escalation a moment, then assert no
        // sleeper from THIS test survives (matched by the unique marker).
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        let leaked = run_pgrep(&marker);
        assert_eq!(
            leaked, 0,
            "validation timeout must reap the whole tree; {leaked} sleeper(s) leaked"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Count live `sleep` processes (test helper — the only processes started
    /// by the validation test are sleepers).
    fn run_pgrep(name: &str) -> usize {
        let out = std::process::Command::new("pgrep")
            .arg("-f")
            .arg(name)
            .output();
        match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).lines().count(),
            _ => 0,
        }
    }

    #[tokio::test]
    async fn read_stream_mirrors_raw_output() {
        let dir = std::env::temp_dir().join(format!("ag-raw-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let raw_path = std::path::Path::new(&dir).join("raw.log");
        let f = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&raw_path)
            .await
            .unwrap();
        let raw = Arc::new(Mutex::new(f));
        let input = b"{\"type\":\"log\",\"payload\":{\"text\":\"hello\"}}\nnot json\n".to_vec();
        let reader = tokio::io::BufReader::new(std::io::Cursor::new(input));
        let sink = EventSink::new(
            "a1".into(),
            reqwest::Client::new(),
            "http://x".into(),
            String::new(),
            test_outbox("a1"),
        );
        read_stream(
            reader,
            sink,
            "stdout",
            vec![],
            Some(raw.clone()),
            Arc::new(crate::command_guard::CommandGuard::new(vec![], vec![])),
        )
        .await;
        let got = tokio::fs::read_to_string(&raw_path).await.unwrap();
        assert!(got.contains("hello"), "structured line mirrored: {got}");
        assert!(got.contains("not json"), "unparsed line mirrored: {got}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn read_stream_preserves_trailing_partial_line_on_eof() {
        // Stage 519: an adapter process killed mid-line (no trailing newline)
        // must not silently drop its final partial output. The partial tail is
        // flushed as a final raw line so the crashed adapter's last half-event
        // is preserved (best-effort) instead of lost.
        let dir = std::env::temp_dir().join(format!("ag-part-{}", uuid::Uuid::new_v4()));
        let _ = std::fs::create_dir_all(&dir);
        let raw_path = std::path::Path::new(&dir).join("raw.log");
        let f = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&raw_path)
            .await
            .unwrap();
        let raw = Arc::new(Mutex::new(f));
        // Complete line + a partial line with NO trailing newline.
        let input =
            b"{\"type\":\"log\",\"payload\":{\"text\":\"line1\"}}\ncrashed mid-bytes".to_vec();
        let reader = tokio::io::BufReader::new(std::io::Cursor::new(input));
        let sink = EventSink::new(
            "a1".into(),
            reqwest::Client::new(),
            "http://x".into(),
            String::new(),
            test_outbox("a1"),
        );
        read_stream(
            reader,
            sink,
            "stdout",
            vec![],
            Some(raw.clone()),
            Arc::new(crate::command_guard::CommandGuard::new(vec![], vec![])),
        )
        .await;
        let got = tokio::fs::read_to_string(&raw_path).await.unwrap();
        assert!(got.contains("line1"), "complete line mirrored: {got}");
        assert!(
            got.contains("crashed mid-bytes"),
            "partial tail (no trailing newline) must be preserved, got: {got}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Accept anything on a port and answer 200 OK, so the daemon's event sink
    /// flushes without retry/backoff noise during the test.
    async fn dummy_ingest_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let _ = s
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                        .await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// A dummy CP that routes `/cancel` requests to a fixed `CancelState`
    /// (always cancel-requested) and everything else to empty 200 OK. Used to
    /// exercise `drive_acp_session`'s cancel race without a real control plane.
    async fn dummy_cancel_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 512];
                    let _ = s.read(&mut buf).await;
                    let req = String::from_utf8_lossy(&buf);
                    if req.contains("/cancel") {
                        let body = r#"{"cancel_requested":true}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n\r\n{body}",
                            len = body.len(),
                            body = body
                        );
                        let _ = s.write_all(resp.as_bytes()).await;
                    } else {
                        let _ = s
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                            .await;
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn drive_acp_session_runs_fake_agent_and_streams_events() {
        // Make the test-only ACP agent discoverable on PATH. It is built into
        // the same target dir; locate it relative to CARGO_MANIFEST_DIR.
        let manifest = env!("CARGO_MANIFEST_DIR");
        let fake = [
            "../../target/debug/adapter-fake-acp",
            "../../target/release/adapter-fake-acp",
        ]
        .iter()
        .map(|p| std::path::Path::new(manifest).join(p))
        .find(|p| p.is_file())
        .expect("fake ACP agent built");
        let bin_dir = fake.parent().unwrap();
        let orig = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{orig}", bin_dir.display()));

        let server = dummy_ingest_server().await;
        let cfg = Config {
            server: server.clone(),
            node_name: "test".into(),
            workspace_root: std::env::temp_dir().join("ag-acp-ws"),
            max_concurrency: 2,
            agent_version: "0.1.0".into(),
            adapters: vec![AdapterSpec {
                id: "fake-acp".into(),
                protocol: AdapterProtocol::Acp,
            }],
            repositories: vec!["*".into()],
            heartbeat_secs: 10,
            enroll_token: None,
            credential_path: std::env::temp_dir().join("ag-acp-cred.json"),
            env_file: None,
            repository_root: std::env::temp_dir().join("ag-acp-repos"),
            secrets: vec![],
            sandbox: sandbox::SandboxKind::None,
            adapter_env: vec![],
            outbox_root: std::env::temp_dir()
                .join(format!("ag-acp-outbox-{}", uuid::Uuid::new_v4())),
            artifact_spool_root: std::env::temp_dir()
                .join(format!("ag-acp-spool-{}", uuid::Uuid::new_v4())),
            completion_outbox: Arc::new(
                outbox::CompletionOutbox::open(
                    &std::env::temp_dir().join(format!("ag-acp-comp-{}", uuid::Uuid::new_v4())),
                )
                .unwrap(),
            ),
            autonomy: AutonomyLevel::default(),
            adapter_versions: Default::default(),
            max_artifact_size: 100 * 1024 * 1024,
            network_mode: "none".into(),
            transport: crate::config::Transport::Auto,
            guard_deny: vec![],
            guard_allow: vec![],
            accounts: vec![],
        };
        let ws = std::env::temp_dir().join(format!(
            "ag-acp-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&ws).unwrap();
        let assignment = Assignment {
            attempt_id: format!("att-{}", uuid::Uuid::new_v4()),
            fencing_token: String::new(),
            task_id: "t1".into(),
            repository: "*".into(),
            prompt: "do the thing".into(),
            adapter: "fake-acp".into(),
            number: 1,
            timeout_secs: 30,
            git_url: String::new(),
            default_branch: String::new(),
            validation_command: None,
            validation_timeout_secs: None,
            base_commit: None,
            parent_acp_session_id: None,
            network_mode: None,
            provenance: None,
            upstream_commits: vec![],
            upstream_task_ids: vec![],
            group_id: None,
            read_only: false,
            eval_cases: vec![],
            consensus_group_id: None,
            consensus_member: None,
        };
        let sink = EventSink::new(
            assignment.attempt_id.clone(),
            reqwest::Client::new(),
            cfg.server.clone(),
            String::new(),
            Arc::new(outbox::EventOutbox::open(&cfg.outbox_root, &assignment.attempt_id).unwrap()),
        );
        let res = drive_acp_session(
            &cfg,
            &reqwest::Client::new(),
            &assignment,
            &ws,
            sink.clone(),
        )
        .await
        .unwrap();
        assert!(res.success, "ACP session should succeed");
        assert_eq!(res.error_code, None);
        assert_eq!(
            res.session_id.as_deref(),
            Some("sess-fake-1"),
            "session_id from session/new is reported back (Stage 11.5)"
        );
        assert!(
            sink.adapter_event_count() >= 2,
            "two session/update events should stream; got {}",
            sink.adapter_event_count()
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    /// Plan 1.8 (#15): a provider 429 surfaces as an `error`-type session/
    /// update event carrying a rate-limit marker; `drive_acp_session` flags it
    /// as `rate_limited`. With a 2-token account pool, `rotate_account_token`
    /// advances to the second token and the second drive of the SAME fake agent
    /// (test's marker file deleted) completes cleanly — primary 429 -> completed
    /// via second account.
    #[tokio::test]
    async fn drive_acp_session_flags_rate_limit_then_rotates() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let fake = [
            "../../target/debug/adapter-fake-acp",
            "../../target/release/adapter-fake-acp",
        ]
        .iter()
        .map(|p| std::path::Path::new(manifest).join(p))
        .find(|p| p.is_file())
        .expect("fake ACP agent built");
        let bin_dir = fake.parent().unwrap();
        let orig = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{orig}", bin_dir.display()));

        // Marker file: only the FIRST drive emits the 429.
        let rl_marker = std::env::temp_dir().join(format!("ag-rl-marker-{}", uuid::Uuid::new_v4()));
        std::fs::write(&rl_marker, b"1").unwrap();
        std::env::set_var("AG_FAKE_RATE_LIMIT", &rl_marker);

        let server = dummy_ingest_server().await;
        let rl_token_env = format!("AG_FAKE_TOKEN_{}", uuid::Uuid::new_v4());
        let mut cfg = Config {
            server: server.clone(),
            node_name: "test".into(),
            workspace_root: std::env::temp_dir().join("ag-acp-ws-rl"),
            max_concurrency: 2,
            agent_version: "0.1.0".into(),
            adapters: vec![AdapterSpec {
                id: "fake-acp".into(),
                protocol: AdapterProtocol::Acp,
            }],
            repositories: vec!["*".into()],
            heartbeat_secs: 10,
            enroll_token: None,
            credential_path: std::env::temp_dir().join("ag-acp-cred-rl.json"),
            env_file: None,
            repository_root: std::env::temp_dir().join("ag-acp-repos-rl"),
            secrets: vec![],
            sandbox: sandbox::SandboxKind::None,
            // Two-token pool backing a credential env var the fake agent ignores —
            // rotation only swaps the value, which is enough to exercise the path.
            accounts: vec![crate::config::AccountConfig {
                env: rl_token_env.clone(),
                tokens: vec!["tok-primary".into(), "tok-secondary".into()],
            }],
            adapter_env: vec![
                (rl_token_env.clone(), "tok-primary".into()),
                // env_clear strips the daemon env; the marker must ride in
                // adapter_env like AG_FAKE_HANG does in the other tests.
                ("AG_FAKE_RATE_LIMIT".into(), rl_marker.display().to_string()),
            ],
            outbox_root: std::env::temp_dir()
                .join(format!("ag-acp-outbox-{}", uuid::Uuid::new_v4())),
            artifact_spool_root: std::env::temp_dir()
                .join(format!("ag-acp-spool-{}", uuid::Uuid::new_v4())),
            completion_outbox: Arc::new(
                outbox::CompletionOutbox::open(
                    &std::env::temp_dir().join(format!("ag-acp-comp-{}", uuid::Uuid::new_v4())),
                )
                .unwrap(),
            ),
            autonomy: AutonomyLevel::default(),
            adapter_versions: Default::default(),
            max_artifact_size: 100 * 1024 * 1024,
            network_mode: "none".into(),
            transport: crate::config::Transport::Auto,
            guard_deny: vec![],
            guard_allow: vec![],
        };
        let ws = std::env::temp_dir().join(format!(
            "ag-acp-rl-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&ws).unwrap();
        let assignment = Assignment {
            attempt_id: format!("att-{}", uuid::Uuid::new_v4()),
            fencing_token: String::new(),
            task_id: "t1".into(),
            repository: "*".into(),
            prompt: "do the thing".into(),
            adapter: "fake-acp".into(),
            number: 1,
            timeout_secs: 30,
            git_url: String::new(),
            default_branch: String::new(),
            validation_command: None,
            validation_timeout_secs: None,
            base_commit: None,
            parent_acp_session_id: None,
            network_mode: None,
            provenance: None,
            upstream_commits: vec![],
            upstream_task_ids: vec![],
            group_id: None,
            read_only: false,
            eval_cases: vec![],
            consensus_group_id: None,
            consensus_member: None,
        };
        let sink = EventSink::new(
            assignment.attempt_id.clone(),
            reqwest::Client::new(),
            cfg.server.clone(),
            String::new(),
            Arc::new(outbox::EventOutbox::open(&cfg.outbox_root, &assignment.attempt_id).unwrap()),
        );

        // First drive: the marker file is present, so the fake agent emits a 429
        // error event, then returns success — rate_limited must be true.
        let res1 = drive_acp_session(
            &cfg,
            &reqwest::Client::new(),
            &assignment,
            &ws,
            sink.clone(),
        )
        .await
        .unwrap();
        assert!(
            res1.rate_limited,
            "first drive should report rate_limited after the 429 event"
        );

        // Rotate the account token (one re-usable helper in attempt_runner).
        let mut idx = 0usize;
        assert!(
            crate::attempt_runner::rotate_account_token(&mut cfg, &mut idx),
            "rotation should succeed (second token available)"
        );
        assert_eq!(
            cfg.adapter_env
                .iter()
                .find(|(k, _)| k == &rl_token_env)
                .map(|(_, v)| v.clone()),
            Some("tok-secondary".into()),
            "adapter_env should now carry the second token"
        );

        // Second drive: marker file deleted by the fake — no 429, clean success.
        let res2 = drive_acp_session(
            &cfg,
            &reqwest::Client::new(),
            &assignment,
            &ws,
            sink.clone(),
        )
        .await
        .unwrap();
        assert!(
            !res2.rate_limited,
            "second drive should not report rate_limited"
        );
        assert!(res2.success, "second drive should succeed");

        std::env::remove_var("AG_FAKE_RATE_LIMIT");
        std::fs::remove_dir_all(&ws).ok();
    }

    /// Stage 5: an ACP subprocess that hangs mid-frame (writes a truncated
    /// JSON line then blocks forever) must be torn down by the session
    /// timeout — the attempt fails with `timeout`, no hang.
    #[tokio::test]
    async fn drive_acp_session_hang_mid_frame_times_out() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let fake = [
            "../../target/debug/adapter-fake-acp",
            "../../target/release/adapter-fake-acp",
        ]
        .iter()
        .map(|p| std::path::Path::new(manifest).join(p))
        .find(|p| p.is_file())
        .expect("fake ACP agent built");
        let bin_dir = fake.parent().unwrap();
        let orig = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{orig}", bin_dir.display()));
        // Fake agent: write a truncated JSON-RPC line then block forever.
        std::env::set_var("PATH", format!("{}:{orig}", bin_dir.display()));
        let server = dummy_ingest_server().await;
        // Fake agent: write a truncated JSON-RPC line then block forever.
        // Pass via adapter_env so it only reaches THIS child (no env cross-talk
        // with parallel ACP tests in the same process).
        let cfg = Config {
            server: server.clone(),
            node_name: "test".into(),
            workspace_root: std::env::temp_dir().join("ag-acp-ws-hang"),
            max_concurrency: 2,
            agent_version: "0.1.0".into(),
            adapters: vec![AdapterSpec {
                id: "fake-acp".into(),
                protocol: AdapterProtocol::Acp,
            }],
            repositories: vec!["*".into()],
            heartbeat_secs: 10,
            enroll_token: None,
            credential_path: std::env::temp_dir().join("ag-acp-cred-hang.json"),
            env_file: None,
            repository_root: std::env::temp_dir().join("ag-acp-repos-hang"),
            secrets: vec![],
            sandbox: sandbox::SandboxKind::None,
            adapter_env: vec![("AG_FAKE_HANG".into(), "1".into())],
            outbox_root: std::env::temp_dir()
                .join(format!("ag-acp-outbox-hang-{}", uuid::Uuid::new_v4())),
            artifact_spool_root: std::env::temp_dir()
                .join(format!("ag-acp-spool-hang-{}", uuid::Uuid::new_v4())),
            completion_outbox: Arc::new(
                outbox::CompletionOutbox::open(
                    &std::env::temp_dir()
                        .join(format!("ag-acp-comp-hang-{}", uuid::Uuid::new_v4())),
                )
                .unwrap(),
            ),
            autonomy: AutonomyLevel::default(),
            adapter_versions: Default::default(),
            max_artifact_size: 100 * 1024 * 1024,
            network_mode: "none".into(),
            transport: crate::config::Transport::Auto,
            guard_deny: vec![],
            guard_allow: vec![],
            accounts: vec![],
        };
        let ws = std::env::temp_dir().join(format!(
            "ag-acp-hang-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&ws).unwrap();
        let assignment = Assignment {
            attempt_id: format!("att-hang-{}", uuid::Uuid::new_v4()),
            fencing_token: String::new(),
            task_id: "t1".into(),
            repository: "*".into(),
            prompt: "do the thing".into(),
            adapter: "fake-acp".into(),
            number: 1,
            timeout_secs: 3,
            git_url: String::new(),
            default_branch: String::new(),
            validation_command: None,
            validation_timeout_secs: None,
            base_commit: None,
            parent_acp_session_id: None,
            network_mode: None,
            provenance: None,
            upstream_commits: vec![],
            upstream_task_ids: vec![],
            group_id: None,
            read_only: false,
            eval_cases: vec![],
            consensus_group_id: None,
            consensus_member: None,
        };
        let sink = EventSink::new(
            assignment.attempt_id.clone(),
            reqwest::Client::new(),
            cfg.server.clone(),
            String::new(),
            Arc::new(outbox::EventOutbox::open(&cfg.outbox_root, &assignment.attempt_id).unwrap()),
        );
        let res = tokio::time::timeout(
            Duration::from_secs(20),
            drive_acp_session(
                &cfg,
                &reqwest::Client::new(),
                &assignment,
                &ws,
                sink.clone(),
            ),
        )
        .await
        .expect("drive_acp_session must not hang on a mid-frame ACP death")
        .unwrap();
        assert!(!res.success, "hung ACP session should not succeed");
        assert_eq!(
            res.error_code.as_deref(),
            Some("timeout"),
            "expected timeout error_code, got {:?}",
            res.error_code
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    /// Stage 5 / line 192: a cancel requested mid prompt turn must interrupt
    /// the ACP `session/prompt`, send `session/cancel`, reap the subprocess,
    /// and resolve the attempt as `cancelled` (not timeout, not success).
    #[tokio::test]
    async fn drive_acp_session_cancel_mid_prompt_turn() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let fake = [
            "../../target/debug/adapter-fake-acp",
            "../../target/release/adapter-fake-acp",
        ]
        .iter()
        .map(|p| std::path::Path::new(manifest).join(p))
        .find(|p| p.is_file())
        .expect("fake ACP agent built");
        let bin_dir = fake.parent().unwrap();
        let orig = std::env::var("PATH").unwrap_or_default();
        std::env::set_var("PATH", format!("{}:{orig}", bin_dir.display()));
        // Dummy CP that reports `cancel_requested=true` on the cancel GET,
        // so `wait_for_cancel` (polls every 1s) races against the hung prompt.
        let server = dummy_cancel_server().await;
        let cfg = Config {
            server: server.clone(),
            node_name: "test".into(),
            workspace_root: std::env::temp_dir().join("ag-acp-ws-cancel"),
            max_concurrency: 2,
            agent_version: "0.1.0".into(),
            adapters: vec![AdapterSpec {
                id: "fake-acp".into(),
                protocol: AdapterProtocol::Acp,
            }],
            repositories: vec!["*".into()],
            heartbeat_secs: 10,
            enroll_token: None,
            credential_path: std::env::temp_dir().join("ag-acp-cred-cancel.json"),
            env_file: None,
            repository_root: std::env::temp_dir().join("ag-acp-repos-cancel"),
            secrets: vec![],
            sandbox: sandbox::SandboxKind::None,
            adapter_env: vec![("AG_FAKE_HANG".into(), "1".into())],
            outbox_root: std::env::temp_dir()
                .join(format!("ag-acp-outbox-cancel-{}", uuid::Uuid::new_v4())),
            artifact_spool_root: std::env::temp_dir()
                .join(format!("ag-acp-spool-cancel-{}", uuid::Uuid::new_v4())),
            completion_outbox: Arc::new(
                outbox::CompletionOutbox::open(
                    &std::env::temp_dir()
                        .join(format!("ag-acp-comp-cancel-{}", uuid::Uuid::new_v4())),
                )
                .unwrap(),
            ),
            autonomy: AutonomyLevel::default(),
            adapter_versions: Default::default(),
            max_artifact_size: 100 * 1024 * 1024,
            network_mode: "none".into(),
            transport: crate::config::Transport::Auto,
            guard_deny: vec![],
            guard_allow: vec![],
            accounts: vec![],
        };
        let ws = std::env::temp_dir().join(format!(
            "ag-acp-cancel-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&ws).unwrap();
        let assignment = Assignment {
            attempt_id: format!("att-cancel-{}", uuid::Uuid::new_v4()),
            task_id: "t1".into(),
            fencing_token: String::new(),
            repository: "*".into(),
            prompt: "do the thing".into(),
            adapter: "fake-acp".into(),
            number: 1,
            // Long timeout so the cancel race wins, not the timeout.
            timeout_secs: 30,
            git_url: String::new(),
            default_branch: String::new(),
            validation_command: None,
            validation_timeout_secs: None,
            base_commit: None,
            parent_acp_session_id: None,
            network_mode: None,
            provenance: None,
            upstream_commits: vec![],
            upstream_task_ids: vec![],
            group_id: None,
            read_only: false,
            eval_cases: vec![],
            consensus_group_id: None,
            consensus_member: None,
        };
        let sink = EventSink::new(
            assignment.attempt_id.clone(),
            reqwest::Client::new(),
            cfg.server.clone(),
            String::new(),
            Arc::new(outbox::EventOutbox::open(&cfg.outbox_root, &assignment.attempt_id).unwrap()),
        );
        let res = tokio::time::timeout(
            Duration::from_secs(20),
            drive_acp_session(
                &cfg,
                &reqwest::Client::new(),
                &assignment,
                &ws,
                sink.clone(),
            ),
        )
        .await
        .expect("drive_acp_session must not hang on cancel")
        .unwrap();
        assert!(!res.success, "cancelled ACP session should not succeed");
        assert_eq!(
            res.error_code.as_deref(),
            Some("cancelled"),
            "expected cancelled error_code, got {:?}",
            res.error_code
        );
        assert_eq!(
            res.session_id.as_deref(),
            Some("sess-fake-1"),
            "session_id still reported on cancel"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    /// A dummy CP that serves a fixed JSON body for `GET /v1/skills` and 200
    /// OK (empty) for everything else. Used to exercise the
    /// `compose_skills_block` → `session/prompt` wiring inside
    /// `drive_acp_session` without a real control plane.
    async fn dummy_skills_server(skills_body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = s.read(&mut buf).await;
                    let req = String::from_utf8_lossy(&buf);
                    if req.starts_with("GET /v1/skills") {
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                            skills_body.len(),
                            skills_body
                        );
                        let _ = s.write_all(resp.as_bytes()).await;
                    } else {
                        let _ = s
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                            .await;
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    /// Stage 4 integration test (plan Этап 4): a mock ACP agent with an
    /// operator-trusted project skill discovers it and receives the skill
    /// catalogue block in its `session/prompt` prompt; an untrusted skill is
    /// omitted (fail-closed). Both cases run sequentially in one test because
    /// they mutate the process-global `HOME` / `PATH` env (cargo runs
    /// `#[tokio::test]`s in parallel, which would race on those keys).
    #[tokio::test]
    async fn drive_acp_session_injects_trusted_skills_block_into_prompt() {
        async fn one_case(trusted: bool, expect_block: bool) {
            let tmp = std::env::temp_dir().join(format!("ag-skill-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&tmp).unwrap();
            std::env::set_var("HOME", &tmp);

            let manifest = env!("CARGO_MANIFEST_DIR");
            let fake = [
                "../../target/debug/adapter-fake-acp",
                "../../target/release/adapter-fake-acp",
            ]
            .iter()
            .map(|p| std::path::Path::new(manifest).join(p))
            .find(|p| p.is_file())
            .expect("fake ACP agent built");
            let bin_dir = fake.parent().unwrap();
            let orig = std::env::var("PATH").unwrap_or_default();
            std::env::set_var("PATH", format!("{}:{orig}", bin_dir.display()));
            let record = tmp.join("prompt.txt");
            std::env::set_var("AG_FAKE_RECORD_PROMPT", &record);

            let body = format!(r#"[{{"name":"git-help","source":"project","trusted":{trusted}}}]"#);
            let body_static: &'static str = Box::leak(body.into_boxed_str());
            let server = dummy_skills_server(body_static).await;

            let cfg = Config {
                server: server.clone(),
                node_name: "test".into(),
                workspace_root: std::env::temp_dir().join("ag-skill-ws"),
                max_concurrency: 2,
                agent_version: "0.1.0".into(),
                adapters: vec![AdapterSpec {
                    id: "fake-acp".into(),
                    protocol: AdapterProtocol::Acp,
                }],
                repositories: vec!["*".into()],
                heartbeat_secs: 10,
                enroll_token: None,
                credential_path: std::env::temp_dir().join("ag-skill-cred.json"),
                env_file: None,
                repository_root: std::env::temp_dir().join("ag-skill-repos"),
                secrets: vec![],
                sandbox: sandbox::SandboxKind::None,
                // Hardening P1 item 27: the child no longer inherits the
                // daemon env — forward the fake agent's record path via the
                // explicit allowlist instead of set_var in the parent.
                adapter_env: vec![(
                    "AG_FAKE_RECORD_PROMPT".to_string(),
                    record.to_string_lossy().to_string(),
                )],
                outbox_root: std::env::temp_dir()
                    .join(format!("ag-skill-outbox-{}", uuid::Uuid::new_v4())),
                artifact_spool_root: std::env::temp_dir()
                    .join(format!("ag-skill-spool-{}", uuid::Uuid::new_v4())),
                completion_outbox: Arc::new(
                    outbox::CompletionOutbox::open(
                        &std::env::temp_dir()
                            .join(format!("ag-skill-comp-{}", uuid::Uuid::new_v4())),
                    )
                    .unwrap(),
                ),
                autonomy: AutonomyLevel::default(),
                adapter_versions: Default::default(),
                max_artifact_size: 100 * 1024 * 1024,
                network_mode: "none".into(),
                transport: crate::config::Transport::Auto,
                guard_deny: vec![],
                guard_allow: vec![],
                accounts: vec![],
            };

            let ws = tmp.join("ws");
            let skill_dir = ws.join(".agents").join("skills").join("git-help");
            std::fs::create_dir_all(&skill_dir).unwrap();
            std::fs::write(
                skill_dir.join("SKILL.md"),
                "---\nname: git-help\ndescription: Helps with git tasks\n---\n## git-help body\n",
            )
            .unwrap();

            let assignment = Assignment {
                attempt_id: format!("att-{}", uuid::Uuid::new_v4()),
                fencing_token: String::new(),
                task_id: "t1".into(),
                repository: "*".into(),
                prompt: "do the thing".into(),
                adapter: "fake-acp".into(),
                number: 1,
                timeout_secs: 30,
                git_url: String::new(),
                default_branch: String::new(),
                validation_command: None,
                validation_timeout_secs: None,
                base_commit: None,
                parent_acp_session_id: None,
                network_mode: None,
                provenance: None,
                upstream_commits: vec![],
                upstream_task_ids: vec![],
                group_id: None,
                read_only: false,
                eval_cases: vec![],
                consensus_group_id: None,
                consensus_member: None,
            };
            let sink = EventSink::new(
                assignment.attempt_id.clone(),
                reqwest::Client::new(),
                cfg.server.clone(),
                String::new(),
                Arc::new(
                    outbox::EventOutbox::open(&cfg.outbox_root, &assignment.attempt_id).unwrap(),
                ),
            );
            let res = drive_acp_session(
                &cfg,
                &reqwest::Client::new(),
                &assignment,
                &ws,
                sink.clone(),
            )
            .await
            .unwrap();
            assert!(res.success, "ACP session should succeed");

            let recorded = std::fs::read_to_string(&record)
                .expect("fake-acp should have recorded the received prompt");
            if expect_block {
                assert!(
                    recorded.contains("Available agent skills (operator-trusted)"),
                    "trusted-skills block must be injected into prompt; got: {recorded}"
                );
                assert!(
                    recorded.contains("git-help"),
                    "discovered trusted skill name must appear; got: {recorded}"
                );
            } else {
                assert!(
                    !recorded.contains("Available agent skills"),
                    "untrusted skill must be omitted (fail-closed); got: {recorded}"
                );
            }

            std::env::remove_var("AG_FAKE_RECORD_PROMPT");
            std::fs::remove_dir_all(&tmp).ok();
        }

        // trusted → block injected.
        one_case(true, true).await;
        // untrusted → block omitted (fail-closed).
        one_case(false, false).await;
        std::env::remove_var("HOME");
    }

    /// at the line cap instead of growing `acc` without bound; the pipe keeps draining.
    #[tokio::test]
    async fn read_stream_caps_oversized_line() {
        std::env::set_var("AGENTGRID_MAX_LINE_BYTES", "16");
        let sink = EventSink::new(
            "a-cap".into(),
            reqwest::Client::new(),
            "http://x".into(),
            String::new(),
            test_outbox("a-cap"),
        );
        // 100 bytes, no newline — must be split into multiple flushed lines.
        let input: Vec<u8> = vec![b'x'; 100];
        read_stream(
            &input[..],
            sink.clone(),
            "stdout",
            vec![],
            None,
            Arc::new(crate::command_guard::CommandGuard::new(vec![], vec![])),
        )
        .await;
        std::env::remove_var("AGENTGRID_MAX_LINE_BYTES");
        let buf = sink.buffered_events().await;
        // At least a few stdout lines were emitted despite no newline.
        let n = buf.iter().filter(|e| e.r#type == EventType::Stdout).count();
        assert!(n >= 2, "oversized line split into multiple flushes: {n}");
    }

    /// Hardening P1 item 427: read_stream handles invalid UTF-8 without
    /// stopping. from_utf8_lossy replaces invalid sequences with the
    /// Unicode replacement character, so a binary/garbage stream still
    /// produces events instead of crashing the reader.
    #[tokio::test]
    async fn read_stream_handles_invalid_utf8() {
        let sink = EventSink::new(
            "a-utf8".into(),
            reqwest::Client::new(),
            "http://x".into(),
            String::new(),
            test_outbox("a-utf8"),
        );
        // Valid JSON line + invalid UTF-8 bytes (0xFF 0xFE is invalid UTF-8)
        let input = b"{\"type\":\"log\",\"payload\":{\"text\":\"ok\"}}\n\xff\xfe\n".to_vec();
        let reader = tokio::io::BufReader::new(std::io::Cursor::new(input));
        read_stream(
            reader,
            sink.clone(),
            "stdout",
            vec![],
            None,
            Arc::new(crate::command_guard::CommandGuard::new(vec![], vec![])),
        )
        .await;
        let buf = sink.buffered_events().await;
        // Should have produced at least the valid JSON event (Stdout type).
        let n = buf.iter().filter(|e| e.r#type == EventType::Stdout).count();
        assert!(n >= 1, "at least one valid event produced: {n}");
        // The invalid UTF-8 should not crash - it produces a lossy line.
        // Just verify no panic occurred.
    }

    /// Plan 0.2 item 5.3: secret-leak regulator. A configured secret must
    /// never appear in emitted events or in the raw-output artifact — not as
    /// plain text in raw stdout lines, not inside adapter JSON event
    /// payloads, not in a trailing partial line — anywhere the redacted
    /// stream flows. If this test starts failing, some output path bypasses
    /// the redactor: fix the path, do not weaken the test.
    #[tokio::test]
    async fn read_stream_never_leaks_secrets_regulator() {
        let secret = "sk-live-abc123XYZ".to_string();
        let sink = EventSink::new(
            "a-secrets".into(),
            reqwest::Client::new(),
            "http://x".into(),
            String::new(),
            test_outbox("a-secrets"),
        );
        let raw_path =
            std::env::temp_dir().join(format!("ag-secret-regulator-{}.log", uuid::Uuid::new_v4()));
        let raw = Some(Arc::new(Mutex::new(
            tokio::fs::File::create(&raw_path).await.unwrap(),
        )));
        // Mix of: raw line with the secret, adapter JSON event whose payload
        // embeds the secret, the secret split across a chunk boundary (fed in
        // one buffer here; boundary coverage lives in secret_redactor tests),
        // and a trailing partial line containing the secret (no newline).
        let adapter_line =
            json!({"type": "log", "payload": {"text": format!("key={secret}")}}).to_string();
        let input = format!("token={secret}\n{adapter_line}\nfinal line key={secret}");
        let reader = tokio::io::BufReader::new(std::io::Cursor::new(input.into_bytes()));
        read_stream(
            reader,
            sink.clone(),
            "stdout",
            vec![secret.clone()],
            raw,
            Arc::new(crate::command_guard::CommandGuard::new(vec![], vec![])),
        )
        .await;

        for e in sink.buffered_events().await {
            let s = e.payload.to_string();
            assert!(
                !s.contains(&secret),
                "secret leaked into event payload: {s}"
            );
        }
        let raw_content = tokio::fs::read_to_string(&raw_path).await.unwrap();
        assert!(
            !raw_content.contains(&secret),
            "secret leaked into raw artifact: {raw_content}"
        );
        let _ = tokio::fs::remove_file(&raw_path).await;
    }

    /// Hardening P1 item 34: `split_batch` bounds chunk size to the CP ingest
    /// caps (event count + bytes) so a huge flush is never one oversized POST.
    #[test]
    fn split_batch_respects_count_and_byte_caps() {
        // Keep env out of the default (500/4MiB) — 500-event cap is enough.
        std::env::remove_var("AGENTGRID_MAX_EVENT_BATCH");
        std::env::remove_var("AGENTGRID_MAX_EVENT_BATCH_KB");
        let events: Vec<IncomingEvent> = (0..1200u64)
            .map(|i| IncomingEvent {
                sequence: i,
                r#type: EventType::Stdout,
                payload: json!({ "text": format!("line-{i}") }),
            })
            .collect();
        let chunks = split_batch(events);
        assert!(chunks.len() >= 3, "1200 events must split into >=3 chunks");
        for c in &chunks {
            assert!(
                c.len() <= 500,
                "chunk size {} exceeds the CP event cap",
                c.len()
            );
        }
        // Byte cap: tiny env cap forces many small chunks.
        std::env::set_var("AGENTGRID_MAX_EVENT_BATCH_KB", "1"); // ~1 KiB per chunk
        let events: Vec<IncomingEvent> = (0..100u64)
            .map(|i| IncomingEvent {
                sequence: i,
                r#type: EventType::Stdout,
                payload: json!({ "text": "x".repeat(500) }),
            })
            .collect();
        let chunks = split_batch(events);
        assert!(
            chunks.len() >= 5,
            "byte cap must split payload-heavy events into multiple chunks, got {}",
            chunks.len()
        );
        std::env::remove_var("AGENTGRID_MAX_EVENT_BATCH_KB");
    }
}
