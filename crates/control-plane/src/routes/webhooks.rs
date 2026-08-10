//! Plan 1.4 (#2a): GitHub issue webhooks → auto-task creation.
//!
//! `POST /v1/webhooks/github/issues` verifies the `X-Hub-Signature-256`
//! HMAC-SHA256 signature (GitHub's standard), then creates a task from the
//! issue body. Only issues carrying a trigger label (`agent`) become tasks;
//! everything else is acked silently. Fire-and-forget semantics: a bad
//! signature is rejected (401), but GitHub retries on 5xx so the handler
//! never panics.

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
};
use std::sync::Arc;

use crate::AppState;

/// `AGENTGRID_GITHUB_WEBHOOK_SECRET` — shared secret used to verify
/// `X-Hub-Signature-256`. Unset → the webhook route is disabled (404).
fn configured_secret() -> Option<String> {
    match std::env::var("AGENTGRID_GITHUB_WEBHOOK_SECRET") {
        Ok(s) if !s.trim().is_empty() => Some(s),
        _ => None,
    }
}

/// Verify the GitHub `X-Hub-Signature-256` HMAC against the raw body.
/// Plan 1.10: shared by the issue / check_run / pull_request webhooks.
/// Returns `Err(StatusCode)` suitable for the route.
fn verify_github_sig(headers: &HeaderMap, body: &[u8]) -> Result<(), StatusCode> {
    let Some(secret) = configured_secret() else {
        return Err(StatusCode::NOT_FOUND); // webhook not enabled
    };
    let sig = headers
        .get("x-hub-signature-256")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.strip_prefix("sha256="))
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let expected = hmac_sha256(secret.as_bytes(), body);
    let expected_hex: String = expected.iter().map(|b| format!("{b:02x}")).collect();
    if !ct_eq(sig.as_bytes(), expected_hex.as_bytes()) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(())
}

/// Constant-time comparison of two byte slices.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// HMAC-SHA256 (RFC 2104) with the sha2 crate only — avoids a new dependency
/// for one endpoint.
fn hmac_sha256(key: &[u8], msg: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        let h = Sha256::digest(key);
        k[..32].copy_from_slice(&h);
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0u8; BLOCK];
    let mut opad = [0u8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] = k[i] ^ 0x36;
        opad[i] = k[i] ^ 0x5c;
    }
    let inner = Sha256::digest([ipad.as_slice(), msg].concat());
    let full = [opad.as_slice(), inner.as_slice()].concat();
    let out = Sha256::digest(full);
    let mut r = [0u8; 32];
    r.copy_from_slice(&out);
    r
}

