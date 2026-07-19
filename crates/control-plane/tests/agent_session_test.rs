//! Stage 3.2: a node opens an agent session for an attempt; completing the
//! attempt closes it. Exercises the full HTTP flow (enroll -> task -> assign
//! via long-poll -> open session -> read back -> complete -> session closed).
use agentgrid_common::{
    Assignment, CompleteAttemptRequest, CreateAgentSessionRequest, CreateTaskRequest,
    EnrollRequest, EnrollResponse, EnrollTokenResponse, NodeStatus, PollRequest, PollResponse,
};
use agentgrid_control_plane::{build_router, AppState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::Value;
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
    };
    let resp = app
        .clone()
        .oneshot(post("/v1/tasks", serde_json::to_string(&req).unwrap()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
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
        assert_eq!(resp.status(), StatusCode::OK);
        let pr: PollResponse =
            serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        if let Some(a) = pr.assignment {
            return a;
        }
    }
    panic!("assign never happened");
}

#[tokio::test]
async fn agent_session_opened_and_closed_on_complete() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state.clone());
    let (node_id, cred) = enroll(&app, "node-sess1", vec!["mock".into()], vec!["*".into()]).await;
    let assign = create_and_assign(&app, &node_id, &cred, "write:hello.txt:hi").await;
    let resp = app
        .clone()
        .oneshot(post_auth(
            &format!("/v1/node/attempts/{}/session", assign.attempt_id),
            serde_json::to_string(&CreateAgentSessionRequest {
                adapter: "mock".into(),
            })
            .unwrap(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let session_id = serde_json::from_slice::<Value>(&bytes).unwrap()["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let s = state
        .store
        .get_agent_session(&session_id)
        .await
        .unwrap()
        .expect("session exists");
    assert_eq!(s.adapter, "mock");
    assert_eq!(s.status, "running");
    // Completing the attempt closes the session.
    state
        .store
        .complete_attempt(
            &assign.attempt_id,
            &CompleteAttemptRequest {
                exit_code: 0,
                commit_sha: None,
                error_code: None,
            },
        )
        .await
        .unwrap();
    let s2 = state
        .store
        .get_agent_session(&session_id)
        .await
        .unwrap()
        .expect("session exists");
    assert_eq!(s2.status, "done");
    assert!(s2.ended_at.is_some());
    let _ = NodeStatus::Online;
}
