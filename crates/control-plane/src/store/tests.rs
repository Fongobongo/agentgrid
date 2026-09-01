//! Tests extracted from store.rs (mod layout preserved).

#![allow(unused_imports)]
pub use super::*;

#[cfg(test)]
mod opaque_id_tests {
    use super::is_safe_opaque_id;
    #[test]
    fn accepts_uuid_and_safe_tokens() {
        assert!(is_safe_opaque_id("550e8400-e29b-41d4-a716-446655440000"));
        assert!(is_safe_opaque_id("01HXYZABCDEF0123456789"));
        assert!(is_safe_opaque_id("abc-123_def"));
    }
    #[test]
    fn rejects_traversal_and_separators() {
        assert!(!is_safe_opaque_id(".."));
        assert!(!is_safe_opaque_id("../etc"));
        assert!(!is_safe_opaque_id("a/b"));
        assert!(!is_safe_opaque_id("a\\b"));
        assert!(!is_safe_opaque_id("a.b"));
        assert!(!is_safe_opaque_id(""));
        assert!(!is_safe_opaque_id("has space"));
        assert!(!is_safe_opaque_id(&"x".repeat(65)));
    }
}

#[cfg(test)]
mod workflow_tests {
    use super::*;
    use agentgrid_common::{
        CompleteAttemptRequest, CreateTaskRequest, EnrollRequest, IncomingEvent,
        IngestEventsRequest, UploadArtifactRequest, WorkflowRole, WorkflowRunStatus, WorkflowStep,
        WorkflowStepStatus,
    };
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use uuid::Uuid;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    async fn temp_store() -> Store {
        // Disable critical disk watermark for tests (temp fs often has < 512 MB free).
        std::env::set_var("AGENTGRID_DISK_CRITICAL_MB", "0");
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // std::env::temp_dir() returns /tmp which doesn't exist on this system.
        // Use /var/tmp which is the actual temp directory.
        let temp_dir = std::env::temp_dir();
        let p = temp_dir.join(format!("ag-wf-{nanos}-{n}.db"));
        let _ = std::fs::remove_file(&p);
        let path_str = p.to_str().unwrap();
        Store::open(path_str).await.unwrap()
    }

    /// Seed a real node + task + attempt so FK-backed tables (migration 0040)
    /// accept the rows. Returns (node_id, task_id).
    async fn seed_task_attempt(s: &Store, task_id: &str, att_id: &str) -> (String, String) {
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node_id = s
            .enroll_node(&EnrollRequest {
                token,
                name: "n".into(),
                adapters: vec!["mock".into()],
                repositories: vec!["*".into()],
                max_concurrency: 2,
                agent_version: "test".into(),
                protocol_version: None,
                permission_interception: "wrapper".into(),
            })
            .await
            .unwrap()
            .unwrap()
            .node_id;
        sqlx::query(
            "INSERT INTO tasks (id, repository, prompt, adapter, status, created_at, timeout_secs) \
             VALUES (?, '', 'p', 'mock', 'queued', ?, 60)",
        )
        .bind(task_id)
        .bind(now_iso())
        .execute(&s.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO attempts (id, task_id, number, node_id, status, lease_expires_at, ack_deadline, started_at) \
             VALUES (?, ?, 1, ?, 'succeeded', ?, ?, ?)",
        )
        .bind(att_id)
        .bind(task_id)
        .bind(&node_id)
        .bind(now_iso())
        .bind(now_iso())
        .bind(now_iso())
        .execute(&s.pool)
        .await
        .unwrap();
        (node_id, task_id.to_string())
    }

    fn step(id: &str, deps: &[&str], role: WorkflowRole) -> WorkflowStep {
        WorkflowStep {
            id: id.into(),
            prompt: format!("do {id}"),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            role,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: None,
            max_attempts: None,
            expandable: None,
        }
    }

    #[tokio::test]
    async fn rejects_invalid_dag_on_create() {
        let s = temp_store().await;
        let bad = vec![step("a", &["b"], WorkflowRole::Worker)];
        assert!(s.create_workflow_template("x", &bad, &None).await.is_err());
    }

