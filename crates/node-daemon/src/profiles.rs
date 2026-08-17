//! Agent profile handling: discovery, fetching, autonomy resolution, and
//! fail-closed compatibility/secret checks.

use agentgrid_common::{policy::AutonomyLevel, AgentProfile};
use reqwest::Client;

/// Adapter-specific files that are projected into the worktree root before an
/// attempt runs (e.g. `CLAUDE.md` for Claude Code).
pub fn native_projection_files(adapter_id: &str) -> Vec<&'static str> {
    match adapter_id {
        "claude" => vec!["CLAUDE.md"],
        _ => vec![],
    }
}

/// Read the agent profile for `adapter_id` from the node's env
/// (`AGENTGRID_AGENT_PROFILE_<ADAPTER>`). Accepts a literal prompt or a path
/// to a file on disk; returns `None` when unset or empty.
pub fn agent_profile(adapter_id: &str) -> Option<String> {
    let key = format!(
        "AGENTGRID_AGENT_PROFILE_{}",
        adapter_id
            .to_ascii_uppercase()
            .replace(|c: char| !c.is_alphanumeric(), "_")
    );
    let val = std::env::var(&key).ok()?;
    if val.trim().is_empty() {
        return None;
    }
    let p = std::path::Path::new(&val);
    if p.is_file() {
        std::fs::read_to_string(p).ok()
    } else {
        Some(val)
    }
}

/// Stage 13: fetch the active agent profile revision for `adapter_id` from the
/// control plane. Returns the full profile (system prompt + autonomy +
/// resource limits); on any error or when no active profile exists, falls back
/// to the env-based [`agent_profile`] so the node keeps working without a
/// configured profile server-side. Caller applies autonomy (if parseable +
/// stricter than cfg) and resource limits to the `SpawnRequest`.
pub async fn fetch_agent_profile(
    client: &Client,
    server: &str,
    adapter_id: &str,
) -> Option<AgentProfile> {
    let resp = client
        .get(format!("{server}/v1/profiles/{}", adapter_id))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let revs: Vec<AgentProfile> = resp.json().await.ok()?;
    revs.into_iter().find(|p| p.active)
}

/// Parse a profile autonomy string ("l0".."l4") into an `AutonomyLevel`, or
/// `None` if unknown/empty. Used to override `cfg.autonomy` server-side.
pub fn parse_autonomy_str(s: &str) -> Option<AutonomyLevel> {
    match s.trim().to_ascii_lowercase().as_str() {
        "l0" => Some(AutonomyLevel::L0),
        "l1" => Some(AutonomyLevel::L1),
        "l2" => Some(AutonomyLevel::L2),
        "l3" => Some(AutonomyLevel::L3),
        "l4" => Some(AutonomyLevel::L4),
        _ => None,
    }
}

fn level_rank(l: AutonomyLevel) -> u8 {
    match l {
        AutonomyLevel::L0 => 0,
        AutonomyLevel::L1 => 1,
        AutonomyLevel::L2 => 2,
        AutonomyLevel::L3 => 3,
        AutonomyLevel::L4 => 4,
    }
}

/// Effective autonomy is the stricter of the node's configured autonomy and the
/// profile's autonomy (when the profile specifies one). A profile can never
/// *raise* the node operator's configured ceiling (fail-closed), only tighten.
pub fn effective_autonomy(
    cfg_level: AutonomyLevel,
    profile: Option<&AgentProfile>,
) -> AutonomyLevel {
    match profile.and_then(|p| parse_autonomy_str(&p.autonomy)) {
        Some(p_level) => {
            if level_rank(p_level) < level_rank(cfg_level) {
                p_level
            } else {
                cfg_level
            }
        }
        None => cfg_level,
    }
}

/// Hardening P0 item 5: check an adapter's installed version against a
/// profile's declared `adapter_version`. Returns `Some("infrastructure_failed")`
/// when the profile declares a version and the installed adapter is missing or
/// incompatible (major differs / unparseable) — fail-closed so an agent never
/// runs against an adapter the profile wasn't written for. `None` when
/// compatible or when the profile doesn't declare a version (no check).
pub fn check_adapter_compatibility(
    profile: Option<&AgentProfile>,
    installed_version: Option<&str>,
) -> Option<String> {
    let p = profile?;
    let declared = p.adapter_version.as_deref()?;
    if !agentgrid_common::profile::versions_compatible(Some(declared), installed_version) {
        tracing::error!(
            profile = %p.id,
            declared = declared,
            installed = ?installed_version,
            "profile/adapter version mismatch; refusing to run (fail-closed)"
        );
        return Some("infrastructure_failed".to_string());
    }
    None
}

