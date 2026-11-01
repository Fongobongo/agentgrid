//! End-to-end tests for the credential gateway: a real HTTP upstream, a real
//! gateway, real tokens. Everything here is about a property that would be a
//! security bug if it silently stopped holding.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use agentgrid_cred_gateway::{router, Gateway, Route};
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::routing::any;
use axum::{Json, Router};
use serde_json::{json, Value};

async fn spawn(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}")
}

/// Echoes back what it received, so a test can see which credential arrived.
async fn echo(headers: HeaderMap, method: Method, uri: Uri, body: Bytes) -> Json<Value> {
    let get = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    Json(json!({
        "x_api_key": get("x-api-key"),
        "authorization": get("authorization"),
        "method": method.as_str(),
        "path": uri.path(),
        "query": uri.query().unwrap_or(""),
        "body": String::from_utf8_lossy(&body),
    }))
}

fn gateway(routes: HashMap<String, Route>, keys: &[(&str, &str)]) -> Arc<Gateway> {
    let keys = keys
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    Arc::new(Gateway::new(routes, keys, "admin-secret".into()).unwrap())
}

fn route(upstream: &str, header: &str) -> Route {
    Route {
        upstream: upstream.to_string(),
        header: header.to_string(),
    }
}

async fn mint(gw: &str, route: &str, ttl: u64, quota: Option<u32>) -> String {
    let resp = reqwest::Client::new()
        .post(format!("{gw}/tokens"))
        .bearer_auth("admin-secret")
        .json(&json!({
            "attempt_id": "att-1",
            "route": route,
            "ttl_secs": ttl,
            "quota": quota,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "mint failed");
    let body: Value = resp.json().await.unwrap();
    body["token"].as_str().unwrap().to_string()
}

/// The whole point: the upstream sees the provider key, and the capability the
/// sandbox holds is nowhere in the exchange.
#[tokio::test]
async fn the_upstream_sees_the_real_key_and_never_the_capability() {
    let upstream = spawn(Router::new().fallback(echo)).await;
    let mut routes = HashMap::new();
    routes.insert("mock".to_string(), route(&upstream, "x-api-key"));
    let gw = spawn(router(gateway(routes, &[("mock", "sk-real")]))).await;
    let cap = mint(&gw, "mock", 60, None).await;

    let resp = reqwest::Client::new()
        .get(format!("{gw}/p/mock/echo"))
        .header("x-api-key", &cap)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let text = resp.text().await.unwrap();
    assert!(
        text.contains("\"x_api_key\":\"sk-real\""),
        "the upstream did not receive the real key: {text}"
    );
    assert!(
        !text.contains(&cap),
        "the capability was forwarded upstream: {text}"
    );
}

#[tokio::test]
async fn an_unknown_capability_is_rejected() {
    let upstream = spawn(Router::new().fallback(echo)).await;
    let mut routes = HashMap::new();
    routes.insert("mock".to_string(), route(&upstream, "x-api-key"));
    let gw = spawn(router(gateway(routes, &[("mock", "sk-real")]))).await;

    let resp = reqwest::Client::new()
        .get(format!("{gw}/p/mock/echo"))
        .header("x-api-key", "not-a-token")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let text = resp.text().await.unwrap();
    assert!(
        !text.contains("sk-real"),
        "the error echoed the key: {text}"
    );
}

#[tokio::test]
async fn a_revoked_capability_stops_working() {
    let upstream = spawn(Router::new().fallback(echo)).await;
    let mut routes = HashMap::new();
    routes.insert("mock".to_string(), route(&upstream, "x-api-key"));
    let gw = spawn(router(gateway(routes, &[("mock", "sk-real")]))).await;
    let cap = mint(&gw, "mock", 60, None).await;

    let http = reqwest::Client::new();
    let ok = http
        .get(format!("{gw}/p/mock/echo"))
        .header("x-api-key", &cap)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);

    let revoked = http
        .delete(format!("{gw}/tokens/{cap}"))
        .bearer_auth("admin-secret")
        .send()
        .await
        .unwrap();
    assert_eq!(revoked.status(), 204);

    let after = http
        .get(format!("{gw}/p/mock/echo"))
        .header("x-api-key", &cap)
        .send()
        .await
        .unwrap();
    assert_eq!(after.status(), 401, "a revoked capability still worked");
}

#[tokio::test]
async fn an_expired_capability_is_rejected() {
    let upstream = spawn(Router::new().fallback(echo)).await;
    let mut routes = HashMap::new();
    routes.insert("mock".to_string(), route(&upstream, "x-api-key"));
    let gw = spawn(router(gateway(routes, &[("mock", "sk-real")]))).await;
    let cap = mint(&gw, "mock", 1, None).await;

    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let resp = reqwest::Client::new()
        .get(format!("{gw}/p/mock/echo"))
        .header("x-api-key", &cap)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    let text = resp.text().await.unwrap();
    assert!(text.contains("expired"), "{text}");
}

#[tokio::test]
async fn a_capability_cannot_be_replayed_against_another_route() {
    let upstream = spawn(Router::new().fallback(echo)).await;
    let mut routes = HashMap::new();
    routes.insert("mock".to_string(), route(&upstream, "x-api-key"));
    routes.insert("other".to_string(), route(&upstream, "authorization"));
    let gw = spawn(router(gateway(
        routes,
        &[("mock", "sk-real"), ("other", "sk-other")],
    )))
    .await;
    let cap = mint(&gw, "mock", 60, None).await;

    let resp = reqwest::Client::new()
        .get(format!("{gw}/p/other/echo"))
        .header("authorization", format!("Bearer {cap}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        403,
        "a capability minted for one route was accepted by another"
    );
}

#[tokio::test]
async fn the_quota_refuses_the_next_request() {
    let upstream = spawn(Router::new().fallback(echo)).await;
    let mut routes = HashMap::new();
    routes.insert("mock".to_string(), route(&upstream, "x-api-key"));
    let gw = spawn(router(gateway(routes, &[("mock", "sk-real")]))).await;
    let cap = mint(&gw, "mock", 60, Some(2)).await;
    let http = reqwest::Client::new();

    for i in 0..2 {
        let r = http
            .get(format!("{gw}/p/mock/echo"))
            .header("x-api-key", &cap)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "request {i} inside the quota was refused");
    }
    let third = http
        .get(format!("{gw}/p/mock/echo"))
        .header("x-api-key", &cap)
        .send()
        .await
        .unwrap();
    assert_eq!(third.status(), 429);
}

/// A redirect would make reqwest replay the injected header to whatever host
/// the upstream names. The gateway must hand the 302 back instead.
#[tokio::test]
async fn a_redirect_is_not_followed() {
    let hits = Arc::new(AtomicUsize::new(0));
    let target_hits = hits.clone();
    let target = spawn(Router::new().route(
        "/steal",
        any(move || {
            let h = target_hits.clone();
            async move {
                h.fetch_add(1, Ordering::SeqCst);
                "got it"
            }
        }),
    ))
    .await;
    let redirector = spawn(Router::new().route(
        "/go",
        any(move || {
            let t = target.clone();
            async move { (StatusCode::FOUND, [("location", format!("{t}/steal"))]) }
        }),
    ))
    .await;

    let mut routes = HashMap::new();
    routes.insert("mock".to_string(), route(&redirector, "x-api-key"));
    let gw = spawn(router(gateway(routes, &[("mock", "sk-real")]))).await;
    let cap = mint(&gw, "mock", 60, None).await;

    // The *client* must not follow either, or the test measures reqwest's
    // default policy instead of the gateway's: a followed redirect shows up as
    // a 200 from the target and a hit count of one.
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let resp = http
        .get(format!("{gw}/p/mock/go"))
        .header("x-api-key", &cap)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 302, "the gateway followed the redirect");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "the injected key was replayed to the redirect target"
    );
}

