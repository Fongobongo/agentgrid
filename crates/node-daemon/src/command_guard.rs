//! Command guard (plan 1.2): adapter-agnostic allow/deny policy for tool
//! commands the agent tries to run.
//!
//! The check lives in the node-daemon (not in any single adapter) so it
//! protects every adapter uniformly. Patterns are plain substring matches —
//! users can promote to regexes later if needed without breaking contracts.
//!
//! Configuration via env (CSV, joined by ','):
//!   AGENTGRID_GUARD_DENY="rm -rf /,git push --force,curl http,dd of=/dev"
//!   AGENTGRID_GUARD_ALLOW=""     # empty allow-list → allow-list disabled
//!
//! Decision: deny-list is consulted first; an allow-list (if non-empty) is
//! consulted second and acts as a strict whitelist for tool commands.

use serde::Serialize;

#[derive(Debug, Clone)]
pub struct CommandGuard {
    deny: Vec<String>,
    allow: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuardDecision {
    Allowed,
    DeniedMatch,
    DeniedNotAllowlisted,
}

impl CommandGuard {
    /// Plan 1.2: preferred entrypoint — takes env values already parsed by
    /// `Config::config_from_env`. Empty deny → built-in defaults. Empty allow
    /// → allow-list disabled.
    pub fn new(deny: Vec<String>, allow: Vec<String>) -> Self {
        let deny = if deny.is_empty() {
            default_deny().iter().map(|s| s.to_string()).collect()
        } else {
            deny
        };
        Self { deny, allow }
    }

    /// Decide whether `cmd` is permitted. Substring match, case-sensitive.
    pub fn decide(&self, cmd: &str) -> GuardDecision {
        for pat in &self.deny {
            if cmd.contains(pat) {
                return GuardDecision::DeniedMatch;
            }
        }
        if !self.allow.is_empty() {
            let ok = self.allow.iter().any(|pat| cmd.contains(pat));
            if !ok {
                return GuardDecision::DeniedNotAllowlisted;
            }
        }
        GuardDecision::Allowed
    }
}

fn default_deny() -> Vec<&'static str> {
    vec![
        "rm -rf /",
        "rm -rf /*",
        "git push --force",
        "git push -f",
        "git reset --hard origin/",
        // curl-pipe-shell is the classic supply-chain footgun.
        "curl http",
        "curl -s http",
        "dd of=/dev",
        "mkfs.",
        ":(){ :|:&",
        "chmod -R 777 /",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deny_list_blocks_obvious_danger() {
        std::env::remove_var("AGENTGRID_GUARD_ALLOW"); // isolate
        let g = CommandGuard {
            deny: vec!["rm -rf /".into(), "git push --force".into()],
            allow: vec![],
        };
        assert_eq!(g.decide("cargo test"), GuardDecision::Allowed);
        assert_eq!(g.decide("rm -rf /tmp/x"), GuardDecision::DeniedMatch);
        assert_eq!(
            g.decide("git push --force origin main"),
            GuardDecision::DeniedMatch
        );
    }

    #[test]
    fn allow_list_restricts_to_whitelist_when_nonempty() {
        let g = CommandGuard {
            deny: vec![],
            allow: vec!["cargo ".into(), "git status".into()],
        };
        assert_eq!(g.decide("cargo build"), GuardDecision::Allowed);
        assert_eq!(
            g.decide("apt-get install foo"),
            GuardDecision::DeniedNotAllowlisted
        );
    }
}
