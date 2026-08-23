pub mod compress;
pub mod policy;
pub mod workflow;
pub mod ws;
pub use policy::{
    AutonomyLevel, BuiltinPolicyProvider, CommandPolicyProvider, PolicyDecision, PolicyError,
    PolicyVerdict, RiskClass,
};
pub use workflow::{
    build_handoff_payload, compute_budget_usage, parse_plan_steps, ratify_l4_schedule,
    render_handoff_block, AgentMessage, AgentMessageKind, BudgetBreach, BudgetSnapshot,
    BudgetUsage, CreateWorkflowRequest, CreateWorkflowRunRequest, HandoffPackage, RoleRunStatus,
    StepProjection, WorkflowBudget, WorkflowProjection, WorkflowRole, WorkflowRun,
    WorkflowRunStatus, WorkflowRunWithSteps, WorkflowSchedule, WorkflowScheduleCreate,
    WorkflowStep, WorkflowStepRun, WorkflowStepStatus, WorkflowTemplate,
};

use serde::{Deserialize, Serialize};

/// Header the node must present on every attempt mutation (hardening P0
/// item 8). One shared literal — a typo in a per-site copy would silently
/// bypass fencing (audit X-D3).
pub const FENCING_TOKEN_HEADER: &str = "x-agentgrid-fencing-token";

/// Lowercase-hex SHA-256 of `data`. Single shared implementation: the
/// opencode-profile hash round-trips CP↔node to detect config drift, so
/// every hashing site must agree on canonicalization (audit X-D1).
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

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
///
/// Hardening P0 item 9: `ingest_id` is the global, monotonic cursor assigned by
/// the control plane at ingest time (ordered across attempts), while
/// `sequence` remains the per-attempt monotonic counter. SSE `id:` /
/// `Last-Event-ID` and the `after_ingest` query use `ingest_id` so a client
/// resuming after a retry never reorders events across attempts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskEvent {
    pub attempt_id: String,
    pub sequence: u64,
    pub r#type: EventType,
    pub payload: serde_json::Value,
    pub created_at: String,
    /// Global monotonic ingest cursor. `#[serde(default)]` keeps pre-0037
    /// serialized events (and old clients) parseable.
    #[serde(default)]
    pub ingest_id: u64,
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
    /// How the adapter intercepts permission requests
    /// (`"structured"` | `"wrapper"` | `"none"`). Wrapper adapters parse
    /// stdout heuristically and cannot strictly enforce a policy; `"none"`
    /// means the adapter never asks — it must be confined by sandbox or run
    /// with the explicit unsafe bypass. Hardening P0 item 5.
    #[serde(default = "default_permission_interception")]
    pub permission_interception: String,
}

pub fn default_permission_interception() -> String {
    "wrapper".to_string()
}

fn default_sandbox_backend() -> String {
    "none".to_string()
}

// ----- API DTOs -----

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
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
    /// Plan 2.9 (#20): consensus run. When set (`--consensus N` with
    /// `--models m1,m2,...`), the CP stamps one consensus_group_id across N
    /// tasks (one per adapter), each marked with `consensus_member =
    /// adapter name`. Aggregation on complete collapses the group; SHAs
    /// disagreeing → human-review approval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consensus_group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consensus_member: Option<String>,
    /// Hardening P2 item 659: task-level network mode (`none` | `restricted` | `unrestricted`).
    /// Node policy sets max allowed mode; task requests a mode <= node max.
    #[serde(default)]
    pub network_mode: Option<String>,
    /// Optional security profile (e.g. "strict", "default-strict").
    /// When set to a profile ending in "-strict", the task will only be
    /// assigned to nodes with structured permission interception (not wrapper).
    #[serde(default)]
    pub security_profile: Option<String>,
    /// Plan 1.12 (#7): optional shared-context task group id — parallel
    /// attempts in the same group share `shared_context` notes and get the
    /// `AG_GROUP_ID` env var on the node.
    #[serde(default)]
    pub group_id: Option<String>,
    /// Plan 2.1 (#18): optional org-agent attribution. When set, the task
    /// counts against the agent's budget (hard-stop when exhausted).
    #[serde(default)]
    pub agent_id: Option<String>,
    /// Feature "opencode profiles": per-task override merged over the node's
    /// active profile and injected via OPENCODE_CONFIG_CONTENT (env, dies
    /// with the process). None = adapter runs purely under profiled local
    /// config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode_override: Option<OpencodeOverride>,
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
    /// Hardening P2 item 659: task-level network mode (`none` | `restricted` | `unrestricted`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_mode: Option<String>,
    /// Hardening P2 item 36: the security profile of the LATEST attempt
    /// (from `attempts.provenance.security_profile`). Surfaced so operators can
    /// see which policy the agent ran under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_profile: Option<String>,
    /// Plan 1.12 (#7): shared-context task group id, if the creator set one.
    /// Parallel attempts in the same group share `shared_context` notes and
    /// get the `AG_GROUP_ID` env var on the node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// Plan 2.1 (#18): org-agent attribution, if the task is agent-managed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Plan 2.9 (#20): consensus-run tag — sibling tasks fired as one vote
    /// share consensus_group_id; the member name is the adapter. Aggregated
    /// on complete; independent tasks carry None.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consensus_group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consensus_member: Option<String>,
    /// Feature "opencode profiles": the per-task override attached to this
    /// task (echoed from CreateTaskRequest). Not part of scheduling
    /// semantics — informational for dashboards.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode_override: Option<OpencodeOverride>,
}

