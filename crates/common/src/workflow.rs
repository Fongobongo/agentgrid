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

/// Stage 13: a machine-readable plan emitted by an `expandable` architect
/// step, parsed into the worker steps the run should expand into on approval.
/// Only the structural subset needed for a WorkflowStep (`id`, `prompt`,
/// `depends_on`, `role`, optional `adapter`/`requested_node_id`/`retryable`/
/// `max_attempts`); unknown fields are ignored so the architect writer can
/// attach metadata. The architect step itself is NOT re-added here — the
/// caller keeps the original architect step succeeded and only inserts these
/// new worker steps as the run's remaining DAG.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PlanStep {
    id: String,
    prompt: String,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    role: PlanRole,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    adapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requested_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    retryable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    max_attempts: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PlanRole {
    Architect,
    #[default]
    Worker,
    Reviewer,
    Integrator,
    Verifier,
}

impl From<PlanRole> for WorkflowRole {
    fn from(r: PlanRole) -> Self {
        match r {
            PlanRole::Architect => WorkflowRole::Architect,
            PlanRole::Worker => WorkflowRole::Worker,
            PlanRole::Reviewer => WorkflowRole::Reviewer,
            PlanRole::Integrator => WorkflowRole::Integrator,
            PlanRole::Verifier => WorkflowRole::Verifier,
        }
    }
}

/// Stage 13: parse a machine-readable plan (YAML or JSON array of steps) into
/// validated `WorkflowStep`s. Returns the first structural violation as a
/// named error string, or the steps on success. Both YAML and JSON arrays are
/// accepted at the same API field — YAML dovetails with the existing
/// workflow-template language.
/// ponytail: splitting `serde_yaml` out for the architect's free-form plan is
/// speculative; reuse the canonical YAML deserializer via serde for both forms.
pub fn parse_plan_steps(plan: &str) -> Result<Vec<WorkflowStep>, String> {
    if plan.trim().is_empty() {
        return Err("plan is empty".into());
    }
    let parsed: Vec<PlanStep> = if plan.trim_start().starts_with('[') {
        serde_json::from_str(plan).map_err(|e| format!("json parse: {e}"))?
    } else {
        serde_yaml::from_str(plan).map_err(|e| format!("yaml parse: {e}"))?
    };
    if parsed.is_empty() {
        return Err("plan declares no steps".into());
    }
    let steps: Vec<WorkflowStep> = parsed
        .into_iter()
        .map(|p| WorkflowStep {
            id: p.id,
            prompt: p.prompt,
            depends_on: p.depends_on,
            role: WorkflowRole::from(p.role),
            adapter: p.adapter,
            requested_node_id: p.requested_node_id,
            retryable: p.retryable,
            max_attempts: p.max_attempts,
            // Stage 13: per-step base_commit is not part of an architect's
            // generated plan (the plan inherits the run's base_commit).
            base_commit: None,
            // Expanded worker steps are not themselves plan producers.
            expandable: Some(false),
        })
        .collect();
    // Re-validate the expanded DAG (no cycles, unique ids, no orphan deps).
    // Validate against only the new steps — the plan is the *full* set of
    // remaining steps, not added to the architect step.
    let tmp = WorkflowTemplate {
        id: "__plan__".into(),
        name: "__plan__".into(),
        steps: steps.clone(),
        budget: None,
        created_at: "".into(),
    };
    tmp.validate_dag()?;
    Ok(steps)
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
    /// Stage 13 plan expansion: an `expandable` architect step finished
    /// and produced a machine-readable plan (YAML/JSON) of the worker steps to
    /// run. The run pauses here pending a human's explicit approval
    /// (`POST /v1/workflow-runs/{id}/approve-plan`); once approved the plan is
    /// expanded into new workflow steps and the run resumes. Terminal-until-approval,
    /// like `Blocked`.
    PlanReady,
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

/// Stage 13 typed AgentMessage mailbox: orchestrator-mediated messages between
/// workflow steps (no free-form P2P). Only a small set of fixed kinds are
/// allowed; the payload carries a compact structured summary, never a full
/// transcript. The orchestrator emits an `output` message automatically when
/// a step succeeds; a consuming step renders all matching messages into its
/// prompt on activation.
/// ponytail: free-form `event_type` would be a backdoor to P2P; keep a fixed
/// enum and add a variant when a real use case appears.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentMessage {
    pub from_step_id: String,
    /// `"*"` = broadcast to all downstream steps sharing the sender's run.
    pub to_step_id: String,
    pub kind: AgentMessageKind,
    /// Compact structured summary (JSON object: {"summary":...}); already
    /// masked/trusted by the orchestrator before reaching a consumer.
    pub payload: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageKind {
    /// Compact output summary emitted by the orchestrator when a step succeeds
    /// (label + optional commit). The MVP never carries transcripts.
    #[default]
    Output,
    /// A machine-readable plan (an `expandable` architect's plan).
    Plan,
    /// A free-form note the orchestrator captured on a fail/repair escalation.
    Note,
}

impl AgentMessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AgentMessageKind::Output => "output",
            AgentMessageKind::Plan => "plan",
            AgentMessageKind::Note => "note",
        }
    }
}

