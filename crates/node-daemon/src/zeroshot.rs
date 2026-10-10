//! Stage 10: Zeroshot cluster-executor driver.
//!
//! A task with `adapter = "zeroshot"` runs inside a Zeroshot cluster instead
//! of a wrapper-binary subprocess. The node drives the executor CLI through
//! three subcommands (the contract a real `zeroshot` binary must implement;
//! `tests/e2e/zeroshot-stub.sh` is a conformant stub used by E2E):
//!
//! - `zeroshot run --id <attempt-id> --workdir <dir> [--role executor|verifier]
//!   --prompt <prompt>` — blocking. Creates (or re-attaches, same `--id`)
//!   the attempt's cluster, streams adapter-contract lines on stdout (NDJSON
//!   `log`/`tool_call`/`file_change`/`result`/`error` events, unknown lines
//!   fall back to raw `log` via the existing stream reader), exits 0 when the
//!   cluster succeeded. The cluster id IS the attempt id (deterministic), so
//!   cancel/export need no state passing, a retry (new attempt id) is a new
//!   cluster by construction (ADR 0002: retry = new cluster, no resume across
//!   the retry boundary), and `run` with an existing `--id` resumes the live
//!   cluster (resume *within* one attempt, e.g. after a node restart, works).
//! - `zeroshot kill <attempt-id>` — total kill of the cluster (ADR 0002
//!   cancel-total). Idempotent: a missing cluster exits 0. The node calls it
//!   after every cut-short run (cancel/timeout/resource-limit) and after any
//!   non-zero exit, bounded, best-effort.
//! - `zeroshot export <attempt-id> --dir <workdir>` — materialize the
//!   cluster's result files into the worktree (`zeroshot-result.json` plus
//!   whatever the executor produced) so the normal finalize (diff/commit)
//!   and artifact upload pick them up. Bounded, best-effort: export failure
//!   warns but never fails the attempt (the event stream already delivered
//!   the outcome).
//!
//! Role mapping (Stage 10): Agentgrid workflow roles collapse onto the two
//! Zeroshot sides — `architect`/`worker`/`integrator` build the change
//! (`executor`), `reviewer`/`verifier` check it (`verifier`, always paired
//! with the read-only worktree the scheduler already enforces for verifiers).
//! Direct (non-workflow) tasks omit `--role` and the executor default
//! applies.
//!
//! Security review (Stage 10, ADR 0002 credentials-local): the driver never
//! mounts host paths beyond the attempt worktree (the sandbox prefix the
//! wrapper path already applies) — no `~/.config/agentgrid`, no credential
//! files, no `-v/--mount/--volume` flags in the driver argv (asserted by
//! `run_argv_mounts_nothing_but_the_worktree`). The `run` process inherits
//! the same attempt env the wrapper path forwards (LLM keys the agent needs
//! to function — identical to the docker-sandbox container env); host
//! credential *files* never travel. A missing binary at attempt time is
//! `infrastructure_failed` with the `zeroshot_unavailable` detail (the error
//! name ADR 0014 reserves), never a silent fallback to another adapter.

use std::path::Path;

/// Env override for the executor binary (tests/E2E point it at the stub).
/// Default `zeroshot` — the same name `probe_cluster_adapter` resolves.
pub fn zeroshot_bin() -> String {
    std::env::var("AGENTGRID_ZEROSHOT_BIN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "zeroshot".to_string())
}

/// Resolve the executor binary on PATH (or a literal path). `None` → the
/// attempt fails `infrastructure_failed:zeroshot_unavailable`.
pub fn resolve_zeroshot_bin() -> Option<String> {
    let bin = zeroshot_bin();
    crate::capabilities::resolve_in_path(&bin).map(|_| bin)
}

/// Map an Agentgrid workflow role (snake_case, e.g. from
/// `Assignment::workflow_role`) onto the Zeroshot side. `None` (direct task
/// or unrecognized role) → `None`: the `--role` flag is omitted and the
/// executor default applies. Case-insensitive; unknown roles omit rather
/// than invent a side.
pub fn zeroshot_side(workflow_role: Option<&str>) -> Option<&'static str> {
    match workflow_role
        .map(|r| r.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("architect") | Some("worker") | Some("integrator") => Some("executor"),
        Some("reviewer") | Some("verifier") => Some("verifier"),
        _ => None,
    }
}

/// argv tail for `SpawnRequest.extra_args` (the backend already prefixes the
/// binary + sandbox head; `raw_args = true` so no `--prompt` is prepended).
/// Full command: `<bin> run --id <attempt> --workdir <ws> [--role <side>]
/// --prompt <prompt>`. Pure — unit-testable without a binary.
pub fn run_extra_args(
    attempt_id: &str,
    workdir: &Path,
    role_side: Option<&str>,
    prompt: &str,
) -> Vec<String> {
    let mut v = vec![
        "run".to_string(),
        "--id".to_string(),
        attempt_id.to_string(),
        "--workdir".to_string(),
        workdir.display().to_string(),
    ];
    if let Some(side) = role_side {
        v.push("--role".to_string());
        v.push(side.to_string());
    }
    v.push("--prompt".to_string());
    v.push(prompt.to_string());
    v
}