/// Plan 1.3 (#13): single-attempt detail (the `GET /v1/attempts/{id}` view).
/// Echoes the prompt from the owning task so a resumed attempt can inherit
/// context without a second lookup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptView {
    pub id: String,
    pub task_id: String,
    pub number: u32,
    pub node_id: String,
    pub status: AttemptStatus,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub commit_sha: Option<String>,
    pub exit_code: Option<i32>,
    pub error_code: Option<String>,
    /// The owning task's prompt (inherited context for `ag resume`).
    pub prompt: String,
    pub adapter: String,
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
    /// Hardening P0 item 5: node is running an adapter with the unsafe
    /// unattended bypass active (no sandbox). Surfaced so operators can see
    /// which nodes run fully-unrestricted agents.
    #[serde(default)]
    pub unsafe_active: bool,
    /// Hardening P0 item 5: best-available permission interception across the
    /// node's adapters (`structured` | `wrapper` | `none`).
    #[serde(default = "default_permission_interception")]
    pub permission_interception: String,
    /// Hardening P2 item 35: total bytes in the node's durable outbox.
    #[serde(default)]
    pub outbox_bytes: u64,
    /// Hardening P2 item 35: total bytes staged in the node's artifact spool.
    #[serde(default)]
    pub artifact_spool_bytes: u64,
    /// Hardening P0 item 10: total pending event rows across all per-attempt
    /// spools.
    #[serde(default)]
    pub outbox_rows: u64,
    /// Hardening P0 item 10: age in milliseconds of the oldest unacked event
    /// across all per-attempt spools.
    #[serde(default)]
    pub outbox_oldest_pending_age_ms: u64,
    /// Hardening P0 item 10: total quarantined corrupt records in the outbox
    /// quarantine directory.
    #[serde(default)]
    pub outbox_corruption_count: u64,
    /// Hardening P0 item 10: pending completion records in completions.jsonl.
    #[serde(default)]
    pub outbox_completion_rows: u64,
    /// Hardening P2 item 35: cumulative repository-lock wait in milliseconds
    /// measured on the node (cross-process flock contention, surfaced via
    /// /metrics as a per-node gauge). 0 on legacy nodes.
    #[serde(default)]
    pub repo_lock_wait_ms: u64,
    /// Hardening P2 item 35: sandbox backend kind ("none" | "docker").
    #[serde(default = "default_sandbox_backend")]
    pub sandbox_backend: String,
    /// Hardening P2 item 35: whether the sandbox backend enforces resource
    /// limits (memory, CPU, pids).
    #[serde(default)]
    pub enforced_limits: bool,
    /// Hardening P2 item 37: node is drained — it keeps in-flight attempts but
    /// receives no NEW assignments (maintenance mode).
    #[serde(default)]
    pub drained: bool,
    /// Row creation time; the list_nodes keyset cursor is (created_at, id).
    #[serde(default)]
    pub created_at: String,
    /// Hardening P2 item 35: repository cache size in bytes.
    #[serde(default)]
    pub repo_cache_bytes: u64,
    /// Hardening P2 item 35: workspace size in bytes.
    #[serde(default)]
    pub workspace_bytes: u64,
    /// Hardening P2 item 659: node-level network mode (`none` | `restricted` | `unrestricted`).
    #[serde(default = "default_network_mode")]
    pub network_mode: String,
}

