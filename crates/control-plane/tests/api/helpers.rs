#![allow(clippy::needless_borrow)]
//! End-to-end API test: create task -> node enroll + poll assignment -> ingest
//! events (with idempotency) -> complete -> terminal task status. Exercises the
//! full slice without network I/O. Node endpoints require credential auth
//! (Stage 2.3), so tests enroll first.

pub use agentgrid_common::{
    ApprovalStatus, ApprovalView, Assignment, CancelState, CompleteAttemptRequest,
    CreateRepositoryRequest, CreateTaskRequest, CreateWorkflowRequest, CreateWorkflowRunRequest,
    EnrollRequest, EnrollResponse, EnrollTokenResponse, EventType, HeartbeatRequest, IncomingEvent,
    IngestEventsRequest, ListResponse, LoginResponse, NodeStatus, NodeView, PollRequest,
    PollResponse, RepositoryView, SkillTrustView, TaskEligibility, TaskStatus, TaskView,
    UploadArtifactRequest, WorkflowRole, WorkflowRun, WorkflowRunStatus, WorkflowRunWithSteps,
    WorkflowStep, WorkflowStepStatus, WorkflowTemplate,
};
pub use agentgrid_control_plane::{build_router, AppState};
pub use axum::body::{to_bytes, Body};
pub use axum::http::{Request, StatusCode};
pub use axum::Router;
pub use serde_json::json;
pub use tower::ServiceExt;

pub(crate) fn post(uri: &str, body: String) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

pub(crate) fn post_auth(uri: &str, body: String, cred: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {cred}"))
        .body(Body::from(body))
        .unwrap()
}

pub(crate) fn put_auth(uri: &str, body: String, cred: &str) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {cred}"))
        .body(Body::from(body))
        .unwrap()
}

/// Hardening P0 item 8: a node mutates its own attempt with its fenced token
/// (the same one returned in its assignment). Use for /events, /complete,
/// /ack, /session and /artifacts so the CP fence check passes.
pub(crate) fn post_node(uri: &str, body: String, cred: &str, fence: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {cred}"))
        .header("x-agentgrid-fencing-token", fence)
        .body(Body::from(body))
        .unwrap()
}

pub(crate) fn get_auth(uri: &str, cred: &str) -> Request<Body> {
    Request::builder()
        .method("GET")
        .uri(uri)
        .header("authorization", format!("Bearer {cred}"))
        .body(Body::empty())
        .unwrap()
}

pub(crate) fn delete_auth(uri: &str, cred: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .header("authorization", format!("Bearer {cred}"))
        .body(Body::empty())
        .unwrap()
}

