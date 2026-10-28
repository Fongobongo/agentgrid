//! HA configuration. Env-only to match the rest of the control plane.

use std::collections::HashMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Result};

const MIN_CLUSTER_TOKEN_BYTES: usize = 32;

/// Topology role from config (a data host or a witness). The *runtime* role
/// (standby/leader) lives in [`super::role::HaRole`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRole {
    /// CP host with a local database copy (leader or standby).
    Data,
    /// Vote/lease authority only; no database.
    Witness,
}

/// Process-wide mode. `Single` is the default and must stay behaviourally
/// identical to the pre-HA control plane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HaMode {
    Single,
    Ha,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RpoMode {
    /// Periodic `VACUUM INTO` snapshots; RPO = interval.
    Snapshot,
    /// Incremental changesets streamed within `async_max_lag`.
    Async,
    /// Incremental changesets acknowledged by a data quorum before reply.
    Sync,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplBackend {
    Peer,
    S3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArtifactSync {
    Eager,
    OnRead,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HaPeer {
    pub id: String,
    pub url: String,
}

#[derive(Clone, PartialEq, Eq)]
pub struct S3Config {
    pub endpoint: String,
    pub bucket: String,
    pub region: String,
    pub prefix: String,
    pub access_key: String,
    pub secret_key: String,
}

impl fmt::Debug for S3Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("region", &self.region)
            .field("prefix", &self.prefix)
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct HaConfig {
    pub mode: HaMode,
    pub role: NodeRole,
    pub node_id: String,
    pub cluster_id: String,
    pub cluster_token: String,
    pub advertise_url: String,
    /// Data peers (excluding self).
    pub peers: Vec<HaPeer>,
    /// Witness peers (excluding self; a witness also sets `role=witness`).
    pub witnesses: Vec<HaPeer>,
    pub rpo_mode: RpoMode,
    pub repl_backend: ReplBackend,
    pub s3: Option<S3Config>,
    pub snapshot_interval: Duration,
    pub async_max_lag: Duration,
    pub lease_ttl: Duration,
    pub election_timeout: Duration,
    pub apply_timeout: Duration,
    pub artifact_sync: ArtifactSync,
    pub jwt_secret_file: Option<PathBuf>,
    /// Allow `GET /v1/ha/secret` for one-time secret provisioning. Disabled by
    /// default; the endpoint also requires leader role and (outside explicit
    /// plaintext mode) TLS.
    pub secret_fetch: bool,
    pub witness_dir: PathBuf,
    pub allow_plaintext: bool,
    pub max_changeset_bytes: u64,
    /// Object-storage changeset batches kept below the last snapshot's
    /// sequence. Pruning runs on the leader after each snapshot upload; `0`
    /// disables it (batches accumulate forever).
    pub changeset_retention: usize,
    /// Number of data replicas in the cluster, including self when data.
    pub data_replicas: usize,
    /// Number of witness voters, including self when witness.
    pub witnesses_count: usize,
}

impl HaConfig {
    pub fn single() -> Self {
        Self {
            mode: HaMode::Single,
            role: NodeRole::Data,
            node_id: default_node_id(),
            cluster_id: String::new(),
            cluster_token: String::new(),
            advertise_url: String::new(),
            peers: Vec::new(),
            witnesses: Vec::new(),
            rpo_mode: RpoMode::Snapshot,
            repl_backend: ReplBackend::Peer,
            s3: None,
            snapshot_interval: Duration::from_secs(60),
            async_max_lag: Duration::from_millis(5000),
            lease_ttl: Duration::from_secs(10),
            election_timeout: Duration::from_millis(1500),
            apply_timeout: Duration::from_secs(300),
            artifact_sync: ArtifactSync::Eager,
            jwt_secret_file: None,
            secret_fetch: false,
            witness_dir: PathBuf::from("/var/lib/agentgrid/witness"),
            allow_plaintext: false,
            max_changeset_bytes: 8 * 1024 * 1024,
            changeset_retention: 256,
            data_replicas: 1,
            witnesses_count: 0,
        }
    }

    pub fn from_env() -> Result<Self> {
        let map: HashMap<String, String> = ENV_KEYS
            .iter()
            .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_string(), v)))
            .collect();
        Self::from_map(&map)
    }

    fn from_map(map: &HashMap<String, String>) -> Result<Self> {
        let mode = match get(map, "AGENTGRID_HA_MODE").as_deref() {
            None | Some("single") => return Ok(Self::single()),
            Some("ha") => HaMode::Ha,
            Some(other) => bail!("AGENTGRID_HA_MODE must be single|ha, got {other:?}"),
        };
        let role = match get(map, "AGENTGRID_HA_ROLE").as_deref() {
            None | Some("data") => NodeRole::Data,
            Some("witness") => NodeRole::Witness,
            Some(other) => bail!("AGENTGRID_HA_ROLE must be data|witness, got {other:?}"),
        };
        let node_id = get(map, "AGENTGRID_HA_NODE_ID").unwrap_or_else(default_node_id);
        validate_id("AGENTGRID_HA_NODE_ID", &node_id)?;

        let cluster_id = required(map, "AGENTGRID_HA_CLUSTER_ID")?;
        let cluster_token = required(map, "AGENTGRID_HA_CLUSTER_TOKEN")?;
        if cluster_token.len() < MIN_CLUSTER_TOKEN_BYTES {
            bail!("AGENTGRID_HA_CLUSTER_TOKEN must be at least {MIN_CLUSTER_TOKEN_BYTES} bytes");
        }
        let allow_plaintext = get(map, "AGENTGRID_HA_ALLOW_PLAINTEXT").as_deref() == Some("1");
        let advertise_url = required(map, "AGENTGRID_HA_ADVERTISE_URL")?;
        let advertise_url = validate_url(
            "AGENTGRID_HA_ADVERTISE_URL",
            &advertise_url,
            allow_plaintext,
        )?;

        let peers = parse_peers(map.get("AGENTGRID_HA_PEERS"))?;
        let witnesses = parse_peers(map.get("AGENTGRID_HA_WITNESSES"))?;
        let mut seen = std::collections::HashSet::new();
        seen.insert(node_id.clone());
        for p in peers.iter().chain(witnesses.iter()) {
            validate_id("HA peer id", &p.id)?;
            validate_url("HA peer url", &p.url, allow_plaintext)?;
            if !seen.insert(p.id.clone()) {
                bail!("duplicate HA peer id {:?}", p.id);
            }
        }
        let rpo_mode = match get(map, "AGENTGRID_HA_RPO_MODE").as_deref() {
            None | Some("snapshot") => RpoMode::Snapshot,
            Some("async") => RpoMode::Async,
            Some("sync") => RpoMode::Sync,
            Some(other) => {
                bail!("AGENTGRID_HA_RPO_MODE must be snapshot|async|sync, got {other:?}")
            }
        };
        let repl_backend = match get(map, "AGENTGRID_HA_REPL_BACKEND").as_deref() {
            None | Some("peer") => ReplBackend::Peer,
            Some("s3") => ReplBackend::S3,
            Some(other) => bail!("AGENTGRID_HA_REPL_BACKEND must be peer|s3, got {other:?}"),
        };
        let s3 = if repl_backend == ReplBackend::S3 {
            Some(S3Config {
                endpoint: required(map, "AGENTGRID_HA_S3_ENDPOINT")?,
                bucket: required(map, "AGENTGRID_HA_S3_BUCKET")?,
                region: get(map, "AGENTGRID_HA_S3_REGION").unwrap_or_else(|| "us-east-1".into()),
                prefix: get(map, "AGENTGRID_HA_S3_PREFIX").unwrap_or_default(),
                access_key: required(map, "AGENTGRID_HA_S3_ACCESS_KEY")?,
                secret_key: required(map, "AGENTGRID_HA_S3_SECRET_KEY")?,
            })
        } else {
            None
        };

        let lease_ttl = secs(map, "AGENTGRID_HA_LEASE_TTL_SECS", 10, 3..=30)?;
        let election_timeout = ms(map, "AGENTGRID_HA_ELECTION_TIMEOUT_MS", 1500, 500..=5000)?;
        if election_timeout >= lease_ttl {
            bail!("AGENTGRID_HA_ELECTION_TIMEOUT_MS must be shorter than the lease TTL");
        }
        let artifact_sync = match get(map, "AGENTGRID_HA_ARTIFACT_SYNC").as_deref() {
            None | Some("eager") => ArtifactSync::Eager,
            Some("on_read") => ArtifactSync::OnRead,
            Some(other) => bail!("AGENTGRID_HA_ARTIFACT_SYNC must be eager|on_read, got {other:?}"),
        };

        let data_replicas = peers.len() + usize::from(role == NodeRole::Data);
        let witnesses_count = witnesses.len() + usize::from(role == NodeRole::Witness);
        if data_replicas < 2 {
            bail!("HA needs at least 2 data replicas, got {data_replicas}");
        }
        if data_replicas == 2 && witnesses_count == 0 {
            bail!("2 data replicas need at least 1 witness (otherwise a single host loss has no quorum)");
        }
        if rpo_mode != RpoMode::Snapshot && !cfg!(feature = "ha-incremental") {
            bail!(
                "RPO mode {} requires a build with the ha-incremental feature",
                match rpo_mode {
                    RpoMode::Async => "async",
                    RpoMode::Sync => "sync",
                    RpoMode::Snapshot => "snapshot",
                }
            );
        }

        let max_changeset_bytes = get(map, "AGENTGRID_HA_MAX_CHANGESET_MB")
            .map(|v| v.parse::<u64>())
            .transpose()
            .map_err(|_| anyhow::anyhow!("AGENTGRID_HA_MAX_CHANGESET_MB must be an integer"))?
            .unwrap_or(8)
            .saturating_mul(1024 * 1024);

        let changeset_retention = get(map, "AGENTGRID_HA_CHANGESET_RETENTION")
            .map(|v| v.parse::<usize>())
            .transpose()
            .map_err(|_| anyhow::anyhow!("AGENTGRID_HA_CHANGESET_RETENTION must be an integer"))?
            .unwrap_or(256);
        if changeset_retention > 1_000_000 {
            bail!("AGENTGRID_HA_CHANGESET_RETENTION must be <= 1000000, got {changeset_retention}");
        }

        Ok(Self {
            mode,
            role,
            node_id,
            cluster_id,
            cluster_token,
            advertise_url,
            peers,
            witnesses,
            rpo_mode,
            repl_backend,
            s3,
            snapshot_interval: secs(map, "AGENTGRID_HA_SNAPSHOT_INTERVAL_SECS", 60, 10..=3600)?,
            async_max_lag: ms(map, "AGENTGRID_HA_ASYNC_MAX_LAG_MS", 5000, 500..=60_000)?,
            lease_ttl,
            election_timeout,
            apply_timeout: secs(map, "AGENTGRID_HA_APPLY_TIMEOUT_SECS", 300, 10..=3600)?,
            artifact_sync,
            jwt_secret_file: get(map, "AGENTGRID_HA_JWT_SECRET_FILE").map(PathBuf::from),
            secret_fetch: get(map, "AGENTGRID_HA_SECRET_FETCH").as_deref() == Some("1"),
            witness_dir: get(map, "AGENTGRID_HA_WITNESS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/var/lib/agentgrid/witness")),
            allow_plaintext,
            max_changeset_bytes,
            changeset_retention,
            data_replicas,
            witnesses_count,
        })
    }

    pub fn is_ha(&self) -> bool {
        self.mode == HaMode::Ha
    }

    /// All configured voters except self, for election/heartbeat fan-out.
    pub fn voters(&self) -> impl Iterator<Item = &HaPeer> {
        self.peers.iter().chain(self.witnesses.iter())
    }
}