fn default_network_mode() -> String {
    "none".to_string()
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
    /// Hardening P0 item 8: fencing token. Generated by the CP at assignment;
    /// the node echoes it back on every mutating node->CP call (ack, events,
    /// complete, artifact, session) and the CP rejects a stale token (the
    /// node is reporting for an attempt that was reassigned/lost) with 409.
    /// Old nodes that never send a token are accepted under the N/N-1 policy
    /// (a blank token matches the default blank); upgraded nodes always set it.
    #[serde(default)]
    pub fencing_token: String,
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
    /// Hardening P0 item 12: per-attempt validation timeout in seconds
    /// (default 300 when unset). The node kills the validation process tree on
    /// timeout and reports `validation_timeout`.
    #[serde(default)]
    pub validation_timeout_secs: Option<u64>,
    /// Optional exact commit the node should check out (Stage 8 base_commit).
    #[serde(default)]
    pub base_commit: Option<String>,
    /// Optional ACP session id the node should resume via `session/new`
    /// `parent_session_id` (Stage 11.5). `None` => a fresh session.
    #[serde(default)]
    pub parent_acp_session_id: Option<String>,
    /// Hardening P2 item 659: task-level network mode (`none` | `restricted` | `unrestricted`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_mode: Option<String>,
    /// Stage 13: optional external-origin provenance for this attempt, echoed
    /// by the node back on the completion call so the CP persists it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ProvenanceRecord>,
    /// Stage 8 / line 239: worker commit SHAs an Integrator step should land
    /// into its worktree as an integration branch before the agent runs. Each
    /// is an upstream worker's winning commit. Empty for non-integrator steps.
    /// The node cherry-picks them in order (defense-in-depth: token-validated).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstream_commits: Vec<String>,
    /// Stage 8 / line 257: task ids parallel to `upstream_commits` (same
    /// order) so the node can fetch each upstream worker's `changes.patch`
    /// artifact from the control plane and `git apply` it when the commit SHA
    /// is not reachable via the shared Git remote (distributed workflow
    /// without a shared remote). Empty for non-integrator steps.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub upstream_task_ids: Vec<String>,
    /// Plan 1.12 (#7): shared-context task group id (from the task). When
    /// set, the node forwards it to the agent as `AG_GROUP_ID`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// Plan 2.4 (#22a): when true, the node makes the attempt's worktree
    /// read-only (sandbox bind-mount gets `:ro`). Used for verifier steps so
    /// a verifier cannot silently edit the code it is validating.
    #[serde(default)]
    pub read_only: bool,
    /// Plan 2.5 (#22b): self-healing eval cases for retry attempts. After a
    /// passed attempt the CP persists the winning change as
    /// `eval-case-<attempt>-<n>.yaml`; when the task is retried (task-level
    /// retry), the accumulated suite is shipped here, the node materialises
    /// them into the worktree at `.agentgrid/evals/` and the node probes
    /// them after the first successful validation — any failure regenerates
    /// the fix loop with the eval output as feedback.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub eval_cases: Vec<String>,
    /// Plan 2.9 (#20): consensus-run tag — a `consensus_group_id` ties
    /// sibling attempts that all ran the same prompt across different
    /// adapters (e.g. `claude,codex,opencode`). The CP stamps one group id
    /// per consensus batch; the assignment echoes it back so the node can
    /// emit provenance (`AG_CONSENSUS_GROUP=<id>`). Aggregation happens on
    /// complete: the last finisher collapse-evaluates the patch SHAs and
    /// creates a human-review approval when the SHAs disagree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consensus_group_id: Option<String>,
    /// Plan 2.9 (#20): adapter name for THIS member — informative only;
    /// drives nothing decision-wise but helps the reviewer spot which
    /// adapter produced which patch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consensus_member: Option<String>,
    /// Feature "opencode profiles": per-attempt override merged over the
    /// node's active profile and injected via OPENCODE_CONFIG_CONTENT for
    /// the `opencode` adaptaer. The env var dies with the process — an
    /// override can never leak into later attempts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub opencode_override: Option<OpencodeOverride>,
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

/// Stage 4.1: create the first local user (only allowed while no users exist
/// and the one-time bootstrap setup token is presented; hardening P0 closes the
/// open window where anyone reachable could create the first admin).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetupRequest {
    pub username: String,
    pub password: String,
    /// One-time bootstrap setup token printed to the control-plane stdout on
    /// first start (when no users exist). Required whenever a setup token is
    /// active; absent/empty is rejected.
    #[serde(default)]
    pub setup_token: Option<String>,
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

/// Plan 5.2: roles for RBAC. `admin` = full access; `operator` = view +
/// approvals only (enforced by the control-plane middleware).
pub const ROLE_ADMIN: &str = "admin";
pub const ROLE_OPERATOR: &str = "operator";

pub fn is_valid_role(role: &str) -> bool {
    role == ROLE_ADMIN || role == ROLE_OPERATOR
}

/// Plan 5.2: admin creates additional users with a role.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateUserRequest {
    pub username: String,
    pub password: String,
    /// "admin" or "operator"; defaults to "operator".
    #[serde(default = "default_role")]
    pub role: String,
}