#[tokio::test]
async fn the_method_query_and_body_are_forwarded() {
    let upstream = spawn(Router::new().fallback(echo)).await;
    let mut routes = HashMap::new();
    routes.insert("mock".to_string(), route(&upstream, "x-api-key"));
    let gw = spawn(router(gateway(routes, &[("mock", "sk-real")]))).await;
    let cap = mint(&gw, "mock", 60, None).await;

    let resp = reqwest::Client::new()
        .post(format!("{gw}/p/mock/v1/messages?beta=1"))
        .header("x-api-key", &cap)
        .header("content-type", "application/json")
        .body(r#"{"model":"x"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["method"], "POST");
    assert_eq!(v["path"], "/v1/messages");
    assert_eq!(v["query"], "beta=1");
    assert_eq!(v["body"], r#"{"model":"x"}"#);
}

#[tokio::test]
async fn minting_requires_the_admin_token() {
    let upstream = spawn(Router::new().fallback(echo)).await;
    let mut routes = HashMap::new();
    routes.insert("mock".to_string(), route(&upstream, "x-api-key"));
    let gw = spawn(router(gateway(routes, &[("mock", "sk-real")]))).await;

    let resp = reqwest::Client::new()
        .post(format!("{gw}/tokens"))
        .json(&json!({"attempt_id": "a", "route": "mock", "ttl_secs": 60}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    let unknown = reqwest::Client::new()
        .post(format!("{gw}/tokens"))
        .bearer_auth("admin-secret")
        .json(&json!({"attempt_id": "a", "route": "nope", "ttl_secs": 60}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        unknown.status(),
        404,
        "a token was minted for an unknown route"
    );
}

/// The gateway's own error paths are the ones that could accidentally print the
/// credential. Point a route at a closed port and read what comes back.
#[tokio::test]
async fn an_upstream_failure_does_not_echo_the_key() {
    let dead = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        format!("http://{a}")
    };
    let mut routes = HashMap::new();
    routes.insert("mock".to_string(), route(&dead, "x-api-key"));
    let gw = spawn(router(gateway(routes, &[("mock", "sk-very-secret")]))).await;
    let cap = mint(&gw, "mock", 60, None).await;

    let resp = reqwest::Client::new()
        .get(format!("{gw}/p/mock/echo"))
        .header("x-api-key", &cap)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let text = resp.text().await.unwrap();
    assert!(
        !text.contains("sk-very-secret"),
        "the error leaked the key: {text}"
    );
}
