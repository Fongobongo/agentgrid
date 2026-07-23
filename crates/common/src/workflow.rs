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

/// Stage 13 Loop Engineering: a communication / execution budget attached to a
/// workflow template. Enforced at run time by the scheduler / loop tick (a
/// follow-up): when any ceiling is exceeded the run is parked `Blocked`
/// (waits approval), not killed. `max_repeated_handoffs` is the circuit breaker
/// threshold: if two consecutive steps hand off to each other more than this
/// many times, the loop trips (repeated identical handoff = runaway).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkflowBudget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_messages: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rounds: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_cents: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_wall_seconds: Option<u64>,
    /// Circuit breaker: max identical sequential handoffs before trip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_repeated_handoffs: Option<u32>,
}

impl WorkflowBudget {
    /// Pure check: does `usage` exceed any set ceiling? Returns the first
    /// ceiling breached (so the caller can park the run `Blocked`). `None` =
    /// within budget. Unset ceilings are never breached (treated as unbounded).
    pub fn check(&self, usage: &BudgetUsage) -> Option<BudgetBreach> {
        if let Some(m) = self.max_messages {
            if usage.messages > m {
                return Some(BudgetBreach {
                    field: "max_messages".into(),
                    limit: m as u64,
                    observed: usage.messages as u64,
                });
            }
        }
        if let Some(m) = self.max_rounds {
            if usage.rounds > m {
                return Some(BudgetBreach {
                    field: "max_rounds".into(),
                    limit: m as u64,
                    observed: usage.rounds as u64,
                });
            }
        }
        if let Some(m) = self.max_bytes {
            if usage.bytes > m {
                return Some(BudgetBreach {
                    field: "max_bytes".into(),
                    limit: m,
                    observed: usage.bytes,
                });
            }
        }
        if let Some(m) = self.max_tokens {
            if usage.tokens > m {
                return Some(BudgetBreach {
                    field: "max_tokens".into(),
                    limit: m,
                    observed: usage.tokens,
                });
            }
        }
        if let Some(m) = self.max_cost_cents {
            if usage.cost_cents > m {
                return Some(BudgetBreach {
                    field: "max_cost_cents".into(),
                    limit: m,
                    observed: usage.cost_cents,
                });
            }
        }
        if let Some(m) = self.max_wall_seconds {
            if usage.wall_seconds > m {
                return Some(BudgetBreach {
                    field: "max_wall_seconds".into(),
                    limit: m,
                    observed: usage.wall_seconds,
                });
            }
        }
        if let Some(m) = self.max_repeated_handoffs {
            if usage.repeated_handoffs > m {
                return Some(BudgetBreach {
                    field: "max_repeated_handoffs".into(),
                    limit: m as u64,
                    observed: usage.repeated_handoffs as u64,
                });
            }
        }
        None
    }
}

/// Observed usage fed to `WorkflowBudget::check`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BudgetUsage {
    pub messages: u32,
    pub rounds: u32,
    pub bytes: u64,
    pub tokens: u64,
    pub cost_cents: u64,
    pub wall_seconds: u64,
    pub repeated_handoffs: u32,
}

/// A single ceiling breach (which field fired + how far over).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BudgetBreach {
    pub field: String,
    pub limit: u64,
    pub observed: u64,
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
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub steps: Vec<WorkflowStep>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<WorkflowBudget>,
    #[serde(default)]
    pub created_at: String,
}

impl WorkflowTemplate {
    /// Parse a template from YAML (Stage 8 convenience; the control plane also
    /// accepts YAML bodies on `POST /v1/workflows`).
    pub fn from_yaml(s: &str) -> Result<Self, serde_yaml::Error> {
        serde_yaml::from_str(s)
    }

