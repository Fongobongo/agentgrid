use crate::helpers::*;

#[tokio::test]
async fn workflow_create_list_show_run_and_steps() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);

    let steps = json!([
        {"id":"a","prompt":"design","role":"architect","depends_on":[]},
        {"id":"b","prompt":"impl","role":"worker","depends_on":["a"]},
        {"id":"c","prompt":"verify","role":"verifier","depends_on":["a"]}
    ]);
    let body = json!({"name":"build","steps":steps,"context":null}).to_string();
    let resp = app
        .clone()
        .oneshot(post_auth("/v1/workflows", body, &test_token(&app).await))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let tpl: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let tid = tpl.get("id").unwrap().as_str().unwrap().to_string();
    assert!(tid.starts_with("wft-"));

    // list
    let resp = app
        .clone()
        .oneshot(get_auth("/v1/workflows", &test_token(&app).await))
        .await
        .unwrap();
    let list: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(list.get("items").unwrap().as_array().unwrap().len(), 1);

    // show
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/workflows/{tid}"),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // run
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/workflows/{tid}/runs"),
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let run: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let rid = run.get("id").unwrap().as_str().unwrap().to_string();
    assert_eq!(run.get("status").unwrap(), "pending");

    // show run + steps
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/workflow-runs/{rid}"),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let run_view: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(run_view.get("steps").unwrap().as_array().unwrap().len(), 3);
}

