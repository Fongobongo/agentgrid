//! Wire DTOs for the HA peer RPC (`/v1/ha/*`). Every call carries the
//! cluster token; the token never travels in query strings.

use serde::{Deserialize, Serialize};

pub const HA_TOKEN_HEADER: &str = "x-agentgrid-ha-token";
/// SHA-256 of the shared JWT secret, compared on every peer RPC so a host
/// provisioned with a different secret cannot acknowledge writes and then
/// issue sessions the leader would reject (or vice versa).
pub const HA_JWT_FINGERPRINT_HEADER: &str = "x-agentgrid-ha-jwt-fingerprint";
/// Peer artifact replication: attempt id, on-disk file name and SHA-256 of
/// the stored bytes travel as headers so names need no URL encoding.
pub const ARTIFACT_ATTEMPT_HEADER: &str = "x-agentgrid-attempt-id";
pub const ARTIFACT_NAME_HEADER: &str = "x-agentgrid-stored-name";
pub const ARTIFACT_SHA_HEADER: &str = "x-agentgrid-artifact-sha256";
/// Incremental changeset push: leader epoch, batch sequence range and schema.
pub const HA_EPOCH_HEADER: &str = "x-agentgrid-ha-epoch";
pub const HA_SEQ_FIRST_HEADER: &str = "x-agentgrid-ha-seq-first";
pub const HA_SEQ_LAST_HEADER: &str = "x-agentgrid-ha-seq-last";
pub const HA_SCHEMA_HEADER: &str = "x-agentgrid-ha-schema-version";
/// Rejection detail on a `409` changeset response.
pub const HA_REASON_HEADER: &str = "x-agentgrid-ha-reason";
pub const HA_APPLIED_SEQ_HEADER: &str = "x-agentgrid-ha-applied-seq";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VoteRequest {
    pub cluster_id: String,
    pub term: u64,
    pub candidate_id: String,
    /// Candidate's last applied replication sequence. Data voters refuse a
    /// candidate that is behind them.
    pub applied_seq: u64,
    /// Pre-vote round (Raft §9.6, "disruptive servers"). The peer answers
    /// whether it *would* grant a vote at this term, without persisting the
    /// term, recording a vote, or demoting a live leader. A candidate only
    /// starts a real round — which bumps the term and forces the incumbent to
    /// step down — after a pre-vote quorum agrees.
    #[serde(default)]
    pub pre_vote: bool,
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
    /// Replication sequence captured in `snapshot_id`.
    pub snapshot_seq: u64,
    pub snapshot_created_unix: i64,
    pub rpo_mode: String,
    pub data_replicas: usize,
    pub witnesses: usize,
    /// SHA-256 of the shared JWT secret, when this host has one configured.
    pub jwt_fingerprint: Option<String>,
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

/// Opt-in one-time provisioning response for `ag cluster fetch-secret`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SecretResponse {
    pub secret: String,
    pub fingerprint: String,
}