/// Payload for the `cluster_created` status event pushed when the run starts
/// (cluster id = attempt id by contract).
pub fn cluster_created_payload(attempt_id: &str, role_side: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "kind": "cluster_created",
        "cluster_id": attempt_id,
        "executor": "zeroshot",
        "role_side": role_side,
    })
}

/// ADR 0002 cancel-total: kill the attempt's cluster after a cut-short or
/// failed run. Bounded (15s), best-effort, idempotent — any outcome only
/// warns; the attempt's terminal state is already decided by the run exit.
pub async fn kill_cluster(bin: &str, attempt_id: &str) {
    let res = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(bin)
            .arg("kill")
            .arg(attempt_id)
            .env_clear()
            .output(),
    )
    .await;
    match res {
        Ok(Ok(o)) if o.status.success() => {
            tracing::info!(attempt_id, "zeroshot cluster killed");
        }
        Ok(Ok(o)) => {
            tracing::warn!(
                attempt_id,
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "zeroshot kill failed (best-effort)"
            );
        }
        Ok(Err(e)) => {
            tracing::warn!(attempt_id, error = %e, "zeroshot kill spawn failed (best-effort)");
        }
        Err(_) => {
            tracing::warn!(attempt_id, "zeroshot kill timed out (best-effort)");
        }
    }
}

/// Materialize the cluster's result files into the worktree before finalize.
/// Bounded (60s), best-effort — export failure warns but never fails the
/// attempt (events already delivered the outcome; finalize still diffs
/// whatever the worktree holds).
pub async fn export_results(bin: &str, attempt_id: &str, workdir: &Path) {
    let res = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new(bin)
            .arg("export")
            .arg(attempt_id)
            .arg("--dir")
            .arg(workdir)
            .env_clear()
            .output(),
    )
    .await;
    match res {
        Ok(Ok(o)) if o.status.success() => {
            tracing::info!(attempt_id, "zeroshot results exported");
        }
        Ok(Ok(o)) => {
            tracing::warn!(
                attempt_id,
                stderr = %String::from_utf8_lossy(&o.stderr).trim(),
                "zeroshot export failed (best-effort)"
            );
        }
        Ok(Err(e)) => {
            tracing::warn!(attempt_id, error = %e, "zeroshot export spawn failed (best-effort)");
        }
        Err(_) => {
            tracing::warn!(attempt_id, "zeroshot export timed out (best-effort)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_side_maps_executor_and_verifier_sides() {
        for r in ["architect", "worker", "integrator", "ARCHITECT", " Worker "] {
            assert_eq!(zeroshot_side(Some(r)), Some("executor"), "{r}");
        }
        for r in ["reviewer", "verifier", "VERIFIER"] {
            assert_eq!(zeroshot_side(Some(r)), Some("verifier"), "{r}");
        }
        assert_eq!(zeroshot_side(None), None);
        assert_eq!(zeroshot_side(Some("orchestrator")), None);
        assert_eq!(zeroshot_side(Some("")), None);
    }

    #[test]
    fn run_argv_shape_and_role_flag() {
        let a = run_extra_args("att1", Path::new("/w"), Some("verifier"), "do it");
        assert_eq!(
            a,
            vec![
                "run",
                "--id",
                "att1",
                "--workdir",
                "/w",
                "--role",
                "verifier",
                "--prompt",
                "do it"
            ]
        );
        // Direct task: no --role flag at all.
        let b = run_extra_args("att2", Path::new("/w"), None, "do it");
        assert!(!b.iter().any(|x| x == "--role"));
    }

    #[test]
    fn run_argv_mounts_nothing_but_the_worktree() {
        // Security review (Stage 10): the driver must never smuggle host
        // mounts into the executor — only --workdir travels (the sandbox
        // prefix owns the actual bind). A future flag addition that mounts
        // host paths trips this test.
        let a = run_extra_args("att1", Path::new("/w"), Some("executor"), "p");
        for flag in ["-v", "--volume", "--mount", "--privileged"] {
            assert!(
                !a.iter().any(|x| x == flag),
                "driver argv must not contain {flag}: {a:?}"
            );
        }
        assert_eq!(a.iter().filter(|x| x.as_str() == "/w").count(), 1);
    }

    #[test]
    fn cluster_created_payload_carries_executor_and_side() {
        let p = cluster_created_payload("att9", Some("executor"));
        assert_eq!(p["kind"], "cluster_created");
        assert_eq!(p["cluster_id"], "att9");
        assert_eq!(p["executor"], "zeroshot");
        assert_eq!(p["role_side"], "executor");
    }
}
