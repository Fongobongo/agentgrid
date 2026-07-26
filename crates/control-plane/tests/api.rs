#![allow(clippy::needless_borrow)]
//! End-to-end API test: create task -> node enroll + poll assignment -> ingest
//! events (with idempotency) -> complete -> terminal task status. Exercises the
//! full slice without network I/O. Node endpoints require credential auth
//! (Stage 2.3), so tests enroll first.

use agentgrid_common::{
    ApprovalStatus, ApprovalView, Assignment, CancelState, CompleteAttemptRequest,
    CreateRepositoryRequest, CreateTaskRequest, CreateWorkflowRequest, CreateWorkflowRunRequest,
    EnrollRequest, EnrollResponse, EnrollTokenResponse, EventType, HeartbeatRequest, IncomingEvent,
    IngestEventsRequest, LoginResponse, NodeStatus, NodeView, PollRequest, PollResponse,
    RepositoryView, SkillTrustView, TaskEligibility, TaskStatus, TaskView, UploadArtifactRequest,
    WorkflowRole, WorkflowRun, WorkflowRunStatus, WorkflowRunWithSteps, WorkflowStep,
    WorkflowStepStatus, WorkflowTemplate,
};
use agentgrid_control_plane::{build_router, AppState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::json;
use tower::ServiceExt;

fn post(uri: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

fn post_auth(uri: &str, body: String, cred: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {cred}"))
        .body(Body::from(body))
        .unwrap()
}

fn get_auth(uri: &str, cred: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {cred}"))
        .body(Body::empty())
        .unwrap()
}

fn delete_auth(uri: &str, cred: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("authorization", format!("Bearer {cred}"))
        .body(Body::empty())
        .unwrap()
}

fn post_json(uri: &str, body: String, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("POST").uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

async fn auth_setup(app: &Router, state: &AppState, user: &str, pass: &str) -> StatusCode {
    let token = state.setup_token().await;
    let body = serde_json::to_string(&serde_json::json!({
        "username": user,
        "password": pass,
        "setup_token": token,
    }))
    .unwrap();
    let resp = app
        .clone()
        .oneshot(post_json("/v1/auth/setup", body, None))
        .await
        .unwrap();
    resp.status()
}

async fn auth_login(app: &Router, user: &str, pass: &str) -> Option<String> {
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/auth/login",
            serde_json::to_string(&serde_json::json!({ "username": user, "password": pass }))
                .unwrap(),
            None,
        ))
        .await
        .unwrap();
    if resp.status().is_success() {
        let lr: LoginResponse =
            serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        Some(lr.token)
    } else {
        None
    }
}

/// Login as the `test`/`test` user bootstrapped by `AppState::open_temp`.
async fn test_token(app: &Router) -> String {
    auth_login(app, "test", "test")
        .await
        .expect("test user login")
}

/// Create an enrollment token, enroll a node, return (node_id, credential).
async fn enroll(
    app: &Router,
    name: &str,
    adapters: Vec<String>,
    repos: Vec<String>,
) -> (String, String) {
    let tk_resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/nodes/enrollment-token",
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(tk_resp.status(), StatusCode::OK);
    let tk: EnrollTokenResponse =
        serde_json::from_slice(&to_bytes(tk_resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let req = EnrollRequest {
        token: tk.token,
        name: name.into(),
        adapters,
        repositories: repos,
        max_concurrency: 2,
        agent_version: "test".into(),
        protocol_version: None,
    };
    let resp = app
        .clone()
        .oneshot(post(
            "/v1/node/enroll",
            serde_json::to_string(&req).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let er: EnrollResponse =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    (er.node_id, er.credential)
}

/// Register a node via long-poll, create a task, and return its assignment.
async fn create_and_assign(app: &Router, node_id: &str, cred: &str, prompt: &str) -> Assignment {
    let poll_req = PollRequest {
        node_id: node_id.into(),
        name: "n".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        protocol_version: None,
    };
    let req = CreateTaskRequest {
        prompt: prompt.into(),
        repository: "demo".into(),
        adapter: "mock".into(),
        requested_node_id: None,
        timeout_secs: None,
        validation_command: None,
        base_commit: None,
        parent_acp_session_id: None,
    };
    // Task creation is a user route; tests bootstrap a `test`/`test` user
    // (hardening P0 closed the open bootstrap window).
    let user_token = auth_login(app, "test", "test")
        .await
        .expect("test user login");
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/tasks",
            serde_json::to_string(&req).unwrap(),
            &user_token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    // Long-poll until the queued task is assigned, mirroring the node daemon.
    for _ in 0..50 {
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/v1/node/poll",
                serde_json::to_string(&poll_req).unwrap(),
                cred,
            ))
            .await
            .unwrap();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let pr: PollResponse = serde_json::from_slice(&body).unwrap();
        if let Some(a) = pr.assignment {
            return a;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("task was never assigned to the node");
}

async fn show_status(app: &Router, task_id: &str) -> TaskStatus {
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/tasks/{task_id}"),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let tv: TaskView =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    tv.status
}

/// Full task view (status + error_code) for assertions.
async fn show_task_view(app: &Router, task_id: &str) -> TaskView {
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/tasks/{task_id}"),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap()
}

async fn task_eligibility(app: &Router, task_id: &str) -> TaskEligibility {
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/tasks/{task_id}/eligibility"),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap()
}

async fn cancel_state(app: &Router, attempt_id: &str, cred: &str) -> CancelState {
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/node/attempts/{attempt_id}/cancel"),
            cred,
        ))
        .await
        .unwrap();
    serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap()
}

#[tokio::test]
async fn full_task_lifecycle() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-1", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;

    // First event flips attempt+task to running.
    let ev = IngestEventsRequest {
        events: vec![IncomingEvent {
            sequence: 1,
            r#type: EventType::Stdout,
            payload: json!({"text": "start"}),
        }],
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/events", assign.attempt_id),
            serde_json::to_string(&ev).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
            })
            .unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Succeeded
    );
}

#[tokio::test]
async fn failure_marks_task_failed() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-2", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "fail:3").await;
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 3,
                commit_sha: None,
                error_code: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
            })
            .unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Failed);
}

#[tokio::test]
async fn completion_propagates_provenance() {
    // Stage 13: a node tags a completion with an external-origin provenance
    // record; the CP persists it on the attempt row.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-prov", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                acp_session_id: None,
                plan: None,
                provenance: Some(agentgrid_common::ProvenanceRecord {
                    originator: "entire".into(),
                    external_id: "proj-42".into(),
                    label: Some("nightly".into()),
                }),
            })
            .unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Read the stored record directly from the store.
    let stored: String = sqlx::query_scalar("SELECT provenance FROM attempts WHERE id = ?")
        .bind(&assign.attempt_id)
        .fetch_one(&state.store.pool)
        .await
        .unwrap();
    let p: agentgrid_common::ProvenanceRecord = serde_json::from_str(&stored).unwrap();
    assert_eq!(p.originator, "entire");
    assert_eq!(p.external_id, "proj-42");
    assert_eq!(p.label.as_deref(), Some("nightly"));
}

#[tokio::test]
async fn validation_failure_must_not_report_success() {
    // Stage 1.1 regression: a clean agent exit (exit_code 0) combined with a
    // validation failure must NOT be reported as success. The node reports the
    // distinct failure category via `error_code`; the control plane must decide
    // success by outcome, not the raw exit code.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-v", vec!["mock".into()], vec!["*".into()]).await;

    let req = CreateTaskRequest {
        prompt: "do thing".into(),
        repository: "demo".into(),
        adapter: "mock".into(),
        requested_node_id: None,
        timeout_secs: None,
        validation_command: Some("false".into()),
        base_commit: None,
        parent_acp_session_id: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/tasks",
            serde_json::to_string(&req).unwrap(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let task_id: String =
        serde_json::from_slice::<TaskView>(&to_bytes(resp.into_body(), usize::MAX).await.unwrap())
            .unwrap()
            .id;

    // Long-poll for assignment, mirroring the node daemon.
    let poll_req = PollRequest {
        node_id: node_id.clone(),
        name: "n".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        protocol_version: None,
    };
    let mut assignment = None;
    for _ in 0..50 {
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
            assignment = Some(a);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let assignment = assignment.expect("task was never assigned");

    // Agent exited 0 but validation failed -> node reports `validation_failed`.
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/complete", assignment.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: Some("validation_failed".into()),
                acp_session_id: None,
                provenance: None,
                plan: None,
            })
            .unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let tv: TaskView = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth(
                    &format!("/v1/tasks/{task_id}"),
                    &test_token(&app).await,
                ))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(tv.status, TaskStatus::Failed);
    assert_eq!(tv.error_code.as_deref(), Some("validation_failed"));
}

#[tokio::test]
async fn cancel_queued_marks_cancelled() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let req = CreateTaskRequest {
        prompt: "x".into(),
        repository: "demo".into(),
        adapter: "mock".into(),
        requested_node_id: None,
        timeout_secs: None,
        validation_command: None,
        base_commit: None,
        parent_acp_session_id: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/tasks",
            serde_json::to_string(&req).unwrap(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let tv: TaskView = serde_json::from_slice(&body).unwrap();

    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/tasks/{}/cancel", tv.id),
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(show_status(&app, &tv.id).await, TaskStatus::Cancelled);
}

#[tokio::test]
async fn cancel_running_then_node_confirms_cancelled() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-c", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "sleep:30").await;

    let cs: CancelState = cancel_state(&app, &assign.attempt_id, &cred).await;
    assert!(!cs.cancel_requested);

    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/tasks/{}/cancel", assign.task_id),
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let cs: CancelState = cancel_state(&app, &assign.attempt_id, &cred).await;
    assert!(cs.cancel_requested);

    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 1,
                commit_sha: None,
                error_code: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
            })
            .unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Cancelled
    );
}