fn default_role() -> String {
    ROLE_OPERATOR.to_string()
}

/// Plan 5.2: user entry in the admin users list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UserEntry {
    pub username: String,
    pub role: String,
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
    /// Permission interception mode for this node's adapters. One of:
    /// "wrapper" (legacy stdout parsing) or "structured" (ACP-style).
    #[serde(default = "default_permission_interception")]
    pub permission_interception: String,
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
    /// Stage 9.2: skills the node discovered on disk this heartbeat. The
    /// control plane upserts `(name, source)` rows into the trust ledger as
    /// untrusted (operator never decided) without overwriting an existing
    /// operator decision. Absent on legacy nodes (auto-discovery stays a
    /// hint, never blocks a task).
    #[serde(default)]
    pub discovered_skills: Vec<HeartbeatSkill>,
    /// Plan 1.8 (#15): per-account usage counters. Absent on legacy nodes.
    #[serde(default)]
    pub account_usage: Vec<AccountUsage>,
    /// Hardening P0 item 5: this node is running an adapter with the unsafe
    /// unattended bypass active (no sandbox). Absent on legacy nodes.
    #[serde(default)]
    pub unsafe_active: bool,
    /// Hardening P0 item 5: best-available permission interception across the
    /// node's adapters (`structured` | `wrapper` | `none`). Absent on legacy
    /// nodes (defaults to `wrapper`).
    #[serde(default = "default_permission_interception")]
    pub permission_interception: String,
    /// Hardening P2 item 35: total bytes currently buffered in the node's
    /// durable outbox (pending event + completion spools). 0 on legacy nodes.
    #[serde(default)]
    pub outbox_bytes: u64,
    /// Hardening P2 item 35: total bytes staged in the node's durable artifact
    /// spool (artifacts not yet delivered to the CP). 0 on legacy nodes.
    #[serde(default)]
    pub artifact_spool_bytes: u64,
    /// Hardening P0 item 10: total pending event rows across all per-attempt
    /// spools. 0 on legacy nodes.
    #[serde(default)]
    pub outbox_rows: u64,
    /// Hardening P0 item 10: age in milliseconds of the oldest unacked event
    /// across all per-attempt spools. 0 on legacy nodes or when empty.
    #[serde(default)]
    pub outbox_oldest_pending_age_ms: u64,
    /// Hardening P0 item 10: total quarantined corrupt records in the outbox
    /// quarantine directory. 0 on legacy nodes.
    #[serde(default)]
    pub outbox_corruption_count: u64,
    /// Opencode config drift detector: hash of the on-disk
    /// `~/.config/opencode/opencode.json` the node most recently applied.
    /// The CP compares it to the assigned profile's hash on heartbeat; a
    /// mismatch marks the node `degraded` until the next pull/reapply.
    /// Absent on legacy nodes → no drift check fires.
    #[serde(default)]
    pub applied_opencode_hash: Option<String>,
    /// Hardening P0 item 10: pending completion records in completions.jsonl.
    /// 0 on legacy nodes.
    #[serde(default)]
    pub outbox_completion_rows: u64,
    /// Hardening P2 item 35: cumulative repository-lock wait in milliseconds
    /// measured on the node. 0 on legacy nodes.
    #[serde(default)]
    pub repo_lock_wait_ms: u64,
    /// Hardening P2 item 35: sandbox backend kind ("none" | "docker").
    /// Absent on legacy nodes (defaults to "none").
    #[serde(default = "default_sandbox_backend")]
    pub sandbox_backend: String,
    /// Hardening P2 item 35: whether the sandbox backend enforces resource
    /// limits (memory, CPU, pids). Always false for "none"; true for
    /// "docker" only when limits are actually configured. 0 on legacy nodes.
    #[serde(default)]
    pub enforced_limits: bool,
    /// Hardening P2 item 35: repository cache size in bytes. 0 on legacy nodes.
    #[serde(default)]
    pub repo_cache_bytes: u64,
    /// Hardening P2 item 35: workspace size in bytes. 0 on legacy nodes.
    #[serde(default)]
    pub workspace_bytes: u64,
    /// Hardening P2 item 659: node-level network mode (`none` | `restricted` | `unrestricted`).
    #[serde(default = "default_network_mode")]
    pub network_mode: String,
    /// Plan 2.14 (#27): this node's resident-set size in MiB, sampled from
    /// `/proc/self/status` (`VmRSS`). The capacity-pressure gate reads
    /// `nodes.active_rss_mib` and refuses an assignment when
    /// `active_rss + forecast*attempts` exceeds `max_rss_mib`; before this
    /// writer existed the gate always saw 0 and never rejected on real
    /// memory pressure. Absent on legacy nodes (defaults to 0 → gate
    /// falls back to the per-attempt forecast only).
    #[serde(default)]
    pub active_rss_mib: u64,
    /// Plan 2.14 (#27): the hard memory ceiling in MiB this node wants the
    /// gate to enforce. The node operator declares it (typically matching the
    /// sandbox memory cap or the host's available RAM); before this writer
    /// existed the gate's `max_rss_mib` stayed pinned to the schema default
    /// (1024 MiB) forever, so an operator on a small host (Termux 256, RPi
    /// 512, …) could never lower it and the gate let real OOM pressure
    /// through. `0` = node does not want to override the schema default
    /// (legacy / value unset — the CP then keeps whatever row value it had).
    #[serde(default)]
    pub max_rss_mib: u64,
}

