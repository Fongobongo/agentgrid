//! Provisioning from the panel: the gate, and the fact that it is closed.
//!
//! The install itself spawns ssh/scp, so the happy path is not exercised here —
//! it is verified against a live control plane (see `docs/agent-ops.md`). What
//! this file pins is the part that must never regress: with the flag unset the
//! endpoint refuses, and it says which flag to set. A control plane that can
//! ssh into hosts is a different threat model from one that cannot, so "off"
//! has to be the default and it has to be visible in the API, not only in a doc.

use crate::helpers::*;

fn body_json(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap()
}

#[tokio::test]
async fn provisioning_is_disabled_by_default_and_says_so() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let tok = test_token(&app).await;

    let resp = app
        .clone()
        .oneshot(get_auth("/v1/nodes/provisioning", &tok))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let info: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(info["enabled"], serde_json::json!(false));
    // The UI renders this verbatim, so it must name the variable to set.
    let reason = info["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("AGENTGRID_PROVISION_ENABLED"),
        "reason must tell the operator what to set, got: {reason:?}"
    );
}

#[tokio::test]
async fn install_is_refused_with_403_when_disabled() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let tok = test_token(&app).await;

    let resp = app
        .oneshot(post_auth(
            "/v1/nodes/install",
            body_json(&serde_json::json!({
                "host": "root@10.0.0.5",
                "server": "http://cp.example.com:7800"
            })),
            &tok,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let err: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(
        err["error"]
            .as_str()
            .unwrap_or_default()
            .contains("AGENTGRID_PROVISION_ENABLED"),
        "403 must explain the gate, got: {err}"
    );
}

/// A job id that never existed must not read as "still running" — the UI polls
/// until it sees a terminal state, so a silent 200 with an empty body would
/// leave a spinner forever.
#[tokio::test]
async fn unknown_install_job_is_404() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let tok = test_token(&app).await;

    let resp = app
        .oneshot(get_auth("/v1/nodes/install/does-not-exist", &tok))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// The list endpoint is what the panel shows after a reload; with nothing ever
/// installed it must be an empty array, not a 404 or a null.
#[tokio::test]
async fn install_list_is_empty_and_not_an_error() {
    let state = AppState::open_temp().await.unwrap();
    let app = build_router(state);
    let tok = test_token(&app).await;

    let resp = app
        .oneshot(get_auth("/v1/nodes/installs", &tok))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let jobs: serde_json::Value =
        serde_json::from_slice(&to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(jobs, serde_json::json!([]));
}
