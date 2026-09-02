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
mod config_error;
mod enrollment;
pub mod evals;
mod event_sink;
mod git;
mod github;
mod heartbeat;
mod mcp;
mod opencode_config;
mod outbox;
mod polling;
mod process_supervisor;
mod profiles;
mod proxy;
mod recovery;
mod sandbox;
mod secret_redactor;
pub(crate) mod skills;
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
use skills::{compose_brain_block, compose_context_block, compose_skills_block};

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
        Some(&sandbox::container_name(&assignment.attempt_id)),
    );
    let mut cmd = tokio::process::Command::new(&program);
    cmd.args(&args);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // Audit ND-10: inherit() sent the agent's stderr straight to the
        // daemon log, bypassing the secret redaction every wrapper-path
        // stream goes through. Pipe it and drain through the same masked
        // read_stream below.
        .stderr(std::process::Stdio::piped())
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
    // Audit follow-up: a spawn failure used to propagate via `?` and skip
    // the caller's completion/outbox-drain/cleanup entirely — the attempt
    // sat `running` until the CP reaper, the worktree leaked, and ND-4
    // redelivery later re-ran the whole task. A spawn failure is an
    // infrastructure failure result like the missing-binary arm above.
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                attempt_id = %assignment.attempt_id,
                adapter = %assignment.adapter,
                "ACP agent spawn failed: {e}"
            );
            return Ok(AcpResult {
                success: false,
                error_code: Some("infrastructure_failed".into()),
                session_id: None,
                rate_limited: false,
            });
        }
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stdin = child.stdin.take().expect("piped stdin");
    // Audit ND-10: drain the piped stderr through the same redacting
    // read_stream the wrapper path uses, so agent stderr lands in the event
    // stream masked instead of raw in the daemon log. The task ends when the
    // child exits (or is killed via kill_on_drop) and its pipe closes.
    if let Some(stderr) = child.stderr.take() {
        let sink2 = sink.clone();
        let guard = Arc::new(crate::command_guard::CommandGuard::new(
            cfg.guard_deny.clone(),
            cfg.guard_allow.clone(),
        ));
        tokio::spawn(event_sink::read_stream(
            stderr,
            sink2,
            "stderr",
            cfg.secrets.clone(),
            None,
            guard,
        ));
    }
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
    // Competitor-gap feature (project brain): append the repo's persistent
    // project memory (AGENTS-BRAIN.md) when present in the worktree.
    prompt_text.push_str(&compose_brain_block(ws_path).await);
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
            // Audit ND-6: the kill above stops the `docker run` client, not
            // necessarily the container — remove it by name.
            sandbox::remove_sandbox_container(&assignment.attempt_id).await;
            let _ = tokio::time::timeout(
                Duration::from_secs(12),
                child.wait(),
            )
            .await;
            AcpResult { success: false, error_code: Some("cancelled".into()), session_id: Some(session_id.clone()), rate_limited: false }
        }
        _ = tokio::time::sleep(timeout) => {
            terminate_group(pid);
            sandbox::remove_sandbox_container(&assignment.attempt_id).await;
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
    // Hardening P2 item 30: the release smoke test invokes `--version` on
    // every published binary. The daemon otherwise takes no CLI args, so an
    // unrecognized flag used to be ignored and the daemon booted for real
    // (probing adapters, pruning workspaces, then failing to enroll) —
    // handle it explicitly before any startup side effects.
    for a in std::env::args().skip(1) {
        if a == "--version" || a == "-V" {
            println!("agentgrid-node-daemon {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
    }
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
    crate::config::stash_credential(&cred);
    sandbox::set_node_id(&cred.node_id);

    // Feature "opencode profiles": initial application at startup, so a node
    // that came online during a CP outage converges to the profiled state.
    // Errors are logged, never fatal — the node still needs to run.
    {
        let cfg_init = cfg.clone();
        let cred_init = cred.clone();
        let client_init = polling::daemon_http_client()?;
        tokio::spawn(async move {
            if let Err(e) = crate::opencode_config::pull_and_apply(
                &cfg_init,
                &client_init,
                &cred_init,
                "startup",
                None,
            )
            .await
            {
                tracing::warn!("startup opencode-config sync failed: {e}");
            }
        });
    }
    // Feature "opencode profiles": interval pull — OFF by default
    // (`AGENTGRID_CONFIG_PULL_INTERVAL_SECS=0` or unset). Only for paranoid
    // deploys that want the node to re-converge even when the WS push channel
    // is healthy but the operator distrusts its delivery (e.g. aggressive proxy
    // in front of the CP). Constant: when enabled, ticks every N seconds and
    // applies iff the on-disk hash drifted.
    if let Ok(ivstr) = std::env::var("AGENTGRID_CONFIG_PULL_INTERVAL_SECS") {
        if let Ok(secs) = ivstr.parse::<u64>() {
            if secs >= 30 {
                let cfg_i = cfg.clone();
                // The credential is refreshed per tick via process_credential —
                // enrol-side restores after a CP restart deliver a fresh bearer
                // without restarting the daemon.
                let client_i = polling::daemon_http_client()?;
                tracing::info!(
                    interval_secs = secs,
                    "opencode-config interval pull enabled"
                );
                tokio::spawn(async move {
                    let mut tick = tokio::time::interval(tokio::time::Duration::from_secs(secs));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    loop {
                        tick.tick().await;
                        if let Some(cred) = crate::config::process_credential() {
                            if let Err(e) = crate::opencode_config::pull_and_apply(
                                &cfg_i, &client_i, &cred, "interval", None,
                            )
                            .await
                            {
                                tracing::warn!("interval opencode-config pull failed: {e}");
                            }
                        }
                    }
                });
            } else if secs > 0 {
                tracing::warn!(
                    "AGENTGRID_CONFIG_PULL_INTERVAL_SECS pinned below 30 s — ignored (guard rail)"
                );
            }
        }
    }
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
#[path = "main_tests.rs"]
mod main_tests;
