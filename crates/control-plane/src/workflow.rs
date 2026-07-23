//! Workflow DAG validation (Stage 7). Pure logic — no database, fully testable.

use agentgrid_common::WorkflowStep;
use std::collections::{HashMap, HashSet};

/// Why a workflow DAG is invalid. Returned by [`validate_workflow_dag`].
#[derive(Debug, PartialEq, Eq)]
pub enum DagError {
    /// No steps at all.
    Empty,
    /// Two steps share an id.
    DuplicateStep(String),
    /// A step depends on an id that does not exist.
    MissingDependency { step: String, depends_on: String },
    /// The dependency graph contains a cycle.
    Cycle(Vec<String>),
}

/// Validate a workflow DAG. Checks, in order:
/// 1. non-empty,
/// 2. unique step ids,
/// 3. every `depends_on` target exists,
/// 4. no cycles (Kahn's algorithm).
///
/// Returns `Ok(())` if valid, or the first error found.
pub fn validate_workflow_dag(steps: &[WorkflowStep]) -> Result<(), DagError> {
    if steps.is_empty() {
        return Err(DagError::Empty);
    }

    // 2. unique ids
    let mut ids = HashSet::with_capacity(steps.len());
    for s in steps {
        if !ids.insert(&s.id) {
            return Err(DagError::DuplicateStep(s.id.clone()));
        }
    }

    // 3. dependencies exist
    for s in steps {
        for dep in &s.depends_on {
            if !ids.contains(dep) {
                return Err(DagError::MissingDependency {
                    step: s.id.clone(),
                    depends_on: dep.clone(),
                });
            }
        }
    }

    // 4. cycle detection via Kahn's algorithm.
    let mut indeg: HashMap<&str, usize> = steps.iter().map(|s| (s.id.as_str(), 0)).collect();
    let mut adj: HashMap<&str, Vec<&str>> = HashMap::new();
    for s in steps {
        for dep in &s.depends_on {
            adj.entry(dep.as_str()).or_default().push(s.id.as_str());
            *indeg.get_mut(s.id.as_str()).unwrap() += 1;
        }
    }
    let mut queue: Vec<&str> = indeg
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(k, _)| *k)
        .collect();
    let mut visited = 0usize;
    while let Some(n) = queue.pop() {
        visited += 1;
        if let Some(nexts) = adj.get(n) {
            for m in nexts {
                let d = indeg.get_mut(m).unwrap();
                *d -= 1;
                if *d == 0 {
                    queue.push(*m);
                }
            }
        }
    }
    if visited != steps.len() {
        return Err(DagError::Cycle(
            steps.iter().map(|s| s.id.clone()).collect(),
        ));
    }

    // `role` is always valid (enum guarantees it); nothing more to check.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentgrid_common::WorkflowRole;

    fn step(id: &str, deps: &[&str]) -> WorkflowStep {
        WorkflowStep {
            id: id.into(),
            prompt: format!("do {id}"),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            role: WorkflowRole::Worker,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: None,
            max_attempts: None,
            expandable: None,
        }
    }

    #[test]
    fn empty_is_invalid() {
        assert_eq!(validate_workflow_dag(&[]), Err(DagError::Empty));
    }

    #[test]
    fn duplicate_ids_rejected() {
        let steps = vec![step("a", &[]), step("a", &[])];
        assert_eq!(
            validate_workflow_dag(&steps),
            Err(DagError::DuplicateStep("a".into()))
        );
    }

    #[test]
    fn missing_dependency_rejected() {
        let steps = vec![step("a", &["ghost"])];
        assert_eq!(
            validate_workflow_dag(&steps),
            Err(DagError::MissingDependency {
                step: "a".into(),
                depends_on: "ghost".into()
            })
        );
    }

    #[test]
    fn two_node_cycle_rejected() {
        let steps = vec![step("a", &["b"]), step("b", &["a"])];
        assert_eq!(
            validate_workflow_dag(&steps),
            Err(DagError::Cycle(vec!["a".into(), "b".into()]))
        );
    }

    #[test]
    fn self_loop_is_cycle() {
        let steps = vec![step("a", &["a"])];
        assert_eq!(
            validate_workflow_dag(&steps),
            Err(DagError::Cycle(vec!["a".into()]))
        );
    }

    #[test]
    fn diamond_is_valid() {
        // a -> b, a -> c, b -> d, c -> d
        let steps = vec![
            step("a", &[]),
            step("b", &["a"]),
            step("c", &["a"]),
            step("d", &["b", "c"]),
        ];
        assert_eq!(validate_workflow_dag(&steps), Ok(()));
    }

    #[test]
    fn linear_chain_is_valid() {
        let steps = vec![step("a", &[]), step("b", &["a"]), step("c", &["b"])];
        assert_eq!(validate_workflow_dag(&steps), Ok(()));
    }
}