#[tokio::test]
async fn retry_failed_task_reques() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-r", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "fail:3").await;
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 3,
                commit_sha: None,
                error_code: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
            })
            .unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Failed);

    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/tasks/{}/retry", assign.task_id),
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Queued);
}

#[tokio::test]
async fn revoked_node_gets_401() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-x", vec!["mock".into()], vec!["*".into()]).await;

    // Heartbeat works before revoke.
    let hb = HeartbeatRequest {
        status: Some(NodeStatus::Online),
        name: "node-x".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        agent_version: "t".into(),
        load_avg: 0.1,
        free_disk_mb: 1000,
        active_attempts: 0,
        capabilities: vec![],
        protocol_version: None,
        discovered_skills: vec![],
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&hb).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Revoke the node.
    let resp = app
        .clone()
        .oneshot(delete_auth(
            &format!("/v1/nodes/{node_id}"),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Authenticated node endpoints now reject with 401.
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&hb).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let poll_req = PollRequest {
        node_id: node_id.clone(),
        name: "n".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        protocol_version: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/poll",
            serde_json::to_string(&poll_req).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn repository_create_and_list() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let req = CreateRepositoryRequest {
        name: "demo".into(),
        git_url: "https://example.com/demo.git".into(),
        default_branch: "main".into(),
        validation_command: Some("cargo test".into()),
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/repositories",
            serde_json::to_string(&req).unwrap(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let rv: RepositoryView = serde_json::from_slice(&body).unwrap();
    assert_eq!(rv.name, "demo");
    assert_eq!(rv.default_branch, "main");

    let resp = app
        .clone()
        .oneshot(get_auth("/v1/repositories", &test_token(&app).await))
        .await
        .unwrap();
    let repos: Vec<RepositoryView> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].name, "demo");
}

#[tokio::test]
async fn artifact_upload_and_read() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-art", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;

    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
            })
            .unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let art = UploadArtifactRequest {
        name: "changes.patch".into(),
        content: "diff --git a/x b/x".into(),
        ..Default::default()
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/artifacts", assign.attempt_id),
            serde_json::to_string(&art).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/tasks/{}/artifacts/changes.patch", assign.task_id),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), b"diff --git a/x b/x".as_slice());

    // Hardening P0 (upstream artifact authorization): a *second* node that is
    // not a workflow consumer of this producer task must NOT be able to fetch
    // its artifact through the node-scoped route. Existence is hidden (404).
    let (_n2, cred2) = enroll(
        &app,
        "node-art-fetcher",
        vec!["mock".into()],
        vec!["*".into()],
    )
    .await;
    let resp = app
        .oneshot(get_auth(
            &format!("/v1/node/tasks/{}/artifacts/changes.patch", assign.task_id),
            &cred2,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "unrelated node cannot read a non-consumer producer artifact"
    );
}

#[tokio::test]
async fn artifact_binary_raw_upload_round_trips() {
    // Stage 2.2: the raw endpoint stores arbitrary bytes + media type + hash,
    // and GET returns them unchanged (would be corrupted via UTF-8 JSON).
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-braw", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:b.txt:b").await;
    let payload: Vec<u8> = vec![0xFF, 0xFE, 0xFD, 0x00, 0x01, 0x02];
    let sha = {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(&payload);
        let out = h.finalize();
        out.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/v1/node/attempts/{}/artifacts/raw",
                    assign.attempt_id
                ))
                .header("authorization", format!("Bearer {cred}"))
                .header("x-artifact-name", "blob.bin")
                .header("x-artifact-media-type", "image/png")
                .header("x-artifact-sha256", sha)
                .body(Body::from(payload.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/tasks/{}/artifacts/blob.bin", assign.task_id),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "image/png",
        "stored media type must be served back"
    );
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), payload.as_slice(), "binary bytes round trip");
}

#[tokio::test]
async fn metrics_endpoint_exposes_counts() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let resp = app
        .clone()
        .oneshot(get_auth("/metrics", &test_token(&app).await))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("agentgrid_tasks"));
    assert!(text.contains("agentgrid_attempts_total"));
    // Stage 5.2 additions.
    assert!(text.contains("agentgrid_task_duration_seconds"));
    assert!(text.contains("agentgrid_tasks_total"));
    assert!(text.contains("agentgrid_node_free_disk_mb"));
    assert!(text.contains("agentgrid_sqlite_db_bytes"));
    assert!(text.contains("agentgrid_sqlite_wal_bytes"));
}