/// Plan 1.8 (#15): per-account usage reported by a node in its heartbeat so
/// the control plane can surface it at `GET /v1/nodes/{id}/accounts/usage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountUsage {
    /// Credential env var the account backs (e.g. `ANTHROPIC_API_KEY`).
    pub env: String,
    /// 0-based index of the token currently in use within the pool.
    pub token_index: usize,
    /// Attempts run on this account.
    pub attempts: u64,
    /// 429 / rate-limit hits that rotated to the next token.
    pub rate_limited: u64,
}

/// A skill name + source ("project" | "user" | "managed") advertised in a
/// heartbeat. Carries no path or body — the trust ledger only needs the
/// identity, never the skill content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatSkill {
    pub name: String,
    pub source: String,
}

fn default_max_concurrency() -> u32 {
    1
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollResponse {
    pub assignment: Option<Assignment>,
    /// Plan 0.3 item 1.2: the full assignment batch (up to the node's free
    /// concurrency slots). Legacy nodes read only `assignment` (N/N-1
    /// compat); new nodes consume every entry.
    #[serde(default)]
    pub assignments: Vec<Assignment>,
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

/// Response to `POST /v1/node/attempts/{id}/events`.
///
/// `highest_contiguous_sequence` is the largest in-order sequence the control
/// plane currently holds for this attempt: the contiguous run of sequences
/// 1..=N present in `task_events`. A client that batched up to S and gets back
/// `highest_contiguous_sequence < S` knows the CP has a gap (some earlier batch
/// did not land); one that gets `>= S` knows the full prefix landed. This is
/// purely advisory today (hardening P1 item 14): the durable outbox still
/// drives redelivery, so a client may safely ignore it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct IngestEventsAck {
    pub accepted: u64,
    pub highest_contiguous_sequence: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct CompleteAttemptRequest {
    pub exit_code: i32,
    /// Commit SHA produced by the attempt, if it ran in a git worktree.
    #[serde(default)]
    pub commit_sha: Option<String>,
    /// Hardening P2 item 32-5: the exact upstream commit this attempt started
    /// from (the resolved base). Persisted on the attempt row so the prepared
    /// base can be reconstructed later. Populated by the node daemon; NULL when
    /// the node did not pin/report a base (default-branch checkout, or plain
    /// non-git tasks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_base_sha: Option<String>,
    /// Hardening P1 item 32: the remote HEAD SHA captured at attempt *start*
    /// (before the agent ran), so audits/diffs can reconstruct what upstream
    /// looked like when the attempt began — independent of the resolved base.
    /// Populated by the node daemon; NULL when not a git repo / not reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_head_at_start: Option<String>,
    /// Hardening P1 item 32: the remote HEAD SHA captured at attempt *finish*
    /// (after the agent ran / before completion). Captures upstream moves that
    /// happened during the attempt; useful for quarantine / re-run decisions.
    /// Populated by the node daemon; NULL when not a git repo / not reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_head_at_finish: Option<String>,
    /// Distinct failure category: `agent_failed` vs `validation_failed` etc.
    #[serde(default)]
    pub error_code: Option<String>,
    /// ACP session id returned by `session/new`, so the control plane can offer
    /// it as `parent_acp_session_id` for a follow-up task (Stage 11.5).
    #[serde(default)]
    pub acp_session_id: Option<String>,
    /// Stage 13 plan expansion: an `expandable` architect step may emit a
    /// machine-readable plan (YAML/JSON, an array of worker steps). Persisted on
    /// the attempt row; when the architect step succeeds, the run pauses in
    /// `PlanReady` and awaits approval before the plan is parsed (via
    /// `parse_plan_steps`) and expanded into new workflow steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    /// Stage 13: optional provenance record — an external id that links
    /// this attempt's outcome back to the system that requested it
    /// (Entire/h5i/Guild). Carried through to the attempt row so operators
    /// can trace a run back to its external origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ProvenanceRecord>,
    /// Hardening P1 item 11: artifact names this attempt staged locally but
    /// could not deliver before completion (control plane was down). The node
    /// retries them on the next startup; the CP records the list so operators
    /// can see which artifacts are still owed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_artifacts: Vec<String>,
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
    /// Security profile used for this attempt (Stage 4.2 hardening).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_profile: Option<String>,
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
///
/// Legacy text path: `content` is UTF-8 text. Binary artifacts (binary diffs,
/// archives, images) must use the raw-bytes endpoint instead — a UTF-8 round
/// trip corrupts them. `media_type`/`sha256` are optional metadata honoured by
/// both paths so a downloader always gets the stored content type and a
/// caller can verify integrity.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UploadArtifactRequest {
    pub name: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Metadata for a stored artifact (binary-safe path). `size_bytes` is the raw
/// byte length; `media_type` is the stored content type (default
/// `application/octet-stream`); `sha256` is the hex SHA-256 of the bytes, if
/// the uploader provided one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactMeta {
    #[serde(default)]
    pub name: String,
    pub size_bytes: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Plan 1.12 (#7): one shared-context note for a task group (flat key→value).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedContextEntry {
    pub key: String,
    pub value: String,
    pub updated_at: String,
}

/// Plan 2.1 (#18): a long-lived org agent — identity, role, prompt template,
/// attached skills, and a budget (max tasks + optional USD display).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub role: String,
    pub prompt: String,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub budget_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tasks: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_interval_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_heartbeat_at: Option<String>,
    pub created_at: String,
    /// Task count so far (computed at read time). NULL/0 when unmanaged.
    #[serde(default)]
    pub tasks_spent: i64,
}

/// Plan 2.1 (#18): create-an-agent request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentCreate {
    pub name: String,
    #[serde(default = "default_agent_role")]
    pub role: String,
    #[serde(default)]
    pub prompt: String,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub budget_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tasks: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_interval_secs: Option<i64>,
}

fn default_agent_role() -> String {
    "worker".into()
}

/// Plan 2.1 (#18): one immutable row of the agent audit trail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentAction {
    pub id: String,
    pub agent_id: String,
    pub action: String,
    pub detail: String,
    pub created_at: String,
}