/// Check a profile's declared secret requirements against the node env.
/// Returns `Some("infrastructure_failed")` if a required secret is unset
/// (fail-closed: refuse to run); `None` if all required secrets are present or
/// the profile is absent. Optional secrets only emit a warn (not returned).
pub fn check_profile_secrets(profile: Option<&AgentProfile>) -> Option<String> {
    let p = profile?;
    for req in &p.secret_requirements {
        if std::env::var_os(&req.env).is_none() {
            if req.required {
                tracing::error!(
                    profile = %p.id,
                    secret = %req.env,
                    "required profile secret unset; refusing to run (fail-closed)"
                );
                return Some("infrastructure_failed".to_string());
            } else {
                tracing::warn!(profile = %p.id, secret = %req.env, "optional profile secret unset");
            }
        }
    }
    None
}

/// Map an agent profile's resource ceilings onto `ResourceLimits`. Profile
/// fields are `i64`; negatives are ignored (no ceiling). The process backend
/// does not enforce these yet (reports `enforced_limits=false`) — wiring is
/// forward-looking and stays cheap for compliance feedback.
pub fn profile_limits(profile: Option<&AgentProfile>) -> agentgrid_adapters::ResourceLimits {
    match profile {
        Some(p) => agentgrid_adapters::ResourceLimits {
            memory_max: p
                .memory_max
                .and_then(|m| if m > 0 { Some(m as u64) } else { None }),
            cpu_quota_percent: p
                .cpu_quota
                .and_then(|c| if c > 0 { Some(c as u32) } else { None }),
            tasks_max: p
                .tasks_max
                .and_then(|t| if t > 0 { Some(t as u32) } else { None }),
        },
        None => agentgrid_adapters::ResourceLimits::default(),
    }
}

