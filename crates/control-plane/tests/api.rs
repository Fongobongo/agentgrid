//! End-to-end API test: create task -> node poll assignment -> ingest events
//! (with idempotency) -> complete -> terminal task status. Exercises the full
//! Stage-1 vertical slice without network I/O.

use agentgrid_common::{
    Assignment, CancelState, CompleteAttemptRequest, CreateTaskRequest, EventType, IncomingEvent,
    IngestEventsRequest, PollRequest, PollResponse, TaskStatus, TaskView,
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

#[tokio::test]
async fn full_task_lifecycle() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);

    // 1. Node polls before any task exists -> no assignment, but registers.
    let poll_req = PollRequest {
        node_id: "node-1".into(),
        name: "daemon-1".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
    };
    let poll_app = app.clone();
    let poll_handle = tokio::spawn(async move {
        poll_app
            .oneshot(post(
                "/v1/node/poll",
                serde_json::to_string(&poll_req).unwrap(),
            ))
            .await
            .unwrap()
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // 2. Create a queued task.
    let create_req = CreateTaskRequest {
        prompt: "write:hello.txt:hi".into(),
        repository: "demo".into(),
        adapter: "mock".into(),
        requested_node_id: None,
        timeout_secs: None,
    };
    let resp = app
        .clone()
        .oneshot(post(
            "/v1/tasks",
            serde_json::to_string(&create_req).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // 3. Poll returns the assignment.
    let resp = poll_handle.await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let pr: PollResponse = serde_json::from_slice(&body).unwrap();
    let assign = pr.assignment.expect("expected an assignment");
    assert_eq!(assign.adapter, "mock");
    assert_eq!(assign.repository, "demo");

    // 4. Ingest an event, then re-ingest the same sequence (idempotent).
    let ingest = |seq| IngestEventsRequest {
        events: vec![IncomingEvent {
            sequence: seq,
            r#type: EventType::Stdout,
            payload: json!({ "text": "hi" }),
        }],
    };
    let uri = format!("/v1/node/attempts/{}/events", assign.attempt_id);
    let resp = app
        .clone()
        .oneshot(post(&uri, serde_json::to_string(&ingest(1)).unwrap()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // duplicate
    let resp = app
        .clone()
        .oneshot(post(&uri, serde_json::to_string(&ingest(1)).unwrap()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 5. Event retrieval shows exactly one event.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/v1/tasks/{}/events?after_sequence=0",
                    assign.task_id
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let events: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(events.len(), 1, "duplicate sequence must be dropped");

    // 6. Complete with success.
    let resp = app
        .clone()
        .oneshot(post(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest { exit_code: 0 }).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 7. Task is terminal/succeeded.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/tasks/{}", assign.task_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let tv: TaskView = serde_json::from_slice(&body).unwrap();
    assert_eq!(tv.status, TaskStatus::Succeeded);
}

#[tokio::test]
async fn failure_marks_task_failed() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);

    // Directly create + assign via poll with a pre-registered node.
    let poll_req = PollRequest {
        node_id: "node-2".into(),
        name: "d2".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 1,
    };
    let app2 = app.clone();
    let h = tokio::spawn(async move {
        app2.oneshot(post(
            "/v1/node/poll",
            serde_json::to_string(&poll_req).unwrap(),
        ))
        .await
        .unwrap()
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let req = CreateTaskRequest {
        prompt: "fail:3".into(),
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
    let assign = pr.assignment.expect("assignment");

    let resp = app
        .clone()
        .oneshot(post(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest { exit_code: 3 }).unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/tasks/{}", assign.task_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let tv: TaskView =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(tv.status, TaskStatus::Failed);
}

/// Register a node via long-poll, create a task, and return its assignment.
async fn create_and_assign(app: &Router, node_id: &str, prompt: &str) -> Assignment {
    let poll_req = PollRequest {
        node_id: node_id.into(),
        name: "n".into(),
        adapters: vec!["mock".into()],
        repositories: vec!["*".into()],
        max_concurrency: 2,
    };
    let app2 = app.clone();
    let h = tokio::spawn(async move {
        app2.oneshot(post(
            "/v1/node/poll",
            serde_json::to_string(&poll_req).unwrap(),
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
    let assign = create_and_assign(&app, "node-c", "sleep:30").await;

    let cs: CancelState = cancel_state(&app, &assign.attempt_id).await;
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

    let cs: CancelState = cancel_state(&app, &assign.attempt_id).await;
    assert!(cs.cancel_requested);

    let resp = app
        .clone()
        .oneshot(post(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest { exit_code: 1 }).unwrap(),
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
    let assign = create_and_assign(&app, "node-r", "fail:3").await;
    let resp = app
        .clone()
        .oneshot(post(
            &format!("/v1/node/attempts/{}/complete", assign.attempt_id),
            serde_json::to_string(&CompleteAttemptRequest { exit_code: 3 }).unwrap(),
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

async fn cancel_state(app: &Router, attempt_id: &str) -> CancelState {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/v1/node/attempts/{attempt_id}/cancel"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap()
}
