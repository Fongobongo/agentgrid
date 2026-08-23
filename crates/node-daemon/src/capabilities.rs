//! Adapter capability discovery: binary resolution, version probing.

use std::path::PathBuf;

use agentgrid_common::cluster::probe_decision;

/// Internal probe result for adapter discovery.
pub struct AdapterProbe {
    pub found: bool,
    pub version: Option<String>,
}

/// Resolve `bin` to an executable file on `PATH` (or a literal path if it
/// contains `/`). No shell is involved, so a crafted adapter id cannot inject
/// commands. Adapter ids come from operator config, not tasks.
pub fn resolve_in_path(bin: &str) -> Option<PathBuf> {
    if bin.contains('/') {
        return if PathBuf::from(bin).is_file() {
            Some(PathBuf::from(bin))
        } else {
            None
        };
    }
    let path = std::env::var("PATH").ok()?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(bin);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Map an adapter id (e.g. `mock`, `claude`) to its binary name (`adapter-mock`).
pub fn adapter_bin_name(adapter_id: &str) -> String {
    format!("adapter-{}", adapter_id.replace('_', "-"))
}

/// Resolve the adapter binary for `adapter_id` on PATH. Returns None when the
/// node cannot run that adapter, so the attempt can be failed as
/// `infrastructure_failed`.
pub fn resolve_adapter_bin(adapter_id: &str) -> Option<String> {
    let bin = adapter_bin_name(adapter_id);
    resolve_in_path(&bin).map(|_| bin)
}

/// Native ACP launcher: if `AGENTGRID_ACP_LAUNCH_<ID>` is set, the node runs
/// that command directly (e.g. `claude --acp`, `codex --acp`) over stdio
/// instead of spawning a `adapter-<id>` wrapper binary.
/// Returns `(program, args)`. The value is split on whitespace; operator config
/// (not task input), so quoting is the operator's responsibility.
// ponytail: naive whitespace split; use shlex if args need spaces.
pub fn resolve_acp_launch(adapter_id: &str) -> Option<(String, Vec<String>)> {
    let key = format!(
        "AGENTGRID_ACP_LAUNCH_{}",
        adapter_id
            .to_ascii_uppercase()
            .replace(|c: char| !c.is_alphanumeric(), "_")
    );
    let val = std::env::var(&key).ok()?;
    let mut parts = val.split_whitespace();
    let program = parts.next()?.to_string();
    let args = parts.map(|s| s.to_string()).collect();
    Some((program, args))
}

/// Stage 3.1 capability discovery: resolve the adapter binary in `PATH` and
/// capture its `--version` (best-effort). A missing binary means the node
/// should report `degraded` so the scheduler excludes it.
pub async fn probe_adapter(bin: &str) -> AdapterProbe {
    let Some(path) = resolve_in_path(bin) else {
        return AdapterProbe {
            found: false,
            version: None,
        };
    };
    // Try to get version via `--version` flag.
    // Audit X-B10: bounded probe — a wedged binary used to hang the
    // sequential heartbeat loop and sweep the node offline.
    let output =
        match tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::process::Command::new(&path)
                .arg("--version")
                .output()
                .await
        })
        .await
        {
            Ok(o) => o.ok(),
            Err(_) => {
                tracing::warn!(bin = %bin, "adapter --version probe timed out");
                None
            }
        };
    let version = output
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    AdapterProbe {
        found: true,
        version,
    }
}

/// Stage 10 / line 333: capability probe for a cluster executor adapter
/// (`zeroshot`). Unlike the simple wrapper-binary probe, this checks the
/// executor + runtime binary pair. Returns version from executor if available.
pub async fn probe_cluster_adapter(executor_bin: &str, runtime_bin: &str) -> AdapterProbe {
    let runtime_present = resolve_in_path(runtime_bin).is_some();
    let executor_present = resolve_in_path(executor_bin).is_some();
    let executor_version = if executor_present {
        tokio::process::Command::new(executor_bin)
            .arg("--version")
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string()
            })
    } else {
        None
    };
    let required_prefix =
        std::env::var("AGENTGRID_ZEROSHOT_VERSION").unwrap_or_else(|_| "0.".into());
    let p = probe_decision(
        runtime_present,
        executor_version.as_deref(),
        &required_prefix,
        executor_present,
    );
    AdapterProbe {
        found: p.available,
        version: p.version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_bin_name_maps_id() {
        assert_eq!(adapter_bin_name("mock"), "adapter-mock");
        assert_eq!(adapter_bin_name("claude"), "adapter-claude");
        assert_eq!(adapter_bin_name("my_adapter"), "adapter-my-adapter");
    }

    #[tokio::test]
    async fn probe_adapter_finds_real_binary_and_reports_missing() {
        let good = probe_adapter("sh").await;
        assert!(good.found);
        // version is optional - some binaries don't support --version or return non-standard output

        let bad = probe_adapter("definitely-not-an-agentgrid-adapter-xyz").await;
        assert!(!bad.found);
        assert!(bad.version.is_none());
    }

    #[tokio::test]
    async fn probe_cluster_adapter_reports_missing() {
        let p = probe_cluster_adapter("sh", "definitely-no-such-runtime-xyz").await;
        assert!(!p.found);
        assert!(p.version.is_none());

        let p = probe_cluster_adapter("definitely-no-such-executor-xyz", "sh").await;
        assert!(!p.found);
        assert!(p.version.is_none());

        // When both binaries are present, the probe succeeds only if the executor
        // version matches the required prefix (default "0."). Since `sh --version`
        // may not match, we don't assert on found=true here.
        let _ = probe_cluster_adapter("sh", "sh").await;
    }

    #[tokio::test]
    async fn cluster_probe_fail_closed_on_version_mismatch() {
        // Both binaries present (sh as a stand-in), but pin against a version
        // prefix that `sh --version` will not match → not found.
        // ponytail: env override scoped via a thread-local guard isn't trivial
        // here; instead pin to a deliberately unreachable prefix and assert
        // the probe falls through to unavailable.
        std::env::set_var("AGENTGRID_ZEROSHOT_VERSION", "zz-no-such-major");
        let p = probe_cluster_adapter("sh", "sh").await;
        std::env::remove_var("AGENTGRID_ZEROSHOT_VERSION");
        // Either the runtime is missing, or the version does not match in
        // either case `found` must be false (fail-closed).
        assert!(
            !p.found,
            "version mismatch / runtime probe must fail closed"
        );
    }

    #[test]
    fn resolve_adapter_bin_rejects_missing() {
        assert!(resolve_adapter_bin("definitely-not-an-adapter-xyz").is_none());
    }
}
