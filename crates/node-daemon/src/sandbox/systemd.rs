//! systemd transient-scope backend and cgroup inspection.

use super::*;

pub(super) fn systemd_run_head(
    unit: &str,
    limits: Option<&agentgrid_adapters::ResourceLimits>,
) -> Vec<String> {
    let mut v = vec![
        "run".to_string(),
        "--user".to_string(),
        "--scope".to_string(),
        "--unit".to_string(),
        unit.to_string(),
    ];
    let limits = limits.cloned().unwrap_or_default();
    let mem = limits
        .memory_max
        .map(|b| format!("{}M", (b / (1024 * 1024)).max(1)))
        .or_else(|| env_nonempty("AGENTGRID_SANDBOX_MEMORY"));
    if let Some(m) = mem {
        v.push(format!("--property=MemoryMax={m}"));
    }
    let quota = limits
        .cpu_quota_percent
        .map(|c| format!("{c}%"))
        .or_else(|| {
            env_nonempty("AGENTGRID_SANDBOX_CPUS").map(|c| {
                // Reuse the same "% = cores×100" convention as the docker path
                // when the operator pinned cores (e.g. "1.5" → 150%).
                c.parse::<f64>()
                    .ok()
                    .map(|cores| format!("{}%", (cores * 100.0).round() as u64))
                    .unwrap_or(c)
            })
        });
    if let Some(q) = quota {
        v.push(format!("--property=CPUQuota={q}"));
    }
    let tasks = limits
        .tasks_max
        .map(|n| n.to_string())
        .or_else(|| env_nonempty("AGENTGRID_SANDBOX_PIDS_LIMIT"));
    if let Some(t) = tasks {
        v.push(format!("--property=TasksMax={t}"));
    }
    v
}

/// Plan 6.11: kill every process in the attempt's transient scope
/// (`systemctl --user kill <unit>`), then stop the unit so a dead scope
/// does not linger in the user manager. Best-effort — the process-group
/// SIGTERM/SIGKILL from `terminate_group` still applies to the direct
/// child, and the startup sweep (`cleanup_orphan_scopes`) is the backstop.
pub async fn kill_scope_unit(attempt_id: &str) {
    let sandbox = std::env::var("AGENTGRID_SANDBOX").unwrap_or_default();
    if !matches!(
        sandbox.trim().to_ascii_lowercase().as_str(),
        "systemd" | "systemd-scope" | "cgroups"
    ) {
        return;
    }
    let unit = scope_unit(attempt_id);
    for args in [["kill", &unit], ["stop", &unit]] {
        match tokio::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .await
        {
            Ok(o) if o.status.success() => {}
            Ok(o) => tracing::warn!(
                attempt_id,
                "systemctl --user {} {} failed: {}",
                args[0],
                unit,
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => tracing::warn!(attempt_id, "systemctl --user spawn failed: {e}"),
        }
    }
}

/// Plan 6.11: after a systemd-scope attempt exits, ask the user manager
/// whether the kernel OOM-killed the scope's cgroup (memory.max breach).
/// `systemctl --user show -p OOMKill --value <unit>` answers
/// `1`/`0`; a failed/absent answer yields None (caller keeps the plain
/// exit-code classification). Upgrades exit-137 into the first-class
/// `resource_limit:memory` error code, mirroring
/// [`inspect_container_oom`] for the docker path.
pub async fn inspect_scope_oom(attempt_id: &str) -> Option<String> {
    let sandbox = std::env::var("AGENTGRID_SANDBOX").unwrap_or_default();
    if !matches!(
        sandbox.trim().to_ascii_lowercase().as_str(),
        "systemd" | "systemd-scope" | "cgroups"
    ) {
        return None;
    }
    let unit = scope_unit(attempt_id);
    let out = tokio::process::Command::new("systemctl")
        .args(["--user", "show", "-p", "OOMKill", "--value", &unit])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if stdout == "1" {
        let limit = std::env::var("AGENTGRID_SANDBOX_MEMORY").unwrap_or_default();
        Some(if limit.is_empty() {
            "memory".to_string()
        } else {
            format!("memory (limit {limit})")
        })
    } else {
        None
    }
}

/// Plan 6.11: sweep this daemon's leftover scopes at startup — a SIGKILLed
/// daemon can leave `agentgrid-scope-*` units registered in the user
/// manager. Lists failed/running units matching the prefix and stops them
/// (best-effort, like `cleanup_orphan_containers`).
pub async fn cleanup_orphan_scopes() {
    let out = match tokio::process::Command::new("systemctl")
        .args(["--user", "list-units", "--all", "--plain", "--no-legend"])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            tracing::warn!(
                "orphan-scope scan failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return;
        }
        Err(e) => {
            tracing::warn!(error = %e, "orphan-scope scan spawn failed");
            return;
        }
    };
    let stale: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let name = l.split_whitespace().next()?;
            (name.starts_with("agentgrid-scope-")).then(|| name.to_string())
        })
        .collect();
    if stale.is_empty() {
        return;
    }
    let mut cmd = tokio::process::Command::new("systemctl");
    cmd.arg("--user").arg("stop").args(&stale);
    match cmd.output().await {
        Ok(o) if o.status.success() => {
            tracing::info!(count = stale.len(), "removed orphan agentgrid scopes");
        }
        Ok(o) => tracing::warn!(
            "orphan-scope removal failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => tracing::warn!(error = %e, "orphan-scope removal spawn failed"),
    }
}

/// Stage 12 / ADR 0003: after a sandboxed attempt exits, ask the runtime
/// whether the container was OOM-killed (`docker inspect -f {{.State.OOMKilled}}`).
/// Upgrades a bare exit-137 into the first-class `resource_limit` terminal
/// outcome so the CP can treat OOM differently from an agent crash (e.g. never
/// auto-retry). `None` when not sandboxed, the container is already gone
/// (raced removal / non-docker runtime quirk), or the answer is inconclusive —
/// the caller keeps the plain exit-code classification then.
/// Plan 6.11: a systemd-scope attempt routes to [`inspect_scope_oom`].
pub async fn probe_systemd_scope() -> Option<String> {
    // (a) binary present.
    resolve_binary("systemd-run")?;
    // (b) a user manager answers: `systemctl --user is-system-running`
    // succeeds (any "running/degraded" verdict) only with DBus + the user
    // manager up. This is exactly what `systemd-run --user` needs.
    let out = tokio::process::Command::new("systemctl")
        .args(["--user", "is-system-running"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // (c) cgroups v2 (unified hierarchy) mounted — the properties
    // MemoryMax/CPUQuota/TasksMax are v2 knobs.
    if !std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists() {
        return None;
    }
    let v = tokio::process::Command::new("systemctl")
        .arg("--version")
        .output()
        .await
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| {
            s.lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1).map(|v| v.to_string()))
        })
        .filter(|s| !s.is_empty());
    Some(v.unwrap_or_else(|| "unknown".to_string()))
}
