//! Stage 10 (Zeroshot): a *cluster executor* runs an Agentgrid attempt's work
//! inside an external verified-loop cluster (Zeroshot: an agent+verifier team
//! in containers), instead of as a single subprocess. ADR 0002 fixes the
//! ownership invariant: **1 Agentgrid attempt = 1 cluster**, 1:1 — cancel kills
//! the whole cluster, a daemon kill reclaims orphans, retry = new cluster
//! (resume does not cross this boundary).
//!
//! This module is the contract only. The first concrete impl is the Zeroshot
//! adapter (a later spike that shells out to the Zeroshot CLI); `ProbedExecutor`
//! below is a capability probe result a node uses to decide whether it can even
//! serve the `zeroshot` adapter before claiming a task.

use serde::{Deserialize, Serialize};

/// Probe result for a cluster executor at runtime: is the backing runtime
/// (Docker/Podman) present, is the executor binary present, is its version
/// pinned? Not available → the node does **not** claim a `zeroshot` task
/// (capability honesty, same discipline as the wrapper-adapter boundary in
/// Stage 9.1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbedExecutor {
    pub id: String,
    pub available: bool,
    /// Why unavailable, if not (e.g. `docker not found`, `zeroshot < 0.4`).
    pub reason: Option<String>,
    /// Reported version of the executor binary (best-effort).
    pub version: Option<String>,
    /// Reported version of the container runtime.
    pub runtime_version: Option<String>,
}

/// A cluster executor's lifecycle contract. The implementation owns the mapping
/// from Zeroshot cluster events → `AgentEventEnvelope` lines the daemon streams;
/// the daemon itself never speaks Zeroshot directly.
///
/// Lifecycle (matches the daemon's existing attempt lifecycle):
/// - `create` — bring up a cluster for this attempt; record `cluster_id`.
/// - `stream` — emit progress/result events into the provided sink (NDJSON
///   envelope).
/// - `kill` — total cluster teardown (cancel / lost-node reclaim).
///
/// `ponytail:` the trait returns an `enum` of named steps rather than a trait
/// object with async methods, because the concrete Zeroshot impl shells out to
/// a CLI and the daemon already has a subprocess-launch discipline; modeling
/// steps as data keeps the trait object-free and unit-testable.
pub enum ClusterStep {
    /// The executor could not start a cluster for the attempt.
    Failed { error_code: String, reason: String },
    /// A cluster was created; `cluster_id` is the handle to kill later.
    Created { cluster_id: String },
}

/// Cluster-handle returned by a successful create. The daemon stores it for
/// the cancel/reclaim path; `cluster_id` piggybacks on `session_id` in the
/// event envelope (ADR 0002 §5).
#[derive(Debug, Clone)]
pub struct ClusterHandle {
    pub executor_id: String,
    pub cluster_id: String,
}