    /// Validate the step graph is a well-formed DAG (ADR 0004). Returns the
    /// first structural violation as a named error string, or `Ok(())`.
    /// Checks: unique ids, no self-dep, no orphan dep, acyclic.
    pub fn validate_dag(&self) -> Result<(), String> {
        let mut ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for s in &self.steps {
            if !ids.insert(s.id.as_str()) {
                return Err(format!("duplicate step id: {}", s.id));
            }
        }
        let known: std::collections::HashSet<&str> =
            self.steps.iter().map(|s| s.id.as_str()).collect();
        for s in &self.steps {
            for dep in &s.depends_on {
                if dep == &s.id {
                    return Err(format!("self-dependency: {}", s.id));
                }
                if !known.contains(dep.as_str()) {
                    return Err(format!("unknown dependency {dep} on step {}", s.id));
                }
            }
        }
        // Acyclic via DFS colour-mark (white/gray/black). A gray revisit = cycle.
        let adj: std::collections::HashMap<&str, &Vec<String>> = self
            .steps
            .iter()
            .map(|s| (s.id.as_str(), &s.depends_on))
            .collect();
        let mut color: std::collections::HashMap<&str, u8> = std::collections::HashMap::new(); // 0=white,1=gray,2=black
        for s in &self.steps {
            if color.get(s.id.as_str()).copied().unwrap_or(0) != 0 {
                continue;
            }
            let mut stack: Vec<(&str, usize)> = vec![(s.id.as_str(), 0)];
            while let Some(&(node, i)) = stack.last() {
                let deps = adj[node];
                if i == 0 {
                    color.insert(node, 1);
                }
                if i < deps.len() {
                    let next = deps[i].as_str();
                    stack.last_mut().unwrap().1 += 1;
                    let c = color.get(next).copied().unwrap_or(0);
                    if c == 1 {
                        return Err(format!("cycle detected via {node} -> {next}"));
                    }
                    if c == 0 {
                        stack.push((next, 0));
                    }
                } else {
                    color.insert(node, 2);
                    stack.pop();
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_round_trips_to_template() {
        let yaml = r#"
name: demo
steps:
  - id: plan
    prompt: plan it
    role: architect
  - id: work
    prompt: do it
    depends_on: [plan]
    role: worker
"#;
        let t = WorkflowTemplate::from_yaml(yaml).expect("yaml parses");
        assert_eq!(t.name, "demo");
        assert_eq!(t.steps.len(), 2);
        assert_eq!(t.steps[0].id, "plan");
        assert_eq!(t.steps[1].depends_on, vec!["plan".to_string()]);
        // Re-serialize to JSON to prove serde compatibility with the API shape.
        let json = serde_json::to_string(&t).unwrap();
        let back: WorkflowTemplate = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, t.name);
    }

    fn tmpl(steps: &[(&str, &[&str])]) -> WorkflowTemplate {
        WorkflowTemplate {
            id: "t".into(),
            name: "x".into(),
            created_at: "".into(),
            budget: None,
            steps: steps
                .iter()
                .map(|(id, deps)| WorkflowStep {
                    id: (*id).into(),
                    prompt: "".into(),
                    role: WorkflowRole::Worker,
                    depends_on: deps.iter().map(|s| (*s).to_string()).collect(),
                    adapter: None,
                    requested_node_id: None,
                    base_commit: None,
                    retryable: None,
                    max_attempts: None,
                })
                .collect(),
        }
    }

    #[test]
    fn validate_dag_accepts_simple_chain() {
        assert!(
            tmpl(&[("plan", &[]), ("work", &["plan"]), ("verify", &["work"])])
                .validate_dag()
                .is_ok()
        );
    }

    #[test]
    fn validate_dag_accepts_parallel_fan_out() {
        assert!(
            tmpl(&[("a", &[]), ("b", &["a"]), ("c", &["a"]), ("d", &["b", "c"])])
                .validate_dag()
                .is_ok()
        );
    }

    #[test]
    fn budget_check_no_breach_when_unset_or_within() {
        // No ceiling set => unbounded, never breaches.
        let b = WorkflowBudget::default();
        assert_eq!(
            b.check(&BudgetUsage {
                messages: 999,
                ..Default::default()
            }),
            None
        );
        // Within all set ceilings.
        let b = WorkflowBudget {
            max_messages: Some(10),
            max_tokens: Some(5000),
            max_repeated_handoffs: Some(3),
            ..Default::default()
        };
        let u = BudgetUsage {
            messages: 10,
            tokens: 4999,
            repeated_handoffs: 3,
            ..Default::default()
        };
        assert_eq!(
            b.check(&u),
            None,
            "equal-to-limit is NOT a breach (strict >)"
        );
    }

    #[test]
    fn budget_check_reports_first_breach() {
        let b = WorkflowBudget {
            max_messages: Some(5),
            max_rounds: Some(2),
            max_repeated_handoffs: Some(3),
            ..Default::default()
        };
        // Only max_messages is over.
        let u = BudgetUsage {
            messages: 6,
            rounds: 1,
            repeated_handoffs: 1,
            ..Default::default()
        };
        let breach = b.check(&u).expect("max_messages breached");
        assert_eq!(breach.field, "max_messages");
        assert_eq!(breach.limit, 5);
        assert_eq!(breach.observed, 6);
        // Circuit breaker trips on repeated handoffs over threshold.
        let u = BudgetUsage {
            messages: 1,
            rounds: 1,
            repeated_handoffs: 4,
            ..Default::default()
        };
        let breach = b.check(&u).expect("circuit tripped");
        assert_eq!(breach.field, "max_repeated_handoffs");
        assert_eq!(breach.observed, 4);
    }

    #[test]
    fn budget_round_trips_in_template_yaml() {
        let yaml = r#"
name: looped
budget:
  max_messages: 10
  max_rounds: 5
  max_repeated_handoffs: 3
steps:
  - id: a
    prompt: hi
    role: architect
"#;
        let t = WorkflowTemplate::from_yaml(yaml).expect("yaml parses");
        let b = t.budget.clone().expect("budget present");
        assert_eq!(b.max_messages, Some(10));
        assert_eq!(b.max_rounds, Some(5));
        assert_eq!(b.max_repeated_handoffs, Some(3));
        assert!(b.max_tokens.is_none(), "unset ceilings stay unset");
        // Re-serialize to JSON and back.
        let json = serde_json::to_string(&t).unwrap();
        let back: WorkflowTemplate = serde_json::from_str(&json).unwrap();
        assert_eq!(back.budget, t.budget);
    }

    #[test]
    fn validate_dag_rejects_duplicate_ids() {
        let e = tmpl(&[("a", &[]), ("a", &[])]).validate_dag().unwrap_err();
        assert!(e.contains("duplicate step id"));
    }

    #[test]
    fn validate_dag_rejects_self_dep() {
        let e = tmpl(&[("a", &["a"])]).validate_dag().unwrap_err();
        assert!(e.contains("self-dependency"));
    }

    #[test]
    fn validate_dag_rejects_orphan_dep() {
        let e = tmpl(&[("a", &["nope"])]).validate_dag().unwrap_err();
        assert!(e.contains("unknown dependency nope"));
    }

    #[test]
    fn validate_dag_rejects_direct_cycle() {
        let e = tmpl(&[("a", &["b"]), ("b", &["a"])])
            .validate_dag()
            .unwrap_err();
        assert!(e.contains("cycle detected"));
    }

    #[test]
    fn validate_dag_rejects_transitive_cycle() {
        let e = tmpl(&[("a", &["c"]), ("b", &["a"]), ("c", &["b"])])
            .validate_dag()
            .unwrap_err();
        assert!(e.contains("cycle detected"));
    }
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
    /// Stage 13 Loop Engineering: optional budget + circuit breaker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<WorkflowBudget>,
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

/// Stage 13: a scheduled trigger that creates a `WorkflowRun` of a template
/// on a fixed interval (seconds). A schedule carries the autonomy the runs
/// execute under — `l4` is only allowed when the template also declares a
/// command policy + budget (enforced at create time, Stage 13 follow-up: the
/// budget check; this lands the schedule infra).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSchedule {
    pub id: String,
    pub template_id: String,
    /// Interval between runs in seconds (>=1).
    pub interval_seconds: i64,
    /// Autonomy level for the runs this schedule spawns (`l0`..`l4`).
    #[serde(default = "default_autonomy_l2")]
    pub autonomy: String,
    /// ISO timestamp of the last run this schedule triggered, or empty.
    #[serde(default)]
    pub last_run_at: String,
    /// Whether the schedule is active (false = paused).
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub created_at: String,
}

/// Body for `POST /v1/workflows/{tid}/schedules`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowScheduleCreate {
    pub interval_seconds: i64,
    #[serde(default = "default_autonomy_l2")]
    pub autonomy: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_autonomy_l2() -> String {
    "l2".into()
}

fn default_true() -> bool {
    true
}
