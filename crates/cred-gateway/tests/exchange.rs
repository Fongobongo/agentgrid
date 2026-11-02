//! End-to-end tests for **exchange** mode (ADR 0029 F/exchange).
//!
//! The properties that would be security bugs if they silently stopped holding:
//!
//! - the App key never leaves the gateway — not in a request, not in an error,
//!   not in a log line;
//! - the sandbox receives a *provider* token, not the key;
//! - revoking at the daemon's request reaches the provider, so an attempt's
//!   token dies with the attempt instead of waiting out its hour;
//! - a provider failure is a refusal, not a fallback to something weaker.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agentgrid_cred_gateway::{parse_exchange, router, Gateway};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{delete, post};
use axum::Router;
use serde_json::{json, Value};

/// A throwaway RSA key, committed on purpose: it signs nothing that matters and
/// the alternative is a test that cannot exercise RS256 at all.
const TEST_KEY: &str = include_str!("fixtures/test-app-key.pem");

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

#[derive(Default)]
struct FakeGithub {
    mints: AtomicUsize,
    revokes: AtomicUsize,
    /// The `authorization` header of the last mint request, i.e. the App
    /// assertion. Kept so a test can decode `iss` and see it is the App id.
    last_assertion: std::sync::Mutex<String>,
    /// Set to make the mint fail, which must become a 502 and nothing else.
    fail: std::sync::Mutex<Option<(StatusCode, String)>>,
}

fn fake_github(state: Arc<FakeGithub>) -> Router {
    Router::new()
        .route(
            "/app/installations/{id}/access_tokens",
            post({
                let state = state.clone();
                move |headers: HeaderMap| async move {
                    state.mints.fetch_add(1, Ordering::SeqCst);
                    let assertion = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .trim_start_matches("Bearer ")
                        .to_string();
                    *state.last_assertion.lock().unwrap() = assertion;
                    if let Some((status, body)) = state.fail.lock().unwrap().clone() {
                        return (status, body);
                    }
                    let expires = (chrono::Utc::now() + chrono::Duration::hours(1))
                        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                    (
                        StatusCode::CREATED,
                        format!(
                            r#"{{"token":"ghs_testtoken","expires_at":"{expires}","permissions":{{}}}}"#
                        ),
                    )
                }
            }),
        )
        .route(
            "/installation/token",
            delete({
                let state = state.clone();
                move |headers: HeaderMap| async move {
                    state.revokes.fetch_add(1, Ordering::SeqCst);
                    let seen = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    if seen == "Bearer ghs_testtoken" {
                        StatusCode::NO_CONTENT
                    } else {
                        StatusCode::UNAUTHORIZED
                    }
                }
            }),
        )
        .with_state(state)
}

/// A gateway with one exchange route pointing at a fake GitHub, plus the fake.
async fn gateway_pointing_at_fake() -> (Arc<Gateway>, Arc<FakeGithub>) {
    let fake = Arc::new(FakeGithub::default());
    let api = spawn(fake_github(fake.clone())).await;

    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("app.pem");
    std::fs::write(&key, TEST_KEY).unwrap();
    let spec = format!(
        "github=github-app;app_id=42;installation_id=7;api={api};key={}",
        key.display()
    );
    // The key is read here, at parse time, so the tempdir may go out of scope
    // immediately: the gateway keeps the PEM content, not the path.
    let exchange = parse_exchange(&spec).unwrap();
    let gw = Arc::new(
        Gateway::new_with_exchange(
            HashMap::new(),
            HashMap::new(),
            exchange,
            "admin-secret".into(),
        )
        .unwrap(),
    );
    (gw, fake)
}

async fn mint(gw_addr: &str, route: &str) -> (StatusCode, Value) {
    let resp = reqwest::Client::new()
        .post(format!("{gw_addr}/tokens"))
        .bearer_auth("admin-secret")
        .json(&json!({"attempt_id": "att-1", "route": route, "ttl_secs": 21600}))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

#[tokio::test]
async fn an_exchange_hands_the_sandbox_a_provider_token() {
    let (gw, fake) = gateway_pointing_at_fake().await;
    let gw_addr = spawn(router(gw)).await;

    let (status, body) = mint(&gw_addr, "github").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["token"], "ghs_testtoken",
        "the sandbox must get the token"
    );
    assert_eq!(fake.mints.load(Ordering::SeqCst), 1);
    // The provider's own expiry, not the 6 h the daemon asked for.
    let expires_in = body["expires_in"].as_u64().unwrap();
    assert!(
        (3500..=3600).contains(&expires_in),
        "expires_in should follow the provider, got {expires_in}"
    );

    // The assertion is a real three-part JWT whose `iss` is the App id.
    let assertion = fake.last_assertion.lock().unwrap().clone();
    assert_eq!(assertion.split('.').count(), 3, "{assertion}");
    let payload = assertion.split('.').nth(1).unwrap();
    use base64::Engine;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .unwrap();
    let claims: Value = serde_json::from_slice(&decoded).unwrap();
    assert_eq!(claims["iss"], 42);
    assert!(claims["exp"].as_u64().unwrap() > claims["iat"].as_u64().unwrap());
}

#[tokio::test]
async fn revoking_reaches_the_provider() {
    let (gw, fake) = gateway_pointing_at_fake().await;
    let gw_addr = spawn(router(gw)).await;
    let (_, body) = mint(&gw_addr, "github").await;
    let token = body["token"].as_str().unwrap().to_string();

    let status = reqwest::Client::new()
        .delete(format!("{gw_addr}/tokens/{token}"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(
        fake.revokes.load(Ordering::SeqCst),
        1,
        "the provider must be told, or the token outlives the attempt"
    );
}

#[tokio::test]
async fn an_unknown_token_is_not_found() {
    let (gw, _fake) = gateway_pointing_at_fake().await;
    let gw_addr = spawn(router(gw)).await;
    let status = reqwest::Client::new()
        .delete(format!("{gw_addr}/tokens/ghs_never-issued"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A provider failure must be a refusal. The alternative — minting a capability
/// instead, or an empty token — is exactly the "silently unenforced" state the
/// ADR exists to prevent.
#[tokio::test]
async fn a_provider_failure_is_refused_and_echoes_no_key() {
    let (gw, fake) = gateway_pointing_at_fake().await;
    *fake.fail.lock().unwrap() =
        Some((StatusCode::UNAUTHORIZED, "{\"message\":\"bad jwt\"}".into()));
    let gw_addr = spawn(router(gw)).await;

    let (status, body) = mint(&gw_addr, "github").await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    let text = body.to_string();
    assert!(!text.contains("PRIVATE KEY"), "the key leaked: {text}");
    assert!(!text.contains("BEGIN"), "the key leaked: {text}");
    assert!(
        text.contains("bad jwt"),
        "the provider's reason should survive: {text}"
    );
}

#[tokio::test]
async fn minting_still_needs_the_admin_token() {
    let (gw, fake) = gateway_pointing_at_fake().await;
    let gw_addr = spawn(router(gw)).await;
    let status = reqwest::Client::new()
        .post(format!("{gw_addr}/tokens"))
        .json(&json!({"attempt_id": "a", "route": "github", "ttl_secs": 60}))
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(fake.mints.load(Ordering::SeqCst), 0);
}
