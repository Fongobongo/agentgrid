//! Typed runtime configuration shared by control plane, node daemon, and CLI.
//!
//! `AGENTGRID_CONFIG` points to a YAML file. Explicit environment variables
//! override file values, so existing deployments keep working while new ones
//! can move recurring paths and limits out of unit files.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CONFIG_ENV: &str = "AGENTGRID_CONFIG";

#[derive(Debug, Error)]
pub enum RuntimeConfigError {
    #[error("read runtime config {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parse runtime config {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<serde_saphyr::DeserializeError>,
    },
    #[error("invalid {name} value {value:?}: {reason}")]
    Invalid {
        name: &'static str,
        value: String,
        reason: &'static str,
    },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeConfig {
    #[serde(default)]
    pub control_plane: ControlPlaneRuntimeConfig,
    #[serde(default)]
    pub node: NodeRuntimeConfig,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlPlaneRuntimeConfig {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_database")]
    pub database: PathBuf,
    #[serde(default)]
    pub artifact_root: Option<PathBuf>,
}

impl Default for ControlPlaneRuntimeConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            database: default_database(),
            artifact_root: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRuntimeConfig {
    #[serde(default = "default_server")]
    pub server: String,
    #[serde(default)]
    pub node_name: Option<String>,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "default_workspace_root")]
    pub workspace_root: PathBuf,
    #[serde(default = "default_repository_root")]
    pub repository_root: PathBuf,
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: u32,
    #[serde(default = "default_heartbeat_secs")]
    pub heartbeat_secs: u64,
    #[serde(default = "default_adapters")]
    pub adapters: Vec<String>,
    #[serde(default = "default_transport")]
    pub transport: String,
}

impl Default for NodeRuntimeConfig {
    fn default() -> Self {
        Self {
            server: default_server(),
            node_name: None,
            data_dir: default_data_dir(),
            workspace_root: default_workspace_root(),
            repository_root: default_repository_root(),
            max_concurrency: default_max_concurrency(),
            heartbeat_secs: default_heartbeat_secs(),
            adapters: default_adapters(),
            transport: default_transport(),
        }
    }
}

fn default_listen() -> String {
    "127.0.0.1:7800".into()
}
fn default_database() -> PathBuf {
    PathBuf::from("control-plane.db")
}
fn default_server() -> String {
    "http://127.0.0.1:7800".into()
}
fn default_data_dir() -> PathBuf {
    PathBuf::from("./agentgrid-data")
}
fn default_workspace_root() -> PathBuf {
    PathBuf::from("./agentgrid-workspace")
}
fn default_repository_root() -> PathBuf {
    PathBuf::from("./agentgrid-repos")
}
fn default_max_concurrency() -> u32 {
    2
}
fn default_heartbeat_secs() -> u64 {
    10
}
fn default_adapters() -> Vec<String> {
    vec!["mock".into()]
}
fn default_transport() -> String {
    "auto".into()
}

impl RuntimeConfig {
    /// Load `AGENTGRID_CONFIG`, then apply environment overrides.
    pub fn from_env() -> Result<Self, RuntimeConfigError> {
        let mut config = match std::env::var(CONFIG_ENV) {
            Ok(path) => {
                let path = PathBuf::from(path);
                let raw =
                    std::fs::read_to_string(&path).map_err(|source| RuntimeConfigError::Read {
                        path: path.clone(),
                        source,
                    })?;
                serde_saphyr::from_str(&raw).map_err(|source| RuntimeConfigError::Parse {
                    path: path.clone(),
                    source: Box::new(source),
                })?
            }
            Err(_) => Self::default(),
        };
        config.apply_env()?;
        config.validate()?;
        Ok(config)
    }