/// Hardening P0 item 3: the upload response carries back the artifact name,
/// stored size, media type and the server-computed SHA-256 so a client can
/// verify integrity without a separate GET. (Supersedes the old bare `200`.)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ArtifactUploadResponse {
    pub name: String,
    pub size_bytes: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    pub sha256: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EventsQuery {
    /// Legacy per-attempt cursor (pre-0037 clients). Prefer `after_ingest`.
    #[serde(default)]
    pub after_sequence: u64,
    /// Hardening P0 item 9: resume after this global ingest cursor.
    #[serde(default)]
    pub after_ingest: Option<u64>,
    /// Hardening P0 item 9: server-side page size cap (default 1000).
    #[serde(default)]
    pub limit: Option<u64>,
}

/// Unified response envelope for list endpoints with keyset cursor pagination.
/// Returns `items` and optional `next_cursor` for the next page.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListResponse<T> {
    pub items: Vec<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
}

impl<T> ListResponse<T> {
    pub fn new(items: Vec<T>, next_cursor: Option<String>) -> Self {
        Self { items, next_cursor }
    }
}

/// Plan 1.6 (#3b): an inline annotation a reviewer left on an attempt's
/// diff/plan. `file` is the file path ("" for a whole-patch or plan-level
/// comment); `line_start`/`line_end` are 1-based and inclusibe, `None` for a
/// Plan 2.8 (#19): one repo-level learning row ("instinct"). Rows land as
/// `approved = 0` when nobody has verified them; only `approved = 1` rows
/// are ever injected into an attempt prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoLearning {
    pub id: String,
    pub repository: String,
    pub statement: String,
    pub confidence: f64,
    pub source_attempt_id: Option<String>,
    pub approved: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// Plan 2.8 (#19): `ag learn add` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddLearningRequest {
    pub repository: String,
    pub statement: String,
    #[serde(default = "default_learning_confidence")]
    pub confidence: f64,
    #[serde(default)]
    pub source_attempt_id: Option<String>,
}