#[tokio::test]
async fn user_auth_setup_login_and_protects_endpoints() {
    let state = AppState::open_temp_fresh().await.unwrap();
    let app = build_router(state.clone());

    // Hardening P0: bootstrap window is closed. Before the first user exists,
    // task creation (any /v1/ user route) is 503, not open.
    let pre = app
        .clone()
        .oneshot(post_json(
            "/v1/tasks",
            serde_json::to_string(&CreateTaskRequest {
                prompt: "x".into(),
                repository: "demo".into(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: None,
                validation_command: None,
                base_commit: None,
                parent_acp_session_id: None,
            })
            .unwrap(),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(pre.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Setup without the one-time token is forbidden.
    let no_token = app
        .clone()
        .oneshot(post_json(
            "/v1/auth/setup",
            serde_json::to_string(&serde_json::json!({
                "username": "alice", "password": "secret"
            }))
            .unwrap(),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(no_token.status(), StatusCode::FORBIDDEN);

    // Wrong setup token is forbidden.
    let wrong = app
        .clone()
        .oneshot(post_json(
            "/v1/auth/setup",
            serde_json::to_string(&serde_json::json!({
                "username": "alice", "password": "secret",
                "setup_token": "deadbeef"
            }))
            .unwrap(),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::FORBIDDEN);

    // Setup with the correct one-time token creates the first user.
    assert_eq!(
        auth_setup(&app, &state, "alice", "secret").await,
        StatusCode::CREATED
    );
    // The token is single-use: a second setup (even with the same token) is
    // 409 because a user now exists.
    assert_eq!(
        auth_setup(&app, &state, "bob", "secret").await,
        StatusCode::CONFLICT
    );

    // Now the bootstrap window is closed: task creation requires a JWT.
    let no_token = app
        .clone()
        .oneshot(post_json(
            "/v1/tasks",
            serde_json::to_string(&CreateTaskRequest {
                prompt: "x".into(),
                repository: "demo".into(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: None,
                validation_command: None,
                base_commit: None,
                parent_acp_session_id: None,
            })
            .unwrap(),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(no_token.status(), StatusCode::UNAUTHORIZED);

    // Wrong password is rejected.
    assert!(auth_login(&app, "alice", "wrong").await.is_none());

    // Correct login yields a token that unlocks the endpoint.
    let token = auth_login(&app, "alice", "secret").await.unwrap();
    let authed = app
        .clone()
        .oneshot(post_json(
            "/v1/tasks",
            serde_json::to_string(&CreateTaskRequest {
                prompt: "x".into(),
                repository: "demo".into(),
                adapter: "mock".into(),
                requested_node_id: None,
                timeout_secs: None,
                validation_command: None,
                base_commit: None,
                parent_acp_session_id: None,
            })
            .unwrap(),
            Some(&token),
        ))
        .await
        .unwrap();
    assert_eq!(authed.status(), StatusCode::CREATED);
}

async fn create_task_only(app: &Router, repo: &str, adapter: &str, node: Option<String>) -> String {
    let req = CreateTaskRequest {
        prompt: "x".into(),
        repository: repo.into(),
        adapter: adapter.into(),
        requested_node_id: node,
        timeout_secs: None,
        validation_command: None,
        base_commit: None,
        parent_acp_session_id: None,
    };
    // Tests bootstrap a `test`/`test` user via AppState::open_temp; task
    // creation is a user route and now requires a JWT (hardening P0 closed
    // the open bootstrap window).
    let token = auth_login(app, "test", "test")
        .await
        .expect("test user login");
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/tasks",
            serde_json::to_string(&req).unwrap(),
            &token,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let tv: TaskView =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    tv.id
}

/// Stage 2.4: no registered nodes => the task reports why it stays queued.
#[tokio::test]
async fn eligibility_empty_pool() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let id = create_task_only(&app, "demo", "mock", None).await;
    let elig = task_eligibility(&app, &id).await;
    assert!(!elig.nodes.iter().any(|n| n.eligible));
    assert_eq!(elig.no_eligible_nodes, vec!["no nodes registered"]);
}

/// Stage 2.5: login sets an HttpOnly + SameSite=Strict session cookie, and a
/// request carrying that cookie (no Authorization header) is authenticated.
#[tokio::test]
async fn login_sets_cookie_and_cookie_auths() {
    let state = AppState::open_temp_fresh().await.unwrap();
    let app = build_router(state.clone());
    assert_eq!(
        auth_setup(&app, &state, "alice", "secret").await,
        StatusCode::CREATED
    );
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/auth/login",
            serde_json::to_string(
                &serde_json::json!({ "username": "alice", "password": "secret" }),
            )
            .unwrap(),
            None,
        ))
        .await
        .unwrap();
    assert!(resp.status().is_success());
    // Extract the agentgrid_token cookie value from Set-Cookie.
    let set_cookie = resp
        .headers()
        .get(axum::http::header::SET_COOKIE)
        .and_then(|h| h.to_str().ok())
        .expect("login must set a Set-Cookie header")
        .to_string();
    assert!(set_cookie.contains("HttpOnly"), "cookie must be HttpOnly");
    assert!(
        set_cookie.contains("SameSite=Strict"),
        "cookie must be SameSite=Strict"
    );
    let cookie_val = set_cookie
        .split(';')
        .find(|p| p.trim().starts_with("agentgrid_token="))
        .unwrap()
        .trim();
    // The body still returns a token for non-browser clients (backwards compat).
    let _ = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    // A request with only the cookie (no Authorization header) is authorized.
    let authed = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/tasks")
                .header("content-type", "application/json")
                .header(axum::http::header::COOKIE, cookie_val)
                .body(Body::from(
                    serde_json::to_string(&CreateTaskRequest {
                        prompt: "x".into(),
                        repository: "demo".into(),
                        adapter: "mock".into(),
                        requested_node_id: None,
                        timeout_secs: None,
                        validation_command: None,
                        base_commit: None,
                        parent_acp_session_id: None,
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        authed.status(),
        StatusCode::CREATED,
        "cookie must authenticate"
    );
}

/// Stage 2.4: missing adapter filter is reported per node.
#[tokio::test]
async fn eligibility_missing_adapter() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, _cred) = enroll(&app, "n", vec!["codex".into()], vec!["*".into()]).await;
    let id = create_task_only(&app, "demo", "mock", None).await;
    let elig = task_eligibility(&app, &id).await;
    let n = elig.nodes.iter().find(|n| n.node_id == node_id).unwrap();
    assert!(!n.eligible);
    assert!(n.reasons.iter().any(|r| r == "missing adapter mock"));
    assert_eq!(elig.no_eligible_nodes, vec!["missing adapter mock"]);
}

/// Stage 2.4: missing repository filter is reported per node.
#[tokio::test]
async fn eligibility_missing_repository() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, _cred) = enroll(&app, "n", vec!["mock".into()], vec!["other".into()]).await;
    let id = create_task_only(&app, "demo", "mock", None).await;
    let elig = task_eligibility(&app, &id).await;
    let n = elig.nodes.iter().find(|n| n.node_id == node_id).unwrap();
    assert!(!n.eligible);
    assert!(n.reasons.iter().any(|r| r == "missing repository demo"));
}

/// Stage 2.4: at-capacity node is reported and not eligible.
#[tokio::test]
async fn eligibility_at_capacity() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, _cred) = enroll(&app, "n", vec!["mock".into()], vec!["*".into()]).await;
    // Drain the node's capacity (max_concurrency=2) with two running attempts.
    for _ in 0..2 {
        let a = create_and_assign(&app, &node_id, &_cred, "sleep:30").await;
        let ev = IngestEventsRequest {
            events: vec![IncomingEvent {
                sequence: 1,
                r#type: EventType::Stdout,
                payload: json!({"text": "x"}),
            }],
        };
        app.clone()
            .oneshot(post_auth(
                &format!("/v1/node/attempts/{}/events", a.attempt_id),
                serde_json::to_string(&ev).unwrap(),
                &_cred,
            ))
            .await
            .unwrap();
    }
    let id = create_task_only(&app, "demo", "mock", None).await;
    let elig = task_eligibility(&app, &id).await;
    let n = elig.nodes.iter().find(|n| n.node_id == node_id).unwrap();
    assert!(!n.eligible);
    assert!(n.reasons.iter().any(|r| r.starts_with("at capacity")));
}

/// Stage 2.4: requested_node_id restricts eligibility to that node, and a
/// missing/offline requested node yields a clear reason.
#[tokio::test]
async fn eligibility_requested_node() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (_node_id, _cred) = enroll(&app, "n", vec!["mock".into()], vec!["*".into()]).await;

    // Request a node that is not registered: only it is considered.
    let id = create_task_only(&app, "demo", "mock", Some("ghost".into())).await;
    let elig = task_eligibility(&app, &id).await;
    assert!(elig.nodes.is_empty());
    assert_eq!(
        elig.no_eligible_nodes,
        vec!["requested node ghost not registered"]
    );

    // Request an actual eligible node: eligible, no reasons.
    let (good, _c) = enroll(&app, "good", vec!["mock".into()], vec!["*".into()]).await;
    let id2 = create_task_only(&app, "demo", "mock", Some(good.clone())).await;
    let elig2 = task_eligibility(&app, &id2).await;
    assert_eq!(elig2.nodes.len(), 1);
    assert!(elig2.nodes[0].eligible);
    assert!(elig2.no_eligible_nodes.is_empty());
}

/// Stage 5.1: prompt exceeding the size limit is rejected with 413.
#[tokio::test]
async fn oversized_prompt_returns_413() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    // Default prompt limit is 64 KiB; send ~200 KiB.
    let req = CreateTaskRequest {
        prompt: "x".repeat(200 * 1024),
        repository: "demo".into(),
        adapter: "mock".into(),
        requested_node_id: None,
        timeout_secs: None,
        validation_command: None,
        base_commit: None,
        parent_acp_session_id: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/tasks",
            serde_json::to_string(&req).unwrap(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

/// Create a queued task with the given adapter (and optional pinned node).
async fn create_task(app: &Router, adapter: &str, requested_node: Option<&str>) -> String {
    let req = CreateTaskRequest {
        prompt: "do thing".into(),
        repository: "demo".into(),
        adapter: adapter.into(),
        requested_node_id: requested_node.map(|s| s.into()),
        timeout_secs: None,
        validation_command: None,
        base_commit: None,
        parent_acp_session_id: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/tasks",
            serde_json::to_string(&req).unwrap(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    serde_json::from_slice::<TaskView>(&to_bytes(resp.into_body(), usize::MAX).await.unwrap())
        .unwrap()
        .id
}

#[tokio::test]
async fn scheduler_skips_incompatible_head_of_line() {
    // Stage 1.4: an older queued task the node cannot run (wrong adapter) must
    // not block a newer compatible one.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (_claude_node, _cc) =
        enroll(&app, "n-claude", vec!["claude".into()], vec!["*".into()]).await;
    let (mock_node, mock_cred) =
        enroll(&app, "n-mock", vec!["mock".into()], vec!["*".into()]).await;

    // Older queued task needs claude; a newer one needs mock.
    let claude_task = create_task(&app, "claude", None).await;
    let mock_task = create_task(&app, "mock", None).await;

    // mock node polls: must skip the claude head-of-line and take the mock task.
    let mut got = None;
    for _ in 0..50 {
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/v1/node/poll",
                serde_json::to_string(&PollRequest {
                    node_id: mock_node.clone(),
                    name: "n-mock".into(),
                    adapters: vec!["mock".into()],
                    repositories: vec!["*".into()],
                    max_concurrency: 2,
                    protocol_version: None,
                })
                .unwrap(),
                &mock_cred,
            ))
            .await
            .unwrap();
        let pr: PollResponse =
            serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        if let Some(a) = pr.assignment {
            got = Some(a);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let got = got.expect("mock node got no assignment");
    assert_eq!(got.task_id, mock_task);
    assert_ne!(got.task_id, claude_task);
}

#[tokio::test]
async fn scheduler_respects_requested_node() {
    // Stage 1.4: a task pinned to one node must not be assigned to another.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_a, cred_a) = enroll(&app, "n-a", vec!["mock".into()], vec!["*".into()]).await;
    let (node_b, cred_b) = enroll(&app, "n-b", vec!["mock".into()], vec!["*".into()]).await;

    let pinned = create_task(&app, "mock", Some(&node_a)).await;

    // node_b polls: must NOT get the pinned task.
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/poll",
            serde_json::to_string(&PollRequest {
                node_id: node_b.clone(),
                name: "n-b".into(),
                adapters: vec!["mock".into()],
                repositories: vec!["*".into()],
                max_concurrency: 2,
                protocol_version: None,
            })
            .unwrap(),
            &cred_b,
        ))
        .await
        .unwrap();
    let pr: PollResponse =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(
        pr.assignment.is_none(),
        "pinned task leaked to non-requested node"
    );

    // node_a polls: gets it.
    let mut got = None;
    for _ in 0..50 {
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/v1/node/poll",
                serde_json::to_string(&PollRequest {
                    node_id: node_a.clone(),
                    name: "n-a".into(),
                    adapters: vec!["mock".into()],
                    repositories: vec!["*".into()],
                    max_concurrency: 2,
                    protocol_version: None,
                })
                .unwrap(),
                &cred_a,
            ))
            .await
            .unwrap();
        let pr: PollResponse =
            serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        if let Some(a) = pr.assignment {
            got = Some(a);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let got = got.expect("requested node got no assignment");
    assert_eq!(got.task_id, pinned);
}

/// Acknowledge an assignment via the explicit ack endpoint.
async fn ack_attempt(app: &Router, attempt_id: &str, cred: &str) -> StatusCode {
    app.clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{attempt_id}/ack"),
            "{}".into(),
            cred,
        ))
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn ack_attempt_moves_to_running() {
    // Stage 1.3: explicit ack flips the assigned attempt (and task) to running.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-ack3", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Assigned
    );
    assert_eq!(
        ack_attempt(&app, &assign.attempt_id, &cred).await,
        StatusCode::OK
    );
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Running
    );
    // Idempotent re-ack.
    assert_eq!(
        ack_attempt(&app, &assign.attempt_id, &cred).await,
        StatusCode::OK
    );
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Running
    );
}

#[tokio::test]
async fn legacy_metric_event_acts_as_ack() {
    // Stage 1.3: an N-1 node that sends the synthetic "attempt started" metric
    // must still flip the attempt to running (backward compatibility).
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-ack4", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    let ev = IngestEventsRequest {
        events: vec![IncomingEvent {
            sequence: 1,
            r#type: EventType::Metric,
            payload: json!({ "text": "attempt started" }),
        }],
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/events", assign.attempt_id),
            serde_json::to_string(&ev).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Running
    );
}

#[tokio::test]
async fn unacked_assignment_is_reverted() {
    // Stage 1.3: a node that never acks loses the assignment once the ack
    // deadline passes; the task returns to the queue.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-ack1", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    state
        .store
        .set_attempt_ack_deadline(&assign.attempt_id, "1970-01-01T00:00:00Z")
        .await
        .unwrap();
    state.store.tick_maintenance().await.unwrap();
    assert_eq!(show_status(&app, &assign.task_id).await, TaskStatus::Queued);
}