    fn apply_env(&mut self) -> Result<(), RuntimeConfigError> {
        if let Ok(value) = env_nonempty("AGENTGRID_LISTEN") {
            self.control_plane.listen = value;
        }
        if let Ok(value) = env_nonempty("AGENTGRID_DB") {
            self.control_plane.database = PathBuf::from(value);
        }
        if let Ok(value) = env_nonempty("AGENTGRID_ARTIFACT_ROOT") {
            self.control_plane.artifact_root = Some(PathBuf::from(value));
        }
        if let Ok(value) = env_nonempty("AGENTGRID_SERVER") {
            self.node.server = value.trim_end_matches('/').to_string();
        }
        if let Ok(value) = env_nonempty("AGENTGRID_NODE_NAME") {
            self.node.node_name = Some(value);
        }
        if let Ok(value) = env_nonempty("AGENTGRID_DATA_DIR") {
            self.node.data_dir = PathBuf::from(value);
        }
        if let Ok(value) = env_nonempty("AGENTGRID_WORKSPACE_ROOT") {
            self.node.workspace_root = PathBuf::from(value);
        }
        if let Ok(value) = env_nonempty("AGENTGRID_REPOSITORY_ROOT") {
            self.node.repository_root = PathBuf::from(value);
        }
        if let Ok(value) = env_nonempty("AGENTGRID_MAX_CONCURRENCY") {
            self.node.max_concurrency = value.parse().map_err(|_| RuntimeConfigError::Invalid {
                name: "AGENTGRID_MAX_CONCURRENCY",
                value,
                reason: "must be an integer",
            })?;
        }
        if let Ok(value) = env_nonempty("AGENTGRID_HEARTBEAT_SECS") {
            self.node.heartbeat_secs = value.parse().map_err(|_| RuntimeConfigError::Invalid {
                name: "AGENTGRID_HEARTBEAT_SECS",
                value,
                reason: "must be an integer",
            })?;
        }
        if let Ok(value) = env_nonempty("AGENTGRID_ADAPTERS") {
            self.node.adapters = value
                .split(',')
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
                .collect();
        }
        if let Ok(value) = env_nonempty("AGENTGRID_TRANSPORT") {
            self.node.transport = value;
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), RuntimeConfigError> {
        if !matches!(self.node.transport.as_str(), "poll" | "ws" | "auto") {
            return Err(RuntimeConfigError::Invalid {
                name: "AGENTGRID_TRANSPORT",
                value: self.node.transport.clone(),
                reason: "must be poll, ws, or auto",
            });
        }
        if self.node.max_concurrency == 0 {
            return Err(RuntimeConfigError::Invalid {
                name: "AGENTGRID_MAX_CONCURRENCY",
                value: "0".into(),
                reason: "must be at least 1",
            });
        }
        if self.node.heartbeat_secs == 0 {
            return Err(RuntimeConfigError::Invalid {
                name: "AGENTGRID_HEARTBEAT_SECS",
                value: "0".into(),
                reason: "must be at least 1",
            });
        }
        if self.node.adapters.is_empty() {
            return Err(RuntimeConfigError::Invalid {
                name: "AGENTGRID_ADAPTERS",
                value: String::new(),
                reason: "must contain at least one adapter",
            });
        }
        Ok(())
    }

    /// Production control-plane paths must live under `/var/lib/agentgrid`.
    /// Development keeps relative paths so local runs need no root access.
    pub fn validate_control_plane_paths(&self, production: bool) -> Result<(), RuntimeConfigError> {
        if !production {
            return Ok(());
        }
        if !path_under(&self.control_plane.database, "/var/lib/agentgrid") {
            return Err(RuntimeConfigError::Invalid {
                name: "database",
                value: self.control_plane.database.display().to_string(),
                reason: "production paths must be under /var/lib/agentgrid",
            });
        }
        let artifact_root = self.control_plane.artifact_root.clone().unwrap_or_else(|| {
            self.control_plane
                .database
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("artifacts")
        });
        if !path_under(&artifact_root, "/var/lib/agentgrid") {
            return Err(RuntimeConfigError::Invalid {
                name: "artifact_root",
                value: artifact_root.display().to_string(),
                reason: "production paths must be under /var/lib/agentgrid",
            });
        }
        Ok(())
    }

    /// Production node paths must live under `/var/lib/agentgrid`.
    pub fn validate_node_paths(&self, production: bool) -> Result<(), RuntimeConfigError> {
        if !production {
            return Ok(());
        }
        for (name, path) in [
            ("data_dir", &self.node.data_dir),
            ("workspace_root", &self.node.workspace_root),
            ("repository_root", &self.node.repository_root),
        ] {
            if !path_under(path, "/var/lib/agentgrid") {
                return Err(RuntimeConfigError::Invalid {
                    name,
                    value: path.display().to_string(),
                    reason: "production paths must be under /var/lib/agentgrid",
                });
            }
        }
        Ok(())
    }
}

/// True when `path` equals `root` or lives below it after dot segments are
/// resolved and existing symlinks are followed. A plain `starts_with` accepts
/// both `/var/lib/agentgrid-evil` and `/var/lib/agentgrid/../../tmp/cp.db`, and
/// a symlink placed inside the root can point at any writable directory.
fn path_under(path: &std::path::Path, root: &str) -> bool {
    let root = std::path::Path::new(root);
    let Some(mut normalized) = normalize_path(path) else {
        return false;
    };
    if !normalized.starts_with(root) {
        return false;
    }
    let canonical_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    // `normalized` still has symlinks; resolve the deepest existing ancestor
    // and re-attach the components that do not exist yet (fresh installs).
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(resolved) = std::fs::canonicalize(&normalized) {
            let mut unresolved = std::path::PathBuf::new();
            for part in tail.iter().rev() {
                unresolved.push(part);
            }
            return resolved.join(unresolved).starts_with(canonical_root);
        }
        let Some(name) = normalized.file_name() else {
            return false;
        };
        tail.push(name.to_os_string());
        if !normalized.pop() {
            return false;
        }
    }