/// Stage 13: build a `ProvenanceRecord` from the node env when an operator
/// wants every run on this node tagged with its external origin
/// (`AGENTGRID_PROVENANCE_ORIGINATOR` + `_EXTERNAL_ID`; `_LABEL` optional).
/// `None` when the originator or external_id is unset — no provenance link.
pub fn provenance_from_env() -> Option<agentgrid_common::ProvenanceRecord> {
    let originator = std::env::var("AGENTGRID_PROVENANCE_ORIGINATOR").ok()?;
    let external_id = std::env::var("AGENTGRID_PROVENANCE_EXTERNAL_ID").ok()?;
    if originator.trim().is_empty() || external_id.trim().is_empty() {
        return None;
    }
    Some(agentgrid_common::ProvenanceRecord {
        originator,
        external_id,
        label: std::env::var("AGENTGRID_PROVENANCE_LABEL").ok(),
        security_profile: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentgrid_common::policy::AutonomyLevel;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Accept anything on a port and answer 200 OK with `body`, so
    /// `fetch_agent_profile` has a live CP to query. Serves a full HTTP/1.1
    /// keep-alive connection: reqwest pools the connection, and a handler that
    /// exits after one response races a client reuse — the pooled second
    /// request hits a closed socket and dies on a TCP RST (flaky under
    /// 16-thread test stress).
    async fn dummy_profile_server(body: &'static str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut s, _) = match listener.accept().await {
                    Ok(x) => x,
                    Err(_) => break,
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(), body
                    );
                    loop {
                        match s.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {}
                        }
                        if s.write_all(resp.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    fn prof(
        autonomy: &str,
        mem: Option<i64>,
        cpu: Option<i64>,
        tasks: Option<i64>,
    ) -> AgentProfile {
        AgentProfile {
            id: "x".into(),
            revision: 1,
            system_prompt: "".into(),
            autonomy: autonomy.into(),
            memory_max: mem,
            cpu_quota: cpu,
            tasks_max: tasks,
            created_at: "".into(),
            created_by: None,
            active: true,
            secret_requirements: vec![],
            adapter_version: None,
            mcp_server_ids: vec![],
        }
    }

    #[tokio::test]
    async fn fetch_agent_profile_picks_active_revision() {
        // Two revisions, the older marked active; fetch must return its prompt.
        let body = r#"[
            {"id":"claude","revision":2,"system_prompt":"new","autonomy":"l3","memory_max":null,"cpu_quota":null,"tasks_max":null,"created_at":"","created_by":null,"active":false},
            {"id":"claude","revision":1,"system_prompt":"v1 active","autonomy":"l2","memory_max":null,"cpu_quota":null,"tasks_max":null,"created_at":"","created_by":null,"active":true}
        ]"#;
        let server = dummy_profile_server(body).await;
        let client = reqwest::Client::new();
        let p = fetch_agent_profile(&client, &server, "claude").await;
        assert_eq!(
            p.as_ref().map(|p| p.system_prompt.as_str()),
            Some("v1 active")
        );
        assert_eq!(p.as_ref().map(|p| p.autonomy.as_str()), Some("l2"));
    }

    #[tokio::test]
    async fn fetch_agent_profile_none_when_no_active() {
        // No active revision → None (caller falls back to env).
        let body = r#"[
            {"id":"claude","revision":1,"system_prompt":"x","autonomy":"l2","memory_max":null,"cpu_quota":null,"tasks_max":null,"created_at":"","created_by":null,"active":false}
        ]"#;
        let server = dummy_profile_server(body).await;
        let client = reqwest::Client::new();
        assert_eq!(fetch_agent_profile(&client, &server, "claude").await, None);
    }

    #[tokio::test]
    async fn fetch_agent_profile_none_on_empty_prompt() {
        // An active revision with an empty prompt: fetch still returns the
        // profile (the caller filters empty prompts before projecting).
        let body = r#"[
            {"id":"claude","revision":1,"system_prompt":"","autonomy":"l2","memory_max":null,"cpu_quota":null,"tasks_max":null,"created_at":"","created_by":null,"active":true}
        ]"#;
        let server = dummy_profile_server(body).await;
        let client = reqwest::Client::new();
        let p = fetch_agent_profile(&client, &server, "claude").await;
        assert_eq!(p.as_ref().map(|p| p.system_prompt.as_str()), Some(""));
    }

    #[test]
    fn effective_autonomy_takes_stricter_level() {
        // Profile autonomy stricter than cfg wins; looser is ignored.
        // cfg L4, profile L1 → L1 (profile tightens).
        assert_eq!(
            effective_autonomy(AutonomyLevel::L4, Some(&prof("l1", None, None, None))),
            AutonomyLevel::L1
        );
        // cfg L2, profile L4 → L2 (profile can't raise).
        assert_eq!(
            effective_autonomy(AutonomyLevel::L2, Some(&prof("l4", None, None, None))),
            AutonomyLevel::L2
        );
        // cfg L3, no profile → L3.
        assert_eq!(
            effective_autonomy(AutonomyLevel::L3, None),
            AutonomyLevel::L3
        );
        // cfg L2, profile bogus autonomy → cfg L2.
        assert_eq!(
            effective_autonomy(AutonomyLevel::L2, Some(&prof("zz", None, None, None))),
            AutonomyLevel::L2
        );
        // cfg L2, profile L2 → L2.
        assert_eq!(
            effective_autonomy(AutonomyLevel::L2, Some(&prof("l2", None, None, None))),
            AutonomyLevel::L2
        );
    }

    #[test]
    fn provenance_from_env_builds_record() {
        let p = provenance_from_env();
        assert_eq!(p, None);
        std::env::set_var("AGENTGRID_PROVENANCE_ORIGINATOR", "entire");
        std::env::set_var("AGENTGRID_PROVENANCE_EXTERNAL_ID", "proj-7");
        std::env::remove_var("AGENTGRID_PROVENANCE_LABEL");
        let r = provenance_from_env().expect("present when originator + external_id set");
        assert_eq!(r.originator, "entire");
        assert_eq!(r.external_id, "proj-7");
        assert!(r.label.is_none());
        std::env::set_var("AGENTGRID_PROVENANCE_LABEL", "nightly");
        let r = provenance_from_env().unwrap();
        assert_eq!(r.label.as_deref(), Some("nightly"));
        // Missing external_id returns None even with originator set.
        std::env::remove_var("AGENTGRID_PROVENANCE_EXTERNAL_ID");
        assert_eq!(provenance_from_env(), None);
        std::env::remove_var("AGENTGRID_PROVENANCE_ORIGINATOR");
        std::env::remove_var("AGENTGRID_PROVENANCE_LABEL");
    }

    #[test]
    fn check_adapter_compatibility_fails_on_major_mismatch() {
        let p = |ver: Option<&str>| AgentProfile {
            adapter_version: ver.map(|s| s.to_string()),
            ..prof("l2", None, None, None)
        };
        assert_eq!(
            check_adapter_compatibility(Some(&p(Some("1.4.0"))), Some("2.0.0")),
            Some("infrastructure_failed".to_string())
        );
        assert_eq!(
            check_adapter_compatibility(Some(&p(Some("1.4.0"))), Some("1.0.0")),
            None
        );
        assert_eq!(
            check_adapter_compatibility(Some(&p(None)), Some("2.0.0")),
            None
        );
        assert_eq!(
            check_adapter_compatibility(Some(&p(Some("1.0.0"))), Some("garbage")),
            Some("infrastructure_failed".to_string())
        );
        assert_eq!(check_adapter_compatibility(None, Some("1.0.0")), None);
    }

    #[test]
    fn check_profile_secrets_fail_closed_on_required_unset() {
        // Use a unique env name unlikely to be set in the test process.
        let unset = "AGENTGRID_TEST_UNSET_SECRET_XYZ";
        std::env::remove_var(unset);
        let p = |reqs: Vec<agentgrid_common::SecretRequirement>| AgentProfile {
            secret_requirements: reqs,
            ..prof("l2", None, None, None)
        };
        // Required unset → fail-closed.
        let code = check_profile_secrets(Some(&p(vec![agentgrid_common::SecretRequirement {
            env: unset.into(),
            required: true,
        }])));
        assert_eq!(code.as_deref(), Some("infrastructure_failed"));
        // Optional unset → None (warn only, not a refuse).
        assert_eq!(
            check_profile_secrets(Some(&p(vec![agentgrid_common::SecretRequirement {
                env: unset.into(),
                required: false,
            }]))),
            None
        );
        // Required and set → None (run allowed).
        let set = "AGENTGRID_TEST_SET_SECRET_XYZ";
        std::env::set_var(set, "v");
        assert_eq!(
            check_profile_secrets(Some(&p(vec![agentgrid_common::SecretRequirement {
                env: set.into(),
                required: true,
            }]))),
            None
        );
        std::env::remove_var(set);
        // No profile → None.
        assert_eq!(check_profile_secrets(None), None);
    }

    #[test]
    fn profile_limits_maps_positive_ceilings() {
        let l = profile_limits(Some(&prof("l2", Some(536870912), Some(50), Some(100))));
        assert_eq!(l.memory_max, Some(536870912));
        assert_eq!(l.cpu_quota_percent, Some(50));
        assert_eq!(l.tasks_max, Some(100));
        // Negatives/zero ignored.
        let l = profile_limits(Some(&prof("l2", Some(-1), Some(0), Some(-100))));
        assert_eq!(l.memory_max, None);
        assert_eq!(l.cpu_quota_percent, None);
        assert_eq!(l.tasks_max, None);
        // None profile → default.
        assert_eq!(
            profile_limits(None),
            agentgrid_adapters::ResourceLimits::default()
        );
    }

    #[test]
    fn agent_profile_reads_inline_and_none() {
        std::env::set_var("AGENTGRID_AGENT_PROFILE_TESTAG", "be brief");
        assert_eq!(agent_profile("testag"), Some("be brief".into()));
        std::env::set_var("AGENTGRID_AGENT_PROFILE_TESTAG", "");
        assert_eq!(agent_profile("testag"), None);
        std::env::remove_var("AGENTGRID_AGENT_PROFILE_TESTAG");
    }

    #[test]
    fn native_projection_files_table() {
        // Stage 11.3 / line 363: claude -> CLAUDE.md; adapters that honor
        // AGENTS.md (mock, opencode, unknown) -> empty.
        assert_eq!(native_projection_files("claude"), vec!["CLAUDE.md"]);
        assert!(native_projection_files("mock").is_empty());
        assert!(native_projection_files("unknown-adapter").is_empty());
    }
}