    #[tokio::test]
    async fn create_template_and_run_roundtrips() {
        let s = temp_store().await;
        let steps = vec![
            step("a", &[], WorkflowRole::Architect),
            step("b", &["a"], WorkflowRole::Worker),
            step("c", &["a"], WorkflowRole::Verifier),
        ];
        let tpl = s
            .create_workflow_template("build", &steps, &None)
            .await
            .unwrap();
        assert!(tpl.id.starts_with("wft-"));
        assert_eq!(tpl.steps.len(), 3);

        let got = s.get_workflow_template(&tpl.id).await.unwrap().unwrap();
        assert_eq!(got.steps.len(), 3);

        let run = s
            .create_workflow_run(&tpl.id, Some(r#"{"branch":"feat"}"#), None, None)
            .await
            .unwrap();
        assert_eq!(run.status, WorkflowRunStatus::Pending);
        assert_eq!(run.context.as_deref(), Some(r#"{"branch":"feat"}"#));

        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(run_got.id, run.id);

        let steps_run = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(steps_run.len(), 3);
        // Each step instance got one role-run; verify roles carried through.
        let roles: Vec<_> = steps_run.iter().map(|x| x.role).collect();
        assert!(roles.contains(&WorkflowRole::Architect));
        assert!(roles.contains(&WorkflowRole::Worker));
        assert!(roles.contains(&WorkflowRole::Verifier));

        let all = s.list_workflow_runs(None, None).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(
            s.list_workflow_templates(None, None).await.unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn unknown_template_rejected_on_run() {
        let s = temp_store().await;
        assert!(s
            .create_workflow_run("wft-nope", None, None, None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn tick_activates_ready_step_and_is_idempotent() {
        let s = temp_store().await;
        // Single ready step (no deps) -> first tick spawns its task.
        let tpl = s
            .create_workflow_template("one", &[step("a", &[], WorkflowRole::Worker)], &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1);
        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(run_got.status, WorkflowRunStatus::Running);
        let steps = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(steps[0].status, WorkflowStepStatus::Running);
        assert!(steps[0].adapter.is_none() || steps[0].adapter.is_some());
        // Second tick must not spawn another task (step already running).
        let again = s.tick_workflow_run(&run.id).await.unwrap();
        assert!(again.is_empty());
    }

    /// Plan 0.2 item 2.2: the background ticker and the complete_attempt
    // path can tick the same run concurrently. 20 simultaneous ticks must
    // still spawn exactly one task for the ready step (CAS, no duplicates).
    #[tokio::test]
    async fn concurrent_ticks_do_not_duplicate_step_tasks() {
        let s = temp_store().await;
        let tpl = s
            .create_workflow_template("conc", &[step("a", &[], WorkflowRole::Worker)], &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let mut handles = Vec::new();
        for _ in 0..20 {
            let st = s.clone();
            let id = run.id.clone();
            handles.push(tokio::spawn(async move { st.tick_workflow_run(&id).await }));
        }
        let mut spawned = 0usize;
        for h in handles {
            spawned += h.await.unwrap().unwrap().len();
        }
        assert_eq!(spawned, 1, "exactly one concurrent tick may spawn the step");
        let steps = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(steps.len(), 1);
        let task = s.step_task_id(&steps[0].id).await.unwrap();
        assert!(task.is_some(), "step bound to exactly one task");
    }

    #[tokio::test]
    async fn restart_does_not_duplicate_in_flight_workflow_step_tasks() {
        // line 487: a workflow run idempotently survives a "CP restart" — no
        // duplicate steps and no duplicate tasks. Steps: tick activates the only
        // ready step (a), printing its task id; a "restart" is modelled by
        // re-asking `running_workflow_run_ids` + ticking again before the task
        // finishes (must not re-spawn); then we complete a's task and confirm
        // the second tick advances to run Succeeded with exactly one step task id.
        let s = temp_store().await;
        let tpl = s
            .create_workflow_template("one-r", &[step("a", &[], WorkflowRole::Worker)], &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();

        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1, "tick spawns a single task");
        let first_task = s
            .step_task_id(&s.get_workflow_run_steps(&run.id).await.unwrap()[0].id)
            .await
            .unwrap();
        assert!(first_task.is_some(), "task bound to the step");

        // "CP restart": ticker re-lists in-flight runs and ticks; step is
        // already Running, so no duplicate task id is recorded.
        assert!(s
            .running_workflow_run_ids()
            .await
            .unwrap()
            .contains(&run.id));
        let again = s.tick_workflow_run(&run.id).await.unwrap();
        assert!(again.is_empty(), "restart tick does not re-spawn tasks");
        let still_first = s
            .step_task_id(&s.get_workflow_run_steps(&run.id).await.unwrap()[0].id)
            .await
            .unwrap();
        assert_eq!(still_first, first_task, "step still bound to the same task");

        // Node finishes the step task; tick advances the run to Succeeded with no new spawn.
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node = EnrollRequest {
            token,
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            agent_version: "test".into(),
            protocol_version: None,
            permission_interception: "wrapper".into(),
        };
        let node_id = s.enroll_node(&node).await.unwrap().expect("enroll").node_id;
        let a = s.try_assign(&node_id).await.unwrap().expect("assign");
        s.ack_attempt(&a.attempt_id).await.unwrap();
        s.complete_attempt(
            &a.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            },
        )
        .await
        .unwrap();
        let post = s.tick_workflow_run(&run.id).await.unwrap();
        assert!(post.is_empty(), "completion tick spawns no new tasks");
        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            run_got.status,
            WorkflowRunStatus::Succeeded,
            "run succeeds when step done",
        );
    }

    #[tokio::test]
    async fn step_requested_node_id_pins_task() {
        let s = temp_store().await;
        let steps = vec![agentgrid_common::WorkflowStep {
            id: "a".into(),
            prompt: "do a".into(),
            depends_on: vec![],
            role: WorkflowRole::Worker,
            adapter: None,
            requested_node_id: Some("node-pinned".into()),
            base_commit: None,
            retryable: None,
            max_attempts: None,
            expandable: None,
        }];
        let tpl = s
            .create_workflow_template("pin", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1);
        let task = s.show_task(&created[0]).await.unwrap().unwrap();
        assert_eq!(task.requested_node_id.as_deref(), Some("node-pinned"));
        let steps_run = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(
            steps_run[0].requested_node_id.as_deref(),
            Some("node-pinned")
        );
    }

    #[tokio::test]
    async fn workflow_run_carries_base_commit() {
        let s = temp_store().await;
        let steps = vec![step("a", &[], WorkflowRole::Worker)];
        let tpl = s
            .create_workflow_template("t", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), Some("deadbeef"))
            .await
            .unwrap();
        assert_eq!(run.base_commit.as_deref(), Some("deadbeef"));
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1);
        let task = s.show_task(&created[0]).await.unwrap().unwrap();
        assert_eq!(task.base_commit.as_deref(), Some("deadbeef"));
        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(run_got.base_commit.as_deref(), Some("deadbeef"));
    }

    #[tokio::test]
    async fn retryable_step_retries_then_succeeds() {
        let s = temp_store().await;
        let steps = vec![agentgrid_common::WorkflowStep {
            id: "a".into(),
            prompt: "do a".into(),
            depends_on: vec![],
            role: WorkflowRole::Worker,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: Some(true),
            max_attempts: Some(3),
            expandable: None,
        }];
        let tpl = s
            .create_workflow_template("retry", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();

        // Tick -> first task.
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1);
        // Assign + fail it; retryable step should respawn.
        let a1 = s.try_assign("n1").await.unwrap().unwrap();
        s.ack_attempt(&a1.attempt_id).await.unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("agent_failed".into()),
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            },
        )
        .await
        .unwrap();
        let created2 = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created2.len(), 1, "retryable step must respawn a task");
        let steps_run = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(steps_run[0].attempts, 1);
        // Assign + succeed the retry.
        let a2 = s.try_assign("n1").await.unwrap().unwrap();
        s.ack_attempt(&a2.attempt_id).await.unwrap();
        s.complete_attempt(
            &a2.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            },
        )
        .await
        .unwrap();
        s.tick_workflow_run(&run.id).await.unwrap();
        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(run_got.status, WorkflowRunStatus::Succeeded);
    }

    #[tokio::test]
    async fn integrator_failure_blocks_run_not_failed() {
        let s = temp_store().await;
        let steps = vec![step("a", &[], WorkflowRole::Integrator)];
        let tpl = s
            .create_workflow_template("integ", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1);
        let a1 = s.try_assign("n1").await.unwrap().unwrap();
        s.ack_attempt(&a1.attempt_id).await.unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("merge_conflict".into()),
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            },
        )
        .await
        .unwrap();
        s.tick_workflow_run(&run.id).await.unwrap();
        let steps_run = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(
            steps_run[0].status,
            WorkflowStepStatus::Blocked,
            "integrator failure must block, not fail"
        );
        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            run_got.status,
            WorkflowRunStatus::Blocked,
            "run must be blocked, not failed"
        );
    }

    #[tokio::test]
    async fn integrator_assignment_carries_upstream_worker_commits() {
        // line 239: an integrator step's assignment lists the winning commit
        // SHAs of its dependency steps under `upstream_commits` so the node can
        // land them as an integration branch. Modeled end-to-end in the store:
        // two parallel workers complete with commit SHAs, then tick activates
        // the integrator step; `try_assign` must surface both SHAs.
        let s = temp_store().await;
        let steps = vec![
            step("w1", &[], WorkflowRole::Worker),
            step("w2", &[], WorkflowRole::Worker),
            step("int", &["w1", "w2"], WorkflowRole::Integrator),
        ];
        let tpl = s
            .create_workflow_template("int", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 4,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();

        // activate w1 + w2.
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 2, "both parallel workers activate");
        let _ = created; // consume

        // Complete worker 1 with a commit sha.
        let a1 = s.try_assign("n1").await.unwrap().unwrap();
        s.ack_attempt(&a1.attempt_id).await.unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: Some("sha-worker-1".into()),
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            },
        )
        .await
        .unwrap();

        // Complete worker 2 with a commit sha.
        let a2 = s.try_assign("n1").await.unwrap().unwrap();
        s.ack_attempt(&a2.attempt_id).await.unwrap();
        s.complete_attempt(
            &a2.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: Some("sha-worker-2".into()),
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            },
        )
        .await
        .unwrap();

        // Workers done. status_by_id is updated as steps transition inside the
        // loop (plan 534 fix), so ONE tick both advances the workers to
        // Succeeded and activates the pending integrator whose deps are now met.
        let act = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(act.len(), 1, "integrator activates after workers succeeded");

        // try_assign the integrator task and confirm upstream_commits is set.
        let int_a = s.try_assign("n1").await.unwrap().unwrap();
        let mut got = int_a.upstream_commits.clone();
        got.sort();
        assert_eq!(
            got,
            vec!["sha-worker-1".to_string(), "sha-worker-2".to_string()],
            "integrator carries upstream worker commit SHAs",
        );
        // Stage 8 / line 257: parallel task_ids are also surfaced so the node
        // can fetch each worker's changes.patch artifact as a fallback when
        // the SHA is not reachable via a shared Git remote.
        assert_eq!(
            int_a.upstream_commits.len(),
            int_a.upstream_task_ids.len(),
            "upstream_task_ids parallel to upstream_commits",
        );
        assert!(
            !int_a.upstream_task_ids.is_empty(),
            "integrator carries upstream worker task ids",
        );
    }

    #[tokio::test]
    async fn verifier_assignment_carries_upstream_worker_commit_for_isolation() {
        // line 240: an independent verifier step should start from the worker's
        // commit (so it can review the change) but never see the worker's
        // private transcripts. Modeling: verifier's `upstream_commits` carries
        // the worker's winning SHA (cherry-pick lands the worker tree on the
        // verifier's base) — the handoff block only references the SHA + summary,
        // never the transcript, so isolation holds by construction.
        let s = temp_store().await;
        let steps = vec![
            step("w1", &[], WorkflowRole::Worker),
            step("ver", &["w1"], WorkflowRole::Verifier),
        ];
        let tpl = s
            .create_workflow_template("v", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();

        // Activate + complete the worker with a commit.
        let _ = s.tick_workflow_run(&run.id).await.unwrap();
        let a = s.try_assign("n1").await.unwrap().unwrap();
        s.ack_attempt(&a.attempt_id).await.unwrap();
        s.complete_attempt(
            &a.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: Some("sha-worker-1".into()),
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            },
        )
        .await
        .unwrap();
        // One tick: worker -> Succeeded and verifier activates in the same pass
        // (status_by_id updates in-loop, plan 534 fix).
        let act = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(act.len(), 1, "verifier activates after worker succeeded");

        let v = s.try_assign("n1").await.unwrap().unwrap();
        assert_eq!(
            v.upstream_commits,
            vec!["sha-worker-1".to_string()],
            "verifier carries the worker's winning commit SHA (no transcript)",
        );
        assert_eq!(
            v.upstream_task_ids.len(),
            1,
            "verifier carries the upstream worker task id for patch fallback",
        );
    }

    #[tokio::test]
    async fn retryable_step_exhausting_repair_budget_escalates_blocked() {
        // Stage 13 repair escalation: a `retryable` step that exhausts its
        // `max_attempts` escalates to a human (run `Blocked`) instead of
        // hard-failing the run. A non-retryable worker still fails fast.
        let s = temp_store().await;
        let steps_retry = vec![agentgrid_common::WorkflowStep {
            id: "a".into(),
            prompt: "do a".into(),
            depends_on: vec![],
            role: WorkflowRole::Worker,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: Some(true),
            max_attempts: Some(2),
            expandable: None,
        }];
        let tpl = s
            .create_workflow_template("rep", &steps_retry, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();

        // attempt 1 -> fail
        s.tick_workflow_run(&run.id).await.unwrap();
        let a1 = s.try_assign("n1").await.unwrap().unwrap();
        s.ack_attempt(&a1.attempt_id).await.unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("agent_failed".into()),
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            },
        )
        .await
        .unwrap();
        s.tick_workflow_run(&run.id).await.unwrap();
        // attempt 2 -> fail (exhausts max_attempts=2)
        let a2 = s.try_assign("n1").await.unwrap().unwrap();
        s.ack_attempt(&a2.attempt_id).await.unwrap();
        s.complete_attempt(
            &a2.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("agent_failed".into()),
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            },
        )
        .await
        .unwrap();
        s.tick_workflow_run(&run.id).await.unwrap();
        // Repair budget exhausted -> step Blocked (escalation), run Blocked.
        let rs = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(rs[0].status, WorkflowStepStatus::Blocked, "escalation");
        let after = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            after.status,
            WorkflowRunStatus::Blocked,
            "escalation parks the run"
        );

        // Sanity: a non-retryable worker fails the run outright on the first
        // attempt (fast fail).
        let steps_hard = vec![agentgrid_common::WorkflowStep {
            id: "h".into(),
            prompt: "do h".into(),
            depends_on: vec![],
            role: WorkflowRole::Worker,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: Some(false),
            max_attempts: Some(1),
            expandable: None,
        }];
        let tpl2 = s
            .create_workflow_template("hard", &steps_hard, &None)
            .await
            .unwrap();
        let run2 = s
            .create_workflow_run(&tpl2.id, None, Some("demo"), None)
            .await
            .unwrap();
        s.tick_workflow_run(&run2.id).await.unwrap();
        let b1 = s.try_assign("n1").await.unwrap().unwrap();
        s.ack_attempt(&b1.attempt_id).await.unwrap();
        s.complete_attempt(
            &b1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("agent_failed".into()),
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            },
        )
        .await
        .unwrap();
        s.tick_workflow_run(&run2.id).await.unwrap();
        let rs2 = s.get_workflow_run_steps(&run2.id).await.unwrap();
        assert_eq!(rs2[0].status, WorkflowStepStatus::Failed, "fast fail");
        let after2 = s.get_workflow_run(&run2.id).await.unwrap().unwrap();
        assert_eq!(after2.status, WorkflowRunStatus::Failed);
    }

    #[tokio::test]
    async fn approval_timeout_blocks_linked_step() {
        let s = temp_store().await;
        let steps = vec![step("a", &[], WorkflowRole::Architect)];
        let tpl = s
            .create_workflow_template("ap", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();
        let _ = s.tick_workflow_run(&run.id).await.unwrap();
        let a1 = s.try_assign("n1").await.unwrap().unwrap();
        let steps_run = s.get_workflow_run_steps(&run.id).await.unwrap();
        let step_id = steps_run[0].id.clone();
        // Approval already expired, linked to the running step.
        let _ = s
            .create_approval(
                &a1.task_id,
                &a1.attempt_id,
                None,
                "run Bash",
                -10,
                Some(&step_id),
                "step",
            )
            .await
            .unwrap();
        let n = s.tick_approval_expiry().await.unwrap();
        assert_eq!(n, 1, "one approval should expire");
        let steps_run = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(
            steps_run[0].status,
            WorkflowStepStatus::Blocked,
            "timed-out approval must block the step, not hang"
        );
        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            run_got.status,
            WorkflowRunStatus::Blocked,
            "run must be blocked, not left hanging"
        );
    }

    #[tokio::test]
    async fn worker_failure_still_fails_run() {
        let s = temp_store().await;
        let steps = vec![step("a", &[], WorkflowRole::Worker)];
        let tpl = s
            .create_workflow_template("w", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1);
        let a1 = s.try_assign("n1").await.unwrap().unwrap();
        s.ack_attempt(&a1.attempt_id).await.unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: Some("agent_failed".into()),
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            },
        )
        .await
        .unwrap();
        s.tick_workflow_run(&run.id).await.unwrap();
        let steps_run = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(steps_run[0].status, WorkflowStepStatus::Failed);
        let run_got = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(run_got.status, WorkflowRunStatus::Failed);
    }

    #[tokio::test]
    async fn workflow_run_projection_exposes_roles_nodes_verdicts() {
        let s = temp_store().await;
        let steps = vec![
            step("arch", &[], WorkflowRole::Architect),
            step("work", &["arch"], WorkflowRole::Worker),
        ];
        let tpl = s
            .create_workflow_template("p", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let poll = agentgrid_common::PollRequest {
            node_id: "n1".into(),
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            protocol_version: None,
        };
        s.register_or_touch_node(&poll).await.unwrap();
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1);
        let a1 = s.try_assign("n1").await.unwrap().unwrap();
        s.ack_attempt(&a1.attempt_id).await.unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            },
        )
        .await
        .unwrap();
        // Tick until the worker (dependent on arch) is spawned.
        for _ in 0..4 {
            s.tick_workflow_run(&run.id).await.unwrap();
        }

        let proj = s
            .get_workflow_run_projection(&run.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(proj.steps.len(), 2);
        let arch = proj.steps.iter().find(|x| x.step_id == "arch").unwrap();
        assert_eq!(arch.role, WorkflowRole::Architect);
        assert_eq!(arch.verdict, "succeeded");
        assert_eq!(arch.node_id.as_deref(), Some("n1"));
        assert!(arch.task_id.is_some());
        // Stage 11.6: timing lands on transitions for the span waterfall.
        assert!(arch.started_at.is_some(), "started_at set when step ran");
        assert!(arch.finished_at.is_some(), "finished_at set on terminal");
        let work = proj.steps.iter().find(|x| x.step_id == "work").unwrap();
        assert_eq!(work.role, WorkflowRole::Worker);
        assert!(work.task_id.is_some(), "worker task should be spawned");
        assert_eq!(work.node_id, None, "worker not assigned yet");
    }

    #[tokio::test]
    async fn workflow_projection_surfaces_budget_snapshot_when_template_has_budget() {
        // Stage 13 Loop Engineering: a projection of a run whose template
        // declares a budget carries a `BudgetSnapshot` with the observable
        // usage and a breach once a ceiling is exceeded. A template with no
        // budget yields no snapshot.
        let s = temp_store().await;
        let steps = vec![step("a", &[], WorkflowRole::Worker)];
        // No budget -> snapshot is None.
        let tpl_none = s
            .create_workflow_template("nobud", &steps, &None)
            .await
            .unwrap();
        let run_none = s
            .create_workflow_run(&tpl_none.id, None, Some("demo"), None)
            .await
            .unwrap();
        let proj_none = s
            .get_workflow_run_projection(&run_none.id)
            .await
            .unwrap()
            .unwrap();
        assert!(proj_none.budget.is_none(), "no budget => no snapshot");

        // With max_rounds = 0 the first tick starts the single root step
        // (rounds pre-checked at 0), and the second tick breaches.
        let budget = WorkflowBudget {
            max_rounds: Some(0),
            ..Default::default()
        };
        let tpl = s
            .create_workflow_template("looped", &steps, &Some(budget))
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        s.tick_workflow_run(&run.id).await.unwrap();
        // Snapshot mid-run before the breach fires: no breach yet.
        let mid = s
            .get_workflow_run_projection(&run.id)
            .await
            .unwrap()
            .unwrap();
        let snap = mid.budget.expect("budget template -> snapshot present");
        assert_eq!(snap.limits.max_rounds, Some(0));
        assert_eq!(snap.usage.rounds, 1, "one task started => rounds=1");
        // Rounds=1 > 0 => breach.
        assert!(snap.breach.is_some(), "rounds 1 > 0 must breach");
        assert_eq!(snap.breach.as_ref().unwrap().field, "max_rounds");
        // Tick again parks the run Blocked (enforcement path).
        s.tick_workflow_run(&run.id).await.unwrap();
        let after = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(after.status, WorkflowRunStatus::Blocked);
    }

    #[tokio::test]
    async fn backup_round_trips() {
        let s = temp_store().await;
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Hardened backup_to only accepts a plain file name and confines the
        // output to the data dir (parent of the artifact root).
        let name = format!("ag-backup-{stamp}.db");
        assert!(
            s.backup_to(std::env::temp_dir().join("evil.db").to_str().unwrap(),)
                .await
                .is_err(),
            "absolute paths must be rejected"
        );
        assert!(
            s.backup_to("../evil.db").await.is_err(),
            "path separators must be rejected"
        );
        let backup = s.artifact_root().parent().unwrap().join(&name);
        if backup.exists() {
            let _ = std::fs::remove_file(&backup);
        }
        s.backup_to(&name).await.unwrap();
        assert!(backup.exists(), "VACUUM INTO must create the backup file");
        // Re-opening the backup must succeed and yield a usable store.
        let reopened = Store::open(backup.to_str().unwrap()).await.unwrap();
        assert_eq!(reopened.user_count().await.unwrap(), 0);
        let _ = std::fs::remove_file(&backup);
    }

    #[tokio::test]
    async fn cleanup_old_artifacts() {
        let s = temp_store().await;
        // FK-valid attempt (migration 0040) so the artifacts FK accepts rows.
        let (_node_id, _task_id) = seed_task_attempt(&s, "task-att1", "att-1").await;
        // Hardening P1 item 15: plant the backing files so we can assert the
        // reaped row's file is unlinked while the kept row's file survives.
        let old_path = s.artifact_path("att-1", "old.txt").unwrap();
        let new_path = s.artifact_path("att-1", "new.txt").unwrap();
        tokio::fs::create_dir_all(old_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&old_path, b"old").await.unwrap();
        tokio::fs::write(&new_path, b"new").await.unwrap();
        sqlx::query(
            "INSERT INTO artifacts (id, attempt_id, name, size_bytes, stored_at) VALUES (?,?,?,?,?)",
        )
        .bind("a-new")
        .bind("att-1")
        .bind("new.txt")
        .bind(3)
        .bind(now_iso())
        .execute(&s.pool)
        .await
        .unwrap();
        let old = iso_plus_secs(-(200 * 3600));
        sqlx::query(
            "INSERT INTO artifacts (id, attempt_id, name, size_bytes, stored_at) VALUES (?,?,?,?,?)",
        )
        .bind("a-old")
        .bind("att-1")
        .bind("old.txt")
        .bind(3)
        .bind(&old)
        .execute(&s.pool)
        .await
        .unwrap();
        let removed = s.cleanup_artifacts(168).await.unwrap();
        assert_eq!(removed, 1, "only the 200h-old artifact should be reaped");
        let remaining = sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM artifacts")
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(remaining, 1);
        // File-level invariant: the reaped artifact's file is gone; the kept
        // artifact's file survives.
        assert!(
            !tokio::fs::try_exists(&old_path).await.unwrap(),
            "reaped artifact file must be deleted"
        );
        assert!(
            tokio::fs::try_exists(&new_path).await.unwrap(),
            "kept artifact file must survive"
        );
    }

    #[tokio::test]
    async fn scheduler_records_latency_metric() {
        let s = temp_store().await;
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node = EnrollRequest {
            token,
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            agent_version: "test".into(),
            protocol_version: None,
            permission_interception: "wrapper".into(),
        };
        let resp = s.enroll_node(&node).await.unwrap().expect("node enroll");
        let node_id = resp.node_id;
        let task = CreateTaskRequest {
            prompt: "do".into(),
            repository: String::new(),
            adapter: "mock".into(),
            requested_node_id: None,
            timeout_secs: Some(60),
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
            github_push: false,
            github_repo: None,
            github_issue: None,
            github_base_ref: None,
            max_attempts: 1,
            consensus_mode: None,
            review_of: None,
        };
        let _ = s.create_task(&task).await.unwrap();
        let before = s
            .scheduler_assignments
            .load(std::sync::atomic::Ordering::Relaxed);
        let assigned = s.try_assign(&node_id).await.unwrap();
        assert!(assigned.is_some(), "task should be assigned to the node");
        let after = s
            .scheduler_assignments
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            after,
            before + 1,
            "an assignment must increment the scheduler metric"
        );
    }

    /// Competitor-gap feature (GitHub write-back): create + show round-trips
    /// the new task columns (and the assignment carries them to the node).
    #[tokio::test]
    async fn github_writeback_fields_round_trip() {
        let s = temp_store().await;
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node = EnrollRequest {
            token,
            name: "gh-node".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 2,
            agent_version: "test".into(),
            protocol_version: None,
            permission_interception: "wrapper".into(),
        };
        let node_id = s.enroll_node(&node).await.unwrap().expect("enroll").node_id;
        let view = s
            .create_task(&CreateTaskRequest {
                prompt: "push me".into(),
                repository: String::new(),
                adapter: "mock".into(),
                requested_node_id: None,
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
                github_push: true,
                github_repo: Some("acme/demo".into()),
                github_issue: Some(7),
                github_base_ref: Some("develop".into()),
                max_attempts: 1,
                consensus_mode: None,
                review_of: None,
            })
            .await
            .unwrap();
        assert_eq!(view.github_repo.as_deref(), Some("acme/demo"));
        assert_eq!(view.github_issue, Some(7));

        // show_task echoes the same columns.
        let shown = s.show_task(&view.id).await.unwrap().unwrap();
        assert_eq!(shown.github_repo.as_deref(), Some("acme/demo"));
        assert_eq!(shown.github_issue, Some(7));
        assert_eq!(shown.github_base_ref.as_deref(), Some("develop"));

        // The assignment ships the metadata to the node.
        let a = s.try_assign(&node_id).await.unwrap().unwrap();
        assert!(a.github_push);
        assert_eq!(a.github_repo.as_deref(), Some("acme/demo"));
        assert_eq!(a.github_issue, Some(7));
        assert_eq!(a.github_base_ref.as_deref(), Some("develop"));
    }

    // Plan 0.3 item 1.2: a batch of 100 assignments lands in ONE write
    // transaction (was: one BEGIN IMMEDIATE per assignment).
    #[tokio::test]
    async fn assign_batch_hundred_tasks_one_write_txn() {
        // Plan 2.14 (#27) pressure-gate would refuse 100 concurrent attempts
        // (100 × 256 MiB forecast > 1 GiB default ceiling); the test rigs the
        // scenario by disabling the gate locally.
        std::env::set_var("AGENTGRID_CAPACITY_PRESSURE", "0");
        let s = temp_store().await;
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node = EnrollRequest {
            token,
            name: "batch-node".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 100,
            agent_version: "test".into(),
            protocol_version: None,
            permission_interception: "wrapper".into(),
        };
        let node_id = s.enroll_node(&node).await.unwrap().expect("enroll").node_id;
        for i in 0..100 {
            s.create_task(&CreateTaskRequest {
                prompt: format!("task {i}"),
                repository: String::new(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: Some(60),
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
                github_push: false,
                github_repo: None,
                github_issue: None,
                github_base_ref: None,
                max_attempts: 1,
                consensus_mode: None,
                review_of: None,
            })
            .await
            .unwrap();
        }
        let txns_before = s.write_txn_count();
        let batch = s.try_assign_batch(&node_id, 100).await.unwrap();
        let txns_after = s.write_txn_count();
        assert_eq!(batch.len(), 100, "all 100 tasks assigned in one batch");
        assert_eq!(
            txns_after - txns_before,
            1,
            "the batch must be a single write transaction"
        );
        assert_eq!(write_txn_stats().1, 0, "no write-lock failures");
    }

    /// Double-assign must be impossible even when many nodes race the same
    /// pending task concurrently: exactly one `try_assign` may win, and the
    /// loser polls must observe no assignable work.
    #[tokio::test]
    async fn concurrent_nodes_never_double_assign_one_task() {
        use std::sync::Arc;
        let s = Arc::new(temp_store().await);
        let mut node_ids = Vec::new();
        for i in 0..8 {
            let (token, _) = s.create_enrollment_token().await.unwrap();
            let node = EnrollRequest {
                token,
                name: format!("race-node-{i}"),
                adapters: vec!["mock".into()],
                repositories: vec!["*".into()],
                max_concurrency: 4,
                agent_version: "test".into(),
                protocol_version: None,
                permission_interception: "wrapper".into(),
            };
            node_ids.push(s.enroll_node(&node).await.unwrap().expect("enroll").node_id);
        }
        s.create_task(&CreateTaskRequest {
            prompt: "one task".into(),
            repository: String::new(),
            adapter: "mock".into(),
            requested_node_id: None,
            timeout_secs: Some(60),
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
            github_push: false,
            github_repo: None,
            github_issue: None,
            github_base_ref: None,
            max_attempts: 1,
            consensus_mode: None,
            review_of: None,
        })
        .await
        .unwrap();
        let mut handles = Vec::new();
        for node_id in node_ids {
            let s2 = Arc::clone(&s);
            handles.push(tokio::spawn(async move { s2.try_assign(&node_id).await }));
        }
        let mut winners = Vec::new();
        for h in handles {
            if let Some(a) = h.await.unwrap().unwrap() {
                winners.push(a.attempt_id);
            }
        }
        assert_eq!(winners.len(), 1, "exactly one node may win the task");
    }

    #[tokio::test]
    async fn cancel_workflow_run_cancels_steps_and_tasks() {
        let s = temp_store().await;
        let steps = vec![WorkflowStep {
            id: "a".into(),
            prompt: "do".into(),
            depends_on: vec![],
            role: WorkflowRole::Worker,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: None,
            max_attempts: None,
            expandable: None,
        }];
        let t = s
            .create_workflow_template("t", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&t.id, None, None, None)
            .await
            .unwrap();
        // Link the step to a queued task, then cancel the whole run.
        let task_id = "task-x";
        sqlx::query(
            "INSERT INTO tasks (id, repository, prompt, adapter, status, created_at, timeout_secs) \
             VALUES (?, '', 'p', 'mock', 'queued', ?, 60)",
        )
        .bind(task_id)
        .bind(now_iso())
        .execute(&s.pool)
        .await
        .unwrap();
        let step_run_id: String =
            sqlx::query_scalar("SELECT id FROM workflow_steps WHERE run_id = ?")
                .bind(&run.id)
                .fetch_one(&s.pool)
                .await
                .unwrap();
        sqlx::query("INSERT INTO role_runs (id, step_run_id, task_id, role, created_at) VALUES (?, ?, ?, 'Worker', ?)")
            .bind(Uuid::new_v4().to_string())
            .bind(&step_run_id)
            .bind(task_id)
            .bind(now_iso())
            .execute(&s.pool)
            .await
            .unwrap();
        assert!(s.cancel_workflow_run(&run.id).await.unwrap());
        let run_status: String =
            sqlx::query_scalar("SELECT status FROM workflow_runs WHERE id = ?")
                .bind(&run.id)
                .fetch_one(&s.pool)
                .await
                .unwrap();
        assert_eq!(run_status, "cancelled");
        let step_status: String =
            sqlx::query_scalar("SELECT status FROM workflow_steps WHERE id = ?")
                .bind(&step_run_id)
                .fetch_one(&s.pool)
                .await
                .unwrap();
        assert_eq!(step_status, "cancelled");
        let task_status: String = sqlx::query_scalar("SELECT status FROM tasks WHERE id = ?")
            .bind(task_id)
            .fetch_one(&s.pool)
            .await
            .unwrap();
        assert_eq!(task_status, "cancelled");
        // Already terminal: cancelling again is a no-op.
        assert!(!s.cancel_workflow_run(&run.id).await.unwrap());
    }

    #[tokio::test]
    async fn reconcile_on_startup_runs_maintenance_and_audits() {
        let s = temp_store().await;
        // No in-flight attempts: reconcile is a clean no-op that still audits.
        s.reconcile_on_startup().await.unwrap();
        let audits = s.list_audit(None, 100).await.unwrap();
        assert!(audits.iter().any(|a| a.action == "startup_reconcile"));
    }

    #[tokio::test]
    async fn acp_session_resume_links_conversation_turns() {
        // Stage 11.5: a finished turn's acp_session_id should be the parent of
        // the next turn's task assignment, so the agent resumes instead of
        // re-reading the transcript.
        let s = temp_store().await;
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node = EnrollRequest {
            token,
            name: "n".into(),
            adapters: vec!["mock".into()],
            repositories: vec![String::new()],
            max_concurrency: 2,
            agent_version: "test".into(),
            protocol_version: None,
            permission_interception: "wrapper".into(),
        };
        let node_id = s.enroll_node(&node).await.unwrap().expect("enroll").node_id;

        let conv = s.create_conversation("mock", "").await.unwrap();

        // Turn 1: a task with no resume parent.
        let t1 = s
            .create_task(&CreateTaskRequest {
                prompt: "hello".into(),
                repository: String::new(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: Some(60),
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
                github_push: false,
                github_repo: None,
                github_issue: None,
                github_base_ref: None,
                max_attempts: 1,
                consensus_mode: None,
                review_of: None,
            })
            .await
            .unwrap();
        s.append_conversation_message(&conv.id, "user", "hello", Some(&t1.id))
            .await
            .unwrap();
        let a1 = s.try_assign(&node_id).await.unwrap().expect("assign t1");
        assert_eq!(a1.parent_acp_session_id, None, "first turn has no parent");
        // Before completion, there is no resumable session.
        assert_eq!(
            s.last_conversation_acp_session(&conv.id).await.unwrap(),
            None
        );
        s.ack_attempt(&a1.attempt_id).await.unwrap();
        s.complete_attempt(
            &a1.attempt_id,
            &CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: Some("sess-1".into()),
                provenance: None,
                plan: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            },
        )
        .await
        .unwrap();
        // After completion, the session is resumable.
        assert_eq!(
            s.last_conversation_acp_session(&conv.id).await.unwrap(),
            Some("sess-1".to_string())
        );

        // Turn 2: the API handler would set parent = the resumable session.
        let parent = s.last_conversation_acp_session(&conv.id).await.unwrap();
        let t2 = s
            .create_task(&CreateTaskRequest {
                prompt: "again".into(),
                repository: String::new(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: Some(60),
                validation_command: None,
                base_commit: None,
                parent_acp_session_id: parent,
                security_profile: None,
                network_mode: None,
                group_id: None,
                agent_id: None,
                consensus_group_id: None,
                consensus_member: None,
                opencode_override: None,
                github_push: false,
                github_repo: None,
                github_issue: None,
                github_base_ref: None,
                max_attempts: 1,
                consensus_mode: None,
                review_of: None,
            })
            .await
            .unwrap();
        assert_eq!(
            s.show_task(&t2.id)
                .await
                .unwrap()
                .unwrap()
                .parent_acp_session_id,
            Some("sess-1".to_string())
        );
        let a2 = s.try_assign(&node_id).await.unwrap().expect("assign t2");
        assert_eq!(
            a2.parent_acp_session_id.as_deref(),
            Some("sess-1"),
            "assignment carries the resume parent"
        );
    }

    #[tokio::test]
    async fn conversation_append_allocates_unique_seq_under_concurrency() {
        // Hardening P2 item 21: concurrent appends to the same conversation
        // must each get a distinct, gap-free sequence. The per-message seq is
        // now allocated atomically by a single INSERT ... (SELECT MAX+1), with
        // a UNIQUE(conversation_id, seq) index backstopping the invariant.
        let s = temp_store().await;
        let conv = s.create_conversation("mock", "").await.unwrap();
        const N: usize = 32;
        let mut handles = Vec::with_capacity(N);
        for i in 0..N {
            let s = s.clone();
            let id = conv.id.clone();
            handles.push(tokio::spawn(async move {
                s.append_conversation_message(&id, "user", &format!("m{i}"), None)
                    .await
                    .unwrap()
            }));
        }
        let mut seqs = Vec::with_capacity(N);
        for h in handles {
            seqs.push(h.await.unwrap());
        }
        // Each append returned a distinct seq in 1..=N.
        seqs.sort_unstable();
        let expected: Vec<i64> = (1..=N as i64).collect();
        assert_eq!(seqs, expected, "sequences must be unique and gap-free");
        // And the persisted rows agree with the returned seqs.
        let msgs = s
            .list_conversation_messages(&conv.id, 0, 1000)
            .await
            .unwrap();
        let persisted: Vec<i64> = msgs.iter().map(|m| m.seq).collect();
        assert_eq!(persisted, expected);
    }

    #[tokio::test]
    async fn conversation_messages_pagination_works() {
        // Hardening P2 item 20: cursor pagination for conversation messages.
        let s = temp_store().await;
        let conv = s.create_conversation("mock", "").await.unwrap();
        for i in 1..=10 {
            s.append_conversation_message(&conv.id, "user", &format!("msg{i}"), None)
                .await
                .unwrap();
        }
        // First page: after_seq=0, limit=3
        let page1 = s.list_conversation_messages(&conv.id, 0, 3).await.unwrap();
        assert_eq!(page1.len(), 3);
        assert_eq!(page1[0].seq, 1);
        assert_eq!(page1[2].seq, 3);
        // Second page: after_seq=3, limit=3
        let page2 = s.list_conversation_messages(&conv.id, 3, 3).await.unwrap();
        assert_eq!(page2.len(), 3);
        assert_eq!(page2[0].seq, 4);
        assert_eq!(page2[2].seq, 6);
        // Third page: after_seq=6, limit=3
        let page3 = s.list_conversation_messages(&conv.id, 6, 3).await.unwrap();
        assert_eq!(page3.len(), 3);
        assert_eq!(page3[0].seq, 7);
        assert_eq!(page3[2].seq, 9);
        // Fourth page: after_seq=9, limit=3 (only 1 remaining)
        let page4 = s.list_conversation_messages(&conv.id, 9, 3).await.unwrap();
        assert_eq!(page4.len(), 1);
        assert_eq!(page4[0].seq, 10);
        // After end: after_seq=10, limit=3
        let page5 = s.list_conversation_messages(&conv.id, 10, 3).await.unwrap();
        assert_eq!(page5.len(), 0);
        // Limit clamping: limit=0 -> 1, limit=2000 -> 1000
        let clamped = s.list_conversation_messages(&conv.id, 0, 0).await.unwrap();
        assert_eq!(clamped.len(), 1);
        let clamped2 = s
            .list_conversation_messages(&conv.id, 0, 2000)
            .await
            .unwrap();
        assert_eq!(clamped2.len(), 10);
    }

    #[tokio::test]
    async fn ingest_events_reports_contiguous_prefix_and_dedup() {
        // Hardening P1 item 14: the ACK returns the contiguous sequence prefix
        // (1..=N) and dedups repeated sequence ids via the unique index.
        let s = temp_store().await;
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node_id = s
            .enroll_node(&EnrollRequest {
                token,
                name: "n".into(),
                adapters: vec!["mock".into()],
                repositories: vec![String::new()],
                max_concurrency: 2,
                agent_version: "test".into(),
                protocol_version: None,
                permission_interception: "wrapper".into(),
            })
            .await
            .unwrap()
            .unwrap()
            .node_id;
        let _task = s
            .create_task(&CreateTaskRequest {
                prompt: "p".into(),
                repository: String::new(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: Some(60),
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
                github_push: false,
                github_repo: None,
                github_issue: None,
                github_base_ref: None,
                max_attempts: 1,
                consensus_mode: None,
                review_of: None,
            })
            .await
            .unwrap();
        let a = s.try_assign(&node_id).await.unwrap().unwrap();

        let mk = |seq, text: &str| IncomingEvent {
            sequence: seq,
            r#type: EventType::Stdout,
            payload: serde_json::json!({"text": text}),
        };
        // Land 1,2 then 3,4 → contiguous prefix 4.
        let ack = s
            .ingest_events(
                &a.attempt_id,
                &IngestEventsRequest {
                    events: vec![mk(1, "a"), mk(2, "b")],
                },
            )
            .await
            .unwrap();
        assert_eq!(ack.accepted, 2);
        assert_eq!(ack.highest_contiguous_sequence, Some(2));

        let ack = s
            .ingest_events(
                &a.attempt_id,
                &IngestEventsRequest {
                    events: vec![mk(3, "c"), mk(4, "d")],
                },
            )
            .await
            .unwrap();
        assert_eq!(ack.accepted, 2);
        assert_eq!(ack.highest_contiguous_sequence, Some(4));

        // Re-send 4 (duplicate) → accepted 0, prefix still 4 (idempotent replay).
        let ack = s
            .ingest_events(
                &a.attempt_id,
                &IngestEventsRequest {
                    events: vec![mk(4, "d")],
                },
            )
            .await
            .unwrap();
        assert_eq!(ack.accepted, 0);
        assert_eq!(ack.highest_contiguous_sequence, Some(4));

        // Send 6 (gap at 5) → contiguous prefix stays at 4 until 5 arrives.
        let ack = s
            .ingest_events(
                &a.attempt_id,
                &IngestEventsRequest {
                    events: vec![mk(6, "f")],
                },
            )
            .await
            .unwrap();
        assert_eq!(ack.accepted, 1);
        assert_eq!(ack.highest_contiguous_sequence, Some(4));

        // Backfill 5 → prefix advances to 6 (the prior gap closes).
        let ack = s
            .ingest_events(
                &a.attempt_id,
                &IngestEventsRequest {
                    events: vec![mk(5, "e")],
                },
            )
            .await
            .unwrap();
        assert_eq!(ack.accepted, 1);
        assert_eq!(ack.highest_contiguous_sequence, Some(6));
    }

    // Plan 0.3 item 1.4: duplicates INSIDE one batch land once, and a large
    // batch ingests in a single write transaction.
    #[tokio::test]
    async fn ingest_events_batch_intra_dedup_and_single_txn() {
        let s = temp_store().await;
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node_id = s
            .enroll_node(&EnrollRequest {
                token,
                name: "n".into(),
                adapters: vec!["mock".into()],
                repositories: vec![String::new()],
                max_concurrency: 2,
                agent_version: "test".into(),
                protocol_version: None,
                permission_interception: "wrapper".into(),
            })
            .await
            .unwrap()
            .unwrap()
            .node_id;
        let _task = s
            .create_task(&CreateTaskRequest {
                prompt: "p".into(),
                repository: String::new(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: Some(60),
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
                github_push: false,
                github_repo: None,
                github_issue: None,
                github_base_ref: None,
                max_attempts: 1,
                consensus_mode: None,
                review_of: None,
            })
            .await
            .unwrap();
        let a = s.try_assign(&node_id).await.unwrap().unwrap();

        let mk = |seq, text: &str| IncomingEvent {
            sequence: seq,
            r#type: EventType::Stdout,
            payload: serde_json::json!({"text": text}),
        };
        // Intra-batch duplicates: 1,2,2,3,3,3 → only 3 distinct sequences land.
        let ack = s
            .ingest_events(
                &a.attempt_id,
                &IngestEventsRequest {
                    events: vec![
                        mk(1, "a"),
                        mk(2, "b"),
                        mk(2, "b2"),
                        mk(3, "c"),
                        mk(3, "c2"),
                        mk(3, "c3"),
                    ],
                },
            )
            .await
            .unwrap();
        assert_eq!(ack.accepted, 3);
        assert_eq!(ack.highest_contiguous_sequence, Some(3));

        // 1000-event batch: one write transaction, contiguous prefix 1003.
        let txns_before = s.write_txn_count();
        let big: Vec<IncomingEvent> = (4..=1003u64).map(|i| mk(i, "x")).collect();
        let ack = s
            .ingest_events(&a.attempt_id, &IngestEventsRequest { events: big })
            .await
            .unwrap();
        let txns_after = s.write_txn_count();
        assert_eq!(ack.accepted, 1000);
        assert_eq!(ack.highest_contiguous_sequence, Some(1003));
        assert_eq!(
            txns_after - txns_before,
            1,
            "a 1000-event batch must be one write transaction"
        );
        assert_eq!(write_txn_stats().1, 0);
    }

    #[tokio::test]
    async fn artifact_save_rejects_traversal_names() {
        let s = temp_store().await;
        // FK-valid attempt (migration 0040) so the rejected-name assertions
        // test the NAME guard, not the FK.
        let (_node_id, _task_id) = seed_task_attempt(&s, "task-trav", "att-trav").await;
        for bad in ["../x", "..", ".", "/etc/passwd", "a/b", "a\\b", "", "x\0y"] {
            let r = s
                .save_artifact(
                    "att-trav",
                    &UploadArtifactRequest {
                        name: bad.into(),
                        content: "x".into(),
                        ..Default::default()
                    },
                )
                .await;
            assert!(r.is_err(), "traversal name {bad:?} should be rejected");
        }
        // A plain single-segment name is accepted.
        s.save_artifact(
            "att-trav",
            &UploadArtifactRequest {
                name: "ok.txt".into(),
                content: "ok".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn artifact_read_traversal_returns_none() {
        // Stage 2.2: a crafted read name must not escape the artifact root;
        // invalid names resolve to None (not found), not an error, so a 404 vs
        // 500 cannot leak whether an artifact exists.
        let s = temp_store().await;
        // Seed a task + attempt (FK-valid, migration 0040) so latest_attempt_id
        // resolves and the artifacts FK accepts the rows.
        let (_node_id, task_id) = seed_task_attempt(&s, "task-art", "att-art").await;
        s.save_artifact(
            "att-art",
            &UploadArtifactRequest {
                name: "real.txt".into(),
                content: "data".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            s.read_artifact(&task_id, "real.txt").await.unwrap(),
            Some("data".to_string()),
            "valid artifact reads back"
        );
        // No traversal name reaches the filesystem as an escape.
        for bad in ["../../../etc/passwd", "..", "/etc/passwd", "sub/dir/secret"] {
            assert_eq!(
                s.read_artifact(&task_id, bad).await.unwrap(),
                None,
                "traversal read {bad:?} must be None"
            );
        }
    }

    #[tokio::test]
    async fn artifact_binary_round_trip_preserves_bytes_media_and_hash() {
        // Stage 2.2: non-UTF-8 artifacts (binary diffs, archives) must round trip
        // byte-for-byte through the binary-safe endpoint, with the stored media
        // type and caller-supplied hash read back unchanged.
        let s = temp_store().await;
        let (_node_id, task_id) = seed_task_attempt(&s, "task-bart", "att-bart").await;
        // 0xFF 0xFE 0x00 invalid as UTF-8; would be mangled by read_to_string.
        let bytes: &[u8] = &[0xFFu8, 0xFEu8, 0x00u8, 0x01u8, 0x02u8];
        let sha = sha256_bytes_hex(bytes);
        s.save_artifact_bytes("att-bart", "blob.bin", bytes, Some("image/png"), Some(&sha))
            .await
            .unwrap();
        assert_eq!(
            s.read_artifact_bytes(&task_id, "blob.bin").await.unwrap(),
            Some(bytes.to_vec()),
            "binary bytes must round trip unchanged"
        );
        let meta = s
            .read_artifact_meta(&task_id, "blob.bin")
            .await
            .unwrap()
            .expect("meta present");
        assert_eq!(meta.size_bytes, bytes.len() as i64);
        assert_eq!(meta.media_type.as_deref(), Some("image/png"));
        // Only the server-computed hash is stored (hardening P0), so it equals
        // the computed hash, not any client value — and for a correct hint it's identical.
        assert_eq!(meta.sha256.as_deref(), Some(sha.as_str()));
    }

    #[tokio::test]
    async fn artifact_list_returns_names_and_meta_in_order() {
        // Plan 1.11 (#8): SDK `artifacts()` lists a task's artifact names +
        // metadata via `list_artifacts` (latest attempt only).
        let s = temp_store().await;
        let (_node_id, task_id) = seed_task_attempt(&s, "task-list", "att-list").await;
        s.save_artifact_bytes(
            "att-list",
            "changes.patch",
            b"diff --git a/x b/x",
            None,
            None,
        )
        .await
        .unwrap();
        s.save_artifact_bytes("att-list", "validation.log", b"all green\n", None, None)
            .await
            .unwrap();
        let list = s.list_artifacts(&task_id).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "changes.patch");
        assert_eq!(list[0].size_bytes, 18);
        assert_eq!(list[1].name, "validation.log");
        assert_eq!(list[1].size_bytes, 10);
        // A task with no attempts lists empty (no panic on missing latest).
        let other = s
            .create_task(&agentgrid_common::CreateTaskRequest {
                prompt: "x".into(),
                repository: "*".into(),
                adapter: "mock".into(),
                requested_node_id: None,
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
                github_push: false,
                github_repo: None,
                github_issue: None,
                github_base_ref: None,
                max_attempts: 1,
                consensus_mode: None,
                review_of: None,
            })
            .await
            .unwrap();
        assert!(s.list_artifacts(&other.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn shared_context_round_trip_and_group_isolation() {
        // Plan 1.12 (#7): two attempts of one task group share a note via
        // set/get/list; a different group cannot see it.
        let s = temp_store().await;
        s.set_shared_context("grp-a", "module", "auth.rs")
            .await
            .unwrap();
        s.set_shared_context("grp-a", "convention", "tabs")
            .await
            .unwrap();

        // Second attempt (same group) reads what the first wrote.
        let v = s.get_shared_context("grp-a", "module").await.unwrap();
        assert_eq!(v.as_deref(), Some("auth.rs"));

        // List returns both, ordered by key.
        let all = s.list_shared_context("grp-a").await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].key, "convention");
        assert_eq!(all[1].key, "module");

        // A different group starts empty (isolation).
        assert!(s.list_shared_context("grp-b").await.unwrap().is_empty());
        assert!(s
            .get_shared_context("grp-b", "module")
            .await
            .unwrap()
            .is_none());

        // Overwrite upserts; delete removes.
        s.set_shared_context("grp-a", "module", "auth2.rs")
            .await
            .unwrap();
        assert_eq!(
            s.get_shared_context("grp-a", "module")
                .await
                .unwrap()
                .as_deref(),
            Some("auth2.rs")
        );
        s.delete_shared_context("grp-a", "module").await.unwrap();
        assert!(s
            .get_shared_context("grp-a", "module")
            .await
            .unwrap()
            .is_none());

        // Invalid keys / groups are rejected, not silently stored.
        assert!(s
            .set_shared_context("grp-a", "bad key!", "x")
            .await
            .is_err());
        assert!(s.set_shared_context("../esc", "k", "x").await.is_err());
    }

    #[tokio::test]
    async fn task_group_id_persists_through_create_and_assignment() {
        // Plan 1.12 (#7): group_id set at task creation survives the create
        // view and the fresh-read path.
        let s = temp_store().await;
        let task = s
            .create_task(&agentgrid_common::CreateTaskRequest {
                prompt: "do x".into(),
                repository: "*".into(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: None,
                validation_command: None,
                base_commit: None,
                parent_acp_session_id: None,
                security_profile: None,
                network_mode: None,
                group_id: Some("grp-x".into()),
                agent_id: None,
                consensus_group_id: None,
                consensus_member: None,
                opencode_override: None,
                github_push: false,
                github_repo: None,
                github_issue: None,
                github_base_ref: None,
                max_attempts: 1,
                consensus_mode: None,
                review_of: None,
            })
            .await
            .unwrap();
        assert_eq!(task.group_id.as_deref(), Some("grp-x"));

        let view = s.show_task(&task.id).await.unwrap().unwrap();
        assert_eq!(view.group_id.as_deref(), Some("grp-x"));
    }

    #[tokio::test]
    async fn budget_enforcement_parks_run_blocked_on_rounds_breach() {
        // Stage 13 Loop Engineering: a template with `max_rounds = 0` allows
        // zero step starts past the budget. The first tick starts both root
        // steps (both ready, no deps); the next tick's pre-check then finds
        // rounds >= 1 > 0 => breach => run `Blocked`, and a further tick stays
        // Blocked (terminal-until-approval).
        let s = temp_store().await;
        let steps = vec![
            step("a", &[], WorkflowRole::Worker),
            step("b", &[], WorkflowRole::Worker),
        ];
        let budget = WorkflowBudget {
            max_rounds: Some(0),
            ..Default::default()
        };
        let tpl = s
            .create_workflow_template("looped", &steps, &Some(budget))
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, None, None)
            .await
            .unwrap();
        // First tick: runsatе both root steps (no deps). Rounds is 0 at the
        // pre-check (nothing past Pending yet), so no breach this tick.
        s.tick_workflow_run(&run.id).await.unwrap();
        let s1 = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            s1.status,
            WorkflowRunStatus::Running,
            "first tick starts steps; budget not yet breached"
        );
        let started = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(
            started
                .iter()
                .filter(|s| s.status == WorkflowStepStatus::Running)
                .count(),
            2,
            "both root steps started on the first tick"
        );

        // Second tick pre-checks the budget: two steps past Pending =>
        // rounds=2 > 0 => breach => run Blocked.
        s.tick_workflow_run(&run.id).await.unwrap();
        let s2 = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            s2.status,
            WorkflowRunStatus::Blocked,
            "budget breach parks Blocked"
        );
        let after = s.get_workflow_run_steps(&run.id).await.unwrap();
        assert_eq!(
            after
                .iter()
                .filter(|s| s.status == WorkflowStepStatus::Running)
                .count(),
            2,
            "started steps remain Running; no further activity on the blocked run"
        );
        // A further tick stays Blocked (terminal-until-approval).
        s.tick_workflow_run(&run.id).await.unwrap();
        let s3 = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(s3.status, WorkflowRunStatus::Blocked);
    }

    #[tokio::test]
    async fn budget_max_tokens_breach_cancels_running_step_task() {
        // 1.4: adapters report tokens via `progress` (stored `metric`) events;
        // a `max_tokens` breach must park the run Blocked AND cancel the
        // in-flight step task, not merely stop scheduling new steps.
        let s = temp_store().await;
        let steps = vec![step("a", &[], WorkflowRole::Worker)];
        let budget = WorkflowBudget {
            max_tokens: Some(10),
            ..Default::default()
        };
        let tpl = s
            .create_workflow_template("tokened", &steps, &Some(budget))
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, None, None)
            .await
            .unwrap();
        // First tick starts the step; tokens are 0 so no breach yet.
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 1, "step task created on first tick");
        let task_id = created[0].clone();

        // Enroll a node and attach a running attempt to the step task, then
        // ingest a metric event that blows past max_tokens.
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node_id = s
            .enroll_node(&EnrollRequest {
                token,
                name: "n".into(),
                adapters: vec!["mock".into()],
                repositories: vec!["*".into()],
                max_concurrency: 2,
                agent_version: "test".into(),
                protocol_version: None,
                permission_interception: "wrapper".into(),
            })
            .await
            .unwrap()
            .unwrap()
            .node_id;
        sqlx::query(
            "INSERT INTO attempts (id, task_id, number, node_id, status, lease_expires_at, ack_deadline, started_at) \
             VALUES ('att-tok', ?, 1, ?, 'running', ?, ?, ?)",
        )
        .bind(&task_id)
        .bind(&node_id)
        .bind(now_iso())
        .bind(now_iso())
        .bind(now_iso())
        .execute(&s.pool)
        .await
        .unwrap();
        s.ingest_events(
            "att-tok",
            &IngestEventsRequest {
                events: vec![IncomingEvent {
                    sequence: 1,
                    r#type: EventType::Metric,
                    payload: serde_json::json!({"tokens": 100, "cost_cents": 5}),
                }],
            },
        )
        .await
        .unwrap();

        // Next tick sees tokens 100 > 10: breach → Blocked + task cancelled.
        s.tick_workflow_run(&run.id).await.unwrap();
        let r = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(r.status, WorkflowRunStatus::Blocked, "breach parks Blocked");
        let status = s.get_task_status(&task_id).await.unwrap().unwrap();
        assert_eq!(
            status,
            TaskStatus::Cancelled,
            "over-budget step task is cancelled, not left running"
        );
        // Snapshot reflects the adapter-reported usage + the fired ceiling.
        let snap = s
            .get_workflow_run_projection(&run.id)
            .await
            .unwrap()
            .unwrap()
            .budget
            .expect("budget snapshot present");
        assert_eq!(snap.usage.tokens, 100);
        assert_eq!(snap.usage.cost_cents, 5);
        assert_eq!(snap.breach.unwrap().field, "max_tokens");
    }

    #[tokio::test]
    async fn budget_bytes_enforced_from_message_payload_size() {
        // Stage 13: `max_bytes` counts orchestrator-emitted payload bytes, so a
        // handoff streak that pounds long payloads parks the run `Blocked`, and
        // read-back reports the bytes + breach.
        let s = temp_store().await;
        let steps = vec![
            step("a", &[], WorkflowRole::Worker),
            step("b", &[], WorkflowRole::Worker),
        ];
        let budget = WorkflowBudget {
            max_bytes: Some(5),
            ..Default::default()
        };
        let tpl = s
            .create_workflow_template("looped", &steps, &Some(budget))
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, None, None)
            .await
            .unwrap();
        // Each emit appends a payload -- 6 bytes over the 5-byte ceiling.
        s.emit_workflow_message(
            &run.id,
            "a",
            "b",
            agentgrid_common::AgentMessageKind::Output,
            "hello!",
        )
        .await
        .unwrap();
        assert_eq!(
            s.workflow_message_bytes(&run.id).await.unwrap(),
            6,
            "byte count reflects payload length"
        );
        // tick sees bytes > max_bytes -> breach -> Blocked.
        s.tick_workflow_run(&run.id).await.unwrap();
        let after = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            after.status,
            WorkflowRunStatus::Blocked,
            "byte budget breach parks Blocked"
        );
    }

    #[tokio::test]
    async fn circuit_breaker_trips_on_repeated_step_to_step_handoffs() {
        // Stage 13: a tight ping-pong of step->step handoffs with the same
        // (from, to) pair trips the repeated-handoffs circuit breaker. A
        // broadcast to `*` resets the streak (a step-succeeded broadcast to all
        // downstream steps is a healthy flow, not a solo ping-pong).
        let s = temp_store().await;
        let steps = vec![
            step("a", &[], WorkflowRole::Worker),
            step("b", &[], WorkflowRole::Worker),
        ];
        let budget = WorkflowBudget {
            max_repeated_handoffs: Some(2),
            ..Default::default()
        };
        let tpl = s
            .create_workflow_template("looped", &steps, &Some(budget))
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, None, None)
            .await
            .unwrap();
        // a->b, a->b (streak 2) then broadcast a->* (streak reset, still 2).
        for _ in 0..2 {
            s.emit_workflow_message(
                &run.id,
                "a",
                "b",
                agentgrid_common::AgentMessageKind::Output,
                "out",
            )
            .await
            .unwrap();
        }
        s.emit_workflow_message(
            &run.id,
            "a",
            "*",
            agentgrid_common::AgentMessageKind::Output,
            "broadcast",
        )
        .await
        .unwrap();
        assert_eq!(
            s.workflow_repeated_handoffs(&run.id).await.unwrap(),
            2,
            "streak is the longest consecutive same-pair run; broadcast resets"
        );
        // The check uses `>` (not `>=`), so streak=2 vs limit=2 is fine. Keep
        // going to streak 3 to trip the breaker (3 > 2).
        for _ in 0..3 {
            s.emit_workflow_message(
                &run.id,
                "a",
                "b",
                agentgrid_common::AgentMessageKind::Output,
                "out",
            )
            .await
            .unwrap();
        }
        assert_eq!(
            s.workflow_repeated_handoffs(&run.id).await.unwrap(),
            3,
            "streak grows past the breaker threshold"
        );
        s.tick_workflow_run(&run.id).await.unwrap();
        let after = s.get_workflow_run(&run.id).await.unwrap().unwrap();
        assert_eq!(
            after.status,
            WorkflowRunStatus::Blocked,
            "repeated-handoffs breaker trips -> Blocked"
        );
    }

    #[tokio::test]
    async fn parallel_ready_steps_of_same_repo_activate_in_one_tick() {
        // Stage 7.2: two independent (no deps) worker steps pointing at the
        // same repository must be activated in a single tick — both get tasks
        // queued (later run as independent worktrees under the per-repo lock).
        // The push does NOT serialize the steps: each gets its own task_id and
        // both are `Running`.
        let s = temp_store().await;
        let steps = vec![
            step("a", &[], WorkflowRole::Worker),
            step("b", &[], WorkflowRole::Worker),
        ];
        let tpl = s
            .create_workflow_template("par", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, Some("repo-x"), None, None)
            .await
            .unwrap();
        let created = s.tick_workflow_run(&run.id).await.unwrap();
        assert_eq!(created.len(), 2, "both root steps activate in one tick");
        let st = s.get_workflow_run_steps(&run.id).await.unwrap();
        let running: Vec<_> = st
            .iter()
            .filter(|x| x.status == WorkflowStepStatus::Running)
            .collect();
        assert_eq!(running.len(), 2, "both steps Running");
        // Each step has a distinct task_id (one worktree per step later).
        let mut tasks = std::collections::HashSet::new();
        for r in &running {
            let t = s.step_task_id(&r.id).await.unwrap().unwrap();
            assert!(tasks.insert(t), "distinct task per parallel step");
        }
        assert_eq!(tasks.len(), 2, "two distinct task ids");
    }

    #[tokio::test]
    async fn upsert_discovered_skills_defaults_untrusted_and_preserves_operator_decision() {
        // Stage 9.2: a heartbeat that reports a new skill lands it as
        // untrusted; a second heartbeat does not duplicate or flip trust; an
        // operator decision (trusted) survives subsequent discovery.
        let s = temp_store().await;
        // Fresh skill -> untrusted discovery row.
        s.upsert_discovered_skills(&[("git-helper".into(), "user".into())])
            .await
            .unwrap();
        let v = s.get_skill_trust("git-helper", "user").await.unwrap();
        assert!(!v.trusted, "freshly discovered defaults untrusted");
        // Idempotent: a second heartbeat with the same discovery changes nothing.
        s.upsert_discovered_skills(&[("git-helper".into(), "user".into())])
            .await
            .unwrap();
        let v = s.get_skill_trust("git-helper", "user").await.unwrap();
        assert!(!v.trusted);
        // Operator trusts it; a later discovery must NOT revert trust.
        s.set_skill_trust("git-helper", "user", true, "alice")
            .await
            .unwrap();
        s.upsert_discovered_skills(&[("git-helper".into(), "user".into())])
            .await
            .unwrap();
        let v = s.get_skill_trust("git-helper", "user").await.unwrap();
        assert!(v.trusted, "operator decision preserved across discovery");
        assert_eq!(v.decided_by.as_deref(), Some("alice"));
        // Empty discovery is a cheap no-op (does not error).
        s.upsert_discovered_skills(&[]).await.unwrap();
    }

    /// Hardening P0: a malformed attempt_id (traversal/separator) must never
    /// reach a filesystem path join. save_artifact_bytes rejects it with
    /// InvalidAttemptId before creating any directory.
    #[tokio::test]
    async fn save_artifact_rejects_traversal_attempt_id() {
        let s = temp_store().await;
        for bad in &["..", "../etc", "a/b", "a\\b", "a.b", "has space", ""] {
            let err = s
                .save_artifact_bytes(bad, "ok.txt", b"x", None, None)
                .await
                .expect_err("malformed attempt_id rejected");
            assert!(
                matches!(err, StoreArtifactError::InvalidAttemptId),
                "{bad:?} -> {err:?}"
            );
        }
        // The store rejected every malformed id at the boundary; no
        // traversal-target directory was created. (We assert the rejection
        // itself above; artifact_root may legitimately hold other test data,
        // so we do not assert emptiness.)
    }
    /// Hardening P0: a symlinked artifact directory must be rejected — a node
    /// (or a prior compromise) must not redirect artifact writes outside root.
    #[tokio::test]
    async fn save_artifact_rejects_symlink_dir() {
        let s = temp_store().await;
        // Plant a symlink where the attempt dir would live, pointing outside.
        let real_id = "550e8400-e29b-41d4-a716-446655440000";
        let attempt_dir = s.artifact_root.join(real_id);
        tokio::fs::create_dir_all(&s.artifact_root).await.unwrap();
        let outside = std::env::temp_dir().join("ag-symlink-outside");
        tokio::fs::create_dir_all(&outside).await.unwrap();
        // Clean any symlink/dir left by a prior run so the test is repeatable.
        let _ = tokio::fs::remove_file(&attempt_dir).await;
        let _ = tokio::fs::remove_dir_all(&attempt_dir).await;
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &attempt_dir).unwrap();
        let err = s
            .save_artifact_bytes(real_id, "ok.txt", b"x", None, None)
            .await
            .expect_err("symlinked attempt dir rejected");
        assert!(matches!(err, StoreArtifactError::Other(_)), "{err:?}");
        // The escape never reached the outside target.
        assert!(tokio::fs::read(outside.join("ok.txt")).await.is_err());
    }

    /// Hardening P1 item 21: count_orphan_rows is 0 on a healthy DB and >0
    /// once a parent row is removed out-of-band (simulating corruption).
    #[tokio::test]
    async fn orphan_row_detection_works() {
        use sqlx::Connection;
        let s = temp_store().await;
        // Healthy: no orphans.
        assert_eq!(s.count_orphan_rows().await.unwrap(), 0);
        // Simulate pre-FK corruption on a DEDICATED connection with foreign
        // keys off: the app connection now enforces FKs (migration 0040), so
        // orphan rows can no longer be created through it — they can only
        // pre-exist from an old database. Plant the orphan exactly the way an
        // old DB would look: task + attempt + event, then remove the task.
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let p = std::env::temp_dir().join(format!("ag-wf-orphan-{n}.db"));
        let _ = std::fs::remove_file(&p);
        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&p)
            .create_if_missing(true)
            .foreign_keys(false);
        let mut conn = sqlx::SqliteConnection::connect_with(&opts.clone())
            .await
            .unwrap();
        // Fresh file has no schema — run the migrations on it first.
        sqlx::migrate!("./migrations").run(&mut conn).await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query("INSERT INTO tasks (id, repository, prompt, adapter, status, created_at) VALUES ('t-orphan','r','p','mock','queued','2024-01-01T00:00:00Z')")
            .execute(&mut conn).await.unwrap();
        sqlx::query("INSERT INTO attempts (id, task_id, node_id, number, status, started_at) VALUES ('a-orphan','t-orphan','n-x',1,'running','2024-01-01T00:00:00Z')")
            .execute(&mut conn).await.unwrap();
        sqlx::query("INSERT INTO task_events (id, attempt_id, sequence, type, payload, created_at) VALUES ('e-orphan','a-orphan',1,'log','{}','2024-01-01T00:00:00Z')")
            .execute(&mut conn).await.unwrap();
        drop(conn);
        // Re-open through the app Store (FK on) and check the detector.
        let s2 = Store::open(p.to_str().unwrap()).await.unwrap();
        assert_eq!(
            s2.count_orphan_rows().await.unwrap(),
            0,
            "no orphans while parents exist"
        );
        // Remove the parent task out-of-band (again with FKs off).
        let mut conn = sqlx::SqliteConnection::connect_with(&opts).await.unwrap();
        sqlx::query("PRAGMA foreign_keys = OFF")
            .execute(&mut conn)
            .await
            .unwrap();
        sqlx::query("DELETE FROM tasks WHERE id = 't-orphan'")
            .execute(&mut conn)
            .await
            .unwrap();
        drop(conn);
        let orphans = s2.count_orphan_rows().await.unwrap();
        assert!(orphans >= 1, "detected orphaned attempt: {orphans}");
    }

    #[tokio::test]
    async fn audit_records_rejected_terminal_completion() {
        // Hardening P1 item 13: a late/stale completion for an attempt we
        // already finalized is rejected but audited (with the source state),
        // so a stale node redelivery is traceable.
        let s = temp_store().await;
        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node_id = s
            .enroll_node(&EnrollRequest {
                token,
                name: "n".into(),
                adapters: vec!["mock".into()],
                repositories: vec![String::new()],
                max_concurrency: 2,
                agent_version: "test".into(),
                protocol_version: None,
                permission_interception: "wrapper".into(),
            })
            .await
            .unwrap()
            .unwrap()
            .node_id;
        let _task = s
            .create_task(&CreateTaskRequest {
                prompt: "p".into(),
                repository: String::new(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: Some(60),
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
                github_push: false,
                github_repo: None,
                github_issue: None,
                github_base_ref: None,
                max_attempts: 1,
                consensus_mode: None,
                review_of: None,
            })
            .await
            .unwrap();
        let a = s.try_assign(&node_id).await.unwrap().unwrap();
        s.ack_attempt(&a.attempt_id).await.unwrap();
        // First completion succeeds (running -> succeeded).
        assert!(s
            .complete_attempt(&a.attempt_id, &CompleteAttemptRequest::default())
            .await
            .unwrap());
        // Second (late) completion is an idempotent Ok(true) but audited.
        assert!(s
            .complete_attempt(&a.attempt_id, &CompleteAttemptRequest::default())
            .await
            .unwrap());
        let audits = s.list_audit(None, 100).await.unwrap();
        let rejs: Vec<_> = audits
            .iter()
            .filter(|e| e.action == "complete.rejected_terminal")
            .collect();
        assert_eq!(rejs.len(), 1, "exactly one rejected-terminal audit");
        assert_eq!(rejs[0].actor_type, "attempt");
        assert_eq!(rejs[0].actor_id.as_deref(), Some(a.attempt_id.as_str()));
        assert_eq!(rejs[0].subject.as_deref(), Some("succeeded"));
    }

    #[tokio::test]
    async fn audit_records_rejected_nonterminal_retry() {
        // Hardening P1 item 13: a retry against a non-terminal task (queued)
        // is rejected and audited with the source state.
        let s = temp_store().await;
        let task = s
            .create_task(&CreateTaskRequest {
                prompt: "p".into(),
                repository: String::new(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: Some(60),
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
                github_push: false,
                github_repo: None,
                github_issue: None,
                github_base_ref: None,
                max_attempts: 1,
                consensus_mode: None,
                review_of: None,
            })
            .await
            .unwrap();
        // The task is queued (never failed); retry must be rejected.
        assert!(!s.retry_task(&task.id).await.unwrap());
        let audits = s.list_audit(None, 100).await.unwrap();
        let rejs: Vec<_> = audits
            .iter()
            .filter(|e| e.action == "retry.rejected_nonterminal")
            .collect();
        assert_eq!(rejs.len(), 1, "exactly one rejected-retry audit");
        assert_eq!(rejs[0].actor_type, "task");
        assert_eq!(rejs[0].actor_id.as_deref(), Some(task.id.as_str()));
        assert_eq!(rejs[0].subject.as_deref(), Some("queued"));
    }
}

#[cfg(test)]
mod agent_tests {
    use super::*;
    use agentgrid_common::{AgentCreate, CreateTaskRequest};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    async fn temp_store() -> Store {
        // Disable critical disk watermark for tests (temp fs often has < 512 MB free).
        std::env::set_var("AGENTGRID_DISK_CRITICAL_MB", "0");
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("ag-agent-test-{n}-{nanos}.db"));
        let _ = std::fs::remove_file(&dir);
        Store::open(dir.to_str().unwrap()).await.unwrap()
    }

    #[tokio::test]
    async fn agent_budget_hard_stop_rejects_exhausted() {
        // Plan 2.1 (#18): max_tasks=1 — first attributed task passes, the
        // second is rejected with a budget_exceeded trail row.
        let s = temp_store().await;
        let agent = s
            .create_agent(&AgentCreate {
                name: "nightly-build".into(),
                role: "maintainer".into(),
                prompt: "run nightly checks".into(),
                skills: vec![],
                budget_usd: 10.0,
                max_tasks: Some(1),
                heartbeat_interval_secs: None,
            })
            .await
            .unwrap();

        let base = CreateTaskRequest {
            prompt: "x".into(),
            repository: "*".into(),
            adapter: "mock".into(),
            requested_node_id: None,
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
            github_push: false,
            github_repo: None,
            github_issue: None,
            github_base_ref: None,
            max_attempts: 1,
            consensus_mode: None,
            review_of: None,
        };
        s.create_agent_task(&agent.id, &base).await.unwrap();
        let err = s.create_agent_task(&agent.id, &base).await.unwrap_err();
        assert!(err.to_string().contains("budget exhausted"));

        // Spend is counted; the trail recorded both creations + the rejection.
        let fresh = s.get_agent(&agent.id).await.unwrap().unwrap();
        assert_eq!(fresh.tasks_spent, 1);
        let actions = s.agent_actions(&agent.id).await.unwrap();
        let kinds: Vec<_> = actions.iter().map(|a| a.action.as_str()).collect();
        assert!(kinds.contains(&"task_created"));
        assert!(kinds.contains(&"budget_exceeded"));

        // Unknown agent attribution is rejected too.
        assert!(s.create_agent_task("nope", &base).await.is_err());
    }

    #[tokio::test]
    async fn agent_budget_hard_stop_is_atomic_under_concurrency() {
        // Audit follow-up: the budget check and the attributed insert must be
        // one transaction. 8 concurrent creations against max_tasks = 3 must
        // land exactly 3 tasks — the old check-then-act let racing callers
        // both observe spend = max-1 and both insert.
        use std::sync::Arc;
        let s = Arc::new(temp_store().await);
        let agent = s
            .create_agent(&AgentCreate {
                name: "racer".into(),
                role: "worker".into(),
                prompt: "p".into(),
                skills: vec![],
                budget_usd: 0.0,
                max_tasks: Some(3),
                heartbeat_interval_secs: None,
            })
            .await
            .unwrap();
        let base = CreateTaskRequest {
            prompt: "x".into(),
            repository: "*".into(),
            adapter: "mock".into(),
            ..Default::default()
        };
        let mut handles = Vec::new();
        for _ in 0..8 {
            let s2 = Arc::clone(&s);
            let agent_id = agent.id.clone();
            let req = base.clone();
            handles.push(tokio::spawn(async move {
                s2.create_agent_task(&agent_id, &req).await.is_ok()
            }));
        }
        let mut created = 0;
        for h in handles {
            if h.await.unwrap() {
                created += 1;
            }
        }
        assert_eq!(created, 3, "exactly max_tasks creations may win");
        let fresh = s.get_agent(&agent.id).await.unwrap().unwrap();
        assert_eq!(fresh.tasks_spent, 3);
    }

    /// Review follow-up (v0.3.9): property-style version of the concurrent
    /// budget race above — for a grid of (workers, max_tasks) pairs,
    /// concurrent attributed creations must let exactly min(workers,
    /// max_tasks) through and spend exactly that many. This is the
    /// check-then-act race class the v0.3.6 audit found (budget check and
    /// insert must share one write txn). proptest's macro is sync-only, so
    /// the property runs as a deterministic factorial grid.
    #[tokio::test]
    async fn proptest_agent_budget_race_never_overspends() {
        for workers in [2usize, 3, 5, 8] {
            for max_tasks in [1i64, 2, 3, 5] {
                let s = Arc::new(temp_store().await);
                let agent = s
                    .create_agent(&AgentCreate {
                        name: "racer".into(),
                        role: "worker".into(),
                        prompt: "p".into(),
                        skills: vec![],
                        budget_usd: 0.0,
                        max_tasks: Some(max_tasks),
                        heartbeat_interval_secs: None,
                    })
                    .await
                    .unwrap();
                let req = CreateTaskRequest {
                    prompt: "x".into(),
                    repository: "*".into(),
                    adapter: "mock".into(),
                    ..Default::default()
                };
                let mut handles = Vec::new();
                for _ in 0..workers {
                    let s2 = Arc::clone(&s);
                    let agent_id = agent.id.clone();
                    let rq = req.clone();
                    handles.push(tokio::spawn(async move {
                        s2.create_agent_task(&agent_id, &rq).await.is_ok()
                    }));
                }
                let mut created = 0;
                for h in handles {
                    if h.await.unwrap() {
                        created += 1;
                    }
                }
                let expected = workers.min(max_tasks.max(0) as usize);
                assert_eq!(
                    created, expected,
                    "workers={workers} max_tasks={max_tasks}: exactly min() may win"
                );
                let fresh = s.get_agent(&agent.id).await.unwrap().unwrap();
                assert_eq!(
                    fresh.tasks_spent, expected as i64,
                    "workers={workers} max_tasks={max_tasks}: spend must match winners"
                );
            }
        }
    }

    #[tokio::test]
    async fn agent_heartbeat_due_and_fire_creates_task() {
        // Plan 2.1 (#18): an agent with a heartbeat interval is due when
        // last_heartbeat_at is NULL; firing records the heartbeat and the
        // spawned task is attributed to the agent.
        let s = temp_store().await;
        let agent = s
            .create_agent(&AgentCreate {
                name: "scout".into(),
                role: "worker".into(),
                prompt: "check the queue".into(),
                skills: vec![],
                budget_usd: 0.0,
                max_tasks: None,
                heartbeat_interval_secs: Some(60),
            })
            .await
            .unwrap();

        let due = s
            .due_agents(chrono::Utc::now().timestamp() + 1)
            .await
            .unwrap();
        assert_eq!(due.len(), 1, "fresh agent with interval is due");

        let req = CreateTaskRequest {
            prompt: agent.prompt.clone(),
            repository: "*".into(),
            adapter: "mock".into(),
            requested_node_id: None,
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
            github_push: false,
            github_repo: None,
            github_issue: None,
            github_base_ref: None,
            max_attempts: 1,
            consensus_mode: None,
            review_of: None,
        };
        let task = s.create_agent_task(&agent.id, &req).await.unwrap();
        assert_eq!(task.agent_id.as_deref(), Some(agent.id.as_str()));
        s.record_agent_heartbeat(&agent.id).await.unwrap();

        // Not due again until the interval passes.
        let due2 = s
            .due_agents(chrono::Utc::now().timestamp() + 1)
            .await
            .unwrap();
        assert_eq!(due2.len(), 0);

        // A non-heartbeat agent is never due.
        s.create_agent(&AgentCreate {
            name: "plain".into(),
            role: "worker".into(),
            prompt: "".into(),
            skills: vec![],
            budget_usd: 0.0,
            max_tasks: None,
            heartbeat_interval_secs: None,
        })
        .await
        .unwrap();
        assert!(s
            .due_agents(chrono::Utc::now().timestamp() + 1)
            .await
            .unwrap()
            .is_empty());
    }
}

