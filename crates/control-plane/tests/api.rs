//! End-to-end API test: create task -> node enroll + poll assignment -> ingest
//! events (with idempotency) -> complete -> terminal task status. Exercises the
//! full slice without network I/O. Node endpoints require credential auth
//! (Stage 2.3), so tests enroll first.

use agentgrid_common::{
    ApprovalStatus, ApprovalView, Assignment, CancelState, CompleteAttemptRequest,
    CreateRepositoryRequest, CreateTaskRequest, CreateWorkflowRequest, CreateWorkflowRunRequest,
    EnrollRequest, EnrollResponse, EnrollTokenResponse, EventType, HeartbeatRequest, IncomingEvent,
    IngestEventsRequest, LoginResponse, NodeStatus, NodeView, PollRequest, PollResponse,
    RepositoryView, TaskEligibility, TaskStatus, TaskView, UploadArtifactRequest, WorkflowRole,
    WorkflowRun, WorkflowRunStatus, WorkflowRunWithSteps, WorkflowStep, WorkflowStepStatus,
    WorkflowTemplate,
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

fn delete(uri: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
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

async fn auth_setup(app: &Router, user: &str, pass: &str) -> StatusCode {
    let resp = app
        .clone()
        .oneshot(post_json(
            "/v1/auth/setup",
            serde_json::to_string(&serde_json::json!({ "username": user, "password": pass }))
                .unwrap(),
            None,
        ))
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

/// Create an enrollment token, enroll a node, return (node_id, credential).
async fn enroll(
    app: &Router,
    name: &str,
    adapters: Vec<String>,
    repos: Vec<String>,
) -> (String, String) {
    let tk_resp = app
        .clone()
        .oneshot(post("/v1/nodes/enrollment-token", "{}".into()))
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
   protocol_version: None, };
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
   protocol_version: None, };
    let req = CreateTaskRequest {
        prompt: prompt.into(),
        repository: "demo".into(),
        adapter: "mock".into(),
        requested_node_id: None,
        timeout_secs: None,
        validation_command: None,
        base_commit: None,
    };
    let resp = app
        .clone()
        .oneshot(post("/v1/tasks", serde_json::to_string(&req).unwrap()))
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
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/tasks/{task_id}"))
                .body(Body::empty())
                .unwrap(),
        )
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
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/tasks/{task_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap()
}

async fn task_eligibility(app: &Router, task_id: &str) -> TaskEligibility {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/tasks/{task_id}/eligibility"))
                .body(Body::empty())
                .unwrap(),
        )
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
    };
    let resp = app
        .clone()
        .oneshot(post("/v1/tasks", serde_json::to_string(&req).unwrap()))
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
   protocol_version: None, };
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
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri(format!("/v1/tasks/{task_id}"))
                        .body(Body::empty())
                        .unwrap(),
                )
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
    };
    let resp = app
        .clone()
        .oneshot(post("/v1/tasks", serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let tv: TaskView = serde_json::from_slice(&body).unwrap();

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/tasks/{}/cancel", tv.id))
                .body(Body::empty())
                .unwrap(),
        )
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
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/tasks/{}/cancel", assign.task_id))
                .body(Body::empty())
                .unwrap(),
        )
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
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/tasks/{}/retry", assign.task_id))
                .body(Body::empty())
                .unwrap(),
        )
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
   protocol_version: None, };
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
        .oneshot(delete(&format!("/v1/nodes/{node_id}")))
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
   protocol_version: None, };
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
        .oneshot(post(
            "/v1/repositories",
            serde_json::to_string(&req).unwrap(),
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
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/repositories")
                .body(Body::empty())
                .unwrap(),
        )
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
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/v1/tasks/{}/artifacts/changes.patch",
                    assign.task_id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    assert_eq!(body.as_ref(), b"diff --git a/x b/x".as_slice());
}