fn default_learning_confidence() -> f64 {
    0.5
}

/// whole-file comment. Aggregated into a rework task prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PatchAnnotation {
    pub id: String,
    pub attempt_id: String,
    pub file: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_start: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line_end: Option<i64>,
    pub comment: String,
    pub created_at: String,
}

/// Plan 1.6 (#3b): create one inline annotation on an attempt's diff/plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateAnnotationRequest {
    pub file: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_start: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_end: Option<i64>,
    pub comment: String,
}

/// Plan 1.6 (#3b): "send for rework" — start a new task that re-runs the
/// original work with the reviewer's inline annotations folded into the
/// prompt. The CP looks up the annotated attempt's owning task for the
/// original prompt + repo/adapter, appends an `[ANNOTATIONS]` block, and
/// creates a fresh task. Returns the new task id so the caller can poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReworkResponse {
    pub task_id: String,
}

/// ── opencode-config management (feature "opencode profiles") ──────────
///
/// The control plane is the source of truth for per-node opencode
/// configuration. A profile is a named bundle of opencode settings stored
/// as opaque JSON — the schema belongs to opencode, not to us; the CP
/// validates syntax + a small key allowlist and lets the node-side
/// `opencode debug config` be the final oracle. Secrets stay out: API keys
/// are referenced as `{env:VAR}` placeholders inside the config.
///
/// Delivery: CP pushes `NodeWsMsg::ConfigUpdate` on profile change; the
/// node applies when the assignment matches its rated profile and the hash
/// differs. A node may also self-heal pull after N consecutive
/// config-class errors (default 3), and an opt-in interval pull is
/// available but OFF by default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpencodeProfile {
    pub id: String,
    pub name: String,
    /// Opaque opencode config object (JSON). Unknown fields to us are passed
    /// through untouched.
    pub config: serde_json::Value,
    /// sha256 hex of the canonical JSON; compared by nodes to skip no-change
    /// writes. Bumped by the server on every write.
    pub hash: String,
    /// Previous profile revision (one step back, for operator rollback).
    /// None until at least one PUT has overwritten the row. Emerges from
    /// migration 0067; the rollback endpoint swaps cur→prev and pushes the
    /// swap to every assigned node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev: Option<Box<OpencodeProfileRevision>>,
    /// Optional absolute expiry (RFC3339 UTC). The control-plane janitor
    /// deletes the profile once this timestamp passes — same semantics as a
    /// manual DELETE (assigned nodes drop it, last-applied config stays on
    /// disk). NULL = never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Number of node applies recorded for this profile in the audit feed
    /// (populated by the list route; None elsewhere).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_count: Option<i64>,
    /// Bundle-pinned agentgrid skill names (item 10). The node reconcile
    /// these against the trust ledger on apply and reports untrusted pins in
    /// the apply audit. NULL/empty = no pin set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_skills: Option<Vec<String>>,
    pub created_at: String,
    pub updated_at: String,
}

/// Rolled-back profile contents (still allowlisted the same way). Small —
/// just id, config, hash, updated_at is enough to render the revert button.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpencodeProfileRevision {
    pub hash: String,
    pub config: serde_json::Value,
    pub updated_at: String,
}

/// `PUT /v1/opencode-profiles/{name}` body — create-or-replace (PUT, not
/// PATCH: profiles are small and the client always knows the full desired
/// state; merge-on-server would hide drift).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpsertOpencodeProfileRequest {
    pub config: serde_json::Value,
    /// Optional absolute expiry (RFC3339 UTC); the janitor deletes the
    /// profile once this passes. `null`/absent clears any previous expiry.
    #[serde(default)]
    pub expires_at: Option<String>,
    /// Optional set of agentgrid skill names pinned to this profile; the node
    /// reconcile these against the trust ledger on apply.
    #[serde(default)]
    pub pinned_skills: Option<Vec<String>>,
}

/// `POST /v1/nodes/{id}/opencode-profile` body — assign (or clear with
/// `null`) the profile a node should apply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssignOpencodeProfileRequest {
    pub profile_id: Option<String>,
}

