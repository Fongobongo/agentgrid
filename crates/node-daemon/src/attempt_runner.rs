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
use crate::completion::{ack_attempt, create_agent_session, report_complete, AckOutcome};
use crate::config::Config;
use crate::evals;
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

/// Plan 1.8 (#15): rotate to the next token for the first account whose env
/// var matches `cfg.adapter_env` (or the first account when none match), so
/// `drive_acp_session` re-launches the adapter with the next provider token
/// after a 429. Mutates `cfg.adapter_env` in place (replaces the value for the
/// account's env, or appends it). Returns `false` when the pool is exhausted
/// or empty — the caller must not retry again.
pub(crate) fn rotate_account_token(cfg: &mut Config, index: &mut usize) -> bool {
    let Some(acc) = cfg.accounts.get(*index) else {
        return false; // pool empty or exhausted
    };
    if acc.tokens.len() < 2 {
        return false; // single token: nothing to rotate to
    }
    // Pick the next token (skip the one already in adapter_env, if present).
    let current = cfg
        .adapter_env
        .iter()
        .find(|(k, _)| k == &acc.env)
        .map(|(_, v)| v.clone());
    let next = acc
        .tokens
        .iter()
        .find(|t| Some(t.as_str()) != current.as_deref())
        .cloned()
        .unwrap_or_else(|| acc.tokens[0].clone());
    if let Some(slot) = cfg.adapter_env.iter_mut().find(|(k, _)| k == &acc.env) {
        slot.1 = next;
    } else {
        cfg.adapter_env.push((acc.env.clone(), next));
    }
    *index += 1;
    true
}

