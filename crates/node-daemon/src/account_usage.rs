//! Plan 1.8 (#15): per-account usage counters kept on the node and reported
//! in every heartbeat (`account_usage` field) so the control plane can expose
//! `GET /v1/nodes/{id}/accounts/usage`. Rotation bumps `rate_limited`; each
//! attempt bumps `attempts` for the env the account backs.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use agentgrid_common::AccountUsage;

/// One counter row, keyed by credential env var (e.g. `ANTHROPIC_API_KEY`).
#[derive(Default)]
struct Counter {
    attempts: AtomicU64,
    rate_limited: AtomicU64,
}

static USAGE: std::sync::OnceLock<Mutex<HashMap<String, Counter>>> = std::sync::OnceLock::new();

fn usage() -> &'static Mutex<HashMap<String, Counter>> {
    USAGE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record that an attempt ran using the account backing `env`.
pub fn note_attempt(env: &str) {
    usage()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(env.to_string())
        .or_default()
        .attempts
        .fetch_add(1, Ordering::Relaxed);
}

/// Record a 429 rotation on the account backing `env`.
pub fn note_rate_limited(env: &str) {
    usage()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(env.to_string())
        .or_default()
        .rate_limited
        .fetch_add(1, Ordering::Relaxed);
}

/// Snapshot for a heartbeat; token_index is unknown here (the node does not
/// track it per heartbeat), so it is reported as 0 — the control plane only
/// needs attempts + rate_limited to gauge rotation health.
pub fn snapshot() -> Vec<AccountUsage> {
    let map = usage().lock().unwrap_or_else(|p| p.into_inner());
    let mut out: Vec<AccountUsage> = map
        .iter()
        .map(|(env, c)| AccountUsage {
            env: env.clone(),
            token_index: 0,
            attempts: c.attempts.load(Ordering::Relaxed),
            rate_limited: c.rate_limited.load(Ordering::Relaxed),
        })
        .collect();
    out.sort_by(|a, b| a.env.cmp(&b.env));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_accumulate_and_snapshot_sorts() {
        note_attempt("ANTHROPIC_API_KEY");
        note_attempt("ANTHROPIC_API_KEY");
        note_rate_limited("ANTHROPIC_API_KEY");
        note_attempt("OPENAI_API_KEY");
        let snap = snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].env, "ANTHROPIC_API_KEY");
        assert_eq!(snap[0].attempts, 2);
        assert_eq!(snap[0].rate_limited, 1);
        assert_eq!(snap[1].env, "OPENAI_API_KEY");
        assert_eq!(snap[1].attempts, 1);
    }
}