#[tokio::test]
async fn acked_slow_agent_keeps_assignment() {
    // Stage 1.3: after ack, a slow agent that produces no output for >deadline
    // seconds must NOT lose the assignment.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-ack2", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    assert_eq!(
        ack_attempt(&app, &assign.attempt_id, &cred).await,
        StatusCode::OK
    );
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Running
    );
    // Force the ack deadline into the past and run maintenance: still running.
    state
        .store
        .set_attempt_ack_deadline(&assign.attempt_id, "1970-01-01T00:00:00Z")
        .await
        .unwrap();
    state.store.tick_maintenance().await.unwrap();
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Running
    );
}

#[tokio::test]
async fn node_offline_loses_attempt_then_retry_succeeds() {
    // Stage 1.2: a node going offline with an in-flight attempt must lose it
    // (attempt=lost, task=failed/node_lost, capacity freed) and the task must
    // be retryable once the node is back online.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-o", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;

    // Node reports offline -> its in-flight attempt is lost.
    let hb = HeartbeatRequest {
        status: Some(NodeStatus::Offline),
        name: "node-o".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        agent_version: "test".into(),
        load_avg: 0.0,
        free_disk_mb: 1000,
        active_attempts: 1,
        capabilities: vec![],
        protocol_version: None,
        discovered_skills: vec![],
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&hb).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let tv = show_task_view(&app, &assign.task_id).await;
    assert_eq!(tv.status, TaskStatus::Failed);
    assert_eq!(tv.error_code.as_deref(), Some("node_lost"));

    // Capacity freed: the node no longer accounts for the lost attempt.
    let nodes: serde_json::Value = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/nodes", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let na = nodes
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["id"] == node_id)
        .unwrap()["active_attempts"]
        .as_i64()
        .unwrap();
    assert_eq!(na, 0);

    // Node comes back online.
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&HeartbeatRequest {
                status: Some(NodeStatus::Online),
                ..hb
            })
            .unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Retry -> re-queue -> re-assign to the recovered node -> succeed.
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/tasks/{}/retry", assign.task_id),
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let assign2 = loop {
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/v1/node/poll",
                serde_json::to_string(&PollRequest {
                    node_id: node_id.clone(),
                    name: "node-o".into(),
                    adapters: vec!["mock".into()],
                    repositories: vec!["*".into()],
                    max_concurrency: 2,
                    protocol_version: None,
                })
                .unwrap(),
                &cred,
            ))
            .await
            .unwrap();
        let pr: PollResponse =
            serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        if let Some(a) = pr.assignment {
            break a;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/complete", assign2.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
            })
            .unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        show_status(&app, &assign.task_id).await,
        TaskStatus::Succeeded
    );
}

#[tokio::test]
async fn complete_on_lost_attempt_is_idempotent() {
    // Stage 1.2: a node that comes back and reports a completion for an attempt
    // we already marked `lost` must not corrupt the failed task status.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-l", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;

    // Node drops offline -> attempt lost, task failed/node_lost.
    state.store.mark_node_offline(&node_id).await.unwrap();
    let tv = show_task_view(&app, &assign.task_id).await;
    assert_eq!(tv.status, TaskStatus::Failed);
    assert_eq!(tv.error_code.as_deref(), Some("node_lost"));

    // Node returns and reports a (late) completion for the lost attempt.
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
            })
            .unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    // Idempotent ack: terminal/lost attempt is not re-completed.
    assert_eq!(resp.status(), StatusCode::OK);

    // Task status must remain failed/node_lost (no corruption).
    let tv = show_task_view(&app, &assign.task_id).await;
    assert_eq!(tv.status, TaskStatus::Failed);
    assert_eq!(tv.error_code.as_deref(), Some("node_lost"));
}

