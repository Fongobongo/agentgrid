pub mod compress;
pub mod deploy_env;
pub mod policy;
pub mod runtime_config;
pub mod seal;
pub mod workflow;
pub mod ws;
pub use deploy_env::{
    deploy_env_fail_closed, deploy_env_from_process, parse_deploy_env, DeployEnv,
};
pub use policy::{
    AutonomyLevel, BuiltinPolicyProvider, CommandPolicyProvider, PolicyDecision, PolicyError,
    PolicyVerdict, RiskClass,
};
pub use runtime_config::{RuntimeConfig, RuntimeConfigError};
pub use workflow::{
    assembly_architect_prompt, assembly_collector_prompt, build_handoff_payload,
    compute_budget_usage, default_assembly_budget, parse_plan_steps, ratify_l4_schedule,
    render_handoff_block, AgentMessage, AgentMessageKind, AssemblyRequest, BudgetBreach,
    BudgetSnapshot, BudgetUsage, CreateWorkflowRequest, CreateWorkflowRunRequest, HandoffPackage,
    RoleRunStatus, StepProjection, WorkflowBudget, WorkflowProjection, WorkflowRole, WorkflowRun,
    WorkflowRunStatus, WorkflowRunWithSteps, WorkflowSchedule, WorkflowScheduleCreate,
    WorkflowStep, WorkflowStepRun, WorkflowStepStatus, WorkflowTemplate,
};

use serde::{Deserialize, Serialize};
pub mod approval;
pub mod cluster;
pub mod context;
pub mod mcp;
pub mod profile;
pub mod skills_trust;
mod state_machine;
pub mod version;

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
pub use version::{valid_version_req, version_matches_req};

mod dto;

#[cfg(test)]
mod tests;

pub use dto::*;