#[cfg(test)]
mod verifier_ro_tests {
    use super::*;
    use agentgrid_common::{EnrollRequest, WorkflowRole, WorkflowStep};

    fn step(id: &str, deps: &[&str], role: WorkflowRole) -> WorkflowStep {
        WorkflowStep {
            id: id.into(),
            prompt: format!("do {id}"),
            depends_on: deps.iter().map(|s| s.to_string()).collect(),
            role,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: None,
            max_attempts: None,
            expandable: None,
        }
    }

    #[tokio::test]
    async fn verifier_assignment_is_read_only() {
        // Plan 2.4 (#22a): a workflow step with `role: verifier` assigns the
        // task to the node with `read_only = true`, so the node bind-mounts
        // the worktree `:ro`.
        std::env::set_var("AGENTGRID_DISK_CRITICAL_MB", "0");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("ag-vro-{nanos}.db"));
        let _ = std::fs::remove_file(&path);
        let s = Store::open(path.to_str().unwrap()).await.unwrap();

        let (token, _) = s.create_enrollment_token().await.unwrap();
        let node = EnrollRequest {
            token,
            name: "n1".into(),
            adapters: vec!["mock".into()],
            repositories: vec!["*".into()],
            max_concurrency: 4,
            agent_version: "test".into(),
            protocol_version: None,
            permission_interception: "wrapper".into(),
        };
        let node_id = s.enroll_node(&node).await.unwrap().expect("enroll").node_id;

