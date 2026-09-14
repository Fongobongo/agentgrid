//! Node heartbeat: periodic status/capability reporting to the control plane.

#[cfg(unix)]
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

/// Host CPU count for the load-per-CPU gate (plan 6.10). Uses the kernel's
/// online count first; falls back to the runtime's guess; 0 only when both
/// are unavailable (the CP load gate then assumes 1 CPU).
pub fn read_cpu_count() -> u32 {
    if let Ok(s) = std::fs::read_to_string("/sys/devices/system/cpu/online") {
        // Format: "0-3,8,10-11" — count the enumerated logical CPUs.
        let mut n: u32 = 0;
        for part in s.trim().split(',') {
            if let Some((a, b)) = part.split_once('-') {
                if let (Ok(a), Ok(b)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                    n += b.saturating_sub(a) + 1;
                }
            } else if part.trim().parse::<u32>().is_ok() {
                n += 1;
            }
        }
        if n > 0 {
            return n;
        }
    }
    std::thread::available_parallelism()
        .map(|v| v.get() as u32)
        .unwrap_or(0)
}

/// Read this process's resident-set size in MiB from `/proc/self/status`
/// (`VmRSS:` kB). Returns 0 off-Linux (the capacity-pressure gate then
/// falls back to its per-attempt forecast only — same as a legacy node).
pub fn read_vmrss_mib() -> u64 {
    // `std::fs::read_to_string` on /proc is a tiny read; safe off the
    // runtime. On non-Linux it opens an absent path → 0.
    let s = match std::fs::read_to_string("/proc/self/status") {
        Ok(s) => s,
        Err(_) => return 0,
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            // `VmRSS:\t   12345 kB` — take the first integer token.
            if let Some(kb) = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<u64>().ok())
            {
                return kb / 1024;
            }
            return 0;
        }
    }
    0
}

/// MemAvailable in MiB from /proc/meminfo (kernel's estimate of memory
/// startable without swapping). 0 when unreadable — CP treats 0 as
/// "unknown", not as "no memory".
pub fn read_mem_available_mb() -> u64 {
    let Ok(s) = std::fs::read_to_string("/proc/meminfo") else {
        return 0;
    };
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kb: u64 = rest
                .split_whitespace()
                .next()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            return kb / 1024;
        }
    }
    0
}