/// GitHub sends the payload as raw bytes; axum's `Json` extractor would
/// re-parse, but we need the exact body for HMAC. Read it as bytes via
/// `Bytes` and parse ourselves.
pub async fn github_issue_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<StatusCode, StatusCode> {
    verify_github_sig(&headers, &body)?;

    // Parse the GitHub issue event.
    let v: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    // Only the "opened" action should create tasks; other actions are acked.
    if v.get("action").and_then(|a| a.as_str()) != Some("opened") {
        return Ok(StatusCode::OK);
    }
    let issue = v.get("issue").ok_or(StatusCode::BAD_REQUEST)?;
    let number = issue.get("number").and_then(|n| n.as_i64()).unwrap_or(0);
    let title = issue
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let body_text = issue
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .to_string();
    let labels: Vec<String> = issue
        .get("labels")
        .and_then(|l| l.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Trigger label gate: only `agent`-labeled issues become tasks.
    if !labels.iter().any(|l| l == "agent") {
        return Ok(StatusCode::OK);
    }

    let prompt = if body_text.trim().is_empty() {
        format!("GitHub issue #{number}: {title}")
    } else {
        format!("GitHub issue #{number}: {title}\n\n{body_text}")
    };
    let req = agentgrid_common::CreateTaskRequest {
        prompt,
        repository: "*".into(),
        adapter: "mock".into(),
        requested_node_id: None,
        timeout_secs: None,
        validation_command: None,
        base_commit: None,
        parent_acp_session_id: None,
        security_profile: None,
        network_mode: None,
        group_id: None,
    };
    state
        .store
        .create_task(&req)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

/// Build a fire-and-forget CI-fix/PR-fix task. `adapter=mock` matches the
/// issue webhook convention; nodes carrying a real adapter override it via
/// `adapter` resolution. Repository `*` lets any registered node claim it.
fn create_followup_task(prompt: String) -> agentgrid_common::CreateTaskRequest {
    agentgrid_common::CreateTaskRequest {
        prompt,
        repository: "*".into(),
        adapter: "mock".into(),
        requested_node_id: None,
        timeout_secs: None,
        validation_command: None,
        base_commit: None,
        parent_acp_session_id: None,
        security_profile: None,
        network_mode: None,
        group_id: None,
    }
}

/// Plan 1.10 (#1): `POST /v1/webhooks/github/check_run`. GitHub fires this
/// when a CI check finishes; `action=completed` + `conclusion=failure` means a
/// failed check run. We spawn an autonomous fix task: the agent reads the
/// failed check's log (via `gh`) and attempts the minimal fix. Only `failure`
/// / `cancelled` conclusions count; `success`/`skipped` are acked silently.
pub async fn github_check_run_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<StatusCode, StatusCode> {
    verify_github_sig(&headers, &body)?;
    let v: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    if v.get("action").and_then(|a| a.as_str()) != Some("completed") {
        return Ok(StatusCode::OK);
    }
    let check_run = v.get("check_run").ok_or(StatusCode::BAD_REQUEST)?;
    let conclusion = check_run
        .get("conclusion")
        .and_then(|c| c.as_str())
        .unwrap_or("");
    if conclusion != "failure" && conclusion != "cancelled" {
        return Ok(StatusCode::OK);
    }
    let name = check_run
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("check")
        .to_string();
    let html_url = check_run
        .get("html_url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    let repo = v
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let prompt = format!(
        "GitHub CI check `{name}` on {repo} failed (conclusion: {conclusion}).\
\n\
\nRead the failed check's log and reproduce the failure, then apply the minimal \
fix that makes it pass. Check details: {html_url}\
\n\
\nUseful: `gh run view` / the check log URL to find the failure. Keep the diff \
minimal and match the existing style. Do not change unrelated code."
    );
    state
        .store
        .create_task(&create_followup_task(prompt))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

/// Plan 1.10 (#1): `POST /v1/webhooks/github/pull_request`. GitHub reports
/// `mergeable_state=conflicting` when a PR can't be auto-merged. We spawn an
/// autonomous rebase/resolve-fix task. Considered actions: `opened`,
/// `synchronize`, `reopened`. A non-conflicting state is acked silently.
pub async fn github_pull_request_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Result<StatusCode, StatusCode> {
    verify_github_sig(&headers, &body)?;
    let v: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| StatusCode::BAD_REQUEST)?;
    let action = v.get("action").and_then(|a| a.as_str()).unwrap_or("");
    if action != "opened" && action != "synchronize" && action != "reopened" {
        return Ok(StatusCode::OK);
    }
    let pr = v.get("pull_request").ok_or(StatusCode::BAD_REQUEST)?;
    if pr.get("mergeable_state").and_then(|m| m.as_str()) != Some("conflicting") {
        return Ok(StatusCode::OK);
    }
    let number = pr.get("number").and_then(|n| n.as_i64()).unwrap_or(0);
    let title = pr
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();
    let url = pr
        .get("html_url")
        .and_then(|u| u.as_str())
        .unwrap_or("")
        .to_string();
    let repo = v
        .get("repository")
        .and_then(|r| r.get("full_name"))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string();
    let prompt = format!(
        "GitHub PR #{number} ({title}) on {repo} has merge conflicts.\
\n\
\nResolve them and make the PR mergeable. Rebase onto the target branch (do not \
merge the target into a merge commit), pick the intent of both sides when they \
conflict, and run the narrowest useful check to confirm. PR: {url}"
    );
    state
        .store
        .create_task(&create_followup_task(prompt))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn sign(secret: &str, body: &[u8]) -> String {
        let mac = hmac_sha256(secret.as_bytes(), body);
        mac.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn hmac_sha256_matches_known_vector() {
        // RFC 4231 test case 1: key=0x0b*20, data="Hi There".
        let key = [0x0bu8; 20];
        let mac = hmac_sha256(&key, b"Hi There");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(
            hex,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    #[tokio::test]
    async fn webhook_creates_task_for_agent_labeled_issue() {
        std::env::set_var("AGENTGRID_GITHUB_WEBHOOK_SECRET", "test-secret");
        std::env::set_var("AGENTGRID_DISK_CRITICAL_MB", "0");
        let state = crate::AppState::open_temp().await.unwrap();
        let app = crate::build_router(state.clone());

        let payload = serde_json::json!({
            "action": "opened",
            "issue": {
                "number": 42,
                "title": "Fix the flaky login test",
                "body": "The login flow flakes 1 in 10 runs.\n\nSteps to reproduce...",
                "labels": [{"name": "agent"}, {"name": "bug"}]
            }
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let sig = sign("test-secret", &body);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/webhooks/github/issues")
                    .header("content-type", "application/json")
                    .header("x-hub-signature-256", format!("sha256={sig}"))
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let tasks = state.store.list_tasks().await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].prompt.contains("Fix the flaky login test"));
        assert!(tasks[0].prompt.contains("flakes 1 in 10"));
    }

    #[tokio::test]
    async fn webhook_rejects_bad_signature() {
        std::env::set_var("AGENTGRID_GITHUB_WEBHOOK_SECRET", "test-secret");
        std::env::set_var("AGENTGRID_DISK_CRITICAL_MB", "0");
        let state = crate::AppState::open_temp().await.unwrap();
        let app = crate::build_router(state.clone());

        let payload = serde_json::json!({
            "action": "opened",
            "issue": {"number": 1, "title": "x", "body": "y", "labels": [{"name": "agent"}]}
        });
        let body = serde_json::to_vec(&payload).unwrap();

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/webhooks/github/issues")
                    .header("content-type", "application/json")
                    .header("x-hub-signature-256", "sha256=deadbeef")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert!(state.store.list_tasks().await.unwrap().is_empty());
    }

    /// Plan 1.10 (#1): a failed check_run webhook spawns a CI-fix task whose
    /// prompt names the failed check and points the agent at the log URL.
    #[tokio::test]
    async fn check_run_failed_webhook_spawns_ci_fix_task() {
        std::env::set_var("AGENTGRID_GITHUB_WEBHOOK_SECRET", "test-secret");
        std::env::set_var("AGENTGRID_DISK_CRITICAL_MB", "0");
        let state = crate::AppState::open_temp().await.unwrap();
        let app = crate::build_router(state.clone());

        let payload = serde_json::json!({
            "action": "completed",
            "check_run": {
                "id": 99,
                "name": "build",
                "conclusion": "failure",
                "html_url": "https://github.com/o/r/runs/99"
            },
            "repository": {"full_name": "o/r"}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let sig = sign("test-secret", &body);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/webhooks/github/check_run")
                    .header("content-type", "application/json")
                    .header("x-hub-signature-256", format!("sha256={sig}"))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let tasks = state.store.list_tasks().await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].prompt.contains("CI check `build`"));
        assert!(tasks[0].prompt.contains("on o/r"));
        assert!(tasks[0].prompt.contains("runs/99"));
    }

    /// A successful check run is acked silently (no task spawned).
    #[tokio::test]
    async fn check_run_success_webhook_spawns_no_task() {
        std::env::set_var("AGENTGRID_GITHUB_WEBHOOK_SECRET", "test-secret");
        std::env::set_var("AGENTGRID_DISK_CRITICAL_MB", "0");
        let state = crate::AppState::open_temp().await.unwrap();
        let app = crate::build_router(state.clone());

        let payload = serde_json::json!({
            "action": "completed",
            "check_run": {"id": 1, "name": "build", "conclusion": "success"},
            "repository": {"full_name": "o/r"}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let sig = sign("test-secret", &body);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/webhooks/github/check_run")
                    .header("content-type", "application/json")
                    .header("x-hub-signature-256", format!("sha256={sig}"))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(state.store.list_tasks().await.unwrap().is_empty());
    }

    /// Plan 1.10 (#1): a conflicting `pull_request` webhook spawns a
    /// merge-conflict resolve task.
    #[tokio::test]
    async fn pull_request_conflicting_webhook_spawns_resolve_task() {
        std::env::set_var("AGENTGRID_GITHUB_WEBHOOK_SECRET", "test-secret");
        std::env::set_var("AGENTGRID_DISK_CRITICAL_MB", "0");
        let state = crate::AppState::open_temp().await.unwrap();
        let app = crate::build_router(state.clone());

        let payload = serde_json::json!({
            "action": "synchronize",
            "pull_request": {
                "number": 7,
                "title": "Bump deps",
                "html_url": "https://github.com/o/r/pull/7",
                "mergeable_state": "conflicting"
            },
            "repository": {"full_name": "o/r"}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let sig = sign("test-secret", &body);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/webhooks/github/pull_request")
                    .header("content-type", "application/json")
                    .header("x-hub-signature-256", format!("sha256={sig}"))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let tasks = state.store.list_tasks().await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert!(tasks[0].prompt.contains("PR #7"));
        assert!(tasks[0].prompt.contains("merge conflicts"));
        assert!(tasks[0].prompt.contains("pull/7"));
    }

    /// A mergeable PR is acked silently (no task spawned).
    #[tokio::test]
    async fn pull_request_clean_webhook_spawns_no_task() {
        std::env::set_var("AGENTGRID_GITHUB_WEBHOOK_SECRET", "test-secret");
        std::env::set_var("AGENTGRID_DISK_CRITICAL_MB", "0");
        let state = crate::AppState::open_temp().await.unwrap();
        let app = crate::build_router(state.clone());

        let payload = serde_json::json!({
            "action": "opened",
            "pull_request": {
                "number": 8,
                "title": "x",
                "html_url": "https://github.com/o/r/pull/8",
                "mergeable_state": "clean"
            },
            "repository": {"full_name": "o/r"}
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let sig = sign("test-secret", &body);

        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/webhooks/github/pull_request")
                    .header("content-type", "application/json")
                    .header("x-hub-signature-256", format!("sha256={sig}"))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(state.store.list_tasks().await.unwrap().is_empty());
    }
}
