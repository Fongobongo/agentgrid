//! Wire DTOs for the HA peer RPC (`/v1/ha/*`). Every call carries the
//! cluster token; the token never travels in query strings.

use serde::{Deserialize, Serialize};

pub const HA_TOKEN_HEADER: &str = "x-agentgrid-ha-token";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VoteRequest {
    pub cluster_id: String,
    pub term: u64,
    pub candidate_id: String,
    /// Candidate's last applied replication sequence. Data voters refuse a
    /// candidate that is behind them.
    pub applied_seq: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VoteResponse {
    pub term: u64,
    pub granted: bool,
    pub applied_seq: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub cluster_id: String,
    pub term: u64,
    pub epoch: u64,
    pub leader_id: String,
    pub leader_url: String,
    pub commit_seq: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    pub term: u64,
    pub ok: bool,
    pub applied_seq: u64,
    pub role: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HaStatus {
    pub node_id: String,
    pub cluster_id: String,
    pub role: String,
    pub term: u64,
    pub epoch: u64,
    pub leader_id: Option<String>,
    pub leader_url: Option<String>,
    pub applied_seq: u64,
    pub schema_version: i64,
    pub snapshot_id: Option<String>,
    pub snapshot_created_unix: i64,
    pub rpo_mode: String,
    pub data_replicas: usize,
    pub witnesses: usize,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub cluster_id: String,
    pub snapshot_id: String,
    pub epoch: u64,
    pub term: u64,
    pub seq: u64,
    pub schema_version: i64,
    pub sha256: String,
    pub size: u64,
    pub created_unix: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromoteResponse {
    pub ok: bool,
    pub role: String,
    pub epoch: u64,
    pub message: String,
}
