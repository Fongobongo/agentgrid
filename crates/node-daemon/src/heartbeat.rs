//! Node heartbeat: periodic status/capability reporting to the control plane.

use std::ffi::CString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agentgrid_common::{AdapterCapability, HeartbeatRequest, HeartbeatSkill, NodeStatus};

use reqwest::Client;
use tokio::sync::Semaphore;

use crate::artifact_spool;
use crate::capabilities::{
    adapter_bin_name, probe_adapter, probe_cluster_adapter, resolve_acp_launch, AdapterProbe,
};
use crate::config::{adapter_permission_interception, AdapterProtocol, Config};
use crate::git;
use crate::outbox;
use crate::sandbox;

/// Read 1-minute load average from /proc/loadavg.
pub fn read_load_avg() -> f64 {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse().ok()))
        .unwrap_or(0.0)
}

/// Read free disk space in MB for `path`.
pub fn read_free_disk_mb(path: &Path) -> u64 {
    let cpath = match CString::new(path.to_string_lossy().as_bytes().to_vec()) {
        Ok(p) => p,
        Err(_) => return 0,
    };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: stat is a valid, zeroed statvfs; cpath is a valid NUL-terminated path.
    let free = unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) };
    if free != 0 || stat.f_frsize == 0 {
        return 0;
    }
    (stat.f_bavail as u64 * stat.f_frsize as u64) / (1024 * 1024)
}

/// Check if unsafe/unattended mode is active via environment.
pub fn node_unsafe_active(_cfg: &Config) -> bool {
    agentgrid_adapters::unsafe_unattended_from_env()
}

/// Determine aggregate permission interception mode for the node's adapters.
pub fn node_permission_interception(cfg: &Config) -> String {
    if cfg.adapters.is_empty() {
        return "none".to_string();
    }
    if cfg
        .adapters
        .iter()
        .all(|a| a.protocol == AdapterProtocol::Acp)
    {
        "structured".to_string()
    } else {
        "wrapper".to_string()
    }
}

