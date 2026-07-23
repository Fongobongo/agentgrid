pub mod policy;
pub mod workflow;
pub use policy::{
    AutonomyLevel, BuiltinPolicyProvider, CommandPolicyProvider, PolicyDecision, PolicyError,
    PolicyVerdict, RiskClass,
};
pub use workflow::{
    compute_budget_usage, BudgetBreach, BudgetUsage, CreateWorkflowRequest,
    CreateWorkflowRunRequest, RoleRunStatus, StepProjection, WorkflowBudget, WorkflowProjection,
    WorkflowRole, WorkflowRun, WorkflowRunStatus, WorkflowRunWithSteps, WorkflowSchedule,
    WorkflowScheduleCreate, WorkflowStep, WorkflowStepRun, WorkflowStepStatus, WorkflowTemplate,
};

use serde::{Deserialize, Serialize};

/// Task lifecycle status (control-plane view of a user request).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Assigned,
    Running,
    Validating,
    Succeeded,
    Failed,
    Cancelled,
}

/// Per-attempt status (one execution of a task on a node).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptStatus {
    Assigned,
    Running,
    Validating,
    Succeeded,
    Failed,
    Cancelled,
    Lost,
}

/// Node registration/health status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Pending,
    Online,
    Degraded,
    Offline,
    Revoked,
}

/// Stored event kinds. Mirrors the spec's `status | stdout | stderr | tool |
/// artifact | metric` plus `result`/`error` carried over from the adapter
/// contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    Status,
    Stdout,
    Stderr,
    Tool,
    Artifact,
    Metric,
    Result,
    Error,
}

pub mod approval;
pub mod cluster;
pub mod context;
pub mod mcp;
pub mod profile;
pub mod rss;
pub mod skills_trust;
mod state_machine;

pub use approval::{
    next_approval, ApprovalEvent, ApprovalStatus, ApprovalView, InvalidApprovalTransition,
};
pub use cluster::{probe_decision, ClusterHandle, ClusterStep, ProbedExecutor};
pub use context::{cache_key_for, ContextError, ContextPack, ContextProvider, NoopContextProvider};
pub use mcp::{McpServer, McpServerCreate};
pub use profile::{ActivateProfile, AgentProfile, AgentProfileCreate, SecretRequirement};
pub use skills_trust::SkillTrustView;
pub use state_machine::{
    next_attempt_status, next_task_status, AttemptTransition, InvalidTransition, TaskTransition,
};

/// A single streamed event tied to an attempt, with a monotonic `sequence`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskEvent {
    pub attempt_id: String,
    pub sequence: u64,
    pub r#type: EventType,
    pub payload: serde_json::Value,
    pub created_at: String,
}

macro_rules! display_snake {
    ($t:ty) => {
        impl std::fmt::Display for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                let s = serde_json::to_value(self)
                    .map_err(|_| std::fmt::Error)?
                    .as_str()
                    .ok_or(std::fmt::Error)?
                    .to_string();
                f.write_str(&s)
            }
        }
    };
}
display_snake!(TaskStatus);
display_snake!(AttemptStatus);
display_snake!(NodeStatus);
display_snake!(EventType);

/// Richer adapter event vocabulary introduced in Stage 3.1. Any unrecognized
/// `kind` string is preserved as `Other(String)` so a future adapter cannot
/// break the pipeline (unknown events become raw logs, never a fatal error).
#[derive(Debug, Clone, PartialEq)]
pub enum EventKind {
    Plan,
    ToolCall,
    ToolResult,
    FileChange,
    PermissionRequest,
    Usage,
    Handoff,
    Cancel,
    Status,
    Log,
    Progress,
    Result,
    Error,
    Other(String),
}

impl Serialize for EventKind {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let v = match self {
            EventKind::Plan => "plan",
            EventKind::ToolCall => "tool_call",
            EventKind::ToolResult => "tool_result",
            EventKind::FileChange => "file_change",
            EventKind::PermissionRequest => "permission_request",
            EventKind::Usage => "usage",
            EventKind::Handoff => "handoff",
            EventKind::Cancel => "cancel",
            EventKind::Status => "status",
            EventKind::Log => "log",
            EventKind::Progress => "progress",
            EventKind::Result => "result",
            EventKind::Error => "error",
            EventKind::Other(o) => o.as_str(),
        };
        s.serialize_str(v)
    }
}

