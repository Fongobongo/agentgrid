//! Plan 6.12 (#647): N / N-1 wire-contract tests for the node↔CP surface.
//!
//! The control plane must serve nodes one minor version behind (fields the
//! old node never sends arrive as serde defaults) and tolerate fields from
//! one minor ahead (unknown JSON is ignored, never a 400). A different
//! protocol MAJOR is the only incompatibility, and it degrades the node —
//! it never breaks the heartbeat path itself.
//!
//! All bodies below are hand-written JSON (not the current structs) so the
//! test pins the WIRE shape an N-1 / N+1 node actually sends, instead of
//! re-serializing today's types (which would silently grow with the code).

use agentgrid_control_plane::{build_router, AppState};
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
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

async fn body_json(resp: axum::http::Response<Body>) -> Value {
    serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap()
}

async fn test_token(app: &Router) -> String {
    let resp = app
        .clone()
        .oneshot(post(
            "/v1/auth/login",
            json!({"username": "test", "password": "test"}).to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    body_json(resp).await["token"].as_str().unwrap().to_string()
}

/// Mint an enrollment token and enroll a node with a MINIMAL (legacy)
/// body — no protocol_version, no permission_interception. Returns
/// (node_id, credential).
async fn enroll_legacy(app: &Router, name: &str) -> (String, String) {
    let token = test_token(app).await;
    let resp = app
        .clone()
        .oneshot(post_auth("/v1/nodes/enrollment-token", "{}".into(), &token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let tok = body_json(resp).await["token"].as_str().unwrap().to_string();
    let resp = app
        .clone()
        .oneshot(post(
            "/v1/node/enroll",
            json!({
                "token": tok,
                "name": name,
                "adapters": ["mock"],
                "repositories": ["*"],
                "max_concurrency": 2,
                "agent_version": "n-1",
            })
            .to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "legacy enroll must be accepted"
    );
    let v = body_json(resp).await;
    (
        v["node_id"].as_str().unwrap().to_string(),
        v["credential"].as_str().unwrap().to_string(),
    )
}

async fn node_status(app: &Router, token: &str, node_id: &str) -> String {
    let resp = app
        .clone()
        .oneshot(get_auth("/v1/nodes", token))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let items = v.get("items").unwrap().as_array().unwrap();
    items
        .iter()
        .find(|n| n["id"] == node_id)
        .and_then(|n| n["status"].as_str())
        .unwrap_or("missing")
        .to_string()
}

/// N-1 heartbeat: the pre-6.10 shape (no protocol_version, no cpu_count,
/// free_memory_mb, repo_states, systemd_scope_supported, …) is accepted
/// and the node reads online with defaults.
#[tokio::test]
async fn legacy_heartbeat_accepted_and_online() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll_legacy(&app, "node-legacy").await;
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            json!({
                "name": "node-legacy",
                "adapters": ["mock"],
                "repositories": ["*"],
                "max_concurrency": 2,
                "agent_version": "n-1",
                "load_avg": 0.1,
                "free_disk_mb": 4096,
                "mem_available_mb": 4096,
                "active_attempts": 0,
            })
            .to_string(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "N-1 heartbeat must be accepted"
    );
    let token = test_token(&app).await;
    assert_eq!(node_status(&app, &token, &node_id).await, "online");
}

/// N+1 heartbeat: unknown future fields are ignored, never a 400; the
/// node reads online and its known fields still land.
#[tokio::test]
async fn future_heartbeat_ignores_unknown_fields() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll_legacy(&app, "node-future").await;
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            json!({
                "name": "node-future",
                "adapters": ["mock"],
                "repositories": ["*"],
                "max_concurrency": 2,
                "agent_version": "n+1",
                "load_avg": 0.1,
                "free_disk_mb": 4096,
                "active_attempts": 0,
                // Derived, not hardcoded: this test is about a *compatible*
                // minor, so it must follow the current major (ADR 0028 bumped
                // it to 2, which turned a literal "1.5" into a major mismatch).
                "protocol_version": format!("{}.5", agentgrid_common::NODE_PROTOCOL_VERSION),
                "future_field_xyz": {"nested": [1, 2, 3]},
                "another_future": "yes",
            })
            .to_string(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "unknown fields must be ignored, not rejected"
    );
    let token = test_token(&app).await;
    assert_eq!(node_status(&app, &token, &node_id).await, "online");
}

/// A different protocol MAJOR degrades the node (fail-visible, schedulable
/// nowhere) but the heartbeat itself stays 200 — the node keeps reporting
/// instead of being cut off.
#[tokio::test]
async fn incompatible_protocol_major_degrades_node() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll_legacy(&app, "node-incompat").await;
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            json!({
                "name": "node-incompat",
                "adapters": ["mock"],
                "repositories": ["*"],
                "max_concurrency": 2,
                "agent_version": "far-future",
                "load_avg": 0.1,
                "free_disk_mb": 4096,
                "active_attempts": 0,
                "protocol_version": "999.0",
            })
            .to_string(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "heartbeat stays 200");
    let token = test_token(&app).await;
    assert_eq!(
        node_status(&app, &token, &node_id).await,
        "degraded",
        "major mismatch must degrade, not drop"
    );
}

/// Same major, newer minor stays fully online (rolling upgrade signal).
#[tokio::test]
async fn compatible_protocol_minor_stays_online() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll_legacy(&app, "node-minor").await;
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/heartbeat",
            json!({
                "name": "node-minor",
                "adapters": ["mock"],
                "repositories": ["*"],
                "max_concurrency": 2,
                "agent_version": "new-minor",
                "load_avg": 0.1,
                "free_disk_mb": 4096,
                "active_attempts": 0,
                "protocol_version": format!("{}.9", agentgrid_common::NODE_PROTOCOL_VERSION),
            })
            .to_string(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let token = test_token(&app).await;
    assert_eq!(node_status(&app, &token, &node_id).await, "online");
}

/// ADR 0028: a protocol-1 node must never be handed `sealed` blobs — it would
/// deserialize them as `value` and inject `v1.<base64>` as the API key. The
/// gate is the protocol version, not a missing key, so this enrolls a node
/// that *could* be served the entry and checks it is not.
#[tokio::test]
async fn a_protocol_1_node_is_never_sent_sealed_adapter_env() {
    // An empty queue makes the handler long-poll for 25 s by default; three
    // polls would add 75 s to every CI run for no extra coverage.
    let prev = std::env::var("AGENTGRID_POLL_TIMEOUT_SECS").ok();
    std::env::set_var("AGENTGRID_POLL_TIMEOUT_SECS", "1");

    let state = AppState::open_temp().await.unwrap();
    let store = state.store.clone();
    let app = build_router(state);

    let (public, _private) = agentgrid_common::seal::generate_keypair().unwrap();
    let token = test_token(&app).await;
    let resp = app
        .clone()
        .oneshot(post_auth("/v1/nodes/enrollment-token", "{}".into(), &token))
        .await
        .unwrap();
    let tok = body_json(resp).await["token"].as_str().unwrap().to_string();
    let resp = app
        .clone()
        .oneshot(post(
            "/v1/node/enroll",
            json!({
                "token": tok,
                "name": "node-sealed",
                "adapters": ["mock"],
                "repositories": ["*"],
                "max_concurrency": 2,
                "agent_version": "sealing",
                "public_key": public,
            })
            .to_string(),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let node_id = v["node_id"].as_str().unwrap().to_string();
    let cred = v["credential"].as_str().unwrap().to_string();

    let out = store
        .upsert_adapter_env("*", "ANTHROPIC_API_KEY", "sk-ant-x", None)
        .await
        .unwrap();
    assert_eq!(
        out.sealed_for,
        vec![node_id.clone()],
        "deliverable to this node"
    );

    let poll_body = |proto: Option<&str>| {
        let mut body = json!({
            "node_id": node_id,
            "name": "node-sealed",
            "adapters": ["mock"],
            "repositories": ["*"],
            "max_concurrency": 2,
        });
        if let Some(p) = proto {
            body["protocol_version"] = json!(p);
        }
        body.to_string()
    };

    // Protocol 1 (and the legacy no-field form): nothing on the wire.
    for proto in [Some("1"), None] {
        let resp = app
            .clone()
            .oneshot(post_auth("/v1/node/poll", poll_body(proto), &cred))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_json(resp).await;
        assert_eq!(
            body["adapter_env"].as_array().map(|a| a.len()),
            Some(0),
            "a protocol-1 node must receive no adapter env, got {body}"
        );
        assert_eq!(
            body["adapter_env_complete"],
            json!(false),
            "a protocol-1 node must not be told the (empty) list is complete"
        );
    }

    // Protocol 2: the sealed entry, and no plaintext anywhere in the body.
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/poll",
            poll_body(Some(agentgrid_common::NODE_PROTOCOL_VERSION)),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    let env = body["adapter_env"].as_array().unwrap();
    assert_eq!(env.len(), 1, "protocol 2 gets the entry: {body}");
    assert!(
        env[0].get("sealed").is_some(),
        "sealed blob expected: {body}"
    );
    assert!(
        env[0].get("value").is_none(),
        "the plaintext must not be on the wire: {body}"
    );
    assert!(!body.to_string().contains("sk-ant-x"));
    assert_eq!(body["adapter_env_complete"], json!(true));

    match prev {
        Some(v) => std::env::set_var("AGENTGRID_POLL_TIMEOUT_SECS", v),
        None => std::env::remove_var("AGENTGRID_POLL_TIMEOUT_SECS"),
    }
}

/// N-1 poll: minimal body, no assignments queued → 200 with an empty batch.
#[tokio::test]
async fn legacy_poll_accepted() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let (node_id, cred) = enroll_legacy(&app, "node-poll").await;
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node/poll",
            json!({
                "node_id": node_id,
                "name": "node-poll",
                "adapters": ["mock"],
                "repositories": ["*"],
                "max_concurrency": 2,
            })
            .to_string(),
            &cred,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "N-1 poll must be accepted");
    let v = body_json(resp).await;
    assert!(
        v.get("assignment").is_none() || v["assignment"].is_null(),
        "no tasks queued → no assignment, got {v}"
    );
}