const ENV_KEYS: &[&str] = &[
    "AGENTGRID_HA_MODE",
    "AGENTGRID_HA_ROLE",
    "AGENTGRID_HA_NODE_ID",
    "AGENTGRID_HA_CLUSTER_ID",
    "AGENTGRID_HA_CLUSTER_TOKEN",
    "AGENTGRID_HA_ADVERTISE_URL",
    "AGENTGRID_HA_PEERS",
    "AGENTGRID_HA_WITNESSES",
    "AGENTGRID_HA_RPO_MODE",
    "AGENTGRID_HA_REPL_BACKEND",
    "AGENTGRID_HA_S3_ENDPOINT",
    "AGENTGRID_HA_S3_BUCKET",
    "AGENTGRID_HA_S3_REGION",
    "AGENTGRID_HA_S3_PREFIX",
    "AGENTGRID_HA_S3_ACCESS_KEY",
    "AGENTGRID_HA_S3_SECRET_KEY",
    "AGENTGRID_HA_SNAPSHOT_INTERVAL_SECS",
    "AGENTGRID_HA_ASYNC_MAX_LAG_MS",
    "AGENTGRID_HA_LEASE_TTL_SECS",
    "AGENTGRID_HA_ELECTION_TIMEOUT_MS",
    "AGENTGRID_HA_APPLY_TIMEOUT_SECS",
    "AGENTGRID_HA_ARTIFACT_SYNC",
    "AGENTGRID_HA_JWT_SECRET_FILE",
    "AGENTGRID_HA_SECRET_FETCH",
    "AGENTGRID_HA_WITNESS_DIR",
    "AGENTGRID_HA_ALLOW_PLAINTEXT",
    "AGENTGRID_HA_MAX_CHANGESET_MB",
    "AGENTGRID_HA_CHANGESET_RETENTION",
];

