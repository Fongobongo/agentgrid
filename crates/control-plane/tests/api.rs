//! End-to-end API test: create task -> node enroll + poll assignment -> ingest
//! events (with idempotency) -> complete -> terminal task status. Exercises the
//! full slice without network I/O. Node endpoints require credential auth
//! (Stage 2.3), so tests enroll first.

use agentgrid_common::{
    Assignment, CancelState, CompleteAttemptRequest, CreateRepositoryRequest, CreateTaskRequest,
    EnrollRequest, EnrollResponse, EnrollTokenResponse, EventType, HeartbeatRequest, IncomingEvent,
    IngestEventsRequest, LoginResponse, NodeStatus, PollRequest, PollResponse, RepositoryView,
    TaskEligibility, TaskStatus, TaskView, UploadArtifactRequest,
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
            serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap())
                .unwrap();
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
    };
    let app2 = app.clone();
    let cred2 = cred.to_string();
    let h = tokio::spawn(async move {
        app2.oneshot(post_auth(
            "/v1/node/poll",
            serde_json::to_string(&poll_req).unwrap(),
            &cred2,
        ))
        .await
        .unwrap()
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let req = CreateTaskRequest {
        prompt: prompt.into(),
        repository: "demo".into(),
        adapter: "mock".into(),
        requested_node_id: None,
        timeout_secs: None,
    };
    let resp = app
        .clone()
        .oneshot(post("/v1/tasks", serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = h.await.unwrap();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let pr: PollResponse = serde_json::from_slice(&body).unwrap();
    pr.assignment.expect("assignment")
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
async fn cancel_queued_marks_cancelled() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let req = CreateTaskRequest {
        prompt: "x".into(),
        repository: "demo".into(),
        adapter: "mock".into(),
        requested_node_id: None,
        timeout_secs: None,
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
}

#[tokio::test]
async fn user_auth_setup_login_and_protects_endpoints() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);

    // Open bootstrap window: no users yet, so task creation works without a token.
    let id0 = create_task_only(&app, "demo", "mock", None).await;
    assert!(!id0.is_empty());

    // Setup the first user, then a second setup is rejected.
    assert_eq!(auth_setup(&app, "alice", "secret").await, StatusCode::CREATED);
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
    assert_eq!(elig.no_eligible_nodes, vec!["requested node ghost not registered"]);

    // Request an actual eligible node: eligible, no reasons.
    let (good, _c) = enroll(&app, "good", vec!["mock".into()], vec!["*".into()]).await;
    let id2 = create_task_only(&app, "demo", "mock", Some(good.clone())).await;
    let elig2 = task_eligibility(&app, &id2).await;
    assert_eq!(elig2.nodes.len(), 1);
    assert!(elig2.nodes[0].eligible);
    assert!(elig2.no_eligible_nodes.is_empty());
}