impl<'de> Deserialize<'de> for EventKind {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.as_str() {
            "plan" => EventKind::Plan,
            "tool_call" => EventKind::ToolCall,
            "tool_result" => EventKind::ToolResult,
            "file_change" => EventKind::FileChange,
            "permission_request" => EventKind::PermissionRequest,
            "usage" => EventKind::Usage,
            "handoff" => EventKind::Handoff,
            "cancel" => EventKind::Cancel,
            "status" => EventKind::Status,
            "log" => EventKind::Log,
            "progress" => EventKind::Progress,
            "result" => EventKind::Result,
            "error" => EventKind::Error,
            other => EventKind::Other(other.to_string()),
        })
    }
}

impl EventKind {
    /// Map a 3.1 event kind onto the legacy stored [`EventType`] so the
    /// existing storage/query contract is unchanged.
    pub fn to_event_type(&self) -> EventType {
        match self {
            EventKind::Plan | EventKind::Handoff | EventKind::Status | EventKind::Cancel => {
                EventType::Status
            }
            EventKind::ToolCall | EventKind::ToolResult => EventType::Tool,
            EventKind::FileChange => EventType::Artifact,
            EventKind::PermissionRequest | EventKind::Log => EventType::Stdout,
            EventKind::Usage | EventKind::Progress => EventType::Metric,
            EventKind::Result => EventType::Result,
            EventKind::Error => EventType::Error,
            EventKind::Other(_) => EventType::Stdout,
        }
    }
}

/// Versioned adapter event envelope (Stage 3.1), layered over the stored
/// `TaskEvent`. `raw_ref` optionally points at a content-addressed raw blob
/// when the payload is too large to inline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentEventEnvelope {
    pub version: u8,
    pub kind: EventKind,
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_ref: Option<String>,
}

/// Request to open an agent session for an attempt (Stage 3.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAgentSessionRequest {
    pub adapter: String,
}

/// A single agent execution inside an attempt (Stage 3.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: String,
    pub attempt_id: String,
    pub adapter: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: String,
    pub error_code: Option<String>,
}

/// Per-adapter capability advertised in the heartbeat (Stage 3.2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdapterCapability {
    pub id: String,
    pub version: Option<String>,
    pub ready: bool,
}

