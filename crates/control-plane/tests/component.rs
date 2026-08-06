//! Component test: boot the real HTTP server on a TCP socket and exercise it
//! over the wire (as opposed to `api.rs` which drives the router in-process
//! via `oneshot`). Catches listener/TLS/network-stack issues that in-process
//! tests cannot.

use agentgrid_control_plane::{build_router, AppState};
use reqwest::Client;
use tokio::net::TcpListener;

/// Start the real axum server on an ephemeral port. Returns the base URL.
async fn spawn_server() -> String {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn health_endpoints_respond_over_real_http() {
    let base = spawn_server().await;
    let client = Client::new();

    let live = client
        .get(format!("{base}/health/live"))
        .send()
        .await
        .unwrap();
    assert_eq!(live.status(), 200);

    let ready = client
        .get(format!("{base}/health/ready"))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), 200);

    let metrics = client.get(format!("{base}/metrics")).send().await.unwrap();
    assert_eq!(metrics.status(), 200);
    let body = metrics.text().await.unwrap();
    assert!(
        body.contains("agentgrid_"),
        "metrics body must have agentgrid metrics"
    );
}

#[tokio::test]
async fn full_auth_and_task_flow_over_real_http() {
    let base = spawn_server().await;
    let client = Client::new();

    // Setup + login (test/test user is bootstrapped by open_temp).
    let login: serde_json::Value = client
        .post(format!("{base}/v1/auth/login"))
        .json(&serde_json::json!({"username": "test", "password": "test"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let token = login["token"].as_str().expect("login returns a JWT");

    // Create a task over the wire.
    let task: serde_json::Value = client
        .post(format!("{base}/v1/tasks"))
        .bearer_auth(token)
        .json(&serde_json::json!({
            "repository": "https://example.com/repo.git",
            "prompt": "component test task",
            "adapter": "mock"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let task_id = task["id"].as_str().expect("task has an id");
    assert_eq!(task["status"], "queued");

    // Unauthenticated access must be rejected over the wire too.
    let unauth = client
        .get(format!("{base}/v1/tasks/{task_id}"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), 401, "no token -> 401 over real HTTP");

    // Authenticated show works.
    let show = client
        .get(format!("{base}/v1/tasks/{task_id}"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(show.status(), 200);
    let show: serde_json::Value = show.json().await.unwrap();
    assert_eq!(show["status"], "queued");
}