#[tokio::test]
async fn metrics_endpoint_exposes_counts() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
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
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);

    // Open bootstrap window: no users yet, so task creation works without a token.
    let id0 = create_task_only(&app, "demo", "mock", None).await;
    assert!(!id0.is_empty());

    // Setup the first user, then a second setup is rejected.
    assert_eq!(
        auth_setup(&app, "alice", "secret").await,
        StatusCode::CREATED
    );
    assert_eq!(
        auth_setup(&app, "bob", "secret").await,
        StatusCode::CONFLICT
    );

    // Now the bootstrap window is closed: task creation requires a token.
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
    };
    let resp = app
        .clone()
        .oneshot(post("/v1/tasks", serde_json::to_string(&req).unwrap()))
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
    };
    let resp = app
        .clone()
        .oneshot(post("/v1/tasks", serde_json::to_string(&req).unwrap()))
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
    };
    let resp = app
        .clone()
        .oneshot(post("/v1/tasks", serde_json::to_string(&req).unwrap()))
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
               protocol_version: None, })
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
           protocol_version: None, })
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
               protocol_version: None, })
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
   protocol_version: None, };
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
                .oneshot(get_auth("/v1/nodes", &cred))
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
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/v1/tasks/{}/retry", assign.task_id))
                .body(Body::empty())
                .unwrap(),
        )
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
               protocol_version: None, })
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

    // Allow it.
    let allow = app
        .clone()
        .oneshot(post_json(
            &format!("/v1/approvals/{ap_id}/allow"),
            "{}".into(),
            None,
        ))
        .await
        .unwrap();
    assert_eq!(allow.status(), StatusCode::OK);

    let allowed = list_approvals(&app, Some("allowed")).await;
    assert!(allowed
        .iter()
        .any(|a| a.id == ap_id && a.status == ApprovalStatus::Allowed));
    assert!(list_approvals(&app, Some("pending")).await.is_empty());

    // Answering a terminal approval is a safe no-op (idempotent).
    let again = app
        .clone()
        .oneshot(post_json(
            &format!("/v1/approvals/{ap_id}/deny"),
            "{}".into(),
            None,
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
    let resp = app.clone().oneshot(get_q(&uri)).await.unwrap();
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
            None,
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
        .oneshot(get_q(&format!("/v1/approvals/{id}")))
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
            None,
        ))
        .await
        .unwrap();
    assert_eq!(allow.status(), StatusCode::OK);
    let allowed = app
        .clone()
        .oneshot(get_q(&format!("/v1/approvals/{id}")))
        .await
        .unwrap();
    let view: ApprovalView =
        serde_json::from_slice(&to_bytes(allowed.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(view.status, ApprovalStatus::Allowed);

    // Unknown id 404s.
    let missing = app
        .clone()
        .oneshot(get_q("/v1/approvals/does-not-exist"))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
}

fn get_q(uri: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
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
        .oneshot(post("/v1/workflows", body))
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
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/v1/workflows")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let list: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);

    // show
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/workflows/{tid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // run
    let resp = app
        .clone()
        .oneshot(post(&format!("/v1/workflows/{tid}/runs"), "{}".into()))
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
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/workflow-runs/{rid}"))
                .body(Body::empty())
                .unwrap(),
        )
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
        .oneshot(post("/v1/workflows", body))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
        .oneshot(post("/v1/workflows", tpl_body))
        .await
        .unwrap();
    let tpl: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let tid = tpl.get("id").unwrap().as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(post(
            &format!("/v1/workflows/{tid}/runs"),
            json!({"repository":"demo"}).to_string(),
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
   protocol_version: None, };

    for _ in 0..200 {
        // Scheduler tick: activates ready steps + advances completed ones.
        let resp = app
            .clone()
            .oneshot(post(&format!("/v1/workflow-runs/{rid}/tick"), "{}".into()))
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
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/workflow-runs/{rid}"))
                .body(Body::empty())
                .unwrap(),
        )
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
        .oneshot(post("/v1/workflows", tpl_body))
        .await
        .unwrap();
    let tpl: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    let tid = tpl.get("id").unwrap().as_str().unwrap().to_string();

    let resp = app
        .clone()
        .oneshot(post(
            &format!("/v1/workflows/{tid}/runs"),
            json!({"repository":"demo"}).to_string(),
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
   protocol_version: None, };

    app.clone()
        .oneshot(post(&format!("/v1/workflow-runs/{rid}/tick"), "{}".into()))
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
            })
            .unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    for _ in 0..4 {
        app.clone()
            .oneshot(post(&format!("/v1/workflow-runs/{rid}/tick"), "{}".into()))
            .await
            .unwrap();
    }

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/workflow-runs/{rid}/projection"))
                .body(Body::empty())
                .unwrap(),
        )
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
            .oneshot(post("/v1/policy/evaluate", body))
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
            .oneshot(post("/v1/policy/evaluate", body))
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
        .oneshot(post("/v1/policy/evaluate", body))
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

#[tokio::test]
async fn artifact_name_validation_rejects_traversal() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (_, cred) = enroll(&app, "node-a", vec![], vec![]).await;
    let uri = "/v1/node/attempts/att-1/artifacts";
    let bad = UploadArtifactRequest {
        name: "../../etc/passwd".into(),
        content: "x".into(),
    };
    let resp = app
        .clone()
        .oneshot(post_auth(uri, serde_json::to_string(&bad).unwrap(), &cred))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let ok = UploadArtifactRequest {
        name: "out.txt".into(),
        content: "x".into(),
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
            None,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(path.exists(), "backup file must be created");
    let _ = std::fs::remove_file(&path);
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
        }],
        context: None,
    };
    let r = app
        .clone()
        .oneshot(post("/v1/workflows", serde_json::to_string(&tmpl).unwrap()))
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
        .oneshot(post(
            &format!("/v1/workflows/{}/runs", t.id),
            serde_json::to_string(&run_req).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(rr.status(), StatusCode::CREATED);
    let run: WorkflowRun =
        serde_json::from_slice(&to_bytes(rr.into_body(), usize::MAX).await.unwrap()).unwrap();
    let c = app
        .clone()
        .oneshot(post(
            &format!("/v1/workflow-runs/{}/cancel", run.id),
            "{}".into(),
        ))
        .await
        .unwrap();
    assert_eq!(c.status(), StatusCode::OK);
    let show = app
        .clone()
        .oneshot(get_auth(&format!("/v1/workflow-runs/{}", run.id), ""))
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
        .oneshot(post("/v1/nodes/enrollment-token", "{}".into()))
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
        .oneshot(post("/v1/node/enroll", serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();
    assert_eq!(er.status(), StatusCode::OK);
    let er: EnrollResponse =
        serde_json::from_slice(&to_bytes(er.into_body(), usize::MAX).await.unwrap()).unwrap();
    let nodes = app.clone().oneshot(get_auth("/v1/nodes", "")).await.unwrap();
    assert_eq!(nodes.status(), StatusCode::OK);
    let nodes: Vec<NodeView> =
        serde_json::from_slice(&to_bytes(nodes.into_body(), usize::MAX).await.unwrap()).unwrap();
    let node = nodes.iter().find(|n| n.id == er.node_id).expect("node present");
    assert_eq!(node.status, NodeStatus::Degraded);
}