// ----- API DTOs -----

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateTaskRequest {
    pub prompt: String,
    pub repository: String,
    pub adapter: String,
    #[serde(default)]
    pub requested_node_id: Option<String>,
    /// Optional per-task timeout in seconds (server default if unset).
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Optional validation command overriding the repository default.
    #[serde(default)]
    pub validation_command: Option<String>,
    /// Optional exact commit the node should check out for the worktree
    /// (Stage 8: shared base_commit). `None` => branch from `default_branch`.
    #[serde(default)]
    pub base_commit: Option<String>,
    /// Optional ACP session id to resume (Stage 11.5). `None` => fresh session.
    #[serde(default)]
    pub parent_acp_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskView {
    pub id: String,
    pub repository: String,
    pub prompt: String,
    pub adapter: String,
    pub status: TaskStatus,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub assigned_attempt_id: Option<String>,
    #[serde(default)]
    pub validation_command: Option<String>,
    /// Distinct failure category when the task is not succeeded/cancelled
    /// cleanly: `agent_failed` / `validation_failed` / `timeout` etc. NULL on
    /// success or a clean cancel.
    #[serde(default)]
    pub error_code: Option<String>,
    /// Node this task is pinned to, if the creator requested one (Stage 8
    /// workflow placement). `None` => scheduler picks any eligible node.
    #[serde(default)]
    pub requested_node_id: Option<String>,
    /// Exact commit the node checked out for the worktree (Stage 8), if the
    /// task was pinned to one. `None` => branched from `default_branch`.
    #[serde(default)]
    pub base_commit: Option<String>,
    /// ACP session id to resume (Stage 11.5), if this task should continue a
    /// prior ACP session. `None` => a fresh session.
    #[serde(default)]
    pub parent_acp_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeView {
    pub id: String,
    pub name: String,
    pub status: NodeStatus,
    pub adapters: Vec<String>,
    pub repositories: Vec<String>,
    pub max_concurrency: u32,
    pub active_attempts: u32,
    pub last_heartbeat_at: String,
    pub agent_version: String,
    pub load_avg: f64,
    pub free_disk_mb: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollRequest {
    pub node_id: String,
    pub name: String,
    pub adapters: Vec<String>,
    pub repositories: Vec<String>,
    pub max_concurrency: u32,
    /// Node→control-plane protocol version (Stage 2.5). Absent on legacy
    /// nodes; a major mismatch marks the node `degraded`.
    #[serde(default)]
    pub protocol_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Assignment {
    pub attempt_id: String,
    pub task_id: String,
    pub repository: String,
    pub prompt: String,
    pub adapter: String,
    pub number: u32,
    /// Seconds before the node should forcibly kill the attempt.
    pub timeout_secs: u64,
    /// Git remote URL; empty when the task runs in a plain directory.
    #[serde(default)]
    pub git_url: String,
    /// Branch new attempts branch from (e.g. `main`).
    #[serde(default)]
    pub default_branch: String,
    /// Optional validation command run after the agent succeeds (Stage 3.3).
    #[serde(default)]
    pub validation_command: Option<String>,
    /// Optional exact commit the node should check out (Stage 8 base_commit).
    #[serde(default)]
    pub base_commit: Option<String>,
    /// Optional ACP session id the node should resume via `session/new`
    /// `parent_session_id` (Stage 11.5). `None` => a fresh session.
    #[serde(default)]
    pub parent_acp_session_id: Option<String>,
    /// Stage 13: optional external-origin provenance for this attempt, echoed
    /// by the node back on the completion call so the CP persists it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ProvenanceRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelState {
    pub cancel_requested: bool,
}

/// One-time enrollment token issued by an admin (TTL 10 min, only hash stored).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrollTokenResponse {
    pub token: String,
    pub expires_at: String,
}

/// Stage 4.1: create the first local user (only allowed while no users exist).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetupRequest {
    pub username: String,
    pub password: String,
}

/// Stage 4.1: username + password exchange for a JWT.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

/// Stage 4.1: JWT returned on successful login.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LoginResponse {
    pub token: String,
}

/// Node→control-plane protocol version (Stage 2.5). Bump the major on any
/// incompatible change to enroll/heartbeat/poll; a node advertising a
/// different major is marked `degraded(incompatible_protocol)`.
pub const NODE_PROTOCOL_VERSION: &str = "1";

/// True when a node-advertised `protocol_version` is incompatible with the
/// current major. `None` (legacy node) is treated as compatible.
pub fn is_incompatible_protocol(pv: &Option<String>) -> bool {
    match pv {
        None => false,
        Some(v) => v.split('.').next().unwrap_or("") != NODE_PROTOCOL_VERSION,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrollRequest {
    pub token: String,
    pub name: String,
    #[serde(default)]
    pub adapters: Vec<String>,
    #[serde(default)]
    pub repositories: Vec<String>,
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: u32,
    #[serde(default)]
    pub agent_version: String,
    /// Node→control-plane protocol version (Stage 2.5). Absent on legacy
    /// nodes; a major mismatch marks the node `degraded`.
    #[serde(default)]
    pub protocol_version: Option<String>,
}

/// Node identity + secret credential returned once at enroll (never stored plaintext).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnrollResponse {
    pub node_id: String,
    pub credential: String,
}

/// Periodic node health/capability report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    #[serde(default)]
    pub status: Option<NodeStatus>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub adapters: Vec<String>,
    #[serde(default)]
    pub repositories: Vec<String>,
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: u32,
    #[serde(default)]
    pub agent_version: String,
    #[serde(default)]
    pub load_avg: f64,
    #[serde(default)]
    pub free_disk_mb: u64,
    #[serde(default)]
    pub active_attempts: u32,
    /// Node→control-plane protocol version (Stage 2.5). Absent on legacy
    /// nodes; a major mismatch marks the node `degraded`.
    #[serde(default)]
    pub protocol_version: Option<String>,
    /// Per-adapter capability the node advertises each heartbeat (Stage 3.2):
    /// which adapters it can run, their versions, and whether each is ready.
    #[serde(default)]
    pub capabilities: Vec<AdapterCapability>,
}

fn default_max_concurrency() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollResponse {
    pub assignment: Option<Assignment>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IncomingEvent {
    pub sequence: u64,
    pub r#type: EventType,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IngestEventsRequest {
    pub events: Vec<IncomingEvent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CompleteAttemptRequest {
    pub exit_code: i32,
    /// Commit SHA produced by the attempt, if it ran in a git worktree.
    #[serde(default)]
    pub commit_sha: Option<String>,
    /// Distinct failure category: `agent_failed` vs `validation_failed` etc.
    #[serde(default)]
    pub error_code: Option<String>,
    /// ACP session id returned by `session/new`, so the control plane can offer
    /// it as `parent_acp_session_id` for a follow-up task (Stage 11.5).
    #[serde(default)]
    pub acp_session_id: Option<String>,
    /// Stage 13: optional provenance record — an external id that links
    /// this attempt's outcome back to the system that requested it
    /// (Entire/h5i/Guild). Carried through to the attempt row so operators
    /// can trace a run back to its external origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ProvenanceRecord>,
}

/// A provenance link between an attempt and the external system that
/// originated it (Entire/h5i/Guild MCP). Only carries identifiers — never
/// secrets — so it is safe to persist and surface in the UI/API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ProvenanceRecord {
    /// Which external system produced this run (`entire`/`h5i`/`guild`/...).
    pub originator: String,
    /// Opaque id in that system (e.g. a project/workflow id).
    pub external_id: String,
    /// Optional human-readable label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateRepositoryRequest {
    pub name: String,
    pub git_url: String,
    pub default_branch: String,
    #[serde(default)]
    pub validation_command: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepositoryView {
    pub id: String,
    pub name: String,
    pub git_url: String,
    pub default_branch: String,
    pub validation_command: Option<String>,
    pub created_at: String,
}

/// A multi-turn chat conversation routed through the control plane to a coding
/// agent on some node. Each user message becomes a task whose prompt is the
/// composed conversation history, so any node picking it up sees full context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub adapter: String,
    pub repository: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub seq: i64,
    pub role: String,
    pub content: String,
    pub task_id: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateConversationRequest {
    pub adapter: String,
    #[serde(default)]
    pub repository: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppendMessageRequest {
    pub content: String,
}

/// Per-node eligibility for a (repository, adapter) pair, with reasons when not
/// eligible (Stage 2.4 `no_eligible_nodes` visibility).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeEligibility {
    pub node_id: String,
    pub status: NodeStatus,
    pub eligible: bool,
    pub reasons: Vec<String>,
}

/// Why a queued task has no eligible node, plus per-node detail (Stage 2.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskEligibility {
    pub task_id: String,
    /// Distinct reasons no node can run the task; empty when at least one node is eligible.
    pub no_eligible_nodes: Vec<String>,
    pub nodes: Vec<NodeEligibility>,
}

/// Upload a text artifact (e.g. `changes.patch`) from a node to the control plane.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UploadArtifactRequest {
    pub name: String,
    pub content: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventsQuery {
    #[serde(default)]
    pub after_sequence: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: Serialize + for<'de> Deserialize<'de>>(v: &T) -> T {
        let s = serde_json::to_string(v).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    #[test]
    fn status_enums_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&TaskStatus::Succeeded).unwrap(),
            "\"succeeded\""
        );
        let s: TaskStatus = serde_json::from_str("\"failed\"").unwrap();
        assert_eq!(s, TaskStatus::Failed);
        let a: AttemptStatus = serde_json::from_str("\"lost\"").unwrap();
        assert_eq!(a, AttemptStatus::Lost);
        let n: NodeStatus = serde_json::from_str("\"degraded\"").unwrap();
        assert_eq!(n, NodeStatus::Degraded);
    }

    #[test]
    fn event_type_round_trip() {
        let e = EventType::Stdout;
        assert_eq!(round_trip(&e), e);
        let e = EventType::Result;
        assert_eq!(round_trip(&e), e);
    }

    #[test]
    fn event_kind_round_trips_known_and_preserves_unknown() {
        for (kind, wire) in [
            (EventKind::Plan, "plan"),
            (EventKind::ToolCall, "tool_call"),
            (EventKind::ToolResult, "tool_result"),
            (EventKind::FileChange, "file_change"),
            (EventKind::PermissionRequest, "permission_request"),
            (EventKind::Usage, "usage"),
            (EventKind::Handoff, "handoff"),
            (EventKind::Cancel, "cancel"),
            (EventKind::Status, "status"),
            (EventKind::Log, "log"),
            (EventKind::Progress, "progress"),
            (EventKind::Result, "result"),
            (EventKind::Error, "error"),
        ] {
            assert_eq!(serde_json::to_string(&kind).unwrap(), format!("\"{wire}\""));
            assert_eq!(round_trip(&kind), kind);
        }
        // Unknown kinds are preserved verbatim, never an error.
        let unknown: EventKind = serde_json::from_str("\"future_event\"").unwrap();
        assert_eq!(unknown, EventKind::Other("future_event".into()));
        assert_eq!(serde_json::to_string(&unknown).unwrap(), "\"future_event\"");
        assert_eq!(round_trip(&unknown), unknown);
    }

    #[test]
    fn envelope_round_trip_and_maps_to_legacy_type() {
        let env = AgentEventEnvelope {
            version: 1,
            kind: EventKind::ToolCall,
            payload: serde_json::json!({ "name": "edit" }),
            raw_ref: None,
        };
        assert_eq!(round_trip(&env), env);
        assert_eq!(env.kind.to_event_type(), EventType::Tool);
        // Unknown kind inside an envelope still decodes and maps to a raw log.
        let unknown: AgentEventEnvelope =
            serde_json::from_str(r#"{"version":1,"kind":"weird","payload":{}}"#).unwrap();
        assert_eq!(unknown.kind, EventKind::Other("weird".into()));
        assert_eq!(unknown.kind.to_event_type(), EventType::Stdout);
    }

    #[test]
    fn dto_round_trip() {
        let req = CreateTaskRequest {
            prompt: "write:hello.txt:hi".into(),
            repository: "demo".into(),
            adapter: "mock".into(),
            requested_node_id: Some("node-1".into()),
            timeout_secs: None,
            validation_command: None,
            base_commit: None,
            parent_acp_session_id: None,
        };
        assert_eq!(round_trip(&req), req);

        let ev = TaskEvent {
            attempt_id: "a1".into(),
            sequence: 3,
            r#type: EventType::Stdout,
            payload: serde_json::json!({"text": "hi"}),
            created_at: "2026-01-01T00:00:00Z".into(),
        };
        assert_eq!(round_trip(&ev), ev);

        let pr = PollResponse {
            assignment: Some(Assignment {
                attempt_id: "a1".into(),
                task_id: "t1".into(),
                repository: "demo".into(),
                prompt: "x".into(),
                adapter: "mock".into(),
                number: 1,
                timeout_secs: 3600,
                git_url: String::new(),
                default_branch: String::new(),
                validation_command: None,
                base_commit: None,
                parent_acp_session_id: None,
                provenance: None,
            }),
        };
        assert_eq!(round_trip(&pr), pr);
    }

    #[test]
    fn enroll_dto_round_trip() {
        let er = EnrollRequest {
            token: "t".into(),
            name: "n".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            agent_version: "0.1".into(),
            protocol_version: None,
        };
        assert_eq!(round_trip(&er), er);
        let hb = HeartbeatRequest {
            status: Some(NodeStatus::Online),
            name: "n".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            agent_version: "0.1".into(),
            load_avg: 0.5,
            free_disk_mb: 1024,
            active_attempts: 1,
            capabilities: vec![],
            protocol_version: None,
        };
        assert_eq!(round_trip(&hb), hb);
        let resp = EnrollResponse {
            node_id: "node-1".into(),
            credential: "secret".into(),
        };
        assert_eq!(round_trip(&resp), resp);
    }
}
