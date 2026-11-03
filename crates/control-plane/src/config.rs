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
    /// Hardening P1 item 15: artifact storage quota in bytes (0 = unlimited).
    /// Read once from `AGENTGRID_ARTIFACT_QUOTA_MB` at startup; atomic so
    /// tests can override it on a shared `AppState`.
    pub(crate) artifact_quota_bytes: std::sync::atomic::AtomicU64,
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
        use rand::RngExt;
        // 32 hex chars from a random u128; sufficient for a short-lived,
        // one-time bootstrap token printed to stdout.
        let token = format!("{:032x}", rand::rng().random::<u128>());
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

/// One fixed-window counter map with a hard entry cap.
///
/// The cap matters as much as the counter: keys are attacker-supplied
/// (usernames, source addresses), so an unbounded map is a memory-exhaustion
/// primitive inside the one endpoint that is reachable without a credential.
struct WindowMap {
    entries: std::collections::HashMap<String, (i64, u32)>,
    cap: usize,
}

impl WindowMap {
    fn new(cap: usize) -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            cap,
        }
    }

    /// Record one event for `key`; false once its budget for the window is
    /// spent.
    fn check_and_record(&mut self, key: &str, now: i64, window_secs: i64, max: u32) -> bool {
        if !self.entries.contains_key(key) && self.entries.len() >= self.cap {
            self.entries
                .retain(|_, (start, _)| now - *start < window_secs);
            // Still full of live keys: evict the one closest to expiry. A
            // flood of distinct keys must not grow the map without bound, and
            // dropping the oldest window costs an attacker the least.
            if self.entries.len() >= self.cap {
                if let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, (start, _))| *start)
                    .map(|(k, _)| k.clone())
                {
                    self.entries.remove(&oldest);
                }
            }
        }
        let entry = self.entries.entry(key.to_string()).or_insert((0, 0));
        if now - entry.0 >= window_secs {
            entry.0 = now;
            entry.1 = 0;
        }
        entry.1 += 1;
        entry.1 <= max
    }

    #[cfg(test)]
    fn is_exhausted(&self, key: &str, now: i64, window_secs: i64, max: u32) -> bool {
        match self.entries.get(key) {
            Some((start, count)) => now - *start < window_secs && *count >= max,
            None => false,
        }
    }

    fn reset(&mut self, key: &str) {
        self.entries.remove(key);
    }
}

/// Brute-force limiter for the login endpoint.
///
/// Two buckets, because one is not enough (finding 12):
///
/// * **Per source address** — a hard gate in front of Argon2. It is what stops
///   password spraying across many usernames, and it caps the CPU an
///   unauthenticated caller can make the control plane burn.
/// * **Per account, failures only** — a soft gate applied *after* the password
///   has been checked, and cleared by a successful login. Counting failures
///   rather than attempts is what removes the lockout: an attacker can spend
///   the budget, but the real user's correct password still logs in, and the
///   wrong-password answer degrades to a generic 429 instead of a 401.
///
/// Both answers are generic, so neither bucket says whether the username
/// exists.
pub(crate) struct LoginRate {
    per_ip: WindowMap,
    per_user: WindowMap,
    /// Attempts per address per window (all attempts).
    ip_max: u32,
    /// Failed attempts per account per window.
    user_max: u32,
    window_secs: i64,
}

impl LoginRate {
    pub(crate) fn new() -> Self {
        Self {
            per_ip: WindowMap::new(4096),
            per_user: WindowMap::new(4096),
            ip_max: 30,
            user_max: 10,
            window_secs: 60,
        }
    }

    /// Hard gate, before the expensive password check. `None` (no connect
    /// info: in-process callers) skips it rather than inventing a shared
    /// bucket that would let one caller throttle every test.
    pub(crate) fn admit(&mut self, client: Option<std::net::IpAddr>, now: i64) -> bool {
        match client {
            Some(ip) => {
                self.per_ip
                    .check_and_record(&ip.to_string(), now, self.window_secs, self.ip_max)
            }
            None => true,
        }
    }