fn get(map: &HashMap<String, String>, key: &str) -> Option<String> {
    map.get(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn required(map: &HashMap<String, String>, key: &str) -> Result<String> {
    get(map, key).ok_or_else(|| anyhow::anyhow!("{key} is required in HA mode"))
}

fn secs(
    map: &HashMap<String, String>,
    key: &str,
    default: u64,
    range: std::ops::RangeInclusive<u64>,
) -> Result<Duration> {
    let v = match get(map, key) {
        Some(v) => v
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("{key} must be an integer"))?,
        None => default,
    };
    if !range.contains(&v) {
        bail!("{key} must be in {:?}, got {v}", range);
    }
    Ok(Duration::from_secs(v))
}

fn ms(
    map: &HashMap<String, String>,
    key: &str,
    default: u64,
    range: std::ops::RangeInclusive<u64>,
) -> Result<Duration> {
    let v = match get(map, key) {
        Some(v) => v
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("{key} must be an integer"))?,
        None => default,
    };
    if !range.contains(&v) {
        bail!("{key} must be in {:?}, got {v}", range);
    }
    Ok(Duration::from_millis(v))
}

fn parse_peers(raw: Option<&String>) -> Result<Vec<HaPeer>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in raw.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (id, url) = item
            .split_once('@')
            .ok_or_else(|| anyhow::anyhow!("HA peer must be id@url, got {item:?}"))?;
        out.push(HaPeer {
            id: id.trim().to_string(),
            url: url.trim().trim_end_matches('/').to_string(),
        });
    }
    Ok(out)
}