/// Node pull response: the profile assigned to the calling node, or empty
/// when none is. Served from a small in-memory cache on the CP — this is a
/// hot path on error-threshold self-healing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActiveOpencodeConfigResponse {
    pub profile_id: Option<String>,
    pub hash: Option<String>,
    pub config: Option<serde_json::Value>,
    /// Bundle-pinned skill names for the node to reconcile against the trust
    /// ledger (absent/empty when no profile is assigned or none pinned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_skills: Option<Vec<String>>,
}

/// One row of the per-node apply audit (`GET /v1/nodes/{id}/opencode-audit`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OpencodeConfigAuditEntry {
    pub at: String,
    pub profile_id: Option<String>,
    pub hash: String,
    /// 'ws_push' | 'error_threshold' | 'interval' | 'startup'
    pub trigger: String,
    /// Outcome of the node-side `opencode debug config` oracle (migration
    /// 0069). Absent on pre-oracle nodes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify: Option<String>,
    /// Pinned skill names the node found untrusted this apply (item 10).
    /// Absent when the profile had no pins or the node did not reconcile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_untrusted: Option<Vec<String>>,
}

/// Per-attempt opencode override (plan C #4): the caller can pin a model /
/// small_model and/or pass an inline partial config for ONE task. The node
/// merges it over the profiled config and injects via
/// `OPENCODE_CONFIG_CONTENT`; the env var dies with the process, so the
/// override cannot leak into later attempts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct OpencodeOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub small_model: Option<String>,
    /// Partial opencode config object — merged shallowly over the node's
    /// active profile (override keys win).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
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
            security_profile: None,
            network_mode: None,
            group_id: None,
            agent_id: None,
            consensus_group_id: None,
            consensus_member: None,
            opencode_override: None,
        };
        assert_eq!(round_trip(&req), req);

        let ev = TaskEvent {
            attempt_id: "a1".into(),
            sequence: 3,
            r#type: EventType::Stdout,
            payload: serde_json::json!({"text": "hi"}),
            created_at: "2026-01-01T00:00:00Z".into(),
            ingest_id: 42,
        };
        assert_eq!(round_trip(&ev), ev);

        // Pre-0037 serialized event (no ingest_id) still parses to 0.
        let old = r#"{"attempt_id":"a1","sequence":3,"type":"stdout","payload":{"text":"hi"},"created_at":"2026-01-01T00:00:00Z"}"#;
        let parsed: TaskEvent = serde_json::from_str(old).unwrap();
        assert_eq!(parsed.ingest_id, 0);

        let pr = PollResponse {
            assignment: Some(Assignment {
                attempt_id: "a1".into(),
                fencing_token: "f1".into(),
                task_id: "t1".into(),
                repository: "demo".into(),
                prompt: "x".into(),
                adapter: "mock".into(),
                number: 1,
                timeout_secs: 3600,
                git_url: String::new(),
                default_branch: String::new(),
                validation_command: None,
                validation_timeout_secs: None,
                base_commit: None,
                parent_acp_session_id: None,
                network_mode: None,
                provenance: None,
                upstream_commits: vec![],
                upstream_task_ids: vec![],
                group_id: None,
                read_only: false,
                eval_cases: vec![],
                consensus_group_id: None,
                consensus_member: None,
                opencode_override: None,
            }),
            assignments: vec![],
        };
        assert_eq!(round_trip(&pr), pr);
        // Batch field round-trips and tolerates absence (N/N-1 compat).
        let legacy = r#"{"assignment":null}"#;
        let parsed: PollResponse = serde_json::from_str(legacy).unwrap();
        assert!(parsed.assignments.is_empty());
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
            permission_interception: "wrapper".into(),
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
            discovered_skills: vec![],
            account_usage: vec![],
            unsafe_active: false,
            permission_interception: "wrapper".into(),
            outbox_bytes: 0,
            artifact_spool_bytes: 0,
            outbox_rows: 0,
            outbox_oldest_pending_age_ms: 0,
            outbox_corruption_count: 0,
            outbox_completion_rows: 0,
            repo_lock_wait_ms: 0,
            sandbox_backend: "none".into(),
            enforced_limits: false,
            repo_cache_bytes: 0,
            workspace_bytes: 0,
            network_mode: "none".into(),
            applied_opencode_hash: None,
            active_rss_mib: 0,
            max_rss_mib: 0,
        };
        assert_eq!(round_trip(&hb), hb);
        let resp = EnrollResponse {
            node_id: "node-1".into(),
            credential: "secret".into(),
        };
        assert_eq!(round_trip(&resp), resp);
    }
}
