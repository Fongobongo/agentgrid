//! Deployment mode shared by the control plane and the node daemon.
//!
//! `AGENTGRID_ENV` gates four hardening controls: the fail-closed `/metrics`
//! policy and the weak-JWT-secret refusal (`control-plane/src/lib.rs`), the
//! `Secure` session cookie (`auth.rs`) and the refusal to run with no sandbox
//! (`node-daemon/src/sandbox.rs`). Each used to compare the raw string to
//! `"production"` inline, so `AGENTGRID_ENV=prodution` was indistinguishable
//! from an unset value and silently turned all four off.
//!
//! Boot validates the value once and refuses to start on anything unknown.
//! Guard sites with no error channel use [`deploy_env_fail_closed`], so an
//! unexpected value can only ever *add* restrictions.

use std::env::VarError;

/// The only two modes the hardening checks distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeployEnv {
    /// Local/staging/test host. Unset means this.
    #[default]
    Development,
    /// All production hardening is on.
    Production,
}

impl DeployEnv {
    pub fn is_production(self) -> bool {
        matches!(self, Self::Production)
    }
}

/// Parse a raw `AGENTGRID_ENV` value. `None` (unset) and an empty value mean
/// development. Whitespace is trimmed so a shell-quoted value with a trailing
/// newline does not become a startup error.
pub fn parse_deploy_env(value: Option<&str>) -> Result<DeployEnv, String> {
    let raw = value.map(str::trim).filter(|s| !s.is_empty());
    match raw {
        None | Some("development" | "dev" | "staging" | "test") => Ok(DeployEnv::Development),
        Some("production") => Ok(DeployEnv::Production),
        Some(other) => Err(format!(
            "unknown AGENTGRID_ENV value {other:?}: expected 'production' (hardening on), \
             one of 'development'/'dev'/'staging'/'test', or unset (development). \
             Refusing to start rather than silently disabling production checks"
        )),
    }
}

/// Read and validate the process environment. Call once at boot. A bad value
/// must stop the process — a warning followed by a dev-mode boot is exactly
/// the fail-open this module exists to remove.
pub fn deploy_env_from_process() -> Result<DeployEnv, String> {
    match std::env::var("AGENTGRID_ENV") {
        Ok(v) => parse_deploy_env(Some(&v)),
        Err(VarError::NotPresent) => Ok(DeployEnv::Development),
        Err(VarError::NotUnicode(_)) => {
            Err("AGENTGRID_ENV is not valid UTF-8; refusing to start".to_string())
        }
    }
}

/// Guard sites that cannot return an error (`cookie_secure`, the sandbox
/// startup gate) use this. Boot validation runs first, so this is belt and
/// braces: if validation was ever skipped, an unknown value is treated as
/// production rather than development.
pub fn deploy_env_fail_closed() -> DeployEnv {
    guard_deploy_env(deploy_env_from_process())
}

fn guard_deploy_env(result: Result<DeployEnv, String>) -> DeployEnv {
    result.unwrap_or(DeployEnv::Production)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_and_development_values_are_development() {
        for v in [
            None,
            Some(""),
            Some("development"),
            Some("dev"),
            Some("staging"),
            Some("test"),
        ] {
            assert_eq!(
                parse_deploy_env(v).unwrap(),
                DeployEnv::Development,
                "value {v:?}"
            );
        }
    }

    #[test]
    fn production_is_the_only_hardened_mode() {
        assert!(parse_deploy_env(Some("production"))
            .unwrap()
            .is_production());
        assert!(parse_deploy_env(Some(" production \n"))
            .unwrap()
            .is_production());
        assert!(!parse_deploy_env(Some("staging")).unwrap().is_production());
    }

    #[test]
    fn a_typo_is_an_error_not_a_silent_downgrade() {
        for v in ["prodution", "Production", "PRODUCTION", "prod", "1", "true"] {
            let e = parse_deploy_env(Some(v)).unwrap_err();
            assert!(e.contains("AGENTGRID_ENV"), "value {v:?}: {e}");
            assert!(e.contains("Refusing to start"), "value {v:?}: {e}");
        }
    }

    #[test]
    fn unvalidated_guard_sites_fail_closed() {
        assert_eq!(
            guard_deploy_env(Err("bad value".to_string())),
            DeployEnv::Production
        );
        assert_eq!(
            guard_deploy_env(Ok(DeployEnv::Development)),
            DeployEnv::Development
        );
    }
}