/// One-button assembly (`POST /v1/assembly`): goal in, marked run out;
/// empty goals and bad github targets are 400, oversized goals 413.
#[tokio::test]
async fn assembly_create_run_and_reject_bad_input() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let token = test_token(&app).await;

    let body = json!({
        "goal": "build a blog",
        "repository": "demo",
        "adapter": "mock",
        "auto_approve_plan": true,
    })
    .to_string();
    let resp = app
        .clone()
        .oneshot(post_auth("/v1/assembly", body, &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let run: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(run.get("status").unwrap(), "pending");
    assert_eq!(run.get("assembly").unwrap(), true);
    assert_eq!(run.get("auto_approve_plan").unwrap(), true);
    assert_eq!(run.get("adapter").unwrap(), "mock");
    let rid = run.get("id").unwrap().as_str().unwrap().to_string();

    // The ephemeral template holds one expandable architect step.
    let tpl_id = run.get("template_id").unwrap().as_str().unwrap();
    let resp = app
        .clone()
        .oneshot(get_auth(&format!("/v1/workflows/{tpl_id}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let tpl: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let steps = tpl.get("steps").unwrap().as_array().unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].get("role").unwrap(), "architect");
    assert!(steps[0]
        .get("prompt")
        .unwrap()
        .as_str()
        .unwrap()
        .contains("build a blog"));

    // show run + steps: only the architect so far.
    let resp = app
        .clone()
        .oneshot(get_auth(&format!("/v1/workflow-runs/{rid}"), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Empty goal → 400.
    let resp = app
        .clone()
        .oneshot(post_auth("/v1/assembly", r#"{"goal":"  "}"#.into(), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Bad github target → 400.
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/assembly",
            r#"{"goal":"x","github_push":true,"github_repo":"nope"}"#.into(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Oversized goal → 413.
    let big = "x".repeat(200 * 1024);
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/assembly",
            serde_json::to_string(&json!({"goal": big})).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn workflow_rejects_invalid_dag() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let steps = json!([{"id":"a","prompt":"x","depends_on":["ghost"]}]);
    let body = json!({"name":"bad","steps":steps}).to_string();
    let resp = app
        .clone()
        .oneshot(post_auth("/v1/workflows", body, &test_token(&app).await))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn workflow_create_rejects_cycle_duplicate_self_dep() {
    // ADR 0004: the DAG is validated at template-create time — a malformed
    // graph never reaches the scheduler (loud fail, BAD_REQUEST).
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);

    // Direct cycle a -> b -> a.
    let steps = json!([
        {"id":"a","prompt":"x","depends_on":["b"]},
        {"id":"b","prompt":"y","depends_on":["a"]}
    ]);
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/workflows",
            json!({"name":"cyc","steps":steps}).to_string(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "cycle rejected");

    // Duplicate ids.
    let steps = json!([
        {"id":"a","prompt":"x","depends_on":[]},
        {"id":"a","prompt":"y","depends_on":[]}
    ]);
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/workflows",
            json!({"name":"dup","steps":steps}).to_string(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "duplicate id rejected"
    );

    // Self-dependency.
    let steps = json!([{"id":"a","prompt":"x","depends_on":["a"]}]);
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/workflows",
            json!({"name":"self","steps":steps}).to_string(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "self-dep rejected");
}

#[tokio::test]
async fn workflow_schedule_fires_run_on_tick() {
    // Stage 13: a schedule with a small interval creates a new run when the
    // maintenance tick reaches its due time.
    use agentgrid_common::{WorkflowScheduleCreate, WorkflowTemplate};
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());

    // Create a template (simple single step).
    let body = "name: sched\nsteps:\n  - id: a\n    prompt: x\n    role: worker\n";
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/workflows")
                .header("content-type", "application/yaml")
                .header(
                    "authorization",
                    format!("Bearer {}", test_token(&app).await),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let tpl: WorkflowTemplate =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();

    // Create a schedule with a 2s interval, enabled.
    let create = serde_json::to_string(&WorkflowScheduleCreate {
        interval_seconds: 2,
        autonomy: "l2".into(),
        enabled: true,
    })
    .unwrap();
    let resp = app
        .clone()
        .oneshot(post_json(
            &format!("/v1/workflows/{}/schedules", tpl.id),
            create,
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let sched: agentgrid_common::WorkflowSchedule =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(sched.interval_seconds, 2);
    assert!(sched.enabled);

    // Initially: no runs.
    assert!(state
        .store
        .list_workflow_runs(None, None)
        .await
        .unwrap()
        .is_empty());

    // Tick with now = far future → due (last_run_at empty = due now).
    let created = state
        .store
        .tick_workflow_schedules(1_000_000_000)
        .await
        .unwrap();
    assert_eq!(created.len(), 1, "schedule should fire once");
    let runs = state.store.list_workflow_runs(None, None).await.unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, created[0]);

    // Advancing only 1s past last_run must NOT fire (interval is 2s).
    let again = state
        .store
        .tick_workflow_schedules(1_000_000_001)
        .await
        .unwrap();
    assert!(
        again.is_empty(),
        "schedule must not fire before interval elapses"
    );

    // Advancing 2s past last_run fires again.
    let again2 = state
        .store
        .tick_workflow_schedules(1_000_000_002)
        .await
        .unwrap();
    assert_eq!(again2.len(), 1, "schedule fires again after interval");

    // Disabled schedules never fire.
    state
        .store
        .delete_workflow_schedule(&sched.id)
        .await
        .unwrap();
    let again3 = state
        .store
        .tick_workflow_schedules(9_999_999_999)
        .await
        .unwrap();
    assert!(again3.is_empty(), "deleted schedule never fires");
}

#[tokio::test]
async fn typed_mailbox_emits_output_and_renders_handoff_block_in_pending_step_prompt() {
    // Stage 13 typed AgentMessage mailbox: when a step succeeds, the
    // orchestrator emits an `output` message broadcast; the next pending step
    // (its consumer) renders the handoff block into its task prompt on
    // activation. The rendered prompt carries the upstream step's id + kind.
    use agentgrid_common::{
        CompleteAttemptRequest, CreateWorkflowRequest, WorkflowRole, WorkflowStep, WorkflowTemplate,
    };
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, _cred) =
        enroll(&app, "mailbox-node", vec!["mock".into()], vec!["*".into()]).await;

    // Template: a -> b (b depends on a).
    let steps = vec![
        WorkflowStep {
            id: "a".into(),
            prompt: "do A".into(),
            depends_on: vec![],
            role: WorkflowRole::Worker,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: None,
            max_attempts: None,
            expandable: None,
        },
        WorkflowStep {
            id: "b".into(),
            prompt: "do B".into(),
            depends_on: vec!["a".into()],
            role: WorkflowRole::Worker,
            adapter: None,
            requested_node_id: None,
            base_commit: None,
            retryable: None,
            max_attempts: None,
            expandable: None,
        },
    ];
    let body = serde_json::to_string(&CreateWorkflowRequest {
        name: "mailbox".into(),
        steps,
        context: None,
        budget: None,
    })
    .unwrap();
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/workflows",
            body,
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let tpl: WorkflowTemplate =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let run = state
        .store
        .create_workflow_run(&tpl.id, None, Some("demo"), None)
        .await
        .unwrap();

    // Tick: step a activates.
    state.store.tick_workflow_run(&run.id).await.unwrap();
    let a1 = state.store.try_assign(&node_id).await.unwrap().unwrap();
    state.store.ack_attempt(&a1.attempt_id).await.unwrap();
    state
        .store
        .complete_attempt(
            &a1.attempt_id,
            &CompleteAttemptRequest {
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
    // Tick: a succeeds and emits its `output` message; b activates.
    state.store.tick_workflow_run(&run.id).await.unwrap();
    // Tick again so b actually starts (a's success is observed on this tick;
    // b then becomes ready and is scheduled on the next tick).
    state.store.tick_workflow_run(&run.id).await.unwrap();
    let steps_run = state.store.get_workflow_run_steps(&run.id).await.unwrap();
    let b = steps_run.iter().find(|s| s.step_id == "b").unwrap();
    assert_eq!(b.status, agentgrid_common::WorkflowStepStatus::Running);
    // One typed output message was emitted for a.
    assert_eq!(
        state.store.workflow_message_count(&run.id).await.unwrap(),
        1,
        "step a succeeded => one output message"
    );
    // The consuming task b's prompt was rendered with the handoff block.
    let b_task_id = state
        .store
        .get_workflow_run_projection(&run.id)
        .await
        .unwrap()
        .unwrap()
        .steps
        .into_iter()
        .find(|s| s.step_id == "b")
        .unwrap()
        .task_id
        .unwrap();
    let tv = state.store.show_task(&b_task_id).await.unwrap().unwrap();
    assert!(
        tv.prompt.contains("## Handoff from upstream steps"),
        "b's prompt has the handoff block: {}",
        tv.prompt
    );
    assert!(
        tv.prompt.contains("### `a`: output"),
        "handoff labels the upstream sender a"
    );
}

#[tokio::test]
async fn workflow_golden_architect_workers_integrator_verifier() {
    // Exit 7: architect -> 2 parallel workers -> integrator -> verifier runs
    // locally; the durable scheduler activates ready steps as Agentgrid tasks
    // and advances the DAG to a succeeded run (mock adapters, no network).
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "wf-node", vec!["mock".into()], vec!["*".into()]).await;

    let steps = json!([
        {"id":"arch","prompt":"design","role":"architect","depends_on":[]},
        {"id":"w1","prompt":"impl a","role":"worker","depends_on":["arch"]},
        {"id":"w2","prompt":"impl b","role":"worker","depends_on":["arch"]},
        {"id":"int","prompt":"merge","role":"integrator","depends_on":["w1","w2"]},
        {"id":"ver","prompt":"verify","role":"verifier","depends_on":["int"]}
    ]);
    let tpl_body = json!({"name":"golden","steps":steps}).to_string();
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/workflows",
            tpl_body,
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let tpl: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let tid = tpl.get("id").unwrap().as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/workflows/{tid}/runs"),
            json!({"repository":"demo"}).to_string(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let run: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let rid = run.get("id").unwrap().as_str().unwrap().to_string();

    let poll_req = PollRequest {
        node_id: node_id.clone(),
        name: "wf-node".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        protocol_version: None,
    };

    for _ in 0..200 {
        // Scheduler tick: activates ready steps + advances completed ones.
        let resp = app
            .clone()
            .oneshot(post_auth(
                &format!("/v1/workflow-runs/{rid}/tick"),
                "{}".into(),
                &test_token(&app).await,
            ))
            .await
            .unwrap();
        let rv: serde_json::Value =
            serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        let status = rv
            .get("run")
            .unwrap()
            .get("status")
            .unwrap()
            .as_str()
            .unwrap();
        if status == "succeeded" || status == "failed" {
            assert_eq!(status, "succeeded");
            break;
        }
        // Drive one pending task to completion (mock success), like the daemon.
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/v1/node/poll",
                serde_json::to_string(&poll_req).unwrap(),
                &cred,
            ))
            .await
            .unwrap();
        let pr: PollResponse =
            serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        if let Some(a) = pr.assignment {
            // Acknowledge before completing.
            ack_attempt(&app, &a.attempt_id, &cred, &a.fencing_token).await;
            let resp = app
                .clone()
                .oneshot(post_node(
                    &format!("/v1/node/attempts/{}/complete", a.attempt_id),
                    serde_json::to_string(&CompleteAttemptRequest {
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
                    })
                    .unwrap(),
                    &cred,
                    &a.fencing_token,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    }

    let rv = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/workflow-runs/{rid}"),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let rv: serde_json::Value =
        serde_json::from_slice(&to_bytes(rv.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(rv.get("run").unwrap().get("status").unwrap(), "succeeded");
    // All five steps ran to success.
    let steps_done = rv
        .get("steps")
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .all(|s| s.get("status").unwrap() == "succeeded");
    assert!(steps_done, "every step should succeed: {rv}");
}

/// Plan 1.5 (#16): executor-verifier trust loop. A always-reject verifier
/// (every verifier task exits non-zero) must re-run the upstream worker with
/// verifier feedback on each rejection, until the loop budget (the worker's
/// `max_attempts`) is exhausted — then the verifier step hard-fails and the
/// run fails. This asserts the loop counter actually advances: with worker
/// `max_attempts=3`, after the run settles the worker step has 3 attempts and
/// the run is `failed`.
#[tokio::test]
async fn workflow_verifier_loop_reruns_worker_until_budget() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "vloop-node", vec!["mock".into()], vec!["*".into()]).await;
    // Single login: the login endpoint is brute-force limited (10/60s) and
    // this test ticks many times, each needing an auth token.
    let tok = test_token(&app).await;

    // worker is retryable with max_attempts=3 — that is the loop budget.
    let steps = json!([
        {"id":"work","prompt":"impl","role":"worker","depends_on":[],
         "retryable":true,"max_attempts":3},
        {"id":"ver","prompt":"verify","role":"verifier","depends_on":["work"]}
    ]);
    let tpl_body = json!({"name":"vloop","steps":steps}).to_string();
    let resp = app
        .clone()
        .oneshot(post_auth("/v1/workflows", tpl_body, &tok))
        .await
        .unwrap();
    let tpl: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let tid = tpl.get("id").unwrap().as_str().unwrap().to_string();
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/workflows/{tid}/runs"),
            json!({"repository":"demo"}).to_string(),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let run: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let rid = run.get("id").unwrap().as_str().unwrap().to_string();

    let poll_req = PollRequest {
        node_id: node_id.clone(),
        name: "vloop-node".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        protocol_version: None,
    };

    // Drive: tick → poll → complete. The mock adapter runs the SAME action for
    // every task, so to force the verifier to always reject we complete worker
    // tasks with exit 0 and verifier tasks with exit !=0. Identity the attempt's
    // owning step via the task→role_runs lookup.
    let mut ticks = 0;
    loop {
        ticks += 1;
        assert!(ticks < 400, "run never settled: {rid}");
        let resp = app
            .clone()
            .oneshot(post_auth(
                &format!("/v1/workflow-runs/{rid}/tick"),
                "{}".into(),
                &tok,
            ))
            .await
            .unwrap();
        let rv: serde_json::Value =
            serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        let status = rv
            .get("run")
            .and_then(|r| r.get("status"))
            .and_then(|s| s.as_str())
            .unwrap_or("running");
        if status == "failed" {
            break;
        }
        // Drive one pending task to completion. Worker → success, verifier → fail.
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/v1/node/poll",
                serde_json::to_string(&poll_req).unwrap(),
                &cred,
            ))
            .await
            .unwrap();
        let pr: PollResponse =
            serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        let Some(a) = pr.assignment else {
            continue;
        };
        ack_attempt(&app, &a.attempt_id, &cred, &a.fencing_token).await;
        // Which step owns this attempt?
        let step_role = state.store.step_role_for_attempt(&a.task_id).await.unwrap();
        let req = if step_role.as_deref() == Some("verifier") {
            complete_fail_req()
        } else {
            complete_req()
        };
        let resp = app
            .clone()
            .oneshot(post_node(
                &format!("/v1/node/attempts/{}/complete", a.attempt_id),
                serde_json::to_string(&req).unwrap(),
                &cred,
                &a.fencing_token,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    // After settling: verifier step failed, worker step has exactly 3 attempts.
    let rv = app
        .clone()
        .oneshot(get_auth(&format!("/v1/workflow-runs/{rid}"), &tok))
        .await
        .unwrap();
    let rv: serde_json::Value =
        serde_json::from_slice(&to_bytes(rv.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(
        rv.get("run").unwrap().get("status").unwrap(),
        "failed",
        "run should fail when verifier loop budget is exhausted"
    );
    let steps_arr = rv.get("steps").unwrap().as_array().unwrap();
    let ver = steps_arr
        .iter()
        .find(|s| s.get("step_id").unwrap() == "ver")
        .unwrap();
    assert_eq!(ver.get("status").unwrap(), "failed");
    let work = steps_arr
        .iter()
        .find(|s| s.get("step_id").unwrap() == "work")
        .unwrap();
    let attempts = work.get("attempts").unwrap().as_u64().unwrap();
    assert_eq!(
        attempts, 3,
        "worker should be re-run exactly 3 times (the loop budget)"
    );
}

#[tokio::test]
async fn workflow_projection_endpoint_exposes_roles_and_verdicts() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "proj-node", vec!["mock".into()], vec!["*".into()]).await;

    let steps = json!([
        {"id":"arch","prompt":"design","role":"architect","depends_on":[]},
        {"id":"work","prompt":"impl","role":"worker","depends_on":["arch"]}
    ]);
    let tpl_body = json!({"name":"proj","steps":steps}).to_string();
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/workflows",
            tpl_body,
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let tpl: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let tid = tpl.get("id").unwrap().as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/workflows/{tid}/runs"),
            json!({"repository":"demo"}).to_string(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let run: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let rid = run.get("id").unwrap().as_str().unwrap().to_string();

    let poll_req = PollRequest {
        node_id: node_id.clone(),
        name: "proj-node".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        protocol_version: None,
    };

    app.clone()
        .oneshot(post_auth(
            &format!("/v1/workflow-runs/{rid}/tick"),
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/poll",
            serde_json::to_string(&poll_req).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    let pr: PollResponse =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let a = pr.assignment.expect("architect assigned");
    // Acknowledge before completing.
    ack_attempt(&app, &a.attempt_id, &cred, &a.fencing_token).await;
    let resp = app
        .clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{}/complete", a.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
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
            })
            .unwrap(),
            &cred,
            &a.fencing_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    for _ in 0..4 {
        app.clone()
            .oneshot(post_auth(
                &format!("/v1/workflow-runs/{rid}/tick"),
                "{}".into(),
                &test_token(&app).await,
            ))
            .await
            .unwrap();
    }

    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/workflow-runs/{rid}/projection"),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let proj: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let steps = proj.get("steps").unwrap().as_array().unwrap();
    assert_eq!(steps.len(), 2);
    let arch = steps
        .iter()
        .find(|s| s.get("step_id").unwrap() == "arch")
        .unwrap();
    assert_eq!(arch.get("role").unwrap(), "architect");
    assert_eq!(arch.get("verdict").unwrap(), "succeeded");
    assert_eq!(arch.get("node_id").unwrap().as_str().unwrap(), node_id);
    let work = steps
        .iter()
        .find(|s| s.get("step_id").unwrap() == "work")
        .unwrap();
    assert_eq!(work.get("role").unwrap(), "worker");
    assert!(
        work.get("task_id").unwrap().is_string(),
        "worker task should be spawned"
    );
}

/// Plan 1.6 (#3b): inline diff/plan annotations + "send for rework". A
/// reviewer leaves a comment "diff too big; split" pinned to a file line;
/// rework creates a NEW task whose prompt = original prompt + an
/// `[ANNOTATIONS]` block carrying the feedback.
#[tokio::test]
async fn annotation_and_rework_fold_feedback_into_new_task() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "anno-node", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:big.rs:lots").await;

    // Reviewer leaves an inline annotation pinned to line 42.
    let tok = test_token(&app).await;
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/attempts/{}/annotations", assign.attempt_id),
            serde_json::to_string(&serde_json::json!({
                "file": "src/big.rs",
                "line_start": 42,
                "line_end": 42,
                "comment": "diff too big; split"
            }))
            .unwrap(),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // List shows it back.
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/attempts/{}/annotations", assign.attempt_id),
            &tok,
        ))
        .await
        .unwrap();
    let anns: Vec<serde_json::Value> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(anns.len(), 1);
    assert_eq!(anns[0].get("comment").unwrap(), "diff too big; split");

    // Send for rework → a new task whose prompt carries the annotation.
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/attempts/{}/rework", assign.attempt_id),
            "{}".into(),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let rw: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let new_task_id = rw.get("task_id").unwrap().as_str().unwrap().to_string();
    assert_ne!(new_task_id, assign.task_id, "rework must create a new task");

    let tv = show_task_view(&app, &new_task_id).await;
    // Competitor-gap feature (convergence metrics): the rework task links
    // back to the reviewed attempt.
    assert_eq!(
        tv.rework_of.as_deref(),
        Some(assign.attempt_id.as_str()),
        "rework task must carry rework_of = source attempt"
    );
    assert!(
        tv.prompt.contains("[ANNOTATIONS]"),
        "rework prompt must carry the annotation block: {}",
        tv.prompt
    );
    assert!(
        tv.prompt.contains("diff too big; split"),
        "rework prompt must carry the comment: {}",
        tv.prompt
    );
    assert!(
        tv.prompt.contains("src/big.rs:L42"),
        "rework prompt must carry the file:line location: {}",
        tv.prompt
    );
    assert!(
        tv.prompt.contains("write:big.rs:lots"),
        "rework prompt must keep the original prompt: {}",
        tv.prompt
    );

    // Competitor-gap feature (auto-learnings): the annotation became a pending
    // repo-learning tied to the reviewed attempt.
    let resp = app
        .clone()
        .oneshot(get_auth("/v1/repos/demo/learnings", &tok))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let learnings: Vec<agentgrid_common::RepoLearning> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let auto = learnings
        .iter()
        .find(|l| l.statement.contains("diff too big; split"))
        .expect("rework annotation must become a repo-learning");
    assert!(
        !auto.approved,
        "auto-learning starts pending human approval"
    );
    assert_eq!(
        auto.source_attempt_id.as_deref(),
        Some(assign.attempt_id.as_str())
    );
    assert!(auto.statement.contains("src/big.rs:L42"));
}

/// Plan 1.7 (#14): a review comment pasting a noisy log (10k identical
/// stack-trace lines) must be compressed before it lands in the rework prompt
/// — dedup collapses the run to a `…×N` marker, so the prompt stays small.
#[tokio::test]
async fn rework_compresses_noisy_comment_via_token_budget_pipe() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "comp-node", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:x.rs:y").await;
    let tok = test_token(&app).await;

    // Reviewer pastes a 10k-line stack-trace dump into the comment.
    let noisy: String = "at com.Foo.bar(Foo.java:42)\n".repeat(10_000);
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/attempts/{}/annotations", assign.attempt_id),
            serde_json::to_string(&serde_json::json!({
                "file": "src/x.rs",
                "comment": noisy
            }))
            .unwrap(),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/attempts/{}/rework", assign.attempt_id),
            "{}".into(),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let rw: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let new_task_id = rw.get("task_id").unwrap().as_str().unwrap().to_string();
    let tv = show_task_view(&app, &new_task_id).await;

    assert!(
        tv.prompt.contains("…×"),
        "noisy run must be deduped into a marker: prompt len={}",
        tv.prompt.len()
    );
    assert!(
        tv.prompt.len() < noisy.len(),
        "compressed prompt ({}) must be far smaller than the raw paste ({})",
        tv.prompt.len(),
        noisy.len()
    );
}

#[tokio::test]
async fn workflow_budget_round_trips_via_json_create_and_get() {
    // Stage 13 Loop Engineering: a budget attached on create is persisted and
    // returned on get (NULL stays NULL/None = unbounded).
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let body = serde_json::json!({
        "name": "looped",
        "steps": [{"id":"a","prompt":"hi","role":"architect"}],
        "budget": {
            "max_messages": 10,
            "max_rounds": 5,
            "max_repeated_handoffs": 3
        }
    })
    .to_string();
    let resp = app
        .clone()
        .oneshot(post_auth("/v1/workflows", body, &test_token(&app).await))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let created: WorkflowTemplate =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let b = created
        .budget
        .clone()
        .expect("budget present on create response");
    assert_eq!(b.max_messages, Some(10));
    assert_eq!(b.max_repeated_handoffs, Some(3));
    // Get round-trips.
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/workflows/{}", created.id),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let fetched: WorkflowTemplate =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(fetched.budget, created.budget);
    // Listing reflects it.
    let resp = app
        .clone()
        .oneshot(get_auth("/v1/workflows", &test_token(&app).await))
        .await
        .unwrap();
    let list: ListResponse<WorkflowTemplate> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(list.items.len(), 1);
    assert!(list.items[0].budget.is_some());
}

/// Hardening P2 item 20: keyset cursor pagination for `GET /v1/workflow-runs`
/// (`after_created_at` + `after_id`, stable `(created_at, id)` order, page cap).
#[tokio::test]
async fn workflow_runs_keyset_pagination() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    // Create a minimal template so runs can be started.
    let body = serde_json::json!({
        "name": "page-tpl",
        "steps": [{ "id": "s1", "prompt": "do", "role": "worker" }],
    })
    .to_string();
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/workflows",
            body,
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    let tpl: WorkflowTemplate =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    // Start 5 runs via the store (fast; no tick needed).
    let mut runs = Vec::new();
    for i in 0..5 {
        let r = state
            .store
            .create_workflow_run(&tpl.id, None, Some(&format!("repo-{i}")), None)
            .await
            .unwrap();
        runs.push(r);
    }
    runs.sort_by(|a, b| (&a.created_at, &a.id).cmp(&(&b.created_at, &b.id)));

    let enc = |s: &str| s.replace('+', "%2B").replace(':', "%3A");
    async fn page(app: &Router, qs: &str) -> ListResponse<WorkflowRun> {
        let resp = app
            .clone()
            .oneshot(get_auth(
                &format!("/v1/workflow-runs{qs}"),
                &test_token(app).await,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    // Page 1: limit 2.
    let p1 = page(&app, "?limit=2").await;
    assert_eq!(p1.items.len(), 2);
    assert_eq!(p1.items[0].id, runs[0].id);
    assert_eq!(p1.items[1].id, runs[1].id);
    assert!(p1.next_cursor.is_some());
    // Page 2: after p1's last row.
    let p2 = page(
        &app,
        &format!(
            "?limit=2&after_created_at={}&after_id={}",
            enc(&p1.items[1].created_at),
            p1.items[1].id
        ),
    )
    .await;
    assert_eq!(p2.items.len(), 2);
    assert_eq!(p2.items[0].id, runs[2].id);
    assert_eq!(p2.items[1].id, runs[3].id);
    assert!(p2.next_cursor.is_some());
    // Page 3: remaining 1.
    let p3 = page(
        &app,
        &format!(
            "?limit=2&after_created_at={}&after_id={}",
            enc(&p2.items[1].created_at),
            p2.items[1].id
        ),
    )
    .await;
    assert_eq!(p3.items.len(), 1);
    assert_eq!(p3.items[0].id, runs[4].id);
    assert!(p3.next_cursor.is_none());

    let ids: std::collections::HashSet<String> = p1
        .items
        .iter()
        .chain(p2.items.iter())
        .chain(p3.items.iter())
        .map(|r| r.id.clone())
        .collect();
    assert_eq!(ids.len(), 5, "workflow-run pages must not overlap or skip");
}