fn validate_url(key: &str, url: &str, allow_plaintext: bool) -> Result<String> {
    let ok = url.starts_with("https://") || (allow_plaintext && url.starts_with("http://"));
    if !ok {
        bail!("{key} must be https:// (set AGENTGRID_HA_ALLOW_PLAINTEXT=1 for dev), got {url:?}");
    }
    Ok(url.trim_end_matches('/').to_string())
}

fn validate_id(key: &str, id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 32
        || !id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!("{key} must match [a-z0-9-]{{1,32}}, got {id:?}");
    }
    Ok(())
}

fn default_node_id() -> String {
    let raw = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let mut id: String = raw
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .take(32)
        .collect();
    id = id.trim_matches('-').to_string();
    if id.is_empty() {
        "cp-node".to_string()
    } else {
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("AGENTGRID_HA_MODE".into(), "ha".into());
        m.insert("AGENTGRID_HA_NODE_ID".into(), "cp-a".into());
        m.insert("AGENTGRID_HA_CLUSTER_ID".into(), "prod".into());
        m.insert(
            "AGENTGRID_HA_CLUSTER_TOKEN".into(),
            "0123456789abcdef0123456789abcdef".into(),
        );
        m.insert(
            "AGENTGRID_HA_ADVERTISE_URL".into(),
            "https://cp-a.example:7800".into(),
        );
        m
    }

    #[test]
    fn s3_backend_parses_and_redacts_the_secret() {
        let mut m = base();
        m.insert(
            "AGENTGRID_HA_PEERS".into(),
            "cp-b@https://b,cp-c@https://c".into(),
        );
        m.insert("AGENTGRID_HA_REPL_BACKEND".into(), "s3".into());
        m.insert(
            "AGENTGRID_HA_S3_ENDPOINT".into(),
            "https://s3.example".into(),
        );
        m.insert("AGENTGRID_HA_S3_BUCKET".into(), "agentgrid".into());
        m.insert("AGENTGRID_HA_S3_ACCESS_KEY".into(), "AKIA".into());
        m.insert(
            "AGENTGRID_HA_S3_SECRET_KEY".into(),
            "topsecret-access-key-material".into(),
        );
        let cfg = HaConfig::from_map(&m).unwrap();
        assert_eq!(cfg.repl_backend, ReplBackend::S3);
        let dbg = format!("{:?}", cfg.s3.expect("s3 config"));
        assert!(dbg.contains("<redacted>"), "{dbg}");
        assert!(!dbg.contains("topsecret-access-key-material"), "{dbg}");
    }

    #[test]
    fn single_is_default_and_ignores_ha_vars() {
        let cfg = HaConfig::from_map(&HashMap::new()).unwrap();
        assert_eq!(cfg.mode, HaMode::Single);
        assert!(!cfg.is_ha());
        let mut m = HashMap::new();
        m.insert("AGENTGRID_HA_MODE".into(), "single".into());
        m.insert("AGENTGRID_HA_PEERS".into(), "bogus".into());
        assert_eq!(HaConfig::from_map(&m).unwrap().mode, HaMode::Single);
    }

    #[test]
    fn three_data_hosts_valid() {
        let mut m = base();
        m.insert(
            "AGENTGRID_HA_PEERS".into(),
            "cp-b@https://cp-b.example:7800,cp-c@https://cp-c.example:7800".into(),
        );
        let cfg = HaConfig::from_map(&m).unwrap();
        assert_eq!(cfg.data_replicas, 3);
        assert_eq!(cfg.witnesses_count, 0);
        assert_eq!(cfg.voters().count(), 2);
    }

    #[test]
    fn two_data_plus_witness_valid() {
        let mut m = base();
        m.insert(
            "AGENTGRID_HA_PEERS".into(),
            "cp-b@https://cp-b.example".into(),
        );
        m.insert(
            "AGENTGRID_HA_WITNESSES".into(),
            "w1@https://w1.example".into(),
        );
        let cfg = HaConfig::from_map(&m).unwrap();
        assert_eq!(cfg.data_replicas, 2);
        assert_eq!(cfg.witnesses_count, 1);
    }

    #[test]
    fn witness_self_counts() {
        let mut m = base();
        m.insert("AGENTGRID_HA_ROLE".into(), "witness".into());
        m.insert("AGENTGRID_HA_NODE_ID".into(), "cp-w".into());
        m.insert(
            "AGENTGRID_HA_PEERS".into(),
            "cp-a@https://cp-a.example,cp-b@https://cp-b.example".into(),
        );
        let cfg = HaConfig::from_map(&m).unwrap();
        assert_eq!(cfg.role, NodeRole::Witness);
        assert_eq!(cfg.data_replicas, 2);
        assert_eq!(cfg.witnesses_count, 1);
    }

    #[test]
    fn two_data_without_witness_rejected() {
        let mut m = base();
        m.insert(
            "AGENTGRID_HA_PEERS".into(),
            "cp-b@https://cp-b.example".into(),
        );
        let err = HaConfig::from_map(&m).unwrap_err().to_string();
        assert!(err.contains("witness"), "{err}");
    }

    #[test]
    fn sync_needs_incremental_feature() {
        let mut m = base();
        m.insert(
            "AGENTGRID_HA_PEERS".into(),
            "cp-b@https://cp-b.example,cp-c@https://cp-c.example".into(),
        );
        m.insert("AGENTGRID_HA_RPO_MODE".into(), "sync".into());
        if cfg!(feature = "ha-incremental") {
            assert!(HaConfig::from_map(&m).is_ok());
        } else {
            let err = HaConfig::from_map(&m).unwrap_err().to_string();
            assert!(err.contains("ha-incremental"), "{err}");
        }
    }

    #[test]
    fn plaintext_requires_opt_in() {
        let mut m = base();
        m.insert(
            "AGENTGRID_HA_PEERS".into(),
            "cp-b@http://cp-b,cp-c@http://cp-c".into(),
        );
        assert!(HaConfig::from_map(&m).is_err());
        m.insert("AGENTGRID_HA_ALLOW_PLAINTEXT".into(), "1".into());
        m.insert("AGENTGRID_HA_ADVERTISE_URL".into(), "http://cp-a".into());
        assert!(HaConfig::from_map(&m).is_ok());
    }

    #[test]
    fn duplicate_and_self_peer_ids_rejected() {
        let mut m = base();
        m.insert(
            "AGENTGRID_HA_PEERS".into(),
            "cp-a@https://a,cp-c@https://c".into(),
        );
        assert!(HaConfig::from_map(&m).is_err());
        let mut m = base();
        m.insert(
            "AGENTGRID_HA_PEERS".into(),
            "cp-b@https://b,cp-b@https://c".into(),
        );
        assert!(HaConfig::from_map(&m).is_err());
    }

    #[test]
    fn short_cluster_token_rejected() {
        let mut m = base();
        m.insert("AGENTGRID_HA_CLUSTER_TOKEN".into(), "short".into());
        m.insert(
            "AGENTGRID_HA_PEERS".into(),
            "cp-b@https://b,cp-c@https://c".into(),
        );
        assert!(HaConfig::from_map(&m).is_err());
    }

    #[test]
    fn election_timeout_must_fit_lease() {
        let mut m = base();
        m.insert(
            "AGENTGRID_HA_PEERS".into(),
            "cp-b@https://b,cp-c@https://c".into(),
        );
        m.insert("AGENTGRID_HA_LEASE_TTL_SECS".into(), "3".into());
        m.insert("AGENTGRID_HA_ELECTION_TIMEOUT_MS".into(), "5000".into());
        assert!(HaConfig::from_map(&m).is_err());
    }
}
