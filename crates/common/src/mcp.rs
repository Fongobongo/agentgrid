//! Stage 13: Model Context Protocol server registry. A profile may attach MCP
//! stdio servers to a session; only servers the operator trusted (registered in
//! the control plane `mcp_servers` table) are projected. Untrusted refs are
//! dropped fail-closed (like skills trust), so an agent never auto-spawns a
//! server the operator didn't vet.

use serde::{Deserialize, Serialize};

/// A server registered in the operator-managed registry. Carries the stdio
/// spawn contract + a trust flag; secrets are resolved by the node from its
/// own env at spawn (never stored here, the same Stage 13 secret-ref model).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct McpServer {
    pub id: String,
    /// Short human label.
    pub name: String,
    /// Program to spawn (resolved by the node via PATH).
    pub command: String,
    /// Args passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Env var names the server *requires* (resolved from the node env at
    /// spawn; never values). Same model as `SecretRequirement`.
    #[serde(default)]
    pub env_requirements: Vec<String>,
    /// When false a profile may reference the server but the node will not
    /// spawn it (operator disabled a server without deleting it).
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub created_at: String,
}

/// Body for `POST /v1/mcp-servers` -- register a new (or replace) server.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct McpServerCreate {
    pub id: String,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env_requirements: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_round_trips_without_secret_values() {
        let srv = McpServerCreate {
            id: "github".into(),
            name: "GitHub".into(),
            command: "mcp-github".into(),
            args: vec!["--read-only".into()],
            env_requirements: vec!["GITHUB_TOKEN".into()],
            enabled: true,
        };
        let json = serde_json::to_string(&srv).unwrap();
        assert!(
            json.contains("GITHUB_TOKEN"),
            "env requirement name present"
        );
        assert!(!json.to_lowercase().contains("value"), "no value field");
        assert!(!json.contains("ghp_"), "no secret token bytes");
        let back: McpServerCreate = serde_json::from_str(&json).unwrap();
        assert_eq!(back.env_requirements, vec!["GITHUB_TOKEN".to_string()]);
        assert!(back.enabled);
    }
}