    /// Count a failed verification. False means the account's failure budget
    /// is spent: answer 429 instead of 401, but do not reject a correct
    /// password — [`Self::note_success`] is what makes that possible.
    pub(crate) fn note_failure(&mut self, username: &str, now: i64) -> bool {
        self.per_user.check_and_record(
            &username.to_lowercase(),
            now,
            self.window_secs,
            self.user_max,
        )
    }

    /// A correct password clears the account's failure budget.
    pub(crate) fn note_success(&mut self, username: &str) {
        self.per_user.reset(&username.to_lowercase());
    }

    /// True once this account has spent its failure budget (used by tests and
    /// by the handler's 429-vs-401 decision).
    #[cfg(test)]
    pub(crate) fn account_is_throttled(&self, username: &str, now: i64) -> bool {
        self.per_user.is_exhausted(
            &username.to_lowercase(),
            now,
            self.window_secs,
            self.user_max,
        )
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

    /// Env-independent construction. `AppState::set_event_rate_limits` uses
    /// this so a test can throttle its own AppState without mutating the
    /// process-global env — every concurrently-running test that constructs
    /// an AppState reads that env and would inherit the poisoned tiny budget
    /// (the cause of the flaky 429s in the events ingest tests).
    pub(crate) fn with_limits(max: u32, window_secs: i64) -> Self {
        Self {
            per_node: std::collections::HashMap::new(),
            max,
            window_secs,
        }
    }

    /// `true` if this request is under the per-node budget; the first request of
    /// a new window resets the counter. Stale entries are pruned opportunistically
    /// past 1024 keys — enrollment mints one node id per node, so the map would
    /// otherwise grow without bound on long-lived installs with node churn.
    pub(crate) fn admit(&mut self, node_id: &str, now: i64) -> bool {
        if self.per_node.len() > 1024 {
            let window = self.window_secs;
            self.per_node.retain(|_, (start, _)| now - *start < window);
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> std::net::IpAddr {
        s.parse().unwrap()
    }

    /// Finding 12: the per-account bucket counts failures, not attempts, so a
    /// spent budget cannot lock the real user out of their own account.
    #[test]
    fn a_spent_failure_budget_does_not_block_a_correct_password() {
        let mut rate = LoginRate::new();
        for i in 0..20 {
            rate.note_failure("admin", 1000 + i);
        }
        assert!(rate.account_is_throttled("admin", 1020));
        // A correct password resets it; the handler calls this on success.
        rate.note_success("admin");
        assert!(!rate.account_is_throttled("admin", 1021));
    }

    /// The per-address gate is what bounds spraying across usernames: ten
    /// distinct accounts from one address must not buy ten budgets.
    #[test]
    fn one_address_cannot_spray_across_accounts() {
        let mut rate = LoginRate::new();
        let mut allowed = 0;
        for i in 0..60 {
            if rate.admit(Some(ip("203.0.113.7")), 1000 + i) {
                allowed += 1;
            }
            // Each attempt is a fresh username with its own per-account bucket.
            let _ = rate.note_failure(&format!("user{i}"), 1000 + i);
        }
        assert_eq!(allowed, rate.ip_max as i32);
    }

    /// A different address is unaffected by someone else's exhausted bucket.
    #[test]
    fn the_address_bucket_is_per_address() {
        let mut rate = LoginRate::new();
        for i in 0..40 {
            rate.admit(Some(ip("203.0.113.7")), 1000 + i);
        }
        assert!(!rate.admit(Some(ip("203.0.113.7")), 1040));
        assert!(rate.admit(Some(ip("198.51.100.4")), 1041));
    }

    /// An unbounded key map is a memory-exhaustion primitive on the one route
    /// reachable without a credential.
    #[test]
    fn the_key_maps_are_bounded() {
        let mut rate = LoginRate::new();
        for i in 0..10_000 {
            let _ = rate.admit(Some(ip("10.0.0.1")), 1000);
            rate.note_failure(&format!("user{i}"), 1000);
        }
        assert!(rate.per_user.entries.len() <= rate.per_user.cap);
        assert!(rate.per_ip.entries.len() <= rate.per_ip.cap);
    }
}