    fn normalize_path(path: &std::path::Path) -> Option<std::path::PathBuf> {
        let mut out = std::path::PathBuf::new();
        for component in path.components() {
            match component {
                std::path::Component::Prefix(_) => return None,
                std::path::Component::RootDir => out.push("/"),
                std::path::Component::CurDir => {}
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::Normal(part) => out.push(part),
            }
        }
        Some(out)
    }
}

fn env_nonempty(name: &str) -> Result<String, ()> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_loads_typed_values() {
        let cfg: RuntimeConfig = serde_saphyr::from_str(
            r#"
control_plane:
  listen: 0.0.0.0:7801
  database: /var/lib/agentgrid/cp.db
  artifact_root: /var/lib/agentgrid/artifacts
node:
  server: https://cp.example.com
  data_dir: /var/lib/agentgrid/data
  max_concurrency: 4
  adapters: [mock, claude]
"#,
        )
        .unwrap();
        assert_eq!(cfg.control_plane.listen, "0.0.0.0:7801");
        assert_eq!(
            cfg.control_plane.database,
            PathBuf::from("/var/lib/agentgrid/cp.db")
        );
        assert_eq!(cfg.node.max_concurrency, 4);
        assert_eq!(cfg.node.adapters, ["mock", "claude"]);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let err =
            serde_saphyr::from_str::<RuntimeConfig>("control_plane:\n  lissten: 127.0.0.1:7800\n")
                .unwrap_err();
        assert!(err.to_string().contains("lissten"));
    }

    fn cfg_with(db: &str) -> RuntimeConfig {
        let mut c = RuntimeConfig::default();
        c.control_plane.database = PathBuf::from(db);
        c
    }

    #[test]
    fn production_rejects_paths_outside_var_lib() {
        let err = cfg_with("/tmp/agtest/cp.db")
            .validate_control_plane_paths(true)
            .unwrap_err();
        assert!(err.to_string().contains("/var/lib/agentgrid"));
    }

    #[test]
    fn production_accepts_var_lib_paths() {
        assert!(cfg_with("/var/lib/agentgrid/control-plane.db")
            .validate_control_plane_paths(true)
            .is_ok());
    }

    #[test]
    fn sibling_directory_is_not_inside_var_lib() {
        let mut c = RuntimeConfig::default();
        c.control_plane.database = PathBuf::from("/var/lib/agentgrid-evil/cp.db");
        assert!(c.validate_control_plane_paths(true).is_err());
    }

    #[test]
    fn invalid_transport_is_rejected() {
        let mut c = RuntimeConfig::default();
        c.node.transport = "carrier-pigeon".into();
        assert!(c.validate().is_err());
    }

    #[test]
    fn production_rejects_dotdot_escape() {
        let mut c = RuntimeConfig::default();
        c.control_plane.database = PathBuf::from("/var/lib/agentgrid/../../tmp/cp.db");
        assert!(c.validate_control_plane_paths(true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn path_under_follows_symlinks_and_allows_uncreated_children() {
        let base = std::env::temp_dir().join(format!("ag-path-under-{}", uuid::Uuid::new_v4()));
        let root = base.join("root");
        let outside = base.join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();

        let root = root.to_string_lossy();
        assert!(path_under(&PathBuf::from(format!("{root}/new/db")), &root));
        assert!(!path_under(
            &PathBuf::from(format!("{root}/escape/db")),
            &root
        ));
        assert!(!path_under(
            &PathBuf::from(format!("{root}/../outside/db")),
            &root
        ));
        std::fs::remove_dir_all(base.join("root")).ok();
        std::fs::remove_dir_all(base.join("outside")).ok();
        std::fs::remove_dir(base).ok();
    }
}