/// Spawn the background heartbeat task. Returns a handle that can be awaited
/// (it runs forever unless the process exits).
pub fn spawn_heartbeat(cfg: Config, client: Client, sem: Arc<Semaphore>) {
    tokio::spawn(async move {
        loop {
            // Probe adapters and build capabilities list.
            let mut capabilities = Vec::new();
            let mut all_ok = true;
            for a in &cfg.adapters {
                let probe = if resolve_acp_launch(&a.id).is_some() {
                    AdapterProbe {
                        found: true,
                        version: None,
                    }
                } else if a.id == "zeroshot" {
                    probe_cluster_adapter("zeroshot", "docker").await
                } else {
                    let bin = adapter_bin_name(&a.id);
                    probe_adapter(&bin).await
                };
                if !probe.found {
                    all_ok = false;
                }
                capabilities.push(AdapterCapability {
                    id: a.id.clone(),
                    version: probe.version,
                    ready: probe.found,
                    permission_interception: adapter_permission_interception(a),
                });
            }

            // Disk pressure check.
            let free_disk = read_free_disk_mb(&cfg.workspace_root);
            let disk_low_mb = std::env::var("AGENTGRID_DISK_LOW_MB")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1024);
            let disk_low = free_disk < disk_low_mb;
            if disk_low {
                tracing::warn!(
                    "free disk low on node {}: {} MB < {} MB threshold; marking degraded",
                    cfg.node_name,
                    free_disk,
                    disk_low_mb
                );
            }
            all_ok &= !disk_low;

            let status = if all_ok {
                NodeStatus::Online
            } else {
                NodeStatus::Degraded
            };

            // Collect discovered skills (best-effort, never blocks heartbeat).
            let hb_home = std::env::var_os("HOME").map(std::path::PathBuf::from);
            let hb_roots =
                agentgrid_skills::standard_roots(&cfg.workspace_root, hb_home.as_deref());
            let hb_discovered = agentgrid_skills::discover(&hb_roots).0;
            let discovered_skills = hb_discovered
                .iter()
                .map(|d| HeartbeatSkill {
                    name: d.skill.name.clone(),
                    source: d.source.as_str().to_string(),
                })
                .collect::<Vec<_>>();

            // Build and send heartbeat.
            let active = cfg.max_concurrency - sem.available_permits() as u32;
            // Outbox/artifact scans are synchronous disk I/O; keep them off
            // the runtime.
            let hb_outbox_root = cfg.outbox_root.clone();
            let hb_spool_root = cfg.artifact_spool_root.clone();
            let (
                hb_outbox_bytes,
                hb_spool_bytes,
                hb_outbox_rows,
                hb_outbox_age_ms,
                hb_outbox_corrupt,
                hb_completion_rows,
            ) = tokio::task::spawn_blocking(move || {
                (
                    outbox::total_bytes(&hb_outbox_root).unwrap_or(0),
                    artifact_spool::pending(&hb_spool_root)
                        .map(|p| {
                            p.iter()
                                .filter_map(|(_, _, path)| {
                                    std::fs::metadata(path).ok().map(|m| m.len())
                                })
                                .sum()
                        })
                        .unwrap_or(0),
                    outbox::pending_rows(&hb_outbox_root).unwrap_or(0),
                    outbox::oldest_pending_age_ms(&hb_outbox_root).unwrap_or(0),
                    outbox::corruption_count(&hb_outbox_root).unwrap_or(0),
                    outbox::completion_rows(&hb_outbox_root).unwrap_or(0),
                )
            })
            .await
            .unwrap_or((0, 0, 0, 0, 0, 0));
            let req = HeartbeatRequest {
                status: Some(status),
                name: cfg.node_name.clone(),
                adapters: cfg.adapters.iter().map(|s| s.id.clone()).collect(),
                repositories: cfg.repositories.clone(),
                max_concurrency: cfg.max_concurrency,
                agent_version: cfg.agent_version.clone(),
                load_avg: read_load_avg(),
                free_disk_mb: free_disk,
                active_attempts: active,
                capabilities,
                protocol_version: Some(agentgrid_common::NODE_PROTOCOL_VERSION.into()),
                discovered_skills,
                unsafe_active: node_unsafe_active(&cfg),
                permission_interception: node_permission_interception(&cfg),
                outbox_bytes: hb_outbox_bytes,
                artifact_spool_bytes: hb_spool_bytes,
                outbox_rows: hb_outbox_rows,
                outbox_oldest_pending_age_ms: hb_outbox_age_ms,
                outbox_corruption_count: hb_outbox_corrupt,
                outbox_completion_rows: hb_completion_rows,
                repo_lock_wait_ms: git::repo_lock_wait_ms(),
                repo_cache_bytes: git::repo_cache_bytes(),
                workspace_bytes: git::workspace_bytes(),
                sandbox_backend: match cfg.sandbox {
                    sandbox::SandboxKind::None => "none".to_string(),
                    sandbox::SandboxKind::Docker => "docker".to_string(),
                },
                // Plan 960: report exactly what is applied. Docker always
                // applies cap-drop + no-new-privileges; `enforced_limits` is
                // true when resource limits are actually set AND the effective
                // network is isolated (none). A `--network bridge` override
                // means egress isolation is NOT applied, so the flag reflects
                // that honestly.
                enforced_limits: matches!(cfg.sandbox, sandbox::SandboxKind::Docker)
                    && sandbox::resolved_network_mode(&cfg.network_mode) == "none"
                    && (std::env::var("AGENTGRID_SANDBOX_PIDS_LIMIT").is_ok()
                        || std::env::var("AGENTGRID_SANDBOX_MEMORY").is_ok()
                        || std::env::var("AGENTGRID_SANDBOX_CPUS").is_ok()),
                // Node policy ceiling (max allowed task mode). The resolved
                // docker network applied at spawn (restricted→none) is logged
                // per-attempt (egress audit) — this field stays the policy.
                network_mode: cfg.network_mode.clone(),
                account_usage: crate::account_usage::snapshot(),
                applied_opencode_hash: crate::opencode_config::applied_hash(),
            };
            if let Err(e) = client
                .post(format!("{}/v1/node/heartbeat", cfg.server))
                .json(&req)
                .send()
                .await
            {
                tracing::warn!("heartbeat failed: {e}");
            }

            // Interval until the next heartbeat.
            tokio::time::sleep(Duration::from_secs(cfg.heartbeat_secs)).await;
        }
    });
}
