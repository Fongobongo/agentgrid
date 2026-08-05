//! Node daemon configuration: env parsing, defaults, adapter specs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::outbox::CompletionOutbox;
use crate::sandbox;
use agentgrid_common::policy::AutonomyLevel;

/// How an adapter is driven: a legacy wrapper binary (stdout-parsed) or an
/// ACP-speaking agent (JSON-RPC 2.0 over stdio). Both coexist in the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterProtocol {
    Wrapper,
    Acp,
}

impl AdapterProtocol {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "acp" => AdapterProtocol::Acp,
            _ => AdapterProtocol::Wrapper,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AdapterSpec {
    pub id: String,
    pub protocol: AdapterProtocol,
}

/// Parse `AGENTGRID_ADAPTERS=mock,claude,opencode:acp` into specs. An entry
/// with no `:protocol` suffix defaults to `Wrapper` (backward compatible).
pub fn parse_adapters(s: &str) -> Vec<AdapterSpec> {
    s.split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once(':') {
            Some((id, proto)) => AdapterSpec {
                id: id.trim().to_string(),
                protocol: AdapterProtocol::parse(proto),
            },
            None => AdapterSpec {
                id: p.to_string(),
                protocol: AdapterProtocol::Wrapper,
            },
        })
        .collect()
}

/// Node identity persisted to disk after enrollment (never re-sent in plaintext).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SavedCredential {
    pub node_id: String,
    pub credential: String,
}

#[derive(Clone)]
pub struct Config {
    pub server: String,
    pub node_name: String,
    pub workspace_root: PathBuf,
    pub max_concurrency: u32,
    pub agent_version: String,
    pub adapters: Vec<AdapterSpec>,
    pub repositories: Vec<String>,
    pub heartbeat_secs: u64,
    pub enroll_token: Option<String>,
    pub credential_path: PathBuf,
    pub env_file: Option<PathBuf>,
    pub repository_root: PathBuf,
    pub secrets: Vec<String>,
    pub adapter_env: Vec<(String, String)>,
    pub sandbox: sandbox::SandboxKind,
    pub outbox_root: PathBuf,
    pub artifact_spool_root: PathBuf,
    pub max_artifact_size: u64,
    pub completion_outbox: Arc<CompletionOutbox>,
    pub autonomy: AutonomyLevel,
    pub adapter_versions: HashMap<String, Option<String>>,
    pub network_mode: String,
}

fn split_csv(env: &str, default: &str) -> Vec<String> {
    std::env::var(env)
        .ok()
        .and_then(|v| {
            let items: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if items.is_empty() {
                None
            } else {
                Some(items)
            }
        })
        .unwrap_or_else(|| vec![default.to_string()])
}

fn parse_env_pairs(env: &str) -> Vec<(String, String)> {
    std::env::var(env)
        .ok()
        .map(|v| {
            v.split([' ', ',', '\n'])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .filter_map(|s| {
                    let (k, val) = s.split_once('=')?;
                    Some((k.trim().to_string(), val.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Stage 9.1: parse the command-policy autonomy level from an env string
/// like `l0`..`l4` (case-insensitive). Unknown / missing → default (`L2`).
fn parse_autonomy(v: Option<String>) -> AutonomyLevel {
    let Some(v) = v else {
        return AutonomyLevel::default();
    };
    serde_json::from_value(serde_json::Value::String(v.to_lowercase())).unwrap_or_default()
}

pub fn config_from_env() -> Config {
    let data_dir =
        std::env::var("AGENTGRID_DATA_DIR").unwrap_or_else(|_| "./agentgrid-data".into());
    Config {
        server: std::env::var("AGENTGRID_SERVER")
            .unwrap_or_else(|_| "http://127.0.0.1:7800".into()),
        node_name: std::env::var("AGENTGRID_NODE_NAME")
            .unwrap_or_else(|_| hostname().unwrap_or_else(|| "node".into())),
        workspace_root: PathBuf::from(
            std::env::var("AGENTGRID_WORKSPACE_ROOT")
                .unwrap_or_else(|_| "./agentgrid-workspace".into()),
        ),
        max_concurrency: std::env::var("AGENTGRID_MAX_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2),
        agent_version: std::env::var("AGENTGRID_AGENT_VERSION")
            .unwrap_or_else(|_| "0.1.0-dev".into()),
        adapters: parse_adapters(
            &std::env::var("AGENTGRID_ADAPTERS").unwrap_or_else(|_| "mock".into()),
        ),
        repositories: split_csv("AGENTGRID_REPOSITORIES", "*"),
        heartbeat_secs: std::env::var("AGENTGRID_HEARTBEAT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10),
        enroll_token: std::env::var("AGENTGRID_ENROLL_TOKEN").ok(),
        credential_path: PathBuf::from(&data_dir).join("credential.json"),
        env_file: std::env::var("AGENTGRID_ENV_FILE").ok().map(PathBuf::from),
        repository_root: PathBuf::from(
            std::env::var("AGENTGRID_REPOSITORY_ROOT")
                .unwrap_or_else(|_| "./agentgrid-repos".into()),
        ),
        secrets: split_csv("AGENTGRID_SECRETS", ""),
        adapter_env: parse_env_pairs("AGENTGRID_ADAPTER_ENV"),
        sandbox: sandbox::SandboxKind::from_env(),
        outbox_root: PathBuf::from(&data_dir).join("outbox"),
        artifact_spool_root: PathBuf::from(&data_dir).join("artifact-spool"),
        max_artifact_size: std::env::var("AGENTGRID_MAX_ARTIFACT_SIZE_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(|mb| mb * 1024 * 1024)
            .unwrap_or(100 * 1024 * 1024),
        completion_outbox: Arc::new({
            let dir = PathBuf::from(&data_dir).join("outbox");
            CompletionOutbox::open(&dir).unwrap_or_else(|e| {
                tracing::warn!("completion outbox open failed: {e}; events may be lost on kill");
                CompletionOutbox::open(&std::env::temp_dir()).unwrap()
            })
        }),
        autonomy: parse_autonomy(std::env::var("AGENTGRID_AUTONOMY").ok()),
        adapter_versions: HashMap::new(),
        network_mode: std::env::var("AGENTGRID_NETWORK_MODE")
            .ok()
            .filter(|v| v == "none" || v == "restricted" || v == "unrestricted")
            .unwrap_or_else(|| "none".into()),
    }
}

/// Determine permission interception mode for an adapter spec.
pub fn adapter_permission_interception(a: &AdapterSpec) -> String {
    match a.protocol {
        AdapterProtocol::Acp => "structured".into(),
        AdapterProtocol::Wrapper => "wrapper".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_adapters_basic() {
        let specs = parse_adapters("mock,claude:acp");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].id, "mock");
        assert_eq!(specs[0].protocol, AdapterProtocol::Wrapper);
        assert_eq!(specs[1].id, "claude");
        assert_eq!(specs[1].protocol, AdapterProtocol::Acp);
    }

    #[test]
    fn parse_adapters_empty() {
        let specs = parse_adapters("");
        assert_eq!(specs.len(), 0);
    }
}
