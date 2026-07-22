//! Stage 13: an `AgentProfile` is the desired-state spec the control plane
//! projects to nodes for a given adapter — system prompt, autonomy level, and
//! resource limits. Revisions are immutable; the active revision is flipped by
//! updating an `agent_profiles_active` pointer, so rollback is "point back".
//! Secrets are never stored here (the node resolves secret references from its
//! own env at apply time — Stage 13 sync only carries *requirements*, never
//! values).

use serde::{Deserialize, Serialize};

/// A secret *requirement* a profile declares it needs — name only, never a
/// value. The node resolves the value from its own env at apply time; the
/// profile carries the requirement so a node can refuse to run (or warn) when
/// an expected secret is unset, and so operators can audit what each adapter
/// requires without inspecting node env.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretRequirement {
    /// Logical/env name the node must have set (e.g. `ANTHROPIC_API_KEY`).
    pub env: String,
    /// Required: refuse to start the agent if unset. `false` = warn.
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentProfile {
    pub id: String,
    pub revision: i64,
    pub system_prompt: String,
    /// Autonomy level string (`l0`..`l4`); the node parses it.
    pub autonomy: String,
    /// Optional resource ceilings (Stage 12). `None` = no ceiling.
    pub memory_max: Option<i64>,
    pub cpu_quota: Option<i64>,
    pub tasks_max: Option<i64>,
    pub created_at: String,
    pub created_by: Option<String>,
    /// Whether this revision is the active one for the profile id.
    pub active: bool,
    /// Secret requirements — names only, never values (the node resolves from
    /// its own env at apply time).
    #[serde(default)]
    pub secret_requirements: Vec<SecretRequirement>,
    /// The adapter version this profile targets (optional). A node checks it is
    /// compatible (equal major) before activating; `None` = no check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter_version: Option<String>,
}

/// Body for `POST /v1/profiles/{id}` — create a new revision. Fields the caller
/// omits default to empty/None; the server fills `revision`/`created_at`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentProfileCreate {
    #[serde(default)]
    pub system_prompt: String,
    #[serde(default = "default_autonomy")]
    pub autonomy: String,
    #[serde(default)]
    pub memory_max: Option<i64>,
    #[serde(default)]
    pub cpu_quota: Option<i64>,
    #[serde(default)]
    pub tasks_max: Option<i64>,
    /// Secret requirements (names only, never values).
    #[serde(default)]
    pub secret_requirements: Vec<SecretRequirement>,
    /// Adapter version this profile targets (optional capability check).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter_version: Option<String>,
}

fn default_autonomy() -> String {
    "l2".into()
}

/// Body for `POST /v1/profiles/{id}/activate` — flip the active pointer to an
/// existing revision (rollback = point at an older revision).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivateProfile {
    pub revision: i64,
}

/// Capability check: is the node's installed adapter version compatible with the
/// profile's declared `adapter_version`? Equal major is compatible (minor/patch
/// may differ). `None` declared version = no check (compatible). An unparseable
/// installed version is fail-closed (not compatible).
pub fn versions_compatible(declared: Option<&str>, installed: Option<&str>) -> bool {
    let Some(declared) = declared else {
        return true;
    };
    let Some(installed) = installed else {
        return true;
    };
    let major = |s: &str| s.split('.').next().and_then(|m| m.parse::<u64>().ok());
    match (major(declared), major(installed)) {
        (Some(d), Some(i)) => d == i,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_defaults_to_l2_when_deserialized() {
        let c: AgentProfileCreate = serde_json::from_str("{}").unwrap();
        assert_eq!(c.autonomy, "l2");
        assert!(c.system_prompt.is_empty());
        assert!(c.memory_max.is_none());
        assert!(c.secret_requirements.is_empty());
        assert!(c.adapter_version.is_none());
    }

    #[test]
    fn secret_requirement_is_name_only_no_value() {
        let req = SecretRequirement {
            env: "ANTHROPIC_API_KEY".into(),
            required: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("ANTHROPIC_API_KEY"));
        assert!(json.contains(r#""required":true"#));
        assert!(
            !json.contains("value"),
            "secret value field must not exist: {json}"
        );
        let back: SecretRequirement = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
    }

    #[test]
    fn profile_carries_secret_requirements_and_adapter_version() {
        let c = AgentProfileCreate {
            system_prompt: "be brief".into(),
            autonomy: "l3".into(),
            memory_max: Some(536870912),
            cpu_quota: None,
            tasks_max: Some(100),
            secret_requirements: vec![
                SecretRequirement {
                    env: "A".into(),
                    required: true,
                },
                SecretRequirement {
                    env: "B".into(),
                    required: false,
                },
            ],
            adapter_version: Some("1.2.3".into()),
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("secret_requirements"));
        assert!(json.contains("adapter_version"));
        assert!(!json.to_lowercase().contains("secret_value"));
        let back: AgentProfileCreate = serde_json::from_str(&json).unwrap();
        assert_eq!(back.secret_requirements.len(), 2);
        assert_eq!(back.adapter_version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn adapter_version_compatible_when_equal_major() {
        assert!(versions_compatible(Some("1.2.3"), Some("1.9.0")));
        assert!(!versions_compatible(Some("1.2.3"), Some("2.0.0")));
        assert!(versions_compatible(None, Some("1.0.0")));
        assert!(!versions_compatible(Some("1.0.0"), Some("garbage")));
    }
}
