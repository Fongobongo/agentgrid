//! Config-class error counter driving the error-threshold profile pull
//! (feature "opencode profiles", trigger "error_threshold").
//!
//! The design goal is self-healing opencode config: a model the node
//! doesn't know about (typo in `model`, provider outage, missing whitelist
//! entry after a pin rotation) shows up on every attempt until a human
//! notices. The counter thresholds N consecutive failures classified as
//! "config error", pulls `/v1/node/opencode-config/active`, and applies the
//! fresh profile if the hash changed.

use std::sync::atomic::{AtomicU64, Ordering};

/// Default threshold for the error trigger. Tunable for tests / ops.
const DEFAULT_THRESHOLD: u64 = 3;

fn threshold() -> u64 {
    std::env::var("AGENTGRID_CONFIG_PULL_AFTER_ERRORS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_THRESHOLD)
}

static CONSECUTIVE_CONFIG_ERRS: AtomicU64 = AtomicU64::new(0);

/// substring markers that signal a config/infra error rather than a model
/// complaint the LLM can self-heal around. Case-insensitive; the patterns
/// intentionally include both common HTTP status words and the error
/// envelopes surfaced by our adapters.
const CONFIG_ERR_PATTERNS: &[&str] = &[
    "model_not_found",
    "invalid model",
    "unknown model",
    "404 model",
    "401",
    "unauthorized",
    "forbidden",
    "rate_limit_exceeded",
    "insufficient_quota",
    "permission denied",
    "no such file or directory",
    "command not found",
    "node is degraded",
    "config parse error",
    "bad request",
];

/// Classify an adapter event as a config error. Matches on the raw payload
/// string; unknown events return false and do not move the counter.
pub fn is_config_error(payload: &serde_json::Value) -> bool {
    let hay = serde_json::to_string(payload)
        .unwrap_or_default()
        .to_lowercase();
    CONFIG_ERR_PATTERNS.iter().any(|p| hay.contains(p))
}

/// Note an adapter error during an attempt. Returns `true` when the
/// threshold is crossed and the caller should pull the fresh profile.
pub fn note_config_error(payload: &serde_json::Value) -> bool {
    if !is_config_error(payload) {
        CONSECUTIVE_CONFIG_ERRS.store(0, Ordering::Relaxed);
        return false;
    }
    let n = CONSECUTIVE_CONFIG_ERRS.fetch_add(1, Ordering::Relaxed) + 1;
    if n >= threshold() {
        // Reset before returning so a second task doesn't immediately
        // re-trigger. The pull is "one per N errors", not "every error
        // past the threshold".
        CONSECUTIVE_CONFIG_ERRS.store(0, Ordering::Relaxed);
        return true;
    }
    false
}

/// A successful attempt resets the consecutive-error streak.
pub fn note_attempt_succeeded() {
    CONSECUTIVE_CONFIG_ERRS.store(0, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_trips_after_n_consecutive_config_errors() {
        CONSECUTIVE_CONFIG_ERRS.store(0, Ordering::Relaxed);
        std::env::set_var("AGENTGRID_CONFIG_PULL_AFTER_ERRORS", "2");
        let bad = serde_json::json!({"error": "model_not_found something"});
        let ok = serde_json::json!({"text": "hello"});
        assert!(!super::note_config_error(&bad));
        assert!(super::note_config_error(&bad));
        // After reset, same-streak of non-config errors does nothing.
        assert!(!super::note_config_error(&ok));
        std::env::remove_var("AGENTGRID_CONFIG_PULL_AFTER_ERRORS");
    }

    #[test]
    fn classification_catches_common_markers() {
        for (payload, want) in [
            (serde_json::json!({"error":"model_not_found"}), true),
            (serde_json::json!({"text":"401 Unauthorized"}), true),
            (serde_json::json!({"error":"ok"}), false),
        ] {
            assert_eq!(super::is_config_error(&payload), want, "payload: {payload}");
        }
    }
}
