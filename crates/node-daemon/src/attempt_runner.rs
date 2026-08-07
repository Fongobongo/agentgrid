//! Attempt runner: orchestrates a single task attempt end-to-end — workspace
//! preparation, adapter execution (ACP or wrapper), validation, artifact
//! upload, completion reporting, and cleanup.

use std::sync::Arc;
use std::time::Duration;

use agentgrid_adapters::SpawnRequest;
use agentgrid_common::{Assignment, CompleteAttemptRequest, EventKind};
use anyhow::Result;
use reqwest::Client;
use serde_json::json;
use tokio::sync::Mutex;

use crate::artifact_spool;
use crate::capabilities::resolve_adapter_bin;
use crate::completion::{ack_attempt, create_agent_session, report_complete};
use crate::config::Config;
use crate::event_sink::EventSink;
use crate::git;
use crate::outbox;
use crate::polling::upload_if_exists;
use crate::process_supervisor;
use crate::profiles::{
    agent_profile, check_adapter_compatibility, fetch_agent_profile, native_projection_files,
    profile_limits, provenance_from_env,
};
use crate::sandbox;
use crate::validation::run_validation;

/// Run one task attempt assigned by the control plane.
pub async fn run_attempt(cfg: Config, client: Client, assignment: Assignment) -> Result<()> {
    let repo_root = cfg.repository_root.clone();
    let ws_root = cfg.workspace_root.clone();
    let prep_assignment = assignment.clone();
    let upstream = assignment.upstream_commits.clone();
    let upstream_task_ids = assignment.upstream_task_ids.clone();
    // Stage 8 / line 257: distributed workflow without a shared Git remote —
    // fetch each upstream worker's `changes.patch` artifact from the CP up
    // front so `prepare_workspace` can `git apply` it when the commit SHA is
    // not reachable via `git fetch origin <sha>`. Parallel indices.
    let mut upstream_patches: Vec<(String, Vec<u8>)> = Vec::new();
    for (sha, task_id) in upstream.iter().zip(upstream_task_ids.iter()) {
        if task_id.is_empty() {
            continue;
        }
        let url = format!(
            "{}/v1/node/tasks/{}/artifacts/changes.patch",
            cfg.server, task_id
        );
        match client.get(&url).send().await {
            Ok(r) if r.status().is_success() => match r.bytes().await {
                Ok(b) if !b.is_empty() => upstream_patches.push((sha.clone(), b.to_vec())),
                _ => tracing::warn!("upstream {sha} changes.patch empty; will rely on git fetch"),
            },
            Ok(r) => tracing::warn!(
                "upstream {sha} changes.patch fetch status {}; will rely on git fetch",
                r.status()
            ),
            Err(e) => tracing::warn!("upstream {sha} changes.patch fetch error: {e}"),
        }
    }
    let ws = tokio::task::spawn_blocking(move || {
        git::prepare_workspace(
            &repo_root,
            &ws_root,
            &prep_assignment,
            &upstream,
            &upstream_patches,
        )
    })
    .await??;
    tracing::info!(attempt_id = %assignment.attempt_id, git = ws.is_git, "starting attempt");

    // Stage 2.1: a durable event outbox for this attempt, so a daemon kill no
    // longer drops the in-flight event tail (redelivered on next startup;
    // CP ingest is idempotent on (attempt_id, sequence)).
    let outbox = Arc::new(outbox::EventOutbox::open(
        &cfg.outbox_root,
        &assignment.attempt_id,
    )?);
    // If a prior run left undelivered events for this attempt, re-queue them
    // into the sink once it exists so they go out before new ones (sequence
    // order preserved by pending()).
    let pending = outbox.pending().unwrap_or_default();
    if !pending.is_empty() {
        tracing::info!(attempt_id = %assignment.attempt_id, count = pending.len(), "requeueing undelivered outbox events");
    }

    // Agent profile (idea 6): an optional system prompt for this adapter,
    // projected into the worktree as AGENTS.md before the agent runs. Sourced
    // from AGENTGRID_AGENT_PROFILE_<ID> (a path to a .md file, or inline text).
    // Stage 13: prefer the control-plane active profile (full: prompt +
    // autonomy + resource limits); fall back to env for the system prompt
    // (env sources are text-only, no autonomy/limits).
    let cp_profile = fetch_agent_profile(&client, &cfg.server, &assignment.adapter).await;
    let prompt_text: Option<String> = cp_profile
        .as_ref()
        .map(|p| p.system_prompt.clone())
        .filter(|s| !s.is_empty())
        .or_else(|| agent_profile(&assignment.adapter));
    if let Some(text) = &prompt_text {
        let p = ws.path.join("AGENTS.md");
        let _ = tokio::fs::write(&p, text).await;
        // Stage 11.3 / line 363: also write the per-agent native convention
        // file(s) that an adapter observes in preference to `AGENTS.md`.
        // Each is a verbatim copy of the same profile text; agents reading
        // either see the same guidance.
        for rel in native_projection_files(&assignment.adapter) {
            let f = ws.path.join(rel);
            if let Some(parent) = f.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let _ = tokio::fs::write(&f, text).await;
        }
    }

    // Stage 5: ACP adapters are driven over JSON-RPC 2.0 (stdio), not stdout
    // parsing. Everything below that point lives in drive_acp_session.
    if cfg
        .adapters
        .iter()
        .find(|s| s.id == assignment.adapter)
        .map(|s| s.protocol)
        == Some(crate::config::AdapterProtocol::Acp)
    {
        let sink = EventSink::new(
            assignment.attempt_id.clone(),
            client.clone(),
            cfg.server.clone(),
            assignment.fencing_token.clone(),
            outbox.clone(),
        );
        sink.requeue(pending).await;
        ack_attempt(
            &client,
            &cfg.server,
            &assignment.attempt_id,
            &assignment.fencing_token,
        )
        .await;
        create_agent_session(
            &client,
            &cfg.server,
            &assignment.attempt_id,
            &assignment.adapter,
            &assignment.fencing_token,
        )
        .await;
        let res =
            crate::drive_acp_session(&cfg, &client, &assignment, &ws.path, sink.clone()).await?;
        // Stage 2.3: keep the per-attempt repo_dir/branch for cleanup after finalize takes ws.
        let workdir = ws.path.clone();
        let cleanup_repo = ws.repo_dir.clone();
        let cleanup_branch =
            (ws.is_git && ws.branch.is_some()).then(|| ws.branch.clone().unwrap_or_default());
        let cleanup_path = ws.path.clone();
        let node_name = cfg.node_name.clone();
        let commit_sha =
            tokio::task::spawn_blocking(move || git::finalize_workspace(ws, node_name.as_str()))
                .await??;
        // Run the optional validation command — the ACP path used to skip it,
        // silently leaving validation_command unenforced for ACP agents. The
        // diff is already committed so it survives a validation failure.
        // Stage 11.4.
        let mut exit_code = if res.success { 0 } else { 1 };
        let mut error_code = res.error_code;
        if sink.spool_full() {
            exit_code = 1;
            error_code = Some("spool_full".into());
        }
        if exit_code == 0 {
            if let Some(cmd) = &assignment.validation_command {
                let vto = std::time::Duration::from_secs(
                    assignment.validation_timeout_secs.unwrap_or(300),
                );
                let cancel_url = format!(
                    "{}/v1/node/attempts/{}/cancel",
                    cfg.server, assignment.attempt_id
                );
                match run_validation(
                    &workdir,
                    cmd,
                    vto,
                    cancel_url,
                    client.clone(),
                    &cfg.server,
                    &assignment.attempt_id,
                    &assignment.fencing_token,
                    &sink,
                    &cfg.secrets,
                )
                .await
                {
                    Ok(o) if o.timed_out => {
                        exit_code = o.code.max(1);
                        error_code = Some("validation_timeout".into());
                    }
                    Ok(o) if o.cancelled => {
                        exit_code = o.code.max(1);
                        error_code = Some("validation_cancelled".into());
                    }
                    Ok(o) if o.code != 0 => {
                        exit_code = o.code;
                        error_code = Some("validation_failed".into());
                    }
                    Err(e) => {
                        tracing::error!("ACP validation failed to run: {e}");
                        error_code = Some("validation_failed".into());
                    }
                    _ => {}
                }
            }
        }
        report_complete(
            &client,
            &cfg.server,
            &assignment.attempt_id,
            exit_code,
            commit_sha,
            error_code,
            res.session_id.clone(),
            None,
            None,
            None,
            None,
            assignment.provenance.clone().or_else(provenance_from_env),
            vec![],
            &cfg.completion_outbox,
            &assignment.fencing_token,
        )
        .await;
        // Stage 2.3: reclaim the per-attempt worktree and branch now the attempt
        // is terminal (prevents long-lived worktree/branch retention leaking disk).
        tokio::task::spawn_blocking(move || {
            git::cleanup_workspace(
                &cleanup_path,
                cleanup_repo.as_deref(),
                cleanup_branch.as_deref(),
            )
        })
        .await
        .ok();
        return Ok(());
    }

    // Raw adapter output is mirrored to disk as a safety net against CLI
    // output-format changes (Stage 3.1): the structured events may be lossy,
    // but the raw log is always preserved as an artifact.
    let raw_path = ws.path.join("agent-raw-output.log");
    let raw_file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&raw_path)
        .await
        .ok()
        .map(|f| Arc::new(Mutex::new(f)));

    // Stage 2.4: run strictly the adapter the control plane assigned; an
    // unknown or missing adapter binary is an infrastructure failure, not a
    // silent fallback to whatever binary happens to be configured.
    let bin = match resolve_adapter_bin(&assignment.adapter) {
        Some(b) => b,
        None => {
            tracing::error!(
                attempt_id = %assignment.attempt_id,
                adapter = %assignment.adapter,
                "adapter binary not found; reporting infrastructure_failed"
            );
            report_complete(
                &client,
                &cfg.server,
                &assignment.attempt_id,
                127,
                None,
                Some("infrastructure_failed".into()),
                None,
                None,
                None,
                None,
                None,
                assignment.provenance.clone().or_else(provenance_from_env),
                vec![],
                &cfg.completion_outbox,
                &assignment.fencing_token,
            )
            .await;
            return Ok(());
        }
    };
    // Stage 3.2: spawn through the ExecutionBackend contract (native process).
    // Stage 11.2 / line 358: the legacy wrapper path is now sandboxed too via
    // `sandbox_prefix` -> `SpawnRequest::sandbox_prefix_args` (matches the ACP
    // path's `sandbox_command`); `AGENTGRID_SANDBOX=docker` pulls the adapter
    // runs inside the configured image. Default `none` is passthrough.
    //
    // Feedback loop (Stage 11.4): when a validation_command is configured and
    // the agent exits 0 but validation fails, re-spawn the agent with the
    // validation error appended to the prompt (same worktree) so it can fix
    // its own output. Bounded by AGENTGRID_FEEDBACK_RETRIES (default 0 = off,
    // backward compatible). Each round reuses the same sink/flusher so all
    // events stay under one attempt; the worktree accumulates the agent's
    // fixes and is committed once at the end.
    let retries: usize = std::env::var("AGENTGRID_FEEDBACK_RETRIES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let cancel_url = format!(
        "{}/v1/node/attempts/{}/cancel",
        cfg.server, assignment.attempt_id
    );
    let sink = EventSink::new(
        assignment.attempt_id.clone(),
        client.clone(),
        cfg.server.clone(),
        assignment.fencing_token.clone(),
        outbox.clone(),
    );
    sink.requeue(pending).await;
    let workdir = ws.path.clone();
    let validation_log = workdir.join("validation.log");
    let mut prompt = assignment.prompt.clone();
    let mut last_code: i32;
    let mut last_kill_reason: Option<&'static str> = None;
    // Hardening P0 item 12: distinct validation verdicts (timeout/cancel) are
    // captured here so the post-loop error_code mapping preserves them instead
    // of collapsing into `validation_failed`.
    let mut validation_verdict: Option<&'static str> = None;

    // Ack once; the attempt is `running` for its whole (multi-round) lifetime.
    ack_attempt(
        &client,
        &cfg.server,
        &assignment.attempt_id,
        &assignment.fencing_token,
    )
    .await;
    create_agent_session(
        &client,
        &cfg.server,
        &assignment.attempt_id,
        &assignment.adapter,
        &assignment.fencing_token,
    )
    .await;
    let flusher = tokio::spawn(sink.clone().run_flusher());

    // Stage 13 capability check (legacy raw path): the profile's declared
    // adapter_version is checked against the cached probe. On mismatch we only
    // warn here — the ACP path enforces hard fail-closed; raw-path hard
    // refuse would need plumbing a terminal exit through this loop, deferred.
    if check_adapter_compatibility(
        cp_profile.as_ref(),
        cfg.adapter_versions
            .get(&assignment.adapter)
            .and_then(|v| v.as_deref()),
    )
    .is_some()
    {
        tracing::warn!(
            attempt_id = %assignment.attempt_id,
            "raw path: profile/adapter version mismatch (ACP path enforces fail-closed)"
        );
    }

    let mut round = 0usize;
    // Stage 11.2 / line 358: route the legacy wrapper-binary spawn through
    // the configured sandbox (matches the ACP path). `sandbox_prefix` splits
    // the program from the prefix args because ProcessBackend appends its own
    // `--prompt <prompt>` after the prefix.
    // Hardening P2 item 659: pass task network_mode to sandbox.
    let (sb_program, sb_prefix) = sandbox::sandbox_prefix(
        cfg.sandbox,
        &ws.path,
        &bin,
        assignment.network_mode.as_deref(),
    );
    // Plan §27: egress audit — log the effective isolation applied to this
    // attempt (task mode + resolved docker network) so operators can verify
    // the deployed network policy from the daemon logs.
    tracing::info!(
        attempt_id = %assignment.attempt_id,
        task_network_mode = ?assignment.network_mode,
        resolved_network = sandbox::resolved_network_mode(
            assignment.network_mode.as_deref().unwrap_or("none")
        ),
        sandbox = ?cfg.sandbox,
        "attempt egress policy"
    );
    // Hardening P0/P1 item 5: never let the agent run unsafe-unattended in an
    // unsandboxed environment unless the operator explicitly opted in.
    let env_remove = sandbox::unsafe_env_guard(cfg.sandbox);
    let validation_passed = loop {
        // Hardening P1 item 27: forward profile-declared secrets explicitly —
        // ProcessBackend env_clears the child, so daemon-env secrets must be
        // allowlisted here.
        let mut spawn_env = cfg.adapter_env.clone();
        if let Some(p) = &cp_profile {
            for req in &p.secret_requirements {
                if let Some(v) = std::env::var_os(&req.env) {
                    spawn_env.push((req.env.clone(), v.to_string_lossy().to_string()));
                }
            }
        }
        let req = SpawnRequest {
            bin: sb_program.clone(),
            sandbox_prefix_args: sb_prefix.clone(),
            prompt: prompt.clone(),
            extra_args: vec![],
            raw_args: false,
            workdir: ws.path.clone(),
            attempt_id: assignment.attempt_id.clone(),
            timeout: Duration::from_secs(assignment.timeout_secs.max(1)),
            env: spawn_env,
            env_remove: env_remove.clone(),
            limits: profile_limits(cp_profile.as_ref()),
        };
        let run = match process_supervisor::supervise_adapter(
            req,
            cancel_url.clone(),
            client.clone(),
            sink.clone(),
            cfg.secrets.clone(),
            raw_file.clone(),
            &assignment.attempt_id,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("failed to spawn adapter: {e}");
                last_code = 127;
                break false;
            }
        };
        let code = run.code;
        let kill_reason = run.kill_reason;
        // Stage 2.1: record the terminal completion BEFORE the post-adapter
        // sends so a daemon kill during the (possibly blocking) flush/upload
        // window still redelivers the completion on the next startup. The exit
        // code is known here; commit_sha/validation verdict are refined later
        // (record() replaces the prior line, latest wins).
        let early_req = CompleteAttemptRequest {
            exit_code: code,
            commit_sha: None,
            error_code: kill_reason.map(|k| k.to_string()),
            resolved_base_sha: None,
            remote_head_at_start: None,
            remote_head_at_finish: None,
            acp_session_id: None,
            plan: None,
            provenance: assignment.provenance.clone().or_else(provenance_from_env),
            pending_artifacts: vec![],
        };
        if let Err(e) = cfg.completion_outbox.record(
            &assignment.attempt_id,
            &early_req,
            &assignment.fencing_token,
        ) {
            tracing::warn!(attempt_id = %assignment.attempt_id, "early completion record failed: {e}");
        }
        // Single-shot drain: don't block for tens of seconds on a down CP; the
        // flusher loop + durable outbox cover redelivery.
        sink.flush_quick().await;
        if code == 0 && sink.adapter_event_count() == 0 {
            tracing::warn!(
                attempt_id = %assignment.attempt_id,
                "adapter exited 0 but produced no stdout/stderr events; task output may be empty (silent agent?)"
            );
        }
        last_code = code;
        last_kill_reason = kill_reason;

        // Agent failed (non-zero exit): no fixable validation to feed back; stop.
        if code != 0 {
            break false;
        }
        // Validate; if it passes we're done. If it fails and a retry is left,
        // feed the validation error back into the prompt and re-spawn.
        if let Some(cmd) = &assignment.validation_command {
            let vto =
                std::time::Duration::from_secs(assignment.validation_timeout_secs.unwrap_or(300));
            let cancel_url = format!(
                "{}/v1/node/attempts/{}/cancel",
                cfg.server, assignment.attempt_id
            );
            let v = run_validation(
                &workdir,
                cmd,
                vto,
                cancel_url,
                client.clone(),
                &cfg.server,
                &assignment.attempt_id,
                &assignment.fencing_token,
                &sink,
                &cfg.secrets,
            )
            .await;
            let fail = match &v {
                Ok(o) => {
                    // Timeout/cancel are terminal — no feedback retry loop.
                    if o.timed_out {
                        validation_verdict = Some("validation_timeout");
                        break false;
                    }
                    if o.cancelled {
                        validation_verdict = Some("validation_cancelled");
                        break false;
                    }
                    o.code != 0
                }
                Err(e) => {
                    tracing::error!("validation failed to run: {e}");
                    true
                }
            };
            if fail {
                if round < retries {
                    let log = tokio::fs::read_to_string(&validation_log)
                        .await
                        .ok()
                        .filter(|s| !s.trim().is_empty())
                        .unwrap_or_else(|| "(no validation output)".into());
                    tracing::info!(attempt_id = %assignment.attempt_id, round, "validation failed; feeding error back to agent");
                    sink.push(
                        EventKind::Log.to_event_type(),
                        json!({ "kind": "feedback", "round": round, "retrying": true }),
                    )
                    .await;
                    prompt = format!(
                        "{orig}\n\nValidation failed (round {round}):\n```\n{log}\n```\nFix the code so the validation passes.",
                        orig = assignment.prompt
                    );
                    round += 1;
                    continue;
                }
                break false;
            }
        }
        break true;
    };
    // ponytail: flusher kept alive through finalize/artifacts/report_complete so
    // events buffered during a CP outage keep being retried and are delivered
    // once the CP recovers (the durable outbox also retains them). Aborted
    // after report_complete so a terminal attempt doesn't leak the task.
    // (was: flusher.abort() here, before the post-adapter sends.)

    let node_name = cfg.node_name.clone();
    let patch_path = workdir.join("changes.patch");
    // Stage 2.3: keep the per-attempt repo_dir/branch so the worktree and its
    // ref can be reclaimed after the attempt is terminal (finalize takes ws).
    let cleanup_repo = ws.repo_dir.clone();
    let cleanup_branch =
        (ws.is_git && ws.branch.is_some()).then(|| ws.branch.clone().unwrap_or_default());
    let cleanup_path = ws.path.clone();
    // Hardening P2 item 32-5: capture the resolved base before `ws` is moved
    // into `finalize_workspace`, so the completion can persist it.
    let resolved_base_sha = ws.base_commit.clone();
    // Hardening P1 item 32: capture the remote HEAD at attempt *start* (right
    // after prepare_workspace fetched origin). Best-effort — None on any git
    // failure; audit data must never block the attempt.
    let remote_head_at_start = if ws.is_git {
        let repo_dir = ws.repo_dir.clone();
        tokio::task::spawn_blocking(move || repo_dir.as_deref().and_then(git::remote_head_at))
            .await
            .ok()
            .flatten()
    } else {
        None
    };
    let commit_sha =
        tokio::task::spawn_blocking(move || git::finalize_workspace(ws, node_name.as_str()))
            .await??;

    let code = last_code;
    let error_code: Option<String> = if let Some(v) = validation_verdict {
        // Hardening P0 item 12: timeout/cancel are distinct from a plain
        // validation failure and must not collapse into `validation_failed`.
        Some(v.into())
    } else if sink.spool_full() {
        Some("spool_full".into())
    } else if code == 0 {
        if validation_passed {
            None
        } else {
            Some("validation_failed".into())
        }
    } else {
        Some(last_kill_reason.unwrap_or("agent_failed").into())
    };

    // Upload produced artifacts (changes.patch for git tasks; validation.log;
    // raw adapter output as a format-change safety net, Stage 3.1). Each is
    // staged into the durable spool first (Hardening P1 item 11).
    let spool_root = cfg.artifact_spool_root.clone();
    upload_if_exists(
        &client,
        &cfg.server,
        &assignment.attempt_id,
        "changes.patch",
        &patch_path,
        &assignment.fencing_token,
        &spool_root,
        cfg.max_artifact_size,
    )
    .await;
    upload_if_exists(
        &client,
        &cfg.server,
        &assignment.attempt_id,
        "validation.log",
        &validation_log,
        &assignment.fencing_token,
        &spool_root,
        cfg.max_artifact_size,
    )
    .await;
    upload_if_exists(
        &client,
        &cfg.server,
        &assignment.attempt_id,
        "agent-raw-output.log",
        &raw_path,
        &assignment.fencing_token,
        &spool_root,
        cfg.max_artifact_size,
    )
    .await;

    tracing::info!(attempt_id = %assignment.attempt_id, exit_code = code, "attempt finished");
    // Stage 2.1: drain all pending events from the durable outbox BEFORE the
    // completion so the CP sees the full event stream before marking the task
    // terminal. Read from disk (ground truth), not RAM — an aborted flusher's
    // in-flight batch is gone from RAM but still on disk here.
    sink.drain_outbox(tokio::time::Instant::now() + Duration::from_secs(60))
        .await;
    // Hardening P1 item 32: capture the remote HEAD at attempt *finish*
    // (after the agent ran). Best-effort — None on any git failure or when
    // not a git task; audit data must never block the completion.
    let remote_head_at_finish = {
        let repo_dir = cleanup_repo.clone();
        tokio::task::spawn_blocking(move || repo_dir.as_deref().and_then(git::remote_head_at))
            .await
            .ok()
            .flatten()
    };
    // Hardening P1 item 11: report which artifacts are still owed (staged in
    // the durable spool but not yet acked by the CP) so operators can see the
    // outstanding set and the startup retry knows what to deliver.
    let pending_artifacts: Vec<String> = artifact_spool::pending(&cfg.artifact_spool_root)
        .ok()
        .map(|p| {
            p.iter()
                .filter(|(aid, _, _)| aid == &assignment.attempt_id)
                .map(|(_, name, _)| name.clone())
                .collect()
        })
        .unwrap_or_default();
    report_complete(
        &client,
        &cfg.server,
        &assignment.attempt_id,
        code,
        commit_sha,
        error_code,
        None,
        sink.take_plan(),
        resolved_base_sha,
        remote_head_at_start,
        remote_head_at_finish,
        assignment.provenance.clone().or_else(provenance_from_env),
        pending_artifacts,
        &cfg.completion_outbox,
        &assignment.fencing_token,
    )
    .await;
    // Ground-truth redelivery: any events still on disk (e.g. the CP flapped
    // again, or the pre-completion drain couldn't send) are delivered now.
    // The CP is up (report_complete succeeded).
    sink.drain_outbox(tokio::time::Instant::now() + Duration::from_secs(15))
        .await;
    flusher.abort();
    // Stage 2.3: reclaim the per-attempt worktree and branch now the attempt
    // is terminal. Best-effort in a spawn_blocking so a stuck worktree never
    // turns a successful attempt terminal.
    tokio::task::spawn_blocking(move || {
        git::cleanup_workspace(
            &cleanup_path,
            cleanup_repo.as_deref(),
            cleanup_branch.as_deref(),
        )
    })
    .await
    .ok();
    Ok(())
}