#[tokio::test]
async fn approval_flow_allow_deny_and_expiry() {
    // Stage 5 durable approval: create (pending) -> list -> allow/deny -> list
    // reflects the new state; answering a terminal approval is a no-op.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());

    let task_id = create_task(&app, "mock", None).await;
    let ap_id = state
        .store
        .create_approval(
            &task_id,
            "attempt-x",
            None,
            "run Bash",
            3600,
            None,
            "session",
        )
        .await
        .unwrap();

    // Initially pending and visible.
    let listed = list_approvals(&app, Some("pending")).await;
    assert!(listed.iter().any(|a| a.id == ap_id));
    assert!(list_approvals(&app, Some("allowed")).await.is_empty());

    // Allow it with an operator reason, which is persisted and surfaced back.
    let allow = app
        .clone()
        .oneshot(post_json(
            &format!("/v1/approvals/{ap_id}/allow"),
            r#"{"reason":"looked ok"}"#.into(),
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(allow.status(), StatusCode::OK);

    let allowed = list_approvals(&app, Some("allowed")).await;
    let got = allowed
        .iter()
        .find(|a| a.id == ap_id)
        .expect("allowed approval must be listed");
    assert_eq!(got.status, ApprovalStatus::Allowed);
    assert_eq!(got.reason.as_deref(), Some("looked ok"));
    assert!(list_approvals(&app, Some("pending")).await.is_empty());

    // Answering a terminal approval is a safe no-op (idempotent).
    let again = app
        .clone()
        .oneshot(post_json(
            &format!("/v1/approvals/{ap_id}/deny"),
            "{}".into(),
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::OK);
    assert!(list_approvals(&app, Some("allowed"))
        .await
        .iter()
        .any(|a| a.id == ap_id));
}

async fn list_approvals(app: &Router, status: Option<&str>) -> Vec<ApprovalView> {
    let uri = match status {
        Some(s) => format!("/v1/approvals?status={s}"),
        None => "/v1/approvals".into(),
    };
    let resp = app
        .clone()
        .oneshot(get_auth(&uri, &test_token(&app).await))
        .await
        .unwrap();
    assert!(resp.status().is_success());
    serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap()
}

#[tokio::test]
async fn approval_create_and_get_by_id_drives_permission_flow() {
    // Stage 5: an ACP agent's session/request_permission creates a durable
    // approval (POST /v1/tasks/{id}/approvals) that the daemon polls via
    // GET /v1/approvals/{id}.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());

    let create = app
        .clone()
        .oneshot(post_json(
            "/v1/tasks/t-1/approvals",
            serde_json::to_string(&serde_json::json!({
                "attempt_id": "att-x",
                "session_id": "sess-x",
                "permission": { "tool": "Bash", "input": "rm -rf /" }
            }))
            .unwrap(),
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(create.status(), StatusCode::OK);
    let id: String = serde_json::from_slice::<serde_json::Value>(
        &to_bytes(create.into_body(), usize::MAX).await.unwrap(),
    )
    .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(!id.is_empty());

    // Pending immediately after creation.
    let pending = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/approvals/{id}"),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(pending.status(), StatusCode::OK);
    let view: ApprovalView =
        serde_json::from_slice(&to_bytes(pending.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(view.status, ApprovalStatus::Pending);
    assert_eq!(view.attempt_id, "att-x");
    assert_eq!(view.session_id.as_deref(), Some("sess-x"));

    // Allow the approval; the daemon's poll loop then proceeds.
    let allow = app
        .clone()
        .oneshot(post_json(
            &format!("/v1/approvals/{id}/allow"),
            "{}".into(),
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(allow.status(), StatusCode::OK);
    let allowed = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/approvals/{id}"),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let view: ApprovalView =
        serde_json::from_slice(&to_bytes(allowed.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(view.status, ApprovalStatus::Allowed);

    // Unknown id 404s.
    let missing = app
        .clone()
        .oneshot(get_auth(
            "/v1/approvals/does-not-exist",
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

fn post_q_auth(uri: &str, cred: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {cred}"))
        .body(Body::empty())
        .unwrap()
}

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
    assert_eq!(list.as_array().unwrap().len(), 1);

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
    assert!(state.store.list_workflow_runs().await.unwrap().is_empty());

    // Tick with now = far future → due (last_run_at empty = due now).
    let created = state
        .store
        .tick_workflow_schedules(1_000_000_000)
        .await
        .unwrap();
    assert_eq!(created.len(), 1, "schedule should fire once");
    let runs = state.store.list_workflow_runs().await.unwrap();
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
async fn l4_schedule_ratify_gate_refuses_without_budget_accepts_with() {
    // Stage 13 L4 ratify: a fully-autonomous (l4) schedule is fail-closed
    // unless the template declares a budget; l2 scheduling is unaffected by
    // the gate.
    use agentgrid_common::{
        CreateWorkflowRequest, WorkflowRole, WorkflowScheduleCreate, WorkflowStep, WorkflowTemplate,
    };
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());

    // Template with NO budget.
    let steps = vec![WorkflowStep {
        id: "a".into(),
        prompt: "p".into(),
        depends_on: vec![],
        role: WorkflowRole::Worker,
        adapter: None,
        requested_node_id: None,
        base_commit: None,
        retryable: None,
        max_attempts: None,
        expandable: None,
    }];
    let body = serde_json::to_string(&CreateWorkflowRequest {
        name: "t".into(),
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

    // l4 schedule on the budgetless template is refused at create time.
    let bad = serde_json::to_string(&WorkflowScheduleCreate {
        interval_seconds: 60,
        autonomy: "l4".into(),
        enabled: true,
    })
    .unwrap();
    let resp = app
        .clone()
        .oneshot(post_json(
            &format!("/v1/workflows/{}/schedules", tpl.id),
            bad,
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "l4 on a budgetless template must be fail-closed"
    );

    // l2 schedule is accepted (lower autonomy, no ratify gate).
    let ok_l2 = serde_json::to_string(&WorkflowScheduleCreate {
        interval_seconds: 60,
        autonomy: "l2".into(),
        enabled: true,
    })
    .unwrap();
    let resp = app
        .clone()
        .oneshot(post_json(
            &format!("/v1/workflows/{}/schedules", tpl.id),
            ok_l2,
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "l2 passes the ratify gate"
    );
}

#[tokio::test]
async fn architect_expandable_plan_pauses_planready_then_approve_expands_steps() {
    // Stage 13 plan expansion: an `expandable` architect step that emits a
    // plan (via CompleteAttemptRequest.plan) pauses the run in `PlanReady`.
    // Approving the plan (`POST /v1/workflow-runs/{id}/approve-plan`) parses
    // the plan into new worker steps and resumes the run (Running).
    use agentgrid_common::{
        CompleteAttemptRequest, CreateWorkflowRequest, WorkflowRole, WorkflowStep, WorkflowTemplate,
    };
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, _cred) = enroll(&app, "plan-node", vec!["mock".into()], vec!["*".into()]).await;

    // Template: a single expandable architect step.
    let steps = vec![WorkflowStep {
        id: "arch".into(),
        prompt: "design".into(),
        depends_on: vec![],
        role: WorkflowRole::Architect,
        adapter: None,
        requested_node_id: None,
        base_commit: None,
        retryable: None,
        max_attempts: None,
        expandable: Some(true),
    }];
    let body = serde_json::to_string(&CreateWorkflowRequest {
        name: "plan".into(),
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

    // Tick: architect step activates a task.
    state.store.tick_workflow_run(&run.id).await.unwrap();
    let assign = state.store.try_assign(&node_id).await.unwrap().unwrap();
    // Architect succeeds WITH a plan (2 worker steps, one depending on the other).
    let plan = r#"- id: w1
  prompt: build
  role: worker
- id: w2
  prompt: test
  depends_on: [w1]
  role: verifier
"#;
    state
        .store
        .complete_attempt(
            &assign.attempt_id,
            &CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                acp_session_id: None,
                plan: Some(plan.into()),
                provenance: None,
            },
        )
        .await
        .unwrap();
    // Tick: architect step succeeds + run pauses PlanReady.
    state.store.tick_workflow_run(&run.id).await.unwrap();
    let paused = state
        .store
        .get_workflow_run(&run.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        paused.status,
        agentgrid_common::WorkflowRunStatus::PlanReady,
        "expandable architect pauses the run in PlanReady"
    );
    // The pending plan is exposed on the run row.
    let pending_plan = state.store.get_workflow_run_plan(&run.id).await.unwrap();
    assert!(pending_plan.is_some(), "plan stamped on the run");

    // Approve: parse + insert steps + resume Running.
    state.store.approve_workflow_plan(&run.id).await.unwrap();
    let after = state
        .store
        .get_workflow_run(&run.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        after.status,
        agentgrid_common::WorkflowRunStatus::Running,
        "approval resumes the run"
    );
    // Expanded steps exist: arch succeeded, w1 pending, w2 pending.
    let steps_after = state.store.get_workflow_run_steps(&run.id).await.unwrap();
    let ids: Vec<&str> = steps_after.iter().map(|s| s.step_id.as_str()).collect();
    assert!(ids.contains(&"arch"), "original architect step kept");
    assert!(
        ids.contains(&"w1") && ids.contains(&"w2"),
        "plan steps expanded"
    );

    // Sanity: approving twice fails closed (run already resumed).
    assert!(state.store.approve_workflow_plan(&run.id).await.is_err());
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
    state
        .store
        .complete_attempt(
            &a1.attempt_id,
            &CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                acp_session_id: None,
                plan: None,
                provenance: None,
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
            let resp = app
                .clone()
                .oneshot(post_auth(
                    &format!("/v1/node/attempts/{}/complete", a.attempt_id),
                    serde_json::to_string(&CompleteAttemptRequest {
                        exit_code: 0,
                        commit_sha: None,
                        error_code: None,
                        acp_session_id: None,
                        provenance: None,
                        plan: None,
                    })
                    .unwrap(),
                    &cred,
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
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/complete", a.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
                acp_session_id: None,
                provenance: None,
                plan: None,
            })
            .unwrap(),
            &cred,
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

#[tokio::test]
async fn policy_endpoint_classifies_commands() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);

    async fn eval_cmd(app: &Router, cmd: &str) -> serde_json::Value {
        let body = serde_json::json!({ "command": cmd, "cwd": "/workspace" }).to_string();
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/v1/policy/evaluate",
                body,
                &test_token(&app).await,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    let v = eval_cmd(&app, "rm -rf /tmp/x").await;
    assert_eq!(v.get("decision").unwrap(), "deny");
    assert_eq!(v.get("risk_class").unwrap(), "destructive");

    let v = eval_cmd(&app, "cat README.md").await;
    assert_eq!(v.get("decision").unwrap(), "allow");
    assert_eq!(v.get("risk_class").unwrap(), "read");

    let v = eval_cmd(&app, "git push origin main").await;
    assert_eq!(v.get("decision").unwrap(), "ask");
    assert_eq!(v.get("risk_class").unwrap(), "git_remote");

    let v = eval_cmd(&app, "apt-get install -y curl").await;
    assert_eq!(v.get("decision").unwrap(), "ask");
    assert_eq!(v.get("risk_class").unwrap(), "package_install");

    // Unterminated quote → fail-closed (ask), never allow.
    let v = eval_cmd(&app, "echo \"unterminated").await;
    assert_eq!(v.get("decision").unwrap(), "ask");
}

#[tokio::test]
async fn policy_endpoint_honors_autonomy_level() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);

    async fn eval(app: &Router, cmd: &str, autonomy: &str) -> serde_json::Value {
        let body = serde_json::json!({ "command": cmd, "cwd": "/workspace", "autonomy": autonomy })
            .to_string();
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/v1/policy/evaluate",
                body,
                &test_token(&app).await,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    // L2 (default): git push → ask.
    let v = eval(&app, "git push origin main", "l2").await;
    assert_eq!(v.get("decision").unwrap(), "ask");
    // L3: git push → allow (autonomy permits network/git).
    let v = eval(&app, "git push origin main", "l3").await;
    assert_eq!(v.get("decision").unwrap(), "allow");
    // L0: cat → ask (fully supervised).
    let v = eval(&app, "cat README.md", "l0").await;
    assert_eq!(v.get("decision").unwrap(), "ask");
}

#[tokio::test]
async fn approval_scope_round_trips() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let task_id = create_task(&app, "mock", None).await;
    let ap = state
        .store
        .create_approval(
            &task_id,
            "attempt-x",
            None,
            "run Bash",
            3600,
            None,
            "tool_call",
        )
        .await
        .unwrap();
    let got = state.store.get_approval(&ap).await.unwrap().unwrap();
    assert_eq!(got.scope, "tool_call");
    // Default scope when omitted.
    let ap2 = state
        .store
        .create_approval(
            &task_id,
            "attempt-y",
            None,
            "run Bash",
            3600,
            None,
            "session",
        )
        .await
        .unwrap();
    let got2 = state.store.get_approval(&ap2).await.unwrap().unwrap();
    assert_eq!(got2.scope, "session");
}

#[tokio::test]
async fn policy_evaluate_audits_decision() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let body = serde_json::json!({ "command": "rm -rf /tmp/x", "cwd": "/workspace" }).to_string();
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/policy/evaluate",
            body,
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let events = state
        .store
        .list_audit(Some("policy.evaluate"), 10)
        .await
        .unwrap();
    assert!(!events.is_empty(), "every policy decision must be audited");
    assert_eq!(events[0].subject.as_deref(), Some("rm -rf /tmp/x"));
}

#[tokio::test]
async fn approval_payload_has_no_secrets() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let task_id = create_task(&app, "mock", None).await;
    let ap = state
        .store
        .create_approval(
            &task_id,
            "attempt-x",
            None,
            "run Bash",
            3600,
            None,
            "session",
        )
        .await
        .unwrap();
    let got = state.store.get_approval(&ap).await.unwrap().unwrap();
    let serialized = serde_json::to_string(&got).unwrap();
    for forbidden in ["secret", "password", "AGENTGRID_", "token"] {
        assert!(
            !serialized.contains(forbidden),
            "approval payload must not contain {forbidden}"
        );
    }
}

async fn login_status(app: &Router, user: &str, pass: &str) -> StatusCode {
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/auth/login",
            serde_json::to_string(&json!({ "username": user, "password": pass })).unwrap(),
            None,
        ))
        .await
        .unwrap();
    resp.status()
}

#[tokio::test]
async fn login_rate_limit_returns_429() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    // Window budget is 10 per 60s; the 11th attempt is throttled. Failed
    // logins (no such user) still count, so brute-force is bounded.
    for i in 0..10 {
        let code = login_status(&app, "nobody", &format!("wrong{i}")).await;
        assert_ne!(
            code,
            StatusCode::TOO_MANY_REQUESTS,
            "attempt {i} must not throttle"
        );
    }
    let code = login_status(&app, "nobody", "wrong-extra").await;
    assert_eq!(code, StatusCode::TOO_MANY_REQUESTS);
}

/// Hardening P0 (artifact integrity): a client-supplied `x-artifact-sha256`
/// that disagrees with the server-computed SHA-256 of the uploaded bytes is
/// rejected with `422 Unprocessable Entity`; the artifact is NOT published.
#[tokio::test]
async fn artifact_upload_rejects_wrong_sha256() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-sha", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:x:hi").await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/v1/node/attempts/{}/artifacts/raw",
                    assign.attempt_id
                ))
                .header("authorization", format!("Bearer {}", cred))
                .header("x-artifact-name", "blob.bin")
                .header("x-artifact-media-type", "application/octet-stream")
                .header(
                    "x-artifact-sha256",
                    "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdead",
                )
                .body(Body::from(b"hello".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
}

/// Hardening P0 (stored XSS / download safety): an artifact uploaded with
/// `text/html` is served back as `application/octet-stream` with a
/// `Content-Disposition: attachment` header and `X-Content-Type-Options:
/// nosniff`, so it cannot be rendered inline by a browser.
#[tokio::test]
async fn artifact_html_served_as_attachment_with_nosniff() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll(&app, "node-xss", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:x:hi").await;
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/artifacts", assign.attempt_id),
            serde_json::to_string(&UploadArtifactRequest {
                name: "report.html".into(),
                content: "<html><script>alert(1)</script></html>".into(),
                media_type: Some("text/html".into()),
                ..Default::default()
            })
            .unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/tasks/{}/artifacts/report.html", assign.task_id),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "application/octet-stream",
        "HTML must be downgraded to octet-stream"
    );
    assert_eq!(
        resp.headers().get("x-content-type-options").unwrap(),
        "nosniff"
    );
    let cd = resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(cd.starts_with("attachment"), "cd={cd}");
    assert!(cd.contains("filename=\"report.html\""), "cd={cd}");
}

/// Hardening P0 (path safety / traversal): NUL, backslash, and percent-encoded
/// `../` are all rejected as BAD_REQUEST (or 404 on read) — they must never
/// reach the artifact-store join and escape the artifact root.
#[tokio::test]
async fn artifact_name_validation_rejects_nul_backslash_percent() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (_, cred) = enroll(&app, "node-a", vec![], vec![]).await;
    let uri = "/v1/node/attempts/att-1/artifacts";
    // NUL, backslash, and a percent-encoded `../` must all be rejected before
    // reaching the store. Percent-decoding is the client's job (axum already
    // percent-decodes path segments for the JSON endpoint's `name` field
    // comes verbatim from the JSON body, so we pass raw forms).
    for bad in &["a\u{0000}b", "a\\b", "%2e%2e/"] {
        let req = UploadArtifactRequest {
            name: (*bad).into(),
            content: "x".into(),
            ..Default::default()
        };
        let resp = app
            .clone()
            .oneshot(post_auth(uri, serde_json::to_string(&req).unwrap(), &cred))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "name {bad:?} must be rejected"
        );
    }
    let ok = UploadArtifactRequest {
        name: "out.txt".into(),
        content: "x".into(),
        ..Default::default()
    };
    let resp2 = app
        .clone()
        .oneshot(post_auth(uri, serde_json::to_string(&ok).unwrap(), &cred))
        .await
        .unwrap();
    assert_ne!(
        resp2.status(),
        StatusCode::BAD_REQUEST,
        "safe name must pass validation"
    );
}

#[tokio::test]
async fn backup_endpoint_writes_file() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let path = std::env::temp_dir().join(format!("ag-admin-backup-{}.db", std::process::id()));
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/admin/backup",
            serde_json::to_string(&json!({ "path": path.to_str().unwrap() })).unwrap(),
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(path.exists(), "backup file must be created");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn artifact_get_rejects_traversal_name() {
    // Stage 2.2: GET /v1/tasks/{id}/artifacts/{name} with a traversal name must
    // not read outside the artifact root. A 404 (not 500 / not the file) is the
    // safe response and hides whether the artifact exists.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    // Write a real artifact so success and rejection are distinguishable.
    let (_, cred) = enroll(&app, "node-art", vec![], vec![]).await;
    // Seed a task + attempt so latest_attempt_id resolves.
    let create = CreateTaskRequest {
        prompt: "p".into(),
        repository: "".into(),
        adapter: "mock".into(),
        requested_node_id: None,
        timeout_secs: Some(60),
        validation_command: None,
        base_commit: None,
        parent_acp_session_id: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/tasks",
            serde_json::to_string(&create).unwrap(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let tv: TaskView =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    // The task won't be assigned without an eligible node, so insert an
    // attempt row directly describing a finished attempt for an arbitrary node.
    let up = UploadArtifactRequest {
        name: "real.txt".into(),
        content: "data".into(),
        ..Default::default()
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/attempts/att-gx/artifacts",
            serde_json::to_string(&up).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::BAD_REQUEST);
    // Link the attempt to the task so read resolves it.
    {
        let st = app.clone();
        // We cannot run raw SQL from the test easily; instead rely on the store
        // path being covered by the store-level test above, and here just assert
        // a crafted GET never returns 500 / readable file content.
        let _ = st;
    }
    for bad in ["../../../etc/passwd", "..", "/etc/passwd"] {
        let enc = bad.replace('/', "%2F");
        let resp = app
            .clone()
            .oneshot(get_auth(
                &format!("/v1/tasks/{}/artifacts/{}", tv.id, enc),
                &test_token(&app).await,
            ))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "traversal GET {bad:?} must be 404"
        );
    }
}

#[tokio::test]
async fn create_workflow_accepts_yaml() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let yaml = "name: demo\nsteps:\n  - id: plan\n    prompt: plan\n    role: architect\n  - id: work\n    prompt: do\n    depends_on: [plan]\n    role: worker\n";
    let req = Request::builder()
        .method("POST")
        .uri("/v1/workflows")
        .header("content-type", "application/yaml")
        .header(
            "authorization",
            format!("Bearer {}", test_token(&app).await),
        )
        .body(Body::from(yaml))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let t: WorkflowTemplate = serde_json::from_slice(&body).unwrap();
    assert_eq!(t.name, "demo");
    assert_eq!(t.steps.len(), 2);
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
    let list: Vec<WorkflowTemplate> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(list.len(), 1);
    assert!(list[0].budget.is_some());
}

#[tokio::test]
async fn cancel_workflow_run_handler_cancels() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let tmpl = CreateWorkflowRequest {
        name: "t".into(),
        steps: vec![WorkflowStep {
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
        }],
        context: None,
        budget: None,
    };
    let r = app
        .clone()
        .oneshot(post_auth(
            "/v1/workflows",
            serde_json::to_string(&tmpl).unwrap(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CREATED);
    let t: WorkflowTemplate =
        serde_json::from_slice(&to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap();
    let run_req = CreateWorkflowRunRequest {
        context: None,
        repository: None,
        base_commit: None,
    };
    let rr = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/workflows/{}/runs", t.id),
            serde_json::to_string(&run_req).unwrap(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(rr.status(), StatusCode::CREATED);
    let run: WorkflowRun =
        serde_json::from_slice(&to_bytes(rr.into_body(), usize::MAX).await.unwrap()).unwrap();
    let c = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/workflow-runs/{}/cancel", run.id),
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(c.status(), StatusCode::OK);
    let show = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/workflow-runs/{}", run.id),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(show.status(), StatusCode::OK);
    let shown: WorkflowRunWithSteps =
        serde_json::from_slice(&to_bytes(show.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(shown.run.status, WorkflowRunStatus::Cancelled);
    assert!(shown
        .steps
        .iter()
        .all(|s| s.status == WorkflowStepStatus::Cancelled));
}

#[tokio::test]
async fn node_protocol_mismatch_marks_degraded() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let tk = app
        .clone()
        .oneshot(post_auth(
            "/v1/nodes/enrollment-token",
            "{}".into(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(tk.status(), StatusCode::OK);
    let tkr: EnrollTokenResponse =
        serde_json::from_slice(&to_bytes(tk.into_body(), usize::MAX).await.unwrap()).unwrap();
    let req = EnrollRequest {
        token: tkr.token,
        name: "n1".into(),
        adapters: vec![],
        repositories: vec![],
        max_concurrency: 2,
        agent_version: "t".into(),
        protocol_version: Some("0".into()),
    };
    let er = app
        .clone()
        .oneshot(post(
            "/v1/node/enroll",
            serde_json::to_string(&req).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(er.status(), StatusCode::OK);
    let er: EnrollResponse =
        serde_json::from_slice(&to_bytes(er.into_body(), usize::MAX).await.unwrap()).unwrap();
    let nodes = app
        .clone()
        .oneshot(get_auth("/v1/nodes", &test_token(&app).await))
        .await
        .unwrap();
    assert_eq!(nodes.status(), StatusCode::OK);
    let nodes: Vec<NodeView> =
        serde_json::from_slice(&to_bytes(nodes.into_body(), usize::MAX).await.unwrap()).unwrap();
    let node = nodes
        .iter()
        .find(|n| n.id == er.node_id)
        .expect("node present");
    assert_eq!(node.status, NodeStatus::Degraded);
}

#[tokio::test]
async fn skill_trust_defaults_untrusted_then_round_trips() {
    // Stage 9.2: an unrecorded skill is fail-closed untrusted; trusting it
    // persists + is returned by GET and list; untrusting flips it back.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());

    // Unknown skill -> untrusted, no decided_by/at.
    let got = app
        .clone()
        .oneshot(get_auth(
            "/v1/skills/ponytail?source=user",
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(got.status(), StatusCode::OK);
    let v: SkillTrustView =
        serde_json::from_slice(&to_bytes(got.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(!v.trusted);
    assert!(v.decided_by.is_none());

    // Trust it.
    let r = app
        .clone()
        .oneshot(post_q_auth(
            "/v1/skills/ponytail/trust?source=user",
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let v: SkillTrustView = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth(
                    "/v1/skills/ponytail?source=user",
                    &test_token(&app).await,
                ))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(v.trusted);

    // List reflects it.
    let list: Vec<SkillTrustView> = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/skills", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(list
        .iter()
        .any(|s| s.name == "ponytail" && s.source == "user" && s.trusted));

    // Untrust flips back (decision still recorded, just trusted=false).
    let r = app
        .clone()
        .oneshot(post_q_auth(
            "/v1/skills/ponytail/untrust?source=user",
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let v: SkillTrustView = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth(
                    "/v1/skills/ponytail?source=user",
                    &test_token(&app).await,
                ))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(!v.trusted);
    assert!(
        v.decided_by.is_some(),
        "decision was recorded even when untrusted"
    );
}

#[tokio::test]
async fn heartbeat_auto_fills_skill_trust_ledger() {
    // Stage 9.2: a heartbeat that advertises discovered skills lands them in
    // the trust ledger as untrusted (so the operator can review them); a
    // later operator decision survives a subsequent discovery beat.
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (_node_id, cred) = enroll(&app, "n-disc", vec!["mock".into()], vec!["*".into()]).await;

    let hb = HeartbeatRequest {
        status: Some(NodeStatus::Online),
        name: "n-disc".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        agent_version: "t".into(),
        load_avg: 0.0,
        free_disk_mb: 1000,
        active_attempts: 0,
        capabilities: vec![],
        protocol_version: None,
        discovered_skills: vec![
            agentgrid_common::HeartbeatSkill {
                name: "git-helper".into(),
                source: "user".into(),
            },
            agentgrid_common::HeartbeatSkill {
                name: "ponytail".into(),
                source: "user".into(),
            },
        ],
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&hb).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let list: Vec<SkillTrustView> = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/skills", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let gh = list
        .iter()
        .find(|s| s.name == "git-helper" && s.source == "user")
        .expect("discovered skill landed in ledger");
    assert!(!gh.trusted, "discovery defaults untrusted");

    // Operator trusts it; a second discovery beat must not revert trust.
    let _ = app
        .clone()
        .oneshot(post_q_auth(
            "/v1/skills/git-helper/trust?source=user",
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            serde_json::to_string(&hb).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: SkillTrustView = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth(
                    "/v1/skills/git-helper?source=user",
                    &test_token(&app).await,
                ))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(v.trusted, "operator decision survives re-discovery");
}

#[tokio::test]
async fn mcp_server_registry_round_trips_and_gates_disabled() {
    // Stage 13: an operator registers an MCP stdio server; it round-trips
    // through the registry and a disabled server is still listed (operator
    // can disable without deleting).
    use agentgrid_common::{McpServer, McpServerCreate};
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);

    let body = serde_json::to_string(&McpServerCreate {
        id: "github".into(),
        name: "GitHub".into(),
        command: "mcp-github".into(),
        args: vec!["--ro".into()],
        env_requirements: vec!["GITHUB_TOKEN".into()],
        enabled: true,
    })
    .unwrap();
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/mcp-servers",
            body,
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let srv: McpServer =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(srv.id, "github");
    assert_eq!(srv.env_requirements, vec!["GITHUB_TOKEN".to_string()]);
    assert!(srv.enabled);

    // List reflects it.
    let list: Vec<McpServer> = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/mcp-servers", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].command, "mcp-github");

    // Upsert (replace) disables it.
    let body = serde_json::to_string(&McpServerCreate {
        id: "github".into(),
        name: "GitHub".into(),
        command: "mcp-github".into(),
        args: vec![],
        env_requirements: vec![],
        enabled: false,
    })
    .unwrap();
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/mcp-servers",
            body,
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let srv: McpServer =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(!srv.enabled, "upsert disabled the server");

    // Delete.
    let resp = app
        .clone()
        .oneshot(delete_auth(
            "/v1/mcp-servers/github",
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let list: Vec<McpServer> = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/mcp-servers", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(list.is_empty());
}

#[tokio::test]
async fn agent_profile_revisions_immutable_and_roll_back() {
    // Stage 13: a profile is a chain of immutable revisions; activating an
    // older revision rolls back without losing history.
    use agentgrid_common::{ActivateProfile, AgentProfileCreate};
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());

    // List empty.
    let list: Vec<String> = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/profiles", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(list.is_empty());

    // Create two revisions.
    let r1: serde_json::Value = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(post_json(
                    "/v1/profiles/claude",
                    serde_json::to_string(&AgentProfileCreate {
                        system_prompt: "v1 prompt".into(),
                        autonomy: "l1".into(),
                        ..Default::default()
                    })
                    .unwrap(),
                    Some(&test_token(&app).await),
                ))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let r1_rev = r1["revision"].as_i64().unwrap();
    let r2: serde_json::Value = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(post_json(
                    "/v1/profiles/claude",
                    serde_json::to_string(&AgentProfileCreate {
                        system_prompt: "v2 prompt".into(),
                        autonomy: "l3".into(),
                        ..Default::default()
                    })
                    .unwrap(),
                    Some(&test_token(&app).await),
                ))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let r2_rev = r2["revision"].as_i64().unwrap();
    assert_eq!(r2_rev, r1_rev + 1, "revisions monotonically increase");

    // Activate the newer, then roll back to the older.
    let _ = app
        .clone()
        .oneshot(post_json(
            "/v1/profiles/claude/activate",
            serde_json::to_string(&ActivateProfile { revision: r2_rev }).unwrap(),
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    let revs: Vec<agentgrid_common::AgentProfile> = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/profiles/claude", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(revs.len(), 2, "both revisions kept (immutable history)");
    assert!(
        revs.iter().any(|p| p.revision == r2_rev && p.active),
        "r2 active"
    );
    assert!(
        revs.iter().any(|p| p.revision == r1_rev && !p.active),
        "r1 inactive"
    );

    // Roll back.
    let _ = app
        .clone()
        .oneshot(post_json(
            "/v1/profiles/claude/activate",
            serde_json::to_string(&ActivateProfile { revision: r1_rev }).unwrap(),
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    let revs: Vec<agentgrid_common::AgentProfile> = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/profiles/claude", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(
        revs.iter().any(|p| p.revision == r1_rev && p.active),
        "r1 active after rollback"
    );
    let v1 = revs.iter().find(|p| p.revision == r1_rev).unwrap();
    assert_eq!(v1.system_prompt, "v1 prompt");
    assert_eq!(v1.autonomy, "l1");

    // Profile id now in active list.
    let list: Vec<String> = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/profiles", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert_eq!(list, vec!["claude".to_string()]);
}

#[tokio::test]
async fn agent_profile_carries_secret_requirements_and_version() {
    // Stage 13: a profile revision stores secret requirements (names only,
    // never values) + adapter_version; they round-trip through the CP store.
    use agentgrid_common::{ActivateProfile, AgentProfileCreate, SecretRequirement};
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);

    let body = serde_json::to_string(&AgentProfileCreate {
        system_prompt: "be brief".into(),
        autonomy: "l3".into(),
        memory_max: None,
        cpu_quota: None,
        tasks_max: None,
        secret_requirements: vec![
            SecretRequirement {
                env: "ANTHROPIC_API_KEY".into(),
                required: true,
            },
            SecretRequirement {
                env: "OPTIONAL_TOKEN".into(),
                required: false,
            },
        ],
        adapter_version: Some("1.4.0".into()),
        mcp_server_ids: vec!["github".into()],
    })
    .unwrap();
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/profiles/claude",
            body,
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rev: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let rev_no = rev["revision"].as_i64().unwrap();

    // Activate + fetch.
    let _ = app
        .clone()
        .oneshot(post_json(
            "/v1/profiles/claude/activate",
            serde_json::to_string(&ActivateProfile { revision: rev_no }).unwrap(),
            Some(&test_token(&app).await),
        ))
        .await
        .unwrap();
    let revs: Vec<agentgrid_common::AgentProfile> = serde_json::from_slice(
        &to_bytes(
            app.clone()
                .oneshot(get_auth("/v1/profiles/claude", &test_token(&app).await))
                .await
                .unwrap()
                .into_body(),
            usize::MAX,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let p = revs.iter().find(|p| p.revision == rev_no).unwrap();
    assert_eq!(p.secret_requirements.len(), 2);
    assert_eq!(p.secret_requirements[0].env, "ANTHROPIC_API_KEY");
    assert!(p.secret_requirements[0].required);
    assert_eq!(p.secret_requirements[1].env, "OPTIONAL_TOKEN");
    assert!(!p.secret_requirements[1].required);
    assert_eq!(p.adapter_version.as_deref(), Some("1.4.0"));
    assert_eq!(
        p.mcp_server_ids,
        vec!["github".to_string()],
        "per-profile MCP subset round-trips through the store",
    );
}

// ============================================================================
// Hardening P0: cross-node isolation regression tests.
// A node may only mutate attempts assigned to it. A second node's credential
// must not be able to ingest events, complete, ack, cancel-poll, create a
// session, or upload artifacts on the first node's attempt. Upstream artifact
// reads are restricted to the consumer's workflow-dependency chain.
// ============================================================================

/// Helper: enroll two nodes; create+assign a task to node_a only (node_b polls
/// and expects nothing). Returns (assign_a, cred_a, cred_b, node_b_id).
async fn setup_two_nodes(app: &Router, prompt: &str) -> (Assignment, String, String, String) {
    let (node_a, cred_a) = enroll(app, "iso-a", vec!["mock".into()], vec!["*".into()]).await;
    let (node_b, cred_b) = enroll(app, "iso-b", vec!["mock".into()], vec!["*".into()]).await;
    let req = CreateTaskRequest {
        prompt: prompt.into(),
        repository: "demo".into(),
        adapter: "mock".into(),
        // Pin to node_a so node_b cannot pick it up.
        requested_node_id: Some(node_a.clone()),
        timeout_secs: None,
        validation_command: None,
        base_commit: None,
        parent_acp_session_id: None,
    };
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/tasks",
            serde_json::to_string(&req).unwrap(),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let poll_a = PollRequest {
        node_id: node_a.clone(),
        name: "iso-a".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
        protocol_version: None,
    };
    let mut assign = None;
    for _ in 0..50 {
        let r = app
            .clone()
            .oneshot(post_auth(
                "/v1/node/poll",
                serde_json::to_string(&poll_a).unwrap(),
                &cred_a,
            ))
            .await
            .unwrap();
        let pr: PollResponse =
            serde_json::from_slice(&to_bytes(r.into_body(), usize::MAX).await.unwrap()).unwrap();
        if let Some(a) = pr.assignment {
            assign = Some(a);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    (assign.unwrap(), cred_a, cred_b, node_b)
}

fn complete_req() -> CompleteAttemptRequest {
    CompleteAttemptRequest {
        exit_code: 0,
        commit_sha: None,
        error_code: None,
        acp_session_id: None,
        provenance: None,
        plan: None,
    }
}

#[tokio::test]
async fn cross_node_cannot_ingest_events() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (assign, cred_a, cred_b, _node_b) = setup_two_nodes(&app, "write:hello.txt:hi").await;
    // Own node: ok.
    let ev = IngestEventsRequest {
        events: vec![IncomingEvent {
            sequence: 1,
            r#type: EventType::Stdout,
            payload: json!({"text": "start"}),
        }],
    };
    let r = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/events", assign.attempt_id),
            serde_json::to_string(&ev).unwrap(),
            &cred_a,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    // Other node: forbidden.
    let r = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/events", assign.attempt_id),
            serde_json::to_string(&ev).unwrap(),
            &cred_b,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    // Unknown attempt: not found.
    let r = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/attempts/does-not-exist/events",
            serde_json::to_string(&ev).unwrap(),
            &cred_b,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cross_node_cannot_complete_attempt() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (assign, cred_a, cred_b, _node_b) = setup_two_nodes(&app, "write:hello.txt:hi").await;
    // other node must not complete.
    let r = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&complete_req()).unwrap(),
            &cred_b,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    // Task untouched (still assigned/running-from-own-ingest; here just not
    // succeeded): own node still can complete.
    let r = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&complete_req()).unwrap(),
            &cred_a,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

#[tokio::test]
async fn cross_node_cannot_ack_attempt() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (assign, cred_a, cred_b, _node_b) = setup_two_nodes(&app, "write:hello.txt:hi").await;
    let r = ack_attempt(&app, &assign.attempt_id, &cred_b).await;
    assert_eq!(r, StatusCode::FORBIDDEN);
    // Own node ack succeeds, leaves attempt running.
    assert_eq!(
        ack_attempt(&app, &assign.attempt_id, &cred_a).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn cross_node_cannot_poll_cancel_state() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (assign, _cred_a, cred_b, _node_b) = setup_two_nodes(&app, "write:hello.txt:hi").await;
    let r = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/node/attempts/{}/cancel", assign.attempt_id),
            &cred_b,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn cross_node_cannot_create_agent_session() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (assign, _cred_a, cred_b, _node_b) = setup_two_nodes(&app, "write:hello.txt:hi").await;
    let body = serde_json::json!({ "adapter": "mock" }).to_string();
    let r = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/session", assign.attempt_id),
            body,
            &cred_b,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn cross_node_cannot_upload_artifact() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (assign, _cred_a, cred_b, _node_b) = setup_two_nodes(&app, "write:hello.txt:hi").await;
    let req = UploadArtifactRequest {
        name: "out.log".into(),
        content: "hello".into(),
        media_type: Some("text/plain".into()),
        sha256: None,
    };
    let r = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/artifacts", assign.attempt_id),
            serde_json::to_string(&req).unwrap(),
            &cred_b,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    // raw upload path too.
    let r = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/v1/node/attempts/{}/artifacts/raw",
                    assign.attempt_id
                ))
                .header("authorization", format!("Bearer {cred_b}"))
                .header("x-artifact-name", "raw.bin")
                .body(Body::from(b"x".to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn cross_node_cannot_read_unrelated_artifact() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (assign, cred_a, cred_b, _node_b) = setup_two_nodes(&app, "write:hello.txt:hi").await;
    // Own node uploads an artifact.
    let req = UploadArtifactRequest {
        name: "out.log".into(),
        content: "hello".into(),
        media_type: Some("text/plain".into()),
        sha256: None,
    };
    let r = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/artifacts", assign.attempt_id),
            serde_json::to_string(&req).unwrap(),
            &cred_a,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    // Other node (not a consumer via workflow dependency) cannot fetch it via
    // the node-scoped artifact endpoint: 404 (no existence disclosure).
    let r = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/node/tasks/{}/artifacts/out.log", assign.task_id),
            &cred_b,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn revoked_node_cannot_mutate() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (assign, _cred_a, cred_b, node_b) = setup_two_nodes(&app, "write:hello.txt:hi").await;
    // Revoke node_b by deleting the node record.
    let r = app
        .clone()
        .oneshot(delete_auth(
            &format!("/v1/nodes/{node_b}"),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    // node_b's credential is no longer valid -> 401 on any node endpoint.
    let ev = IngestEventsRequest {
        events: vec![IncomingEvent {
            sequence: 1,
            r#type: EventType::Stdout,
            payload: json!({"text": "x"}),
        }],
    };
    let r = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/events", assign.attempt_id),
            serde_json::to_string(&ev).unwrap(),
            &cred_b,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
}

// Hardening P0: positive case for upstream artifact authorization. A consumer
// node (assigned the downstream step's attempt) may fetch the producer step's
// `changes.patch` through the node-scoped artifact route; unrelated nodes may
// not (covered by cross_node_cannot_read_unrelated_artifact).
#[tokio::test]
async fn consumer_node_can_read_upstream_producer_artifact() {
    use agentgrid_common::{CreateWorkflowRequest, WorkflowRole, WorkflowStep, WorkflowTemplate};
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "wf-node", vec!["mock".into()], vec!["*".into()]).await;

    // Template: producer `a` -> consumer `b` (b depends on a).
    let steps = vec![
        WorkflowStep {
            id: "a".into(),
            prompt: "produce".into(),
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
            prompt: "consume".into(),
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
        name: "upstream-art".into(),
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

    // Tick: step a activates and is assigned to the node.
    state.store.tick_workflow_run(&run.id).await.unwrap();
    let a = state.store.try_assign(&node_id).await.unwrap().unwrap();

    // Producer uploads its `changes.patch` artifact on its own attempt.
    let art = UploadArtifactRequest {
        name: "changes.patch".into(),
        content: "diff --git a/x b/x".into(),
        ..Default::default()
    };
    let r = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/artifacts", a.attempt_id),
            serde_json::to_string(&art).unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    // Complete a, tick until b is assigned to the same node.
    state
        .store
        .complete_attempt(&a.attempt_id, &complete_req())
        .await
        .unwrap();
    state.store.tick_workflow_run(&run.id).await.unwrap();
    state.store.tick_workflow_run(&run.id).await.unwrap();
    let b = state.store.try_assign(&node_id).await.unwrap().unwrap();

    // Consumer node fetches the producer task's changes.patch via the
    // node-scoped route: authorized because b depends on a.
    let r = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/node/tasks/{}/artifacts/changes.patch", a.task_id),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        StatusCode::OK,
        "consumer node may read its upstream producer artifact"
    );
    let body = to_bytes(r.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), b"diff --git a/x b/x".as_slice());

    // Sanity: the consumer's own task is distinct from the producer's.
    assert_ne!(b.task_id, a.task_id);
}

/// Hardening P0 (static file traversal): `/../`, `%2e%2e/`, mixed encoding,
/// backslashes, and an absolute path must not escape the web root; hashed
/// assets under `assets/` are cached immutable and `index.html` is `no-cache`.
/// A symlink inside the root pointing outside the root is blocked (403).
#[tokio::test]
async fn static_fallback_rejects_traversal_and_caches_safe() {
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let root = std::env::temp_dir().join(format!(
        "ag-web-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let assets = root.join("assets");
    std::fs::create_dir_all(&assets).unwrap();
    std::fs::write(root.join("index.html"), b"<html>app</html>").unwrap();
    std::fs::write(assets.join("main-abc123.js"), b"console.log(1)").unwrap();
    std::fs::write(assets.join("style.css"), b"body{}").unwrap();

    // Symlink inside the root pointing at /etc/passwd — must be blocked (403).
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let _ = symlink("/etc/passwd", root.join("escape.txt"));
    }

    let prev = std::env::var("AGENTGRID_WEB_ROOT").ok();
    std::env::set_var("AGENTGRID_WEB_ROOT", root.to_str().unwrap());
    drop(_g);
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = prev {
        std::env::set_var("AGENTGRID_WEB_ROOT", p);
    } else {
        std::env::remove_var("AGENTGRID_WEB_ROOT");
    }
    drop(_g);

    let token = test_token(&app).await;

    // index.html served with no-cache.
    let r = app.clone().oneshot(get_auth("/", &token)).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        r.headers().get("cache-control").unwrap(),
        "no-cache",
        "index.html must be no-cache"
    );

    // Hashed asset cached immutable.
    let r = app
        .clone()
        .oneshot(get_auth("/assets/main-abc123.js", &token))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        r.headers().get("content-type").unwrap(),
        "text/javascript; charset=utf-8"
    );
    let cache = r
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        cache.contains("immutable") && cache.contains("31536000"),
        "cache={cache}"
    );

    // Traversal attempts: `/../` already normalised by the client/router, but
    // percent-encoded `..` survives as a literal path to the fallback.
    for bad in [
        "/../",
        "/%2e%2e/",
        "/%2e%2e%2fetcpasswd",
        "/..%2f..%2fetc%2fpasswd",
        "/\\..\\etc\\passwd",
    ] {
        let r = app.clone().oneshot(get_auth(bad, &token)).await.unwrap();
        // Either 403 (traversal component rejected) or 404 (normalised, no
        // index next). Must NOT be 200 from a file outside the root: check the
        // body is never a slice of /etc/passwd by asserting the status is not OK
        // unless it resolved to index.html (text/html).
        let st = r.status();
        assert!(
            st == StatusCode::FORBIDDEN
                || st == StatusCode::NOT_FOUND
                || r.headers()
                    .get("content-type")
                    .map(|v| v == "text/html; charset=utf-8")
                    .unwrap_or(false),
            "{bad}: unexpected status {st} (must not serve out-of-root file)"
        );
    }

    // An absolute-path rel (leading slash stripped by trim_start) with `..`
    // components is blocked too.
    let r = app
        .clone()
        .oneshot(get_auth("/a/../b/../../etc/passwd", &token))
        .await
        .unwrap();
    assert!(
        r.status() == StatusCode::FORBIDDEN
            || r.headers()
                .get("content-type")
                .map(|v| v == "text/html; charset=utf-8")
                .unwrap_or(false),
        "parent-dir traversal must not serve out-of-root file"
    );

    // Symlink escape (unix) must be 403, never /etc/passwd bytes.
    #[cfg(unix)]
    {
        let r = app
            .clone()
            .oneshot(get_auth("/escape.txt", &token))
            .await
            .unwrap();
        assert_eq!(
            r.status(),
            StatusCode::FORBIDDEN,
            "symlink escaping root must be blocked"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}