/// Stage 13: parse `AgentMessageKind` from a snake_case stored tag.
/// ponytail: only used by the store's read path.
/// Stage 13: build the compact structured payload for an `output` handoff
/// message. References commit_sha + summary, never a full transcript. Encoded
/// as a single-line JSON string (`HandoffPackage`) so the mailbox stays
/// compact and grep-able.
/// ponytail: artifacts aren't stored yet (binary LargeObject API is deferred),
/// `artifacts` stays an empty array until a real artifact-store is wired.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HandoffPackage {
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
}

/// Stage 13: serialize a `HandoffPackage` as the payload for an AgentMessage.
/// Inverse: parse at the consumer (a typed renderer) — keeps transcripts out.
pub fn build_handoff_payload(
    summary: &str,
    commit_sha: Option<&str>,
    artifact_refs: &[String],
) -> String {
    let pkg = HandoffPackage {
        summary: summary.to_string(),
        commit_sha: commit_sha.map(|s| s.to_string()),
        artifacts: artifact_refs.to_vec(),
    };
    serde_json::to_string(&pkg).unwrap_or_else(|_| "{}".into())
}

impl std::str::FromStr for AgentMessageKind {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "output" => Ok(AgentMessageKind::Output),
            "plan" => Ok(AgentMessageKind::Plan),
            "note" => Ok(AgentMessageKind::Note),
            _ => Err(()),
        }
    }
}

/// Stage 13: render the messages a consuming step should see (its direct
/// dependencies that are now `Succeeded`) as a compact handoff block to prepend
/// to the step's prompt. Eliminates free-form P2P: the orchestrator mediates by
/// rendering typed messages from already-succeeded deps only; empty khi there
/// are none.
/// ponytail: a single contiguous render function is a tiny test surface.
pub fn render_handoff_block(prompt: &str, messages: &[AgentMessage]) -> String {
    if messages.is_empty() {
        return prompt.to_string();
    }
    let mut out = String::from("## Handoff from upstream steps\n");
    for m in messages {
        // Prefer the compact `HandoffPackage` JSON payload (summary + commit
        // reference + artifact refs); fall back to raw text for `plan`/`note`
        // kinds that don't use the package shape.
        let rendered = match serde_json::from_str::<HandoffPackage>(&m.payload) {
            Ok(pkg) => {
                let mut s = format!("- summary: {}\n", pkg.summary.trim());
                if let Some(sha) = pkg.commit_sha.as_deref() {
                    s.push_str(&format!("- commit: `{}`\n", sha));
                }
                if !pkg.artifacts.is_empty() {
                    s.push_str(&format!("- artifacts: {}\n", pkg.artifacts.join(", ")));
                }
                s
            }
            Err(_) => format!("{}\n", m.payload.trim()),
        };
        out.push_str(&format!(
            "### `{}`: {}\n{}",
            m.from_step_id,
            m.kind.as_str(),
            rendered
        ));
    }
    out.push('\n');
    out.push_str(prompt);
    out
}