/// Plan 1.13 follow-up: spawn `ag index --out .idx.json` on the worktree
/// (gated by `AGENTGRID_REPO_INDEX=1`, default off — runs the walker on every
/// attempt; only opt in when the adapter has no built-in codebase awareness and
/// the per-attempt token price pays for itself). On success the JSON packet
/// lives at `<worktree>/.idx.json`; a digest (top-levels + symbol names) is
/// spliced above the profile text.
///
/// `ag` is resolved from `AGENTGRID_INDEX_BIN` (default `ag`) and must be on
/// PATH. On any failure (binary missing, parse error, non-zero exit) the
/// profile text is returned unchanged — the slow path never blocks the agent.
fn maybe_with_repo_digest(profile_text: &str, worktree: &std::path::Path) -> String {
    if std::env::var("AGENTGRID_REPO_INDEX").ok().as_deref() != Some("1") {
        return profile_text.to_string();
    }
    let bin = std::env::var("AGENTGRID_INDEX_BIN").unwrap_or_else(|_| "ag".into());
    let out = worktree.join(".idx.json");
    // ponytail: synchronous subprocess inside the attempt prep path. The
    // indexer walk on a small repo is ~50 ms; on a huge one the cost is paid
    // once per attempt and only when an operator opts in. Async spawn would
    // add ceremony without saving wall time (we block prep either way).
    let status = match std::process::Command::new(&bin)
        .arg("index")
        .arg(worktree)
        .arg("--out")
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
    {
        Ok(s) => s,
        Err(_) => return profile_text.to_string(),
    };
    if !status.success() {
        return profile_text.to_string();
    }
    let bytes = match std::fs::read(&out) {
        Ok(b) => b,
        Err(_) => return profile_text.to_string(),
    };
    // Pull just enough of the packet sketch to render a digest; serde_json::Value
    // keeps the helper allocation-light and resilient to packet shape evolution.
    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return profile_text.to_string(),
    };
    let commit = v.get("commit").and_then(|x| x.as_str()).unwrap_or("");
    let total_files = v
        .get("summary")
        .and_then(|s| s.get("total_files"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let total_syms = v
        .get("summary")
        .and_then(|s| s.get("total_symbols"))
        .and_then(|x| x.as_u64())
        .unwrap_or(0);
    let mut digest = format!(
        "## Repo index (commit {commit})\n\
         Top-level entry points across {total_files} files ({total_syms} symbols).\
         See `.idx.json` for the full packet.\n"
    );
    if let Some(files) = v.get("files").and_then(|f| f.as_array()) {
        // ponytail: cap at 20 files in the digest — keeps the token cost modest
        // and avoids dumping the whole index into every system prompt. The
        // full packet is on disk at `.idx.json` if the agent wants to read it.
        for f in files.iter().take(20) {
            let path = f.get("path").and_then(|x| x.as_str()).unwrap_or("");
            if let Some(syms) = f.get("symbols").and_then(|s| s.as_array()) {
                let names: Vec<&str> = syms
                    .iter()
                    .filter_map(|s| s.get("name").and_then(|n| n.as_str()))
                    .collect();
                if !names.is_empty() {
                    digest.push_str(&format!("\n- `{path}`: {}\n", names.join(" · ")));
                }
            }
        }
    }
    format!("{digest}\n---\n\n{profile_text}")
}

/// Audit ND-3: finalize the workspace without `?`-aborting the attempt. A
/// finalize error used to propagate out of `run_attempt` - no completion was
/// reported (the CP kept the attempt `running` until the reaper) and the
/// worktree leaked until the 24h prune. Returns `(sha, failed)`; a `None`
/// sha from a successful non-git / no-commit finalize is NOT a failure.
async fn finalize_or_fail(
    ws: git::Workspace,
    node_name: String,
    attempt_id: &str,
) -> (Option<String>, bool) {
    match tokio::task::spawn_blocking(move || git::finalize_workspace(ws, &node_name)).await {
        Ok(Ok(sha)) => (sha, false),
        Ok(Err(e)) => {
            tracing::error!(attempt_id = %attempt_id, "workspace finalize failed: {e:#}");
            (None, true)
        }
        Err(e) => {
            tracing::error!(attempt_id = %attempt_id, "workspace finalize task panicked: {e}");
            (None, true)
        }
    }
}

/// Run one task attempt assigned by the control plane.
pub async fn run_attempt(cfg: Config, client: Client, assignment: Assignment) -> Result<()> {
    // WS Cancel messages wake the supervisor via this notifier (plan 0.3 2.3);
    // unregistered automatically when the attempt finishes.
    let _cancel_guard = crate::completion::CancelGuard::new(&assignment.attempt_id);
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
        // Plan 1.13 follow-up: inject a top-level symbol digest from a
        // `ag index --out <path>` run on the worktree into the system prompt,
        // cost-gated by `AGENTGRID_REPO_INDEX=1` (default off — every attempt
        // spins the indexer, so the token price every run). The digest lands
        // above the profile text as a `## Repo index (commit …)` section,
        // giving agents without built-in codebase awareness a map of fn/type
        // entry points before they start writing. `--out` writes the JSON
        // packet to `<worktree>/.idx.json` so a future cache layer can hash.
        let text = maybe_with_repo_digest(text, &ws.path);
        let p = ws.path.join("AGENTS.md");
        let _ = tokio::fs::write(&p, &text).await;
        for rel in native_projection_files(&assignment.adapter) {
            let f = ws.path.join(rel);
            if let Some(parent) = f.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let _ = tokio::fs::write(&f, &text).await;
        }
    }

    // Plan 2.5 (#22b): on a retry the CP ships `eval_cases` on the assignment.
    // Materialise them into `<worktree>/.agentgrid/evals/` *before* the agent
    // starts so the agent sees the obligation list when reading the worktree
    // (the verifier reads, not writes), and so the post-agent probe can just
    // shell-probe every `*.yaml` under the dir.
    if !assignment.eval_cases.is_empty() && !assignment.read_only {
        let materialized = evals::materialize_eval_cases(
            &ws.path,
            &assignment.task_id,
            &assignment.eval_cases,
            &cfg.server,
            &assignment.fencing_token,
            &client,
        )
        .await
        .unwrap_or_else(|e| {
            tracing::warn!(attempt_id = %assignment.attempt_id, "eval-case materialize failed: {e}");
            Vec::new()
        });
        if !materialized.is_empty() {
            tracing::info!(
                attempt_id = %assignment.attempt_id,
                n = materialized.len(),
                "materialised eval suite into worktree"
            );
        }
    }

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
        if matches!(
            ack_attempt(
                &client,
                &cfg.server,
                &assignment.attempt_id,
                &assignment.fencing_token,
            )
            .await,
            AckOutcome::Rejected
        ) {
            tracing::warn!(
                attempt_id = %assignment.attempt_id,
                "ack rejected: lease reverted or cancelled on the CP; dropping assignment \
                 before spawning the agent"
            );
            return Ok(());
        }
        create_agent_session(
            &client,
            &cfg.server,
            &assignment.attempt_id,
            &assignment.adapter,
            &assignment.fencing_token,
        )
        .await;
        // Plan 1.8 (#15): account failover — on a provider 429 (surfaced as a
        // rate-limit error event by the adapter), rotate to the next token in
        // the pool and re-drive the whole ACP session. The worktree is
        // untouched across rotations; only the credential env changes.
        let mut cfg = cfg.clone();
        let mut account_index = 0usize;
        let res = loop {
            let res = crate::drive_acp_session(&cfg, &client, &assignment, &ws.path, sink.clone())
                .await?;
            if !res.rate_limited || !rotate_account_token(&mut cfg, &mut account_index) {
                break res;
            }
            tracing::warn!(
                attempt_id = %assignment.attempt_id,
                account_index,
                "provider 429; rotating account and retrying attempt"
            );
            // Plan 1.8 (#15): record the rotation for the heartbeat usage
            // endpoint.
            if let Some(acc) = cfg.accounts.get(account_index.saturating_sub(1)) {
                crate::account_usage::note_rate_limited(&acc.env);
            }
            sink.push(
                EventKind::Log.to_event_type(),
                json!({ "kind": "feedback", "account_rotated": true, "round": account_index }),
            )
            .await;
        };
        // Plan 1.8 (#15): record the attempt against the account env backing
        // this adapter (first account entry whose env is set in adapter_env,
        // or none).
        if let Some(acc) = cfg.accounts.first() {
            crate::account_usage::note_attempt(&acc.env);
        }
        // Stage 2.3: keep the per-attempt repo_dir/branch for cleanup after finalize takes ws.
        let workdir = ws.path.clone();
        let cleanup_repo = ws.repo_dir.clone();
        let cleanup_branch =
            (ws.is_git && ws.branch.is_some()).then(|| ws.branch.clone().unwrap_or_default());
        let cleanup_path = ws.path.clone();
        let (commit_sha, finalize_failed) =
            finalize_or_fail(ws, cfg.node_name.clone(), &assignment.attempt_id).await;
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
        if finalize_failed {
            exit_code = 1;
            error_code = Some("infrastructure_failed".into());
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
        // Plan 2.5 (#22b): eval suite probe for retry attempts (cases were
        // materialised into the worktree at attempt start). Runs after the
        // ACP-loop validation so a passing fix must also re-prove every
        // accumulated eval case.
        if exit_code == 0 && !assignment.eval_cases.is_empty() {
            match evals::probe_evals(&workdir, evals::EVAL_TIMEOUT).await {
                Ok(o) if o.ok => {}
                Ok(o) => {
                    sink.push(
                        EventKind::Log.to_event_type(),
                        json!({ "kind": "eval_fail", "log": o.log }),
                    )
                    .await;
                    exit_code = 1;
                    error_code = Some("eval_failed".into());
                }
                Err(e) => {
                    sink.push(
                        EventKind::Log.to_event_type(),
                        json!({ "kind": "eval_fail", "log": format!("eval probe error: {e}") }),
                    )
                    .await;
                    exit_code = 1;
                    error_code = Some("eval_failed".into());
                }
            }
        }
        // Feature "opencode profiles": error-threshold self-heal. On a
        // successful attempt reset the streak; on a config-class error
        // (invalid model, provider deny, missing credentials) count it and
        // trigger a pulled refresh when the streak crosses the threshold.
        // The pull itself is best-effort: a CP outage leaves the daemon
        // running under its on-disk config.
        let config_err_payload = serde_json::json!({
            "exit_code": exit_code,
            "error_code": error_code.as_deref().unwrap_or(""),
            "adapter": assignment.adapter,
        });
        // Wrapper-branch parity (audit ND-1/ND-2): upload the produced
        // artifacts and drain the durable outbox BEFORE the completion, so
        // the CP sees the full event stream and the patch while the attempt
        // is still live. The ACP flusher died with drive_acp_session, so
        // this disk-ground-truth drain is the only delivery path left —
        // without it the eval/validation events written above would strand
        // on disk forever (startup recovery never replays event outboxes of
        // terminal attempts) and changes.patch would be destroyed by the
        // cleanup below without ever reaching the CP.
        let spool_root = cfg.artifact_spool_root.clone();
        upload_if_exists(
            &client,
            &cfg.server,
            &assignment.attempt_id,
            "changes.patch",
            &workdir.join("changes.patch"),
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
            &workdir.join("validation.log"),
            &assignment.fencing_token,
            &spool_root,
            cfg.max_artifact_size,
        )
        .await;
        sink.drain_outbox(tokio::time::Instant::now() + Duration::from_secs(60))
            .await;
        // Hardening P1 item 11 (wrapper parity): report which artifacts are
        // still owed so operators see the outstanding set.
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
            exit_code,
            commit_sha,
            error_code,
            res.session_id.clone(),
            None,
            None,
            None,
            None,
            assignment.provenance.clone().or_else(provenance_from_env),
            pending_artifacts,
            &cfg.completion_outbox,
            &assignment.fencing_token,
        )
        .await;
        // Wrapper parity: post-completion ground-truth redelivery — events
        // still on disk after the pre-drain (CP flapped mid-drain) are
        // delivered now while the CP is known-up.
        sink.drain_outbox(tokio::time::Instant::now() + Duration::from_secs(15))
            .await;
        // Audit X-N1: terminal attempt — drop the drained spool file so it
        // stops counting against the global outbox quota.
        outbox.discard();
        if exit_code == 0 {
            crate::config_error::note_attempt_succeeded();
        } else {
            if crate::config_error::note_config_error(&config_err_payload) {
                let cfg_cl = cfg.clone();
                let client_cl = client.clone();
                if let Some(cred_cl) = crate::config::process_credential() {
                    tokio::spawn(async move {
                        if let Err(e) = crate::opencode_config::pull_and_apply(
                            &cfg_cl,
                            &client_cl,
                            &cred_cl,
                            "error_threshold",
                            None,
                        )
                        .await
                        {
                            tracing::warn!("error-triggered opencode-config pull failed: {e}");
                        }
                    });
                }
            }
        }
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
    if matches!(
        ack_attempt(
            &client,
            &cfg.server,
            &assignment.attempt_id,
            &assignment.fencing_token,
        )
        .await,
        AckOutcome::Rejected
    ) {
        tracing::warn!(
            attempt_id = %assignment.attempt_id,
            "ack rejected: lease reverted or cancelled on the CP; dropping assignment \
             before spawning the agent"
        );
        return Ok(());
    }
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
        assignment.read_only,
        Some(&sandbox::container_name(&assignment.attempt_id)),
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
    let mut validation_passed = loop {
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
        // Feature "opencode profiles": per-attempt override merged over the
        // node's on-disk profile and injected via OPENCODE_CONFIG_CONTENT
        // (env-only; the variable dies with the child process so it cannot
        // leak into later attempts on the same node).
        if assignment.adapter == "opencode" || assignment.adapter.starts_with("opencode:") {
            if let Some(merged) =
                crate::opencode_config::build_override_env(assignment.opencode_override.as_ref())
                    .await
            {
                spawn_env.push(("OPENCODE_CONFIG_CONTENT".to_string(), merged));
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
            // Plan 1.2 (#4): command guard derived from the daemon's config.
            // A single Arc is shared for the whole attempt; the same instance
            // enforces the policy on every tool_call line.
            std::sync::Arc::new(crate::command_guard::CommandGuard::new(
                cfg.guard_deny.clone(),
                cfg.guard_allow.clone(),
            )),
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

    // Plan 2.5 (#22b): when this retry carries an accumulated eval suite
    // (fetched at attempt start), probe every case after the agent +
    // validation_command pass. A failing eval flips the terminal outcome:
    // we report exit=1 with error_code `eval_failed` so the CP marks the
    // task failed (and the retry loop has the eval log in events).
    if validation_passed && !assignment.eval_cases.is_empty() {
        match evals::probe_evals(&workdir, evals::EVAL_TIMEOUT).await {
            Ok(o) if o.ok => {}
            Ok(o) => {
                sink.push(
                    EventKind::Log.to_event_type(),
                    json!({ "kind": "eval_fail", "log": o.log }),
                )
                .await;
                tracing::warn!(
                    attempt_id = %assignment.attempt_id,
                    "eval suite failed on retry; reporting failure"
                );
                last_code = 1;
                validation_verdict = Some("eval_failed");
                // Force validation_passed off so the terminal code path
                // treats this as a failure, not a clean pass.
                validation_passed = false;
            }
            Err(e) => {
                sink.push(
                    EventKind::Log.to_event_type(),
                    json!({ "kind": "eval_fail", "log": format!("eval probe error: {e}") }),
                )
                .await;
                last_code = 1;
                validation_verdict = Some("eval_failed");
                validation_passed = false;
            }
        }
    }
    let _ = validation_passed;

    // events buffered during a CP outage keep being retried and are delivered
    // once the CP recovers (the durable outbox also retains them). Aborted
    // after report_complete so a terminal attempt doesn't leak the task.
    // (was: flusher.abort() here, before the post-adapter sends.)

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
    // Audit ND-3 (wrapper branch, same as the ACP one): a finalize failure
    // used to `?`-abort run_attempt with no completion report and no
    // worktree cleanup, stranding the attempt `running` until the reaper.
    let (commit_sha, finalize_failed) =
        finalize_or_fail(ws, cfg.node_name.clone(), &assignment.attempt_id).await;

    let mut code = last_code;
    let mut error_code: Option<String> = if let Some(v) = validation_verdict {
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
    // Audit ND-3: the finalize verdict outranks agent/validation outcomes —
    // without a finalized worktree there is no deliverable result even when
    // the agent itself exited 0.
    if finalize_failed {
        code = code.max(1);
        error_code = Some("infrastructure_failed".into());
    }

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
    // Audit X-N1: terminal attempt — drop the drained spool file so it stops
    // counting against the global outbox quota.
    outbox.discard();
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

#[cfg(test)]
mod digest_tests {
    use super::maybe_with_repo_digest;
    use std::sync::Mutex;

    // Both tests mutate `AGENTGRID_REPO_INDEX`, which is a process-global env
    // var. Serialize them so they don't race on the gate.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn digest_idempotent_when_env_off_by_default() {
        let _g = ENV_GUARD.lock().unwrap();
        // AGENTGRID_REPO_INDEX unset (or any value != "1") → text intact, no
        // spawn attempt, no `.idx.json` written.
        std::env::remove_var("AGENTGRID_REPO_INDEX");
        let dir = std::env::temp_dir().join(format!("digest-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = maybe_with_repo_digest("hello", &dir);
        assert_eq!(out, "hello");
        // `.idx.json` never created because the gate closed before spawn.
        assert!(!dir.join(".idx.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn digest_inlines_top_level_symbols_when_ag_present() {
        let _g = ENV_GUARD.lock().unwrap();
        // The harness runs the live debug `ag` binary from this workspace's
        // `target/debug/ag`; gating via `AGENTGRID_INDEX_BIN` lets the test
        // point to it without relying on PATH order.  Skipped no-op when the
        // binary is absent (CI runners without a freshly built `ag`).
        let ag = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("target/debug/ag");
        if !ag.exists() {
            eprintln!(
                "skip digest inline test: {} missing (build `ag` first)",
                ag.display()
            );
            return;
        }
        // Build a tiny rust repo to index.
        let dir = std::env::temp_dir().join(format!("digest-repo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("lib.rs"),
            "pub fn alpha() -> u32 { 1 }\nstruct Foo;\n",
        )
        .unwrap();

        // Gate on.
        std::env::set_var("AGENTGRID_REPO_INDEX", "1");
        std::env::set_var("AGENTGRID_INDEX_BIN", ag.to_str().unwrap());
        let out = maybe_with_repo_digest("PROMPT_BODY", &dir);
        std::env::remove_var("AGENTGRID_REPO_INDEX");
        std::env::remove_var("AGENTGRID_INDEX_BIN");

        // The original prompt stays (after the splice), the index header
        // came first, and the symbol `alpha` appeared in the digest.
        assert!(
            out.starts_with("## Repo index"),
            "header missing; got: {}",
            &out[..out.len().min(80)]
        );
        assert!(out.contains("PROMPT_BODY"), "original prompt dropped");
        assert!(out.contains("alpha"), "symbol `alpha` missing in digest");
        assert!(
            out.contains("---\n\nPROMPT_BODY"),
            "splice separator missing"
        );

        // `.idx.json` written (cache layer).
        let p = dir.join(".idx.json");
        assert!(p.exists(), "index packet .idx.json missing");
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(
            s.contains("\"name\": \"alpha\""),
            "json packet missing symbol"
        );

        // Cleanup.
        let _ = std::fs::remove_dir_all(&dir);
    }
}
