//! Hardening P2 item 16: configuration & environment-derived types, extracted
//! from `lib.rs` so the router file stays focused on HTTP wiring.

/// Request size ceilings (Stage 5.1). Overridable via env; defaults:
/// prompt 64 KiB, event payload 1 MiB, artifact 50 MiB.
pub(crate) struct Limits {
    pub(crate) prompt: usize,
    pub(crate) event: usize,
    pub(crate) artifact: usize,
    /// Hardening P1 (event ingestion): cap events per batch and the total
    /// batch payload size, so a node cannot flood the control plane with one
    /// giant request or O(events) inserts in a single transaction.
    pub(crate) event_batch_count: usize,
    pub(crate) event_batch_bytes: usize,
}

/// One-time bootstrap setup token (hardening P0). Printed to stdout once on
/// first start; must be presented to `POST /v1/auth/setup` to create the
/// first user; consumed on first use; expires after `SETUP_TOKEN_TTL`.
pub(crate) struct SetupToken {
    pub(crate) token: String,
    pub(crate) issued_at: std::time::Instant,
}

pub(crate) const SETUP_TOKEN_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

impl SetupToken {
    pub(crate) fn new() -> Self {
        use rand::Rng;
        // 32 hex chars from a random u128; sufficient for a short-lived,
        // one-time bootstrap token printed to stdout.
        let token = format!("{:032x}", rand::thread_rng().gen::<u128>());
        Self {
            token,
            issued_at: std::time::Instant::now(),
        }
    }

    /// True if the token has not expired.
    pub(crate) fn is_live(&self) -> bool {
        self.issued_at.elapsed() < SETUP_TOKEN_TTL
    }
}

/// Sliding-window brute-force limiter for the login endpoint (Stage 2.5).
/// Keyed per account (lowercased username): a generic 429 (not a per-user
/// signal) is returned when the budget is spent, so it cannot be used to
/// enumerate which usernames exist. Per-account keying means one attacked
/// account cannot exhaust the budget for everyone (a global key let a single
/// attacker lock out all users).
pub(crate) struct LoginRate {
    per_key: std::collections::HashMap<String, (i64, u32)>,
    max: u32,
    window_secs: i64,
}
impl LoginRate {
    pub(crate) fn new() -> Self {
        Self {
            per_key: std::collections::HashMap::new(),
            max: 10,
            window_secs: 60,
        }
    }
    /// Record an attempt for `key`; returns false once its per-window budget
    /// is spent. Stale keys are pruned opportunistically.
    pub(crate) fn check_and_record(&mut self, key: &str, now: i64) -> bool {
        if self.per_key.len() > 1024 {
            let window = self.window_secs;
            self.per_key.retain(|_, (start, _)| now - *start < window);
        }
        let entry = self.per_key.entry(key.to_string()).or_insert((0, 0));
        if now - entry.0 >= self.window_secs {
            entry.0 = now;
            entry.1 = 0;
        }
        entry.1 += 1;
        entry.1 <= self.max
    }
}

/// Hardening P1 item 14: per-node event-ingest rate limiter. Each node has its
/// own fixed window counter, pruned lazily when next touched past the window.
/// Defaults are tuned via `AGENTGRID_EVENT_RATE_MAX` (req / window) and
/// `AGENTGRID_EVENT_RATE_WINDOW_SECS`.
pub(crate) struct EventRate {
    per_node: std::collections::HashMap<String, (i64, u32)>,
    max: u32,
    window_secs: i64,
}

impl EventRate {
    pub(crate) fn new() -> Self {
        Self {
            per_node: std::collections::HashMap::new(),
            max: env_usize("AGENTGRID_EVENT_RATE_MAX", 60) as u32,
            window_secs: env_usize("AGENTGRID_EVENT_RATE_WINDOW_SECS", 10) as i64,
        }
    }

    /// `true` if this request is under the per-node budget; the first request of
    /// a new window resets the counter.
    pub(crate) fn admit(&mut self, node_id: &str, now: i64) -> bool {
        let entry = self.per_node.entry(node_id.to_string()).or_insert((now, 0));
        if now - entry.0 >= self.window_secs {
            entry.0 = now;
            entry.1 = 0;
        }
        entry.1 += 1;
        entry.1 <= self.max
    }
}

pub(crate) fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
