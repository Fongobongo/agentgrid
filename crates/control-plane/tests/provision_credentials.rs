//! `POST /v1/node-installs` credential validation, with the gate **open**.
//!
//! Its own test binary on purpose: opening the gate means setting
//! `AGENTGRID_PROVISION_ENABLED=1`, and environment variables are process-wide.
//! Inside `tests/api` that would flip the switch for ~170 other tests that
//! assert the default is *off*. Each integration test file is a separate
//! process, so the mutation here is invisible everywhere else.
//!
//! Nothing below reaches ssh: the two bodies that get a `400` are rejected
//! before the install thread is spawned, which is the whole point of the
//! checks. The success path spawns `ssh` and is covered by the provisioning
//! crate's fake-ssh test and by `tests/e2e` on a real host.

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

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
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

/// Open the gate for this process and return a router that can install.
///
/// The distribution is a dummy file: the route only checks that the binary is
/// present as a path (`is_some()`), and no request below gets far enough to
/// execute it.
async fn app_with_provisioning_open() -> Router {
    let dist = std::env::temp_dir().join(format!("ag-dist-{}", std::process::id()));
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("agentgrid-node-daemon"), b"#!/bin/sh\n").unwrap();
    std::env::set_var("AGENTGRID_NODE_DIST", &dist);
    std::env::set_var("AGENTGRID_PROVISION_ENABLED", "1");

    let state = AppState::open_temp().await.unwrap();
    build_router(state)
}

/// `ssh_cert` is the one thing the compiler cannot check: a misspelled JSON
/// field deserializes to `None`, the certificate is never passed to ssh, and
/// the install proceeds as plain key auth — which fails on a host that requires
/// a certificate, or worse, succeeds against one that does not. Both bad
/// combinations must therefore be a `400` naming the reason, not a `202`.
#[tokio::test]
async fn a_bad_credential_combination_is_refused_before_ssh_runs() {
    let app = app_with_provisioning_open().await;
    let tok = test_token(&app).await;

    // A certificate without its private key: ssh would fall back to whatever
    // identity it finds and fail on the remote, after the job was queued.
    let resp = app
        .clone()
        .oneshot(post_auth(
            "/v1/node-installs",
            json!({
                "host": "deploy@node-b",
                "server": "http://cp.example.com:7800",
                "ssh_cert": "/home/op/.ssh/id_ed25519-cert.pub"
            })
            .to_string(),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "a certificate without ssh_key must be refused, not installed"
    );
    let err = body_json(resp).await;
    let msg = err["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("--ssh-key"),
        "the 400 must name the missing flag, got: {msg:?}"
    );

    // A certificate plus a password: the `sshpass`/`expect` wrapper is chosen
    // first, so the certificate would be silently ignored.
    let resp = app
        .oneshot(post_auth(
            "/v1/node-installs",
            json!({
                "host": "deploy@node-b",
                "server": "http://cp.example.com:7800",
                "ssh_key": "/home/op/.ssh/id_ed25519",
                "ssh_cert": "/home/op/.ssh/id_ed25519-cert.pub",
                "password": "hunter2"
            })
            .to_string(),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let err = body_json(resp).await;
    assert!(
        err["error"]
            .as_str()
            .unwrap_or_default()
            .contains("mutually exclusive"),
        "got: {err}"
    );
}
