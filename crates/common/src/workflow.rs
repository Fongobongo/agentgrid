//! Workflow engine types (Stage 7). Shared between control plane and CLI.

use serde::{Deserialize, Serialize};

/// Role a workflow step runs as. The orchestrator can fan a step out across
/// these roles; v1 creates one role-run per step for its declared role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRole {
    /// Designs the approach / writes a plan. (Future: produces a spec task.)
    Architect,
    /// Implements the change. (Default.)
    #[default]
    Worker,
    /// Reviews a peer's output before integration.
    Reviewer,
    /// Merges worker results into an integration branch.
    Integrator,
    /// Checks the result against the acceptance criteria. (Future: runs the
    /// verification task and gates downstream steps on its outcome.)
    Verifier,
}

/// Lifecycle of a workflow run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowRunStatus {
    #[default]
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    /// Run is stuck awaiting human/repair resolution (e.g. an integrator
    /// merge conflict); it is terminal-but-not-failed (Stage 8 conflict policy).
    Blocked,
}

/// Lifecycle of an individual step within a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStepStatus {
    #[default]
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    Skipped,
    /// Step is stuck awaiting human/repair resolution (Stage 8 conflict policy).
    Blocked,
}

/// Lifecycle of a single role execution within a step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoleRunStatus {
    #[default]
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

/// One node in a workflow DAG. `depends_on` lists other step ids that must
/// finish before this step starts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub id: String,
    pub prompt: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub role: WorkflowRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adapter: Option<String>,
    /// Optional placement constraint: pin this step's task to a specific node
    /// (Stage 8: node affinity). `None` lets the scheduler pick any eligible node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_node_id: Option<String>,
    /// Optional exact commit all attempts of this step start from (Stage 8
    /// shared base_commit). Overrides the run-level `base_commit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// If `true`, a failed/lost attempt is retried up to `max_attempts`
    /// (Stage 8 lost-step recovery). Side-effectful steps must opt in; the
    /// default (unset) never auto-retries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    /// Max attempts including the first; default 1 (no retry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
}

/// A reusable workflow definition (the DAG).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowTemplate {
    pub id: String,
    pub name: String,
    pub steps: Vec<WorkflowStep>,
    pub created_at: String,
}

/// One execution of a template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: String,
    pub template_id: String,
    pub status: WorkflowRunStatus,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    /// Shared JSON context passed to every step (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Target repository the step tasks run against (optional; v1: whole run).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Shared base_commit for every step's attempts (Stage 8): parallel workers
    /// of one run start from the same commit. `None` => each task branches from
    /// its repository `default_branch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
}

/// A step instance inside a run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStepRun {
    pub id: String,
    pub run_id: String,
    pub step_id: String,
    pub prompt: String,
    pub depends_on: Vec<String>,
    pub role: WorkflowRole,
    pub adapter: Option<String>,
    /// Optional placement constraint (Stage 8): node this step's task is pinned to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_node_id: Option<String>,
    /// Exact commit this step's attempts start from (Stage 8), if pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    /// Retryable step? (Stage 8 lost-step recovery.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    /// Max attempts including the first (Stage 8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_attempts: Option<u32>,
    /// Attempts made so far for this step (Stage 8).
    #[serde(default)]
    pub attempts: u32,
    pub status: WorkflowStepStatus,
    pub created_at: String,
}

/// Request body for `POST /v1/workflows` — define a template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateWorkflowRequest {
    pub name: String,
    pub steps: Vec<WorkflowStep>,
    /// Default shared context JSON for runs of this template (optional).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// Request body for `POST /v1/workflows/{id}/runs`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateWorkflowRunRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Target repository for the step tasks (optional; defaults to none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// Shared base_commit for every step (optional; Stage 8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
}

/// `GET /v1/workflow-runs/{id}` response: the run plus its step instances.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRunWithSteps {
    pub run: WorkflowRun,
    pub steps: Vec<WorkflowStepRun>,
}

/// One step in a workflow projection (Stage 8 ACP plan projection): the live
/// view an external client gets — role, status, placement, the spawned task,
/// the node it is assigned to, and the latest attempt verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepProjection {
    pub step_id: String,
    pub role: WorkflowRole,
    pub status: WorkflowStepStatus,
    pub depends_on: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_node_id: Option<String>,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// Node the step's task is assigned to (None until assigned).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// Latest attempt verdict: `succeeded` | `failed` | `running` | `pending`.
    pub verdict: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

/// Live projection of a workflow run for external (ACP) clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowProjection {
    pub run: WorkflowRun,
    pub steps: Vec<StepProjection>,
}