pub(crate) fn post_json(uri: &str, body: String, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("POST").uri(uri);
    if let Some(t) = token {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    b.header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

pub(crate) async fn auth_setup(
    app: &Router,
    state: &AppState,
    user: &str,
    pass: &str,
) -> StatusCode {
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

pub(crate) async fn auth_login(app: &Router, user: &str, pass: &str) -> Option<String> {
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
pub(crate) async fn test_token(app: &Router) -> String {
    auth_login(app, "test", "test")
        .await
        .expect("test user login")
}

/// Create an enrollment token, enroll a node, return (node_id, credential).
/// Fetch a profile's id via the operator route (used by a few opencode tests).
pub(crate) async fn fetch_profile_id(app: &Router, name: &str) -> String {
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/opencode-profiles/{name}"),
            &test_token(app).await,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let p: agentgrid_common::OpencodeProfile =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    p.id
}

pub(crate) async fn enroll(
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
        permission_interception: "wrapper".into(),
        public_key: None,
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

/// Create a queued task and return its id, without assigning it. Tests that
/// care which node picks the work up (retry placement) drive the polls
/// themselves; `create_and_assign` is the shortcut for tests that do not.
pub(crate) async fn create_task_with(
    app: &Router,
    prompt: &str,
    max_attempts: u32,
    retry_on_node_loss: Option<bool>,
) -> String {
    let req = CreateTaskRequest {
        prompt: prompt.into(),
        repository: "demo".into(),
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
        github_create: false,
        max_attempts,
        consensus_mode: None,
        review_of: None,
        max_tokens: None,
        max_cost_cents: None,
        retry_on_node_loss,
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
    let tv: TaskView =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    tv.id
}

/// One poll from a node's point of view. `None` = the scheduler had nothing
/// this node was allowed to take.
pub(crate) async fn poll_assign(app: &Router, node_id: &str, cred: &str) -> Option<Assignment> {
    let poll_req = PollRequest {
        node_id: node_id.into(),
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
            cred,
        ))
        .await
        .unwrap();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let pr: PollResponse = serde_json::from_slice(&body).unwrap();
    pr.assignment
}

/// Register a node via long-poll, create a task, and return its assignment.
pub(crate) async fn create_and_assign(
    app: &Router,
    node_id: &str,
    cred: &str,
    prompt: &str,
) -> Assignment {
    let _task_id = create_task_with(app, prompt, 1, None).await;
    // Long-poll until the queued task is assigned, mirroring the node daemon.
    for _ in 0..50 {
        if let Some(a) = poll_assign(app, node_id, cred).await {
            return a;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("task was never assigned to the node");
}

pub(crate) async fn show_status(app: &Router, task_id: &str) -> TaskStatus {
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
pub(crate) async fn show_task_view(app: &Router, task_id: &str) -> TaskView {
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

pub(crate) async fn task_eligibility(app: &Router, task_id: &str) -> TaskEligibility {
    let resp = app
        .clone()
        .oneshot(get_auth(
            &format!("/v1/tasks/{task_id}/eligibility"),
            &test_token(&app).await,
        ))
        .await
        .unwrap();
    let status = resp.status();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    if status != StatusCode::OK {
        eprintln!(
            "ERROR: task_eligibility status={} body={}",
            status,
            String::from_utf8_lossy(&body)
        );
    }
    assert_eq!(status, StatusCode::OK);
    serde_json::from_slice(&body).unwrap()
}

pub(crate) async fn cancel_state(app: &Router, attempt_id: &str, cred: &str) -> CancelState {
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

pub(crate) fn post_q_auth(uri: &str, cred: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("authorization", format!("Bearer {cred}"))
        .body(Body::empty())
        .unwrap()
}
pub(crate) async fn list_approvals(app: &Router, status: Option<&str>) -> Vec<ApprovalView> {
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
    let list: ListResponse<ApprovalView> =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    list.items
}
pub(crate) async fn create_task_only(
    app: &Router,
    repo: &str,
    adapter: &str,
    node: Option<String>,
) -> String {
    let req = CreateTaskRequest {
        prompt: "x".into(),
        repository: repo.into(),
        adapter: adapter.into(),
        requested_node_id: node,
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
        github_create: false,
        max_attempts: 1,
        retry_on_node_loss: None,
        consensus_mode: None,
        review_of: None,
        max_tokens: None,
        max_cost_cents: None,
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
pub(crate) fn complete_fail_req() -> CompleteAttemptRequest {
    let mut r = complete_req();
    r.exit_code = 1;
    r
}
pub(crate) fn complete_req() -> CompleteAttemptRequest {
    CompleteAttemptRequest {
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
    }
}
pub(crate) async fn setup_two_nodes(
    app: &Router,
    prompt: &str,
) -> (Assignment, String, String, String) {
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
        github_create: false,
        max_attempts: 1,
        retry_on_node_loss: None,
        consensus_mode: None,
        review_of: None,
        max_tokens: None,
        max_cost_cents: None,
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
pub(crate) async fn ack_attempt(
    app: &Router,
    attempt_id: &str,
    cred: &str,
    fence: &str,
) -> StatusCode {
    app.clone()
        .oneshot(post_node(
            &format!("/v1/node/attempts/{attempt_id}/ack"),
            "{}".into(),
            cred,
            fence,
        ))
        .await
        .unwrap()
        .status()
}
pub(crate) async fn create_repo_raw(
    app: &Router,
    req: CreateRepositoryRequest,
    cred: &str,
) -> StatusCode {
    app.clone()
        .oneshot(post_auth(
            "/v1/repositories",
            serde_json::to_string(&req).unwrap(),
            cred,
        ))
        .await
        .unwrap()
        .status()
}
pub(crate) async fn login_status(app: &Router, user: &str, pass: &str) -> StatusCode {
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
pub(crate) async fn create_task(
    app: &Router,
    adapter: &str,
    requested_node: Option<&str>,
) -> String {
    let req = CreateTaskRequest {
        prompt: "do thing".into(),
        repository: "demo".into(),
        adapter: adapter.into(),
        requested_node_id: requested_node.map(|s| s.into()),
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
        github_create: false,
        max_attempts: 1,
        retry_on_node_loss: None,
        consensus_mode: None,
        review_of: None,
        max_tokens: None,
        max_cost_cents: None,
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