        let steps = vec![
            step("w", &[], WorkflowRole::Worker),
            step("v", &["w"], WorkflowRole::Verifier),
        ];
        let tpl = s
            .create_workflow_template("x", &steps, &None)
            .await
            .unwrap();
        let run = s
            .create_workflow_run(&tpl.id, None, Some("demo"), None)
            .await
            .unwrap();
        let _ = s.tick_workflow_run(&run.id).await.unwrap();

        // Worker assigns first; NOT read-only.
        let worker = s
            .try_assign(&node_id)
            .await
            .unwrap()
            .expect("worker assign");
        assert!(!worker.read_only, "worker assignment is NOT read-only");
        s.ack_attempt(&worker.attempt_id).await.unwrap();
        s.complete_attempt(
            &worker.attempt_id,
            &agentgrid_common::CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                resolved_base_sha: None,
                remote_head_at_start: None,
                remote_head_at_finish: None,
                acp_session_id: None,
                plan: None,
                provenance: None,
                pending_artifacts: vec![],
                validation_rounds: 0,
            },
        )
        .await
        .unwrap();
        let _ = s.tick_workflow_run(&run.id).await.unwrap();

        // The verifier step task now assigns read-only.
        let verifier = s
            .try_assign(&node_id)
            .await
            .unwrap()
            .expect("verifier assign");
        assert!(verifier.read_only, "verifier assignment must be read-only");
    }
}