/// A unit-testable capability probe: is this machine able to run the named
/// cluster executor? Resolves `available` from the presence of the runtime +
/// the pinned binary. The real probe shells out; the helper below is the pure
/// combine the probe uses, so it can be tested without Docker.
///
/// Plan 6.9: `required` accepts a semver requirement (`^0.5`, `>=0.5.0`,
/// `0.5.*`, … — see [`crate::version`]) and falls back to the legacy
/// prefix match, so pins like `"0."` keep working unchanged.
pub fn probe_decision(
    runtime_present: bool,
    executor_version: Option<&str>,
    required_prefix: &str,
    executor_present: bool,
) -> ProbedExecutor {
    let (available, reason) = match (!runtime_present, !executor_present) {
        (true, _) => (false, Some("container runtime not found".into())),
        (false, true) => (false, Some("executor binary not found".into())),
        (false, false) => match executor_version {
            // `--version` output is free-form: real binaries print either a
            // bare version ("0.1.0") or a name-prefixed one ("zeroshot
            // 0.1.0-stub", like every adapter-mock "adapter-mock x.y.z").
            // Match against the first whitespace-separated token that looks
            // like a version (starts with an ASCII digit); fall back to the
            // whole trimmed line so garbage still fails closed below.
            Some(v) => {
                let bare = v
                    .split_whitespace()
                    .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
                    .unwrap_or(v.trim());
                if crate::version::version_matches_req(bare, required_prefix)
                    || bare.starts_with(required_prefix)
                {
                    (true, None)
                } else {
                    (
                        false,
                        Some(format!(
                            "executor {v} does not match pinned prefix {required_prefix}"
                        )),
                    )
                }
            }
            None => (false, Some("executor version unknown".into())),
        },
    };
    ProbedExecutor {
        id: "zeroshot".into(),
        available,
        reason,
        version: executor_version.map(str::to_string),
        runtime_version: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_blocks_missing_runtime() {
        let p = probe_decision(false, Some("0.5.0"), "0.5", true);
        assert!(!p.available);
        assert!(p.reason.as_deref().unwrap().contains("runtime"));
    }

    #[test]
    fn probe_blocks_missing_binary() {
        let p = probe_decision(true, Some("0.5.0"), "0.5", false);
        assert!(!p.available);
        assert!(p.reason.as_deref().unwrap().contains("binary"));
    }

    #[test]
    fn probe_pins_version_prefix() {
        assert!(probe_decision(true, Some("0.5.1"), "0.5", true).available);
        // Wrong major → not available (pinned prefix mismatch).
        let p = probe_decision(true, Some("0.4.0"), "0.5", true);
        assert!(!p.available);
        assert!(p.reason.as_deref().unwrap().contains("pinned prefix"));
    }

    #[test]
    fn probe_blocks_unknown_version() {
        let p = probe_decision(true, None, "0.5", true);
        assert!(!p.available);
        assert!(p.reason.as_deref().unwrap().contains("unknown"));
    }

    /// Plan 6.9: a semver requirement is honored where a prefix would
    /// over- or under-match (`^0.5` admits `0.5.9` but not `0.6.0`;
    /// `>=0.5.0 <0.6.0` is exact); the legacy prefix stays as fallback
    /// (`"0."` still pins the 0.x line).
    #[test]
    fn probe_honors_semver_requirement_with_prefix_fallback() {
        assert!(probe_decision(true, Some("0.5.9"), "^0.5", true).available);
        assert!(!probe_decision(true, Some("0.6.0"), "^0.5", true).available);
        assert!(probe_decision(true, Some("1.2.3"), ">=1.0.0 <2.0.0", true).available);
        assert!(!probe_decision(true, Some("2.0.0"), ">=1.0.0 <2.0.0", true).available);
        // Legacy pins keep working through the prefix fallback.
        assert!(probe_decision(true, Some("0.9.1"), "0.", true).available);
        assert!(!probe_decision(true, Some("1.0.0"), "0.", true).available);
    }

    /// Stage 10 E2E: `--version` output is free-form — the zeroshot stub
    /// prints "zeroshot 0.1.0-stub" (name-prefixed, like adapter-mock).
    /// The pin must match the version token, not string position 0, while
    /// garbage without any version-like token still fails closed.
    #[test]
    fn probe_accepts_name_prefixed_version() {
        assert!(probe_decision(true, Some("zeroshot 0.1.0-stub"), "0.", true).available);
        assert!(probe_decision(true, Some("adapter-mock 0.5.0"), "0.", true).available);
        assert!(probe_decision(true, Some("0.1.0"), "0.", true).available);
        assert!(!probe_decision(true, Some("zeroshot 1.0.0"), "0.", true).available);
        assert!(!probe_decision(true, Some("no version here"), "0.", true).available);
        assert!(!probe_decision(true, Some(""), "0.", true).available);
        // Requirement form works on the extracted token too.
        assert!(probe_decision(true, Some("zeroshot 0.5.9"), "^0.5", true).available);
        assert!(!probe_decision(true, Some("zeroshot 0.6.0"), "^0.5", true).available);
    }
}
