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
            Some(v) if v.starts_with(required_prefix) => (true, None),
            Some(v) => (
                false,
                Some(format!(
                    "executor {v} does not match pinned prefix {required_prefix}"
                )),
            ),
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
}
