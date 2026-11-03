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
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
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

/// `AGENTGRID_WEBHOOK_ADAPTER` — the adapter a webhook-created task asks for.
///
/// Defaults to `mock`, which is what a fresh node installs. A node that does
/// not carry the requested adapter fails the attempt as
/// `infrastructure_failed`, so a fleet running only `claude` needs this set —
/// there is no per-repository adapter config to resolve it from.
fn webhook_adapter() -> String {
    match std::env::var("AGENTGRID_WEBHOOK_ADAPTER") {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => "mock".into(),
    }
}

/// The GitHub delivery GUID (`X-GitHub-Delivery`): unique per delivery,
/// stable across retries of the same delivery — the dedup key for
/// `webhook_delivery_fresh`. None when the header is absent.
fn delivery_guid(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-github-delivery")
        .and_then(|h| h.to_str().ok())
        .map(|s| s.to_string())
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
    // GitHub sends the digest as lowercase hex; anything that is not a
    // 32-byte hex string cannot be the right MAC.
    let presented = hex::decode(sig).map_err(|_| StatusCode::UNAUTHORIZED)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    mac.update(body);
    // `verify_slice` is constant time — comparing hex strings with `==` would
    // leak the position of the first differing byte.
    mac.verify_slice(&presented)
        .map_err(|_| StatusCode::UNAUTHORIZED)
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

    // Delivery dedup (audit CP-4): GitHub delivery is at-least-once — a
    // replayed delivery GUID must not mint a second task for the same
    // issue. Missing header (non-GitHub sender / proxy strip) falls back
    // to the historical create-once behavior.
    //
    // The GUID is remembered *before* the task exists, so every failure path
    // below has to forget it again — otherwise GitHub's retry looks like a
    // duplicate and the issue silently never becomes a task.
    let guid = delivery_guid(&headers);
    if let Some(guid) = &guid {
        match state.store.webhook_delivery_fresh(guid).await {
            Ok(true) => {}
            Ok(false) => return Ok(StatusCode::OK),
            Err(e) => {
                tracing::error!("webhook delivery dedup failed: {e}");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }

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

    // Competitor-gap feature (GitHub write-back): capture the repository so a
    // successful attempt can push its branch and open a PR back at GitHub.
    let repo = v.get("repository").and_then(|r| r.as_object());
    let full_name = repo
        .and_then(|r| r.get("full_name"))
        .and_then(|f| f.as_str())
        .map(String::from);
    let repo_name = repo
        .and_then(|r| r.get("name"))
        .and_then(|n| n.as_str())
        .map(String::from)
        .unwrap_or_else(|| "*".into());

    let prompt = if body_text.trim().is_empty() {
        format!("GitHub issue #{number}: {title}")
    } else {
        format!("GitHub issue #{number}: {title}\n\n{body_text}")
    };
    let req = agentgrid_common::CreateTaskRequest {
        prompt,
        // `owner/name`, not the bare `name`: two organisations can both have
        // an `api` repository, and the short name would put both into one task
        // bucket (and one node cache directory).
        repository: full_name.clone().unwrap_or(repo_name),
        adapter: webhook_adapter(),
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
        // Write-back: the node pushes the agent branch, opens a PR and
        // comments on this issue after a successful finish.
        github_push: full_name.is_some(),
        github_repo: full_name,
        github_issue: Some(number),
        github_base_ref: None,
        github_create: false,
        max_attempts: 1,
        retry_on_node_loss: None,
        consensus_mode: None,
        review_of: None,
        max_tokens: None,
        max_cost_cents: None,
    };
    if let Err(e) = state.store.create_task(&req).await {
        tracing::error!("webhook task creation failed: {e}");
        // The delivery was recorded as seen before the task existed. Left in
        // place, every GitHub retry is answered "duplicate" and the issue
        // never becomes a task — so drop the record and let the retry work.
        if let Some(guid) = &guid {
            if let Err(e) = state.store.webhook_delivery_forget(guid).await {
                tracing::error!("could not clear webhook delivery {guid}: {e}");
            }
        }
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    Ok(StatusCode::OK)
}

/// Build a fire-and-forget CI-fix/PR-fix task. The adapter comes from
/// [`webhook_adapter`] (default `mock`); repository `*` lets any registered
/// node claim it.
fn create_followup_task(prompt: String) -> agentgrid_common::CreateTaskRequest {
    agentgrid_common::CreateTaskRequest {
        prompt,
        repository: "*".into(),
        adapter: webhook_adapter(),
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
        max_attempts: 1,
        retry_on_node_loss: None,
        consensus_mode: None,
        review_of: None,
        max_tokens: None,
        max_cost_cents: None,
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
    if let Some(guid) = delivery_guid(&headers) {
        match state.store.webhook_delivery_fresh(&guid).await {
            Ok(true) => {}
            Ok(false) => return Ok(StatusCode::OK),
            Err(e) => {
                tracing::error!("webhook delivery dedup failed: {e}");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }
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
    if let Some(guid) = delivery_guid(&headers) {
        match state.store.webhook_delivery_fresh(&guid).await {
            Ok(true) => {}
            Ok(false) => return Ok(StatusCode::OK),
            Err(e) => {
                tracing::error!("webhook delivery dedup failed: {e}");
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
    }
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
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(body);
        hex::encode(mac.finalize().into_bytes())
    }

    /// Pins the dependency to the standard: if the `hmac` crate were swapped
    /// for something that computes a different MAC, every signature check in
    /// production would start refusing real deliveries (or accepting forged
    /// ones) and this is the test that says which.
    #[test]
    fn hmac_sha256_matches_rfc_4231_case_1() {
        let mut mac = Hmac::<Sha256>::new_from_slice(&[0x0bu8; 20]).unwrap();
        mac.update(b"Hi There");
        assert_eq!(
            hex::encode(mac.finalize().into_bytes()),
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
            "repository": {
                "name": "demo",
                "full_name": "acme/demo"
            },
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
        // Competitor-gap feature (GitHub write-back): the webhook must capture
        // the repository full_name + issue number so a successful attempt can
        // push a branch, open a PR and comment on the issue.
        // `owner/name`, not the bare name: two orgs can both have `demo`.
        assert_eq!(tasks[0].repository, "acme/demo");
        assert_eq!(tasks[0].github_repo.as_deref(), Some("acme/demo"));
        assert_eq!(tasks[0].github_issue, Some(42));
    }

    /// The delivery GUID is recorded before the task exists, so a failure in
    /// between used to make every GitHub retry look like a duplicate — the
    /// issue silently never became a task. The trigger below is a stand-in for
    /// any transient write failure (disk full, lock timeout).
    #[tokio::test]
    async fn a_retry_after_a_failed_create_still_creates_the_task() {
        std::env::set_var("AGENTGRID_GITHUB_WEBHOOK_SECRET", "test-secret");
        std::env::set_var("AGENTGRID_DISK_CRITICAL_MB", "0");
        let state = crate::AppState::open_temp().await.unwrap();
        let app = crate::build_router(state.clone());

        let payload = serde_json::json!({
            "action": "opened",
            "repository": {"name": "demo", "full_name": "acme/demo"},
            "issue": {
                "number": 7,
                "title": "Retry me",
                "body": "",
                "labels": [{"name": "agent"}]
            }
        });
        let body = serde_json::to_vec(&payload).unwrap();
        let sig = sign("test-secret", &body);
        // Same delivery, same GUID: GitHub retries carry the original one.
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/v1/webhooks/github/issues")
                .header("content-type", "application/json")
                .header("x-hub-signature-256", format!("sha256={sig}"))
                .header("x-github-delivery", "delivery-1")
                .body(Body::from(body.clone()))
                .unwrap()
        };

        sqlx::raw_sql(
            "CREATE TRIGGER block_tasks BEFORE INSERT ON tasks \
             BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END",
        )
        .execute(&state.store.pool)
        .await
        .unwrap();

        let resp = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "the failed insert must surface as a failure, not a silent OK"
        );
        assert!(state.store.list_tasks().await.unwrap().is_empty());

        sqlx::raw_sql("DROP TRIGGER block_tasks")
            .execute(&state.store.pool)
            .await
            .unwrap();

        let resp = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "the retry of a failed delivery must not be dropped as a duplicate"
        );
        let tasks = state.store.list_tasks().await.unwrap();
        assert_eq!(tasks.len(), 1, "the retry must create exactly one task");
        assert!(tasks[0].prompt.contains("Retry me"));

        // And the GUID is recorded again: a second replay is still a no-op.
        let resp = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(state.store.list_tasks().await.unwrap().len(), 1);
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