/// Stage 13: compute a `BudgetUsage` snapshot from observable run state, for
/// `WorkflowBudget::check`. The caller passes the run's `created_at` unix
/// seconds (parse the RFC3339/ISO stored value), the count of tasks created so
/// far (one per step attempt = one round), and the current tick's unix time.
/// `messages`/`bytes`/`tokens`/`cost`/`repeated_handoffs` are left at 0 by
/// default — the caller sets them when the scheduler can observe them. The
/// `wall_seconds` proxy is `now - created_at`.
/// ponytail: rounds = task attempts is a coarse proxy for loop iterations; fine
/// until the adapter reports per-attempt message/token counts.
pub fn compute_budget_usage(created_at_unix: i64, task_count: u32, now_unix: i64) -> BudgetUsage {
    let wall = if now_unix > created_at_unix {
        (now_unix - created_at_unix) as u64
    } else {
        0
    };
    BudgetUsage {
        rounds: task_count,
        wall_seconds: wall,
        ..Default::default()
    }
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
    /// Stage 13 plan expansion: when set on an Architect step, the
    /// architect's emitted plan (YAML/JSON) ends a run in `PlanReady`, and the
    /// plan is expanded into new steps on approval
    /// (`POST /v1/workflow-runs/{id}/approve-plan`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expandable: Option<bool>,
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
                    expandable: None,
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
    fn compute_budget_usage_wall_and_rounds_proxy() {
        // Wall is now - created_at; rounds = task attempts.
        let u = compute_budget_usage(1000, 4, 1015);
        assert_eq!(u.wall_seconds, 15);
        assert_eq!(u.rounds, 4);
        assert_eq!(u.messages, 0, "messages proxy not yet observed");
        // Negative delta clamps to 0 (clock skew / badly set created_at).
        assert_eq!(compute_budget_usage(2000, 1, 1999).wall_seconds, 0);
        // Feeding that usage into a budget with a tight wall_seconds ceiling
        // trips at the proxy boundary.
        let b = WorkflowBudget {
            max_wall_seconds: Some(10),
            ..Default::default()
        };
        assert!(b.check(&compute_budget_usage(1000, 1, 1015)).is_some());
        assert!(b.check(&compute_budget_usage(1000, 1, 1009)).is_none());
    }

    #[test]
    fn ratify_l4_schedule_requires_budget_and_passes_lower_autonomy() {
        // A template with no budget: l4 schedule is refused (fail-closed),
        // lower autonomy passes.
        let no_budget = WorkflowTemplate {
            id: "t".into(),
            name: "x".into(),
            steps: vec![],
            budget: None,
            created_at: "".into(),
        };
        assert!(ratify_l4_schedule(&no_budget, "l4").is_err());
        assert!(ratify_l4_schedule(&no_budget, "l2").is_ok());
        assert!(ratify_l4_schedule(&no_budget, "l0").is_ok());
        // A template with a budget: l4 schedule is ratified.
        let with_budget = WorkflowTemplate {
            id: "t".into(),
            name: "x".into(),
            steps: vec![],
            budget: Some(WorkflowBudget {
                max_rounds: Some(5),
                ..Default::default()
            }),
            created_at: "".into(),
        };
        assert!(ratify_l4_schedule(&with_budget, "l4").is_ok());
    }

    #[test]
    fn parse_plan_steps_yaml_and_json_round_trip() {
        // YAML plan (one worker, one verifier depending on it).
        let yaml = "\
- id: w
  prompt: do work
  role: worker
- id: v
  prompt: verify
  depends_on: [w]
  role: verifier
";
        let steps = parse_plan_steps(yaml).expect("yaml plan parses");
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].id, "w");
        assert_eq!(steps[0].role, WorkflowRole::Worker);
        assert_eq!(steps[1].depends_on, vec!["w".to_string()]);
        assert_eq!(steps[1].role, WorkflowRole::Verifier);
        // JSON array form parses the same.
        let json = r#"[{"id":"a","prompt":"p","role":"worker"}]
"#;
        let s = parse_plan_steps(json).expect("json plan parses");
        assert_eq!(s[0].id, "a");
        // Empty plan is rejected.
        assert!(parse_plan_steps("").is_err());
        assert!(parse_plan_steps("[]").is_err());
        // Cyclic plan is rejected (validate_dag runs on the parsed steps).
        let cyc = "\
- id: a
  role: worker
  depends_on: [b]
- id: b
  role: worker
  depends_on: [a]