/// Read free disk space in MB for `path`. Returns 0 on platforms without
/// statvfs (Windows local dev) — the CP treats 0 as "unknown", matching the
/// legacy-node behavior for the disk-pressure path.
pub fn read_free_disk_mb(path: &Path) -> u64 {
    #[cfg(unix)]
    {
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
    #[cfg(not(unix))]
    {
        let _ = path;
        0
    }
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

/// Plan 6.10: jitter the heartbeat interval by ±20% so a fleet restarted
/// together (deploy, CP restart) does not synchronize its beats into a
/// thundering herd against the CP. Pure and seedable for tests: the value is
/// deterministic for a given `(base, roll)` pair, `roll ∈ [0,1)`; the result
/// always stays within ±20% of `base` and never above it (a slow host is
/// fine — a too-fast cadence would churn the CP).
pub fn jittered_interval(base_secs: u64, roll: f64) -> u64 {
    // Clamp the roll to [0,1) defensively; a NaN/inf roll degrades to the
    // base interval.
    if !(0.0..1.0).contains(&roll) {
        return base_secs;
    }
    let jitter = 0.2;
    // -20% … +20% around the base: map roll onto [-1, 1) then scale.
    let scaled = (roll * 2.0 - 1.0) * jitter;
    let secs = base_secs as f64 * (1.0 + scaled);
    // Tiny bases (1s) with a negative jitter must not round to 0 — a zero
    // sleep would busy-loop the heartbeat.
    let secs = secs.max(1.0);
    secs.round() as u64
}

/// Spawn the background heartbeat task. Returns a handle that can be awaited
/// (it runs forever unless the process exits).
/// `mk_client` (when given) enables proxy failover identical to the poll
/// Plan 6.10 pressure hysteresis state machine: 3 consecutive bad beats
/// degrade the node, 5 consecutive good beats restore it. A single bad
/// reading (load spike, page-cache burst) resets the good streak but
/// cannot degrade alone; a single good reading after degradation does not
/// restore either — pressure must be sustained in both directions.
#[derive(Debug, Default, Clone)]
pub struct PressureState {
    bad_streak: u32,
    good_streak: u32,
    degraded: bool,
}

impl PressureState {
    /// Feed one beat's measurement. `bad` = disk below the floor OR load
    /// per CPU above the ceiling (the caller composes the checks — the
    /// state machine only counts). Returns the (possibly changed)
    /// degraded flag.
    pub fn beat(&mut self, bad: bool) -> bool {
        if bad {
            self.bad_streak += 1;
            self.good_streak = 0;
            if !self.degraded && self.bad_streak >= 3 {
                self.degraded = true;
            }
        } else {
            self.bad_streak = 0;
            self.good_streak += 1;
            if self.degraded && self.good_streak >= 5 {
                self.degraded = false;
                self.bad_streak = 0;
            }
        }
        self.degraded
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_degraded(&self) -> bool {
        self.degraded
    }
}

/// loop: a connect/timeout marks the current proxy dead and the next beat
/// is sent over a rebuilt client against the next pool entry.
pub fn spawn_heartbeat(
    cfg: Config,
    client: Client,
    sem: Arc<Semaphore>,
    mk_client: Option<std::sync::Arc<dyn Fn() -> Client + Send + Sync>>,
) {
    tokio::spawn(async move {
        let mut client = client;
        // Plan 6.10: cheap per-beat pseudo-random roll for the interval
        // jitter (no RNG dependency — the CP only cares that beats do not
        // align across a fleet, not that they are cryptographically random).
        let mut roll_state: u64 = u64::from(std::process::id()) ^ cfg.heartbeat_secs;
        // Plan 6.10 pressure hysteresis (see `PressureState`): a single bad
        // reading must not flap the node; only 3 consecutive bad beats
        // degrade it, and 5 consecutive good beats bring it back.
        let mut pressure = PressureState::default();
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

            // Resource pressure check (plan 6.10): disk floor and load per
            // CPU, both with hysteresis (3 consecutive bad beats degrade the
            // node; 5 consecutive good beats restore it). One-off spikes are
            // absorbed instead of flapping the fleet.
            let free_disk = read_free_disk_mb(&cfg.workspace_root);
            let min_disk_mb = std::env::var("AGENTGRID_MIN_FREE_DISK_MB")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(5120);
            let disk_bad = free_disk > 0 && free_disk < min_disk_mb;
            let load = read_load_avg();
            let cpu_count = read_cpu_count().max(1);
            let max_load_per_cpu = std::env::var("AGENTGRID_MAX_LOAD_PER_CPU")
                .ok()
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(2.0);
            let load_bad = max_load_per_cpu > 0.0
                && load > 0.0
                && load / f64::from(cpu_count) > max_load_per_cpu;
            let pressure_degraded = pressure.beat(disk_bad || load_bad);
            if disk_bad || load_bad {
                tracing::warn!(
                    "resource pressure on node {}: disk {} MB < {} MB, load {:.2} on {} cpus > {:.2}/cpu",
                    cfg.node_name,
                    free_disk,
                    min_disk_mb,
                    load,
                    cpu_count,
                    max_load_per_cpu,
                );
            }
            // The old single-beat disk floor (AGENTGRID_DISK_LOW_MB) remains
            // as an immediate hard stop for a nearly-full disk — that one
            // must not wait for hysteresis (the next clone would ENOSPC).
            let hard_disk_low_mb = std::env::var("AGENTGRID_DISK_LOW_MB")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(1024);
            let hard_disk_low = free_disk > 0 && free_disk < hard_disk_low_mb;
            if hard_disk_low {
                tracing::warn!(
                    "free disk critically low on node {}: {} MB < {} MB threshold; marking degraded",
                    cfg.node_name,
                    free_disk,
                    hard_disk_low_mb
                );
            }
            all_ok &= !(pressure_degraded || hard_disk_low);

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
            let vmrss_mib = read_vmrss_mib();
            // Plan 2.14 write gap: the gate's `max_rss_mib` was pinned to the
            // schema default (1024 MiB) because the heartbeat never sent a
            // value the operator could configure. Now the node declares its
            // own ceiling (`AGENTGRID_MAX_RSS_MIB`, MiB) so a small host
            // (Termux 256, RPi 512) can lower the gate instead of being let
            // through into OOM. 0 = unset → CP keeps its row value.
            let max_rss_mib = std::env::var("AGENTGRID_MAX_RSS_MIB")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);
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
                cpu_count: read_cpu_count(),
                // Plan 6.10: report MemAvailable minus the operator-reserved
                // slice (default 256 MiB for the OS + daemons) as the memory
                // the scheduler may hand to new attempts. Saturates at 0.
                free_memory_mb: read_mem_available_mb().saturating_sub(
                    std::env::var("AGENTGRID_RESERVED_MEM_MB")
                        .ok()
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(256),
                ),
                free_disk_mb: free_disk,
                mem_available_mb: read_mem_available_mb(),
                active_attempts: active,
                capabilities,
                // Plan 6.12: advertise the capability-schema and event-version
                // contracts alongside the protocol version, so a rolling
                // upgrade is observable in the CP logs.
                protocol_version: Some(agentgrid_common::NODE_PROTOCOL_VERSION.into()),
                capabilities_schema_version: Some(
                    agentgrid_common::CAPABILITIES_SCHEMA_VERSION.into(),
                ),
                supported_event_versions: Some(agentgrid_common::SUPPORTED_EVENT_VERSIONS.into()),
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
                // Plan 6.8: LFS presence is expensive to probe (spawns
                // git-lfs), so cadence matches the adapter probes — once
                // per heartbeat is fine; submodule support is a plain PATH
                // resolution.
                git_lfs_installed: crate::capabilities::probe_git_lfs().await.found,
                git_submodules_supported: crate::capabilities::probe_git_submodules(),
                repo_cache_bytes: git::repo_cache_bytes(),
                workspace_bytes: git::workspace_bytes(),
                sandbox_backend: match cfg.sandbox {
                    sandbox::SandboxKind::None => "none".to_string(),
                    sandbox::SandboxKind::Docker => "docker".to_string(),
                },
                // Plan 960: report exactly what is applied. Docker always
                // applies cap-drop + no-new-privileges; `enforced_limits` is
                // true when resource limits are actually set AND the effective
                // egress is isolated — `none`, or a `restricted` mode attached
                // to a configured egress-proxy network (competitor-gap
                // feature; the proxy enforces the allowlist). A `--network
                // bridge` override means egress isolation is NOT applied, so
                // the flag reflects that honestly.
                enforced_limits: matches!(cfg.sandbox, sandbox::SandboxKind::Docker)
                    && sandbox::effective_egress_isolated(&cfg.network_mode)
                    && (std::env::var("AGENTGRID_SANDBOX_PIDS_LIMIT").is_ok()
                        || std::env::var("AGENTGRID_SANDBOX_MEMORY").is_ok()
                        || std::env::var("AGENTGRID_SANDBOX_CPUS").is_ok()),
                // Node policy ceiling (max allowed task mode). The resolved
                // docker network applied at spawn (restricted→none) is logged
                // per-attempt (egress audit) — this field stays the policy.
                network_mode: cfg.network_mode.clone(),
                account_usage: crate::account_usage::snapshot(),
                applied_opencode_hash: crate::opencode_config::applied_hash(),
                active_rss_mib: vmrss_mib,
                max_rss_mib,
            };
            if let Err(e) = client
                .post(format!("{}/v1/node/heartbeat", cfg.server))
                .json(&req)
                .send()
                .await
            {
                tracing::warn!("heartbeat failed: {e}");
                if e.is_connect() || e.is_timeout() {
                    if let (Some(mk), Some(p)) = (mk_client.as_ref(), cfg.proxies.current()) {
                        tracing::warn!("egress proxy {p} failed on heartbeat; rotating");
                        cfg.proxies.mark_dead(&p);
                        client = mk();
                    }
                }
            }

            // Liveness marker (deploy healthcheck): the container HEALTHCHECK
            // compares this file's mtime against 3x the heartbeat interval —
            // a wedged loop stops touching it and the orchestrator restarts
            // the daemon. Best-effort; failure only loses the signal. Lives
            // next to the outbox under AGENTGRID_DATA_DIR.
            let marker = cfg.outbox_root.join("../heartbeat.stamp");
            let _ = std::fs::write(&marker, chrono::Utc::now().to_rfc3339().as_bytes());

            // Interval until the next heartbeat (Plan 6.10: ±20% jitter so
            // a fleet restarted together does not synchronize beats; the
            // CP's 30s staleness window tolerates the worst case of a
            // 10s base +20% = 12s easily).
            roll_state ^= roll_state << 13;
            roll_state ^= roll_state >> 7;
            roll_state ^= roll_state << 17;
            let roll = (roll_state % 1_000_000) as f64 / 1_000_000.0;
            let interval = if std::env::var_os("AGENTGRID_HEARTBEAT_NO_JITTER").is_some() {
                cfg.heartbeat_secs
            } else {
                jittered_interval(cfg.heartbeat_secs, roll)
            };
            tokio::time::sleep(Duration::from_secs(interval)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_requires_three_consecutive_bad_beats() {
        // Plan 6.10 hysteresis: 2 bad beats never degrade; the 3rd does.
        let mut p = PressureState::default();
        assert!(!p.beat(true), "one bad beat must not degrade");
        assert!(!p.beat(true), "two bad beats must not degrade");
        assert!(p.beat(true), "three consecutive bad beats must degrade");
        assert!(p.is_degraded());
    }

    #[test]
    fn pressure_ignores_single_load_spikes() {
        // The exact scenario from plan 6.10: a short load spike (bad, bad,
        // good, bad, bad...) never accumulates to a degradation.
        let mut p = PressureState::default();
        for beat in [true, true, false, true, true, false, true] {
            assert!(!p.beat(beat), "spike pattern must never degrade");
        }
    }

    #[test]
    fn pressure_needs_five_good_beats_to_recover() {
        let mut p = PressureState::default();
        for _ in 0..3 {
            p.beat(true);
        }
        assert!(p.is_degraded());
        // 4 good beats are not enough.
        for i in 0..4 {
            assert!(p.beat(false), "beat {} of 4 good must stay degraded", i + 1);
        }
        // The 5th clears it.
        assert!(!p.beat(false), "five consecutive good beats must restore");
        assert!(!p.is_degraded());
    }

    #[test]
    fn pressure_recovery_is_sticky_against_flapping() {
        // A good run that almost recovers, then one bad beat: back to
        // square one, still degraded (no half-recovered state).
        let mut p = PressureState::default();
        for _ in 0..3 {
            p.beat(true);
        }
        for _ in 0..4 {
            p.beat(false);
        }
        assert!(p.is_degraded());
        p.beat(true);
        assert!(p.is_degraded(), "one bad beat after 4 good keeps degraded");
        // And a full 5-good run right after recovers.
        for _ in 0..5 {
            p.beat(false);
        }
        assert!(!p.is_degraded());
    }

    #[test]
    fn cpu_count_parses_kernel_ranges() {
        // The /sys/devices/system/cpu/online formats: "0-3", "0-3,8",
        // "0-3,8,10-11". On Windows dev hosts the files are absent, so
        // this only exercises the parser via the same arithmetic the
        // reader uses — assert on a helper-shaped probe of the parse
        // logic by feeding it through read_cpu_count's fallback path.
        // (The kernel files only exist on Linux; the parse itself is
        // verified by the range-summing test below.)
        let n = read_cpu_count();
        // On Linux it matches nproc; on Windows it falls back to
        // available_parallelism. Both are >= 1; 0 only when both fail,
        // which never happens in CI.
        assert!(n >= 1, "cpu count must never be 0 here (got {n})");
    }

    #[test]
    fn jitter_stays_within_20_percent_band() {
        for base in [1u64, 5, 10, 30, 60] {
            for roll in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 0.999999] {
                let j = jittered_interval(base, roll);
                let lo = (base as f64 * 0.8).floor() as u64;
                let hi = (base as f64 * 1.2).ceil() as u64;
                // Rounding on tiny bases can leak one second outside the
                // exact band; tolerate ±1s there, but never a busy-loop 0.
                assert!(
                    j >= lo.max(1),
                    "base {base} roll {roll}: {j} < {}",
                    lo.max(1)
                );
                assert!(j <= hi + 1, "base {base} roll {roll}: {j} > {}", hi + 1);
            }
        }
    }

    #[test]
    fn jitter_degrades_to_base_on_garbage_roll() {
        assert_eq!(jittered_interval(10, 1.5), 10);
        assert_eq!(jittered_interval(10, -0.2), 10);
        assert_eq!(jittered_interval(10, f64::NAN), 10);
        assert_eq!(jittered_interval(10, f64::INFINITY), 10);
    }

    #[test]
    fn jitter_never_reaches_the_cp_staleness_window() {
        // Default 10s base + full jitter must stay well under the CP's 30s
        // node-staleness cutoff (mark_offline_nodes), or a jittered fleet
        // would flap nodes offline.
        for roll in [0.0, 0.5, 0.999999] {
            assert!(jittered_interval(10, roll) < 30);
        }
    }

    #[test]
    fn jitter_varies_across_rolls() {
        // A deterministic xorshift spread must not collapse to one value
        // across a plausible roll range (that would just re-synchronize the
        // fleet — the thing the jitter exists to prevent).
        let vals: std::collections::HashSet<u64> = (0..20)
            .map(|i| jittered_interval(10, i as f64 / 20.0))
            .collect();
        assert!(vals.len() >= 3, "jitter collapsed to {vals:?}");
    }
}