";
        assert!(parse_plan_steps(cyc).is_err());
    }

    #[test]
    fn render_handoff_block_injects_typed_messages_and_passes_when_empty() {
        // No messages => prompt unchanged.
        let out_empty = render_handoff_block("do work", &[]);
        assert_eq!(out_empty, "do work");
        // One output + one note => ordered block prepended.
        let msgs = vec![
            AgentMessage {
                from_step_id: "arch".into(),
                to_step_id: "*".into(),
                kind: AgentMessageKind::Output,
                payload: "summary: designed approach".into(),
            },
            AgentMessage {
                from_step_id: "w1".into(),
                to_step_id: "verifier".into(),
                kind: AgentMessageKind::Note,
                payload: "note: edge case X".into(),
            },
        ];
        let out = render_handoff_block("do work", &msgs);
        assert!(out.starts_with("## Handoff from upstream steps\n"));
        assert!(out.contains("### `arch`: output"));
        assert!(out.contains("summary: designed approach"));
        assert!(out.contains("### `w1`: note"));
        assert!(out.ends_with("do work"), "original prompt kept at the tail");
    }

    #[test]
    fn handoff_payload_references_commit_and_artifacts_not_transcripts() {
        // A HandoffPackage payload is a compact structured reference (summary +
        // commit_sha + artifact refs), never a transcript.
        let p = build_handoff_payload(
            "designed approach",
            Some("deadbeef"),
            &["art-1".to_string()],
        );
        let parsed: HandoffPackage = serde_json::from_str(&p).unwrap();
        assert_eq!(parsed.summary, "designed approach");
        assert_eq!(parsed.commit_sha.as_deref(), Some("deadbeef"));
        assert_eq!(parsed.artifacts, vec!["art-1"]);
        assert!(!p.contains("transcript"));
        // render_handoff_block unpacks the package fields, not the raw JSON.
        let m = AgentMessage {
            from_step_id: "arch".into(),
            to_step_id: "*".into(),
            kind: AgentMessageKind::Output,
            payload: p,
        };
        let out = render_handoff_block("do work", &[m]);
        assert!(out.contains("### `arch`: output"));
        assert!(out.contains("- summary: designed approach"));
        assert!(out.contains("- commit: `deadbeef`"));
        assert!(out.contains("- artifacts: art-1"));
    }

    #[test]
    fn agent_message_kind_round_trips_snake_case() {
        for k in [
            AgentMessageKind::Output,
            AgentMessageKind::Plan,
            AgentMessageKind::Note,
        ] {
            let s = k.as_str();
            assert_eq!(s.parse::<AgentMessageKind>().unwrap(), k);
        }
        assert!("bogus".parse::<AgentMessageKind>().is_err());
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
    /// Stage 13 plan expansion flag (mirrors `WorkflowStep.expandable`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expandable: Option<bool>,
    /// Attempts made so far for this step (Stage 8).
    #[serde(default)]
    pub attempts: u32,
    pub status: WorkflowStepStatus,
    pub created_at: String,
    /// Stage 11.6 follow-up: step timing for the span waterfall (set by
    /// `set_step_status` when the step transitions).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
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
    /// Stage 11.6 follow-up: step timing for the span waterfall. `started_at`
    /// is set when the step leaves pending for running; `finished_at` on a
    /// terminal transition. None until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
}

/// Live projection of a workflow run's Loop Engineering budget state: the
/// raw ceiling limits (if any), the observable usage fed to the check, and the
/// first ceiling breach (if the budget is tripped). `None` limits = unbounded.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    pub limits: WorkflowBudget,
    pub usage: BudgetUsage,
    pub breach: Option<BudgetBreach>,
}

/// Live projection of a workflow run for external (ACP) clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowProjection {
    pub run: WorkflowRun,
    pub steps: Vec<StepProjection>,
    /// Stage 13: budget snapshot for the run's template, if the template
    /// declares a `WorkflowBudget`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetSnapshot>,
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

/// Stage 13 L4 ratify: an `l4` schedule is a fully-autonomous trigger that can
/// run a workflow with no human in the loop. To keep the loop bounded we gate
/// it on the template having declared a `WorkflowBudget` (catches runaway
/// cost/rounds/wall) — and the node still routes the spawned tasks through the
/// configured command policy (external provider / default fail-closed `Ask`).
/// Returns `Ok(())` when ratify passes, `Err(reason)` otherwise. Non-l4
/// schedules always pass (human can approve the lower-autonomy runs).
/// ponytail: budget presence is the gating observable; the single secret id
/// and command-policy wiring already live on the node, so we don't re-decide
/// them here.
pub fn ratify_l4_schedule(template: &WorkflowTemplate, autonomy: &str) -> Result<(), String> {
    if autonomy != "l4" {
        return Ok(());
    }
    if template.budget.is_none() {
        return Err(
            "l4 schedule requires the template to declare a budget; refuses to create a \
             fully-autonomous trigger with no run budget"
                .to_string(),
        );
    }
    Ok(())
}

fn default_true() -> bool {
    true
}
