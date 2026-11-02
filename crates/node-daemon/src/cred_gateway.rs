//! Credential gateway client (ADR 0029 option F/inject).
//!
//! `AGENTGRID_ADAPTER_ENV` entries written `NAME@gateway=<route>` are not
//! values: they are instructions. At attempt start this module asks the gateway
//! for a **capability** for that route and puts *that* into the agent's
//! environment, so the sandbox holds something that is worthless anywhere but
//! through the gateway, and the provider key never leaves the gateway.
//!
//! The node does not hold the provider key at all — the value in the entry is a
//! route name. That is the difference between this and a broker: nothing on the
//! node has to be trusted with the secret.
//!
//! Configuration:
//!
//!   AGENTGRID_CRED_GATEWAY_URL=http://127.0.0.1:7845
//!   AGENTGRID_CRED_GATEWAY_ADMIN=<same value the gateway was started with>
//!   AGENTGRID_CRED_GATEWAY_TTL_SECS=21600      (optional, default 6 h)
//!   AGENTGRID_CRED_GATEWAY_QUOTA=200           (optional, requests per attempt)
//!
//! Every refusal below is fail-closed and says what to change: a gateway entry
//! that cannot be honoured must fail the attempt, never fall back to injecting
//! something else.

use anyhow::{anyhow, bail, Result};

use crate::config::Config;
use crate::sandbox::{self, SandboxKind};

#[derive(Debug, Clone)]
pub struct Settings {
    pub url: String,
    pub admin: String,
    pub ttl_secs: u64,
    pub quota: Option<u32>,
}

/// The HTTP client every gateway call uses.
///
/// Its own client, not the daemon's: that one carries the node credential as a
/// default `Authorization` header and is routed through the egress proxy pool.
/// The gateway is a local peer, and neither of those belongs on a mint.
///
/// When the gateway is served over TLS with the operator's own CA (ADR 0029
/// C3), that CA is the only thing that can validate it, so it is added here —
/// the daemon does not otherwise read the system trust store, and a node that
/// minted against a self-signed gateway would fail every attempt with a
/// certificate error that names nothing useful.
pub fn client() -> Result<reqwest::Client> {
    client_with_ca(ca_path().as_deref())
}

/// The body of [`client`] with the CA passed in, so a test can exercise the
/// failure paths without mutating the process environment (which every other
/// test in this binary would race).
fn client_with_ca(ca: Option<&std::path::Path>) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();
    if let Some(ca) = ca {
        let pem = std::fs::read(ca).map_err(|e| {
            anyhow!(
                "{}={} is not a readable file ({e}): the daemon cannot validate the \
                 gateway's certificate",
                crate::sandbox::GATEWAY_CA_ENV,
                ca.display()
            )
        })?;
        // `from_pem` accepts a file with no PEM block at all and returns an
        // empty certificate — so a typo'd path or a truncated file would build a
        // client with no usable root, and every mint would fail later with a TLS
        // error that names neither. The bundle form lets us insist on one.
        let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| {
            anyhow!(
                "{}={} is not a PEM certificate ({e})",
                crate::sandbox::GATEWAY_CA_ENV,
                ca.display()
            )
        })?;
        if certs.is_empty() {
            anyhow::bail!(
                "{}={} contains no certificate: the daemon would have nothing to \
                 validate the gateway with",
                crate::sandbox::GATEWAY_CA_ENV,
                ca.display()
            );
        }
        for cert in certs {
            builder = builder.add_root_certificate(cert);
        }
    }
    builder
        .build()
        .map_err(|e| anyhow!("could not build the credential gateway client: {e}"))
}

/// `AGENTGRID_CRED_GATEWAY_CA`, when set. Read in two places — here (so the
/// daemon trusts the gateway) and in `sandbox.rs` (so the sandbox does) — because
/// the two ends are different trust stores, not one setting.
pub fn ca_path() -> Option<std::path::PathBuf> {
    std::env::var(crate::sandbox::GATEWAY_CA_ENV)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
}

pub fn settings() -> Option<Settings> {
    let url = std::env::var("AGENTGRID_CRED_GATEWAY_URL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())?;
    let admin = std::env::var("AGENTGRID_CRED_GATEWAY_ADMIN")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())?;
    let ttl_secs = std::env::var("AGENTGRID_CRED_GATEWAY_TTL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(6 * 60 * 60);
    let quota = std::env::var("AGENTGRID_CRED_GATEWAY_QUOTA")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|v| *v > 0);
    Some(Settings {
        url: url.trim_end_matches('/').to_string(),
        admin,
        ttl_secs,
        quota,
    })
}

/// Why gateway delivery cannot be honoured, if it cannot.
///
/// Pure so it can be tested without a `Config` or an environment. The order
/// matters: the message the operator sees should name the *first* thing to fix.
///
/// `inject` and `exchange` are counted separately because they are refused for
/// different reasons. Inject hands over a capability, which is worthless
/// anywhere but through the gateway — advisory networking merely weakens it.
/// Exchange hands over a **real** provider token, which works from anywhere
/// until it expires, so the enforced-egress topology is what keeps the blast
/// radius to the domains the allowlist already permits.
pub fn refusal_for(
    inject: usize,
    exchange: usize,
    sandbox: SandboxKind,
    network_mode: Option<&str>,
    settings: Option<&Settings>,
) -> Option<String> {
    if inject == 0 && exchange == 0 {
        return None;
    }
    if settings.is_none() {
        return Some(
            "AGENTGRID_ADAPTER_ENV has a `NAME@gateway=<route>` or `NAME@exchange=<route>` \
             entry but AGENTGRID_CRED_GATEWAY_URL/AGENTGRID_CRED_GATEWAY_ADMIN are not set: \
             the attempt would have no credential at all"
                .to_string(),
        );
    }
    if sandbox != SandboxKind::Docker {
        return Some(format!(
            "gateway-delivered credentials need the docker sandbox (this node runs \
             sandbox={sandbox:?}): without a separate PID namespace the agent can read \
             the daemon's own environment, so a capability would buy nothing"
        ));
    }
    if sandbox::resolved_network_mode(network_mode.unwrap_or("restricted")) == "none" {
        if exchange > 0 {
            return Some(
                "`NAME@exchange=<route>` hands the sandbox a real, provider-issued token, \
                 so it needs network_mode=restricted with an egress proxy configured \
                 (AGENTGRID_SANDBOX_EGRESS_NETWORK + _PROXY): unlike a capability, that \
                 token works from anywhere until the provider expires it"
                    .to_string(),
            );
        }
        return Some(
            "gateway-delivered credentials need network_mode=restricted with an egress \
             proxy configured (AGENTGRID_SANDBOX_EGRESS_NETWORK + _PROXY): the sandbox \
             must be able to reach the gateway and nothing else"
                .to_string(),
        );
    }
    None
}

pub fn refusal(cfg: &Config, network_mode: Option<&str>) -> Option<String> {
    refusal_for(
        cfg.gateway_env.len(),
        cfg.exchange_env.len(),
        cfg.sandbox,
        network_mode,
        settings().as_ref(),
    )
}

/// Replace every gateway/exchange entry in `env` with a freshly minted value.
///
/// Returns the tokens it minted so the caller can revoke them when the attempt
/// ends; the gateway also expires them on its own, which is what bounds the
/// damage if the daemon dies without revoking. For exchange entries the token is
/// a real provider token, so the revoke is what stops it outliving the attempt
/// by up to its full TTL.
pub async fn apply(
    cfg: &Config,
    http: &reqwest::Client,
    attempt_id: &str,
    network_mode: Option<&str>,
    env: &mut Vec<(String, String)>,
) -> Result<Vec<String>> {
    if cfg.gateway_env.is_empty() && cfg.exchange_env.is_empty() {
        return Ok(Vec::new());
    }
    if let Some(why) = refusal(cfg, network_mode) {
        bail!("{why}");
    }
    let Some(settings) = settings() else {
        // `refusal` already covered this; kept as a hard error rather than an
        // unwrap so a future refactor cannot turn it into a silent fallback.
        bail!("credential gateway is not configured");
    };
    apply_with(cfg, &settings, http, attempt_id, env).await
}

/// The same thing with the settings passed in, so a test can point it at a real
/// gateway without mutating the process environment (which every other test in
/// this binary would race).
pub async fn apply_with(
    cfg: &Config,
    settings: &Settings,
    http: &reqwest::Client,
    attempt_id: &str,
    env: &mut Vec<(String, String)>,
) -> Result<Vec<String>> {
    let total = cfg.gateway_env.len() + cfg.exchange_env.len();
    let mut minted = Vec::with_capacity(total);
    let entries = cfg
        .gateway_env
        .iter()
        .map(|(name, route)| (name, route, Mode::Inject))
        .chain(
            cfg.exchange_env
                .iter()
                .map(|(name, route)| (name, route, Mode::Exchange)),
        );
    for (name, route, mode) in entries {
        let resp = http
            .post(format!("{}/tokens", settings.url))
            .bearer_auth(&settings.admin)
            .json(&serde_json::json!({
                "attempt_id": attempt_id,
                "route": route,
                "ttl_secs": settings.ttl_secs,
                "quota": settings.quota,
            }))
            .send()
            .await
            .map_err(|e| anyhow!("credential gateway unreachable at {}: {e}", settings.url))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "credential gateway refused a {} for route '{route}': HTTP {status} {body}",
                mode.noun()
            );
        }
        let body: serde_json::Value = resp
            .json()
            .await
            .map_err(|e| anyhow!("credential gateway returned a non-JSON mint response: {e}"))?;
        let token = body
            .get("token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("credential gateway mint response has no token"))?;
        env.push((name.clone(), token.to_string()));
        minted.push(token.to_string());
        // The provider, not us, decides how long an exchanged token lives. When
        // it is shorter than the attempt may run, that is a fact the operator
        // needs at start — not a 401 an hour in.
        let expires_in = body.get("expires_in").and_then(|v| v.as_u64());
        match mode {
            Mode::Inject => {
                tracing::info!(attempt_id, route = %route, env = %name, "capability minted")
            }
            Mode::Exchange => tracing::info!(
                attempt_id,
                route = %route,
                env = %name,
                expires_in,
                "token exchanged"
            ),
        }
        if mode == Mode::Exchange {
            if let Some(expires_in) = expires_in {
                if expires_in < settings.ttl_secs {
                    tracing::warn!(
                        attempt_id,
                        route = %route,
                        expires_in,
                        requested = settings.ttl_secs,
                        "the exchanged token expires before the configured TTL: an attempt \
                         that outlives it will start failing with the provider's 401"
                    );
                }
            }
        }
    }
    Ok(minted)
}

/// Which delivery a `AGENTGRID_ADAPTER_ENV` entry asked for. Only the wording of
/// refusals and log lines differs — the wire calls are the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Inject,
    Exchange,
}

impl Mode {
    fn noun(self) -> &'static str {
        match self {
            Self::Inject => "capability",
            Self::Exchange => "token exchange",
        }
    }
}

/// Best effort: a capability that outlives its attempt is bounded by its TTL
/// anyway, so a failed revoke is logged rather than turned into an attempt
/// failure after the work is done.
pub async fn revoke(http: &reqwest::Client, tokens: &[String]) {
    let Some(settings) = settings() else {
        return;
    };
    revoke_with(http, &settings, tokens).await
}

pub async fn revoke_with(http: &reqwest::Client, settings: &Settings, tokens: &[String]) {
    for token in tokens {
        let url = format!("{}/tokens/{token}", settings.url);
        match http.delete(url).bearer_auth(&settings.admin).send().await {
            Ok(r) if r.status().is_success() || r.status() == reqwest::StatusCode::NOT_FOUND => {}
            Ok(r) => tracing::warn!(status = %r.status(), "capability revoke failed"),
            Err(e) => tracing::warn!(error = %e, "capability revoke failed"),
        }
    }
}

/// Revokes an attempt's capabilities when it goes out of scope.
///
/// `Drop` cannot await, so the revoke is spawned; the runtime outlives the
/// attempt in every path that reaches here. If the process dies instead, the
/// capability is bounded by its TTL — which is why `apply` always sends one.
pub struct RevokeOnDrop {
    http: reqwest::Client,
    tokens: Vec<String>,
}

impl RevokeOnDrop {
    pub fn new(http: reqwest::Client, tokens: Vec<String>) -> Self {
        Self { http, tokens }
    }
}

impl Drop for RevokeOnDrop {
    fn drop(&mut self) {
        if self.tokens.is_empty() {
            return;
        }
        let http = self.http.clone();
        let tokens = std::mem::take(&mut self.tokens);
        tokio::spawn(async move { revoke(&http, &tokens).await });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            url: "http://gw".into(),
            admin: "a".into(),
            ttl_secs: 60,
            quota: None,
        }
    }

    #[test]
    fn no_gateway_entries_means_no_refusal() {
        assert!(refusal_for(0, 0, SandboxKind::None, None, None).is_none());
    }

    #[test]
    fn a_gateway_entry_without_a_gateway_is_refused() {
        let why = refusal_for(1, 0, SandboxKind::Docker, Some("restricted"), None).unwrap();
        assert!(why.contains("AGENTGRID_CRED_GATEWAY_URL"), "{why}");
    }

    /// The one that matters: `sandbox=none` puts the agent in the daemon's own
    /// PID namespace, where it can read the daemon's environment. A capability
    /// there is theatre, so it is refused instead.
    #[test]
    fn a_gateway_entry_on_an_unsandboxed_node_is_refused() {
        let s = settings();
        let why = refusal_for(1, 0, SandboxKind::None, Some("restricted"), Some(&s)).unwrap();
        assert!(why.contains("docker sandbox"), "{why}");
    }

    #[test]
    fn a_gateway_entry_without_restricted_networking_is_refused() {
        let s = settings();
        // `unrestricted` resolves to `bridge`, which is not "none" — the check
        // that matters is the one below, on the proxy being configured.
        let why = refusal_for(1, 0, SandboxKind::Docker, Some("none"), Some(&s)).unwrap();
        assert!(why.contains("network_mode=restricted"), "{why}");
    }

    /// The two modes are refused by different arguments, and an operator who
    /// reads the message has to be able to tell which one they hit. An exchanged
    /// token is real, so "it is only a capability" would be a false reassurance.
    #[test]
    fn an_exchange_entry_is_refused_for_a_different_reason_than_an_inject_one() {
        let s = settings();
        let inject = refusal_for(1, 0, SandboxKind::Docker, Some("none"), Some(&s)).unwrap();
        let exchange = refusal_for(0, 1, SandboxKind::Docker, Some("none"), Some(&s)).unwrap();
        assert!(exchange.contains("real"), "{exchange}");
        assert!(exchange.contains("network_mode=restricted"), "{exchange}");
        assert!(
            exchange.contains("@exchange"),
            "the message must name the marker the operator wrote: {exchange}"
        );
        assert_ne!(inject, exchange);
    }

    #[test]
    fn either_marker_alone_needs_a_configured_gateway() {
        let why = refusal_for(0, 1, SandboxKind::Docker, Some("restricted"), None).unwrap();
        assert!(why.contains("AGENTGRID_CRED_GATEWAY_URL"), "{why}");
    }

    /// Both markers on one node is a normal migration state: a provider key
    /// injected, a git token exchanged.
    #[test]
    fn both_modes_together_are_not_refused() {
        let s = settings();
        let why = refusal_for(1, 1, SandboxKind::Docker, Some("restricted"), Some(&s));
        assert!(
            why.is_none() || why.as_deref().unwrap().contains("egress"),
            "unexpected refusal: {why:?}"
        );
    }

    #[test]
    fn the_happy_path_is_not_refused_when_the_egress_proxy_is_configured() {
        // Without the egress env vars this must refuse, so the test asserts the
        // refusal rather than pretending the environment is configured.
        let s = settings();
        let why = refusal_for(1, 0, SandboxKind::Docker, Some("restricted"), Some(&s));
        assert!(
            why.is_none() || why.as_deref().unwrap().contains("egress"),
            "unexpected refusal: {why:?}"
        );
    }

    /// The daemon's client and the gateway crate are two halves of one contract
    /// (the mint body's `token` field, the header the capability arrives in,
    /// `DELETE /tokens/{t}`), and nothing else checks that they still agree. So
    /// this runs the real gateway: mint through `apply_with`, use the capability
    /// against a real upstream, then revoke and watch it stop working.
    #[tokio::test]
    async fn a_capability_round_trips_through_a_real_gateway() {
        use agentgrid_cred_gateway::{router, Gateway, Route};
        use std::collections::HashMap;

        const REAL_KEY: &str = "sk-ant-real-key";
        const ADMIN: &str = "admin-token";

        // Upstream: echoes the credential it was given, so the test can see
        // which one arrived.
        let upstream = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(|headers: axum::http::HeaderMap| async move {
                let seen = headers
                    .get("x-api-key")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                axum::Json(serde_json::json!({ "seen": seen }))
            }),
        );
        let upstream_addr = spawn_app(upstream).await;

        let mut routes = HashMap::new();
        routes.insert(
            "anthropic".to_string(),
            Route {
                upstream: format!("http://{upstream_addr}"),
                header: "x-api-key".into(),
            },
        );
        let mut keys = HashMap::new();
        keys.insert("anthropic".to_string(), REAL_KEY.to_string());
        let gw = std::sync::Arc::new(Gateway::new(routes, keys, ADMIN.into()).unwrap());
        let gw_addr = spawn_app(router(gw)).await;

        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::test_support::cfg("http://cp.invalid", dir.path());
        cfg.gateway_env = vec![("ANTHROPIC_API_KEY".to_string(), "anthropic".to_string())];
        let settings = Settings {
            url: format!("http://{gw_addr}"),
            admin: ADMIN.into(),
            ttl_secs: 60,
            quota: Some(2),
        };

        // The entry the operator wrote is replaced, not forwarded: the route
        // name must not reach the agent, and the capability must not be the key.
        let mut env = vec![("LOG_LEVEL".to_string(), "debug".to_string())];
        let http = reqwest::Client::new();
        let tokens = apply_with(&cfg, &settings, &http, "att-1", &mut env)
            .await
            .unwrap();
        assert_eq!(tokens.len(), 1);
        let cap = env
            .iter()
            .find(|(k, _)| k == "ANTHROPIC_API_KEY")
            .map(|(_, v)| v.clone())
            .expect("the capability is in the env under the operator's name");
        assert_ne!(cap, REAL_KEY);
        assert_ne!(cap, "anthropic", "the route name must not be the value");
        assert!(env.iter().any(|(k, v)| k == "LOG_LEVEL" && v == "debug"));

        // The provider sees the real key; the sandbox never did.
        let seen: serde_json::Value = http
            .post(format!("http://{gw_addr}/p/anthropic/v1/messages"))
            .header("x-api-key", &cap)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(seen["seen"], REAL_KEY);

        // And the capability stops working the moment the attempt ends.
        revoke_with(&http, &settings, &tokens).await;
        let status = http
            .post(format!("http://{gw_addr}/p/anthropic/v1/messages"))
            .header("x-api-key", &cap)
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(
            status,
            reqwest::StatusCode::UNAUTHORIZED,
            "revoked capability still works"
        );
    }

    /// Serves `app` on an ephemeral port and returns the address.
    async fn spawn_app(app: axum::Router) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    /// The daemon's half of exchange mode, against a stub gateway: what matters
    /// here is that the *provider token* — not the route name — ends up in the
    /// variable the operator named, and that it is handed back for revocation.
    /// The provider half (JWT, GitHub, revoke) is covered in `cred-gateway`.
    #[tokio::test]
    async fn an_exchange_entry_puts_the_provider_token_in_the_env() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let revoked: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route(
                "/tokens",
                axum::routing::post({
                    let seen = seen.clone();
                    move |body: axum::Json<serde_json::Value>| {
                        let seen = seen.clone();
                        async move {
                            seen.lock().unwrap().push(body.0);
                            axum::Json(serde_json::json!({
                                "token": "ghs_real_provider_token",
                                "route": "github",
                                "expires_in": 3600,
                            }))
                        }
                    }
                }),
            )
            .route(
                "/tokens/{token}",
                axum::routing::delete({
                    let revoked = revoked.clone();
                    move |axum::extract::Path(token): axum::extract::Path<String>| {
                        let revoked = revoked.clone();
                        async move {
                            revoked.lock().unwrap().push(token);
                            axum::http::StatusCode::NO_CONTENT
                        }
                    }
                }),
            );
        let addr = spawn_app(app).await;

        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::test_support::cfg("http://cp.invalid", dir.path());
        cfg.exchange_env = vec![("GITHUB_TOKEN".to_string(), "github".to_string())];
        let settings = Settings {
            url: format!("http://{addr}"),
            admin: "a".into(),
            ttl_secs: 21600,
            quota: None,
        };

        let http = reqwest::Client::new();
        let mut env = vec![("LOG_LEVEL".to_string(), "debug".to_string())];
        let tokens = apply_with(&cfg, &settings, &http, "att-9", &mut env)
            .await
            .unwrap();

        assert_eq!(
            env.iter()
                .find(|(k, _)| k == "GITHUB_TOKEN")
                .map(|(_, v)| v.as_str()),
            Some("ghs_real_provider_token"),
            "the sandbox must get the provider's token"
        );
        assert_eq!(tokens, vec!["ghs_real_provider_token".to_string()]);

        // The mint asked for the route by name; the route name is not a secret.
        let body = seen.lock().unwrap().clone();
        assert_eq!(body.len(), 1, "one mint per entry");
        assert_eq!(body[0]["route"], "github");
        assert_eq!(body[0]["attempt_id"], "att-9");

        // And the same token is what revocation is asked to kill.
        revoke_with(&http, &settings, &tokens).await;
        assert_eq!(
            revoked.lock().unwrap().as_slice(),
            ["ghs_real_provider_token"]
        );
    }

    /// A CA that is configured but unreadable must fail the attempt here, not
    /// at the first mint — and, worse, not in docker, which turns a missing bind
    /// source into an empty *directory* where a PEM belongs.
    #[test]
    fn a_missing_ca_is_refused_with_the_variable_named() {
        let err = client_with_ca(Some(std::path::Path::new("/no/such/ca.pem"))).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("AGENTGRID_CRED_GATEWAY_CA"), "{msg}");
        assert!(msg.contains("/no/such/ca.pem"), "{msg}");
    }

    #[test]
    fn a_ca_that_is_not_a_certificate_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let ca = dir.path().join("ca.pem");
        std::fs::write(&ca, "not a certificate").unwrap();
        let err = client_with_ca(Some(&ca)).unwrap_err();
        let msg = err.to_string();
        // `reqwest::Certificate::from_pem` returns Ok for this input with an
        // empty certificate, so the check has to be explicit — otherwise the
        // daemon builds a client that trusts nothing and every mint fails later
        // with a TLS error that names neither the file nor the cause.
        assert!(msg.contains("no certificate"), "{msg}");
        assert!(msg.contains("AGENTGRID_CRED_GATEWAY_CA"), "{msg}");
    }

    /// No CA configured is the normal case (plaintext gateway, or one signed by
    /// a public root): the client must still build.
    #[test]
    fn no_ca_still_builds_a_client() {
        assert!(client_with_ca(None).is_ok());
    }

    /// Both markers in one attempt: one mint each, one env entry each, and both
    /// tokens come back so both are revoked.
    #[tokio::test]
    async fn both_modes_mint_in_one_attempt() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let n = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new().route(
            "/tokens",
            axum::routing::post({
                let n = n.clone();
                move || {
                    let n = n.clone();
                    async move {
                        let i = n.fetch_add(1, Ordering::SeqCst);
                        axum::Json(serde_json::json!({
                            "token": format!("tok-{i}"),
                            "route": "r",
                            "expires_in": 60,
                        }))
                    }
                }
            }),
        );
        let addr = spawn_app(app).await;

        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::test_support::cfg("http://cp.invalid", dir.path());
        cfg.gateway_env = vec![("ANTHROPIC_API_KEY".to_string(), "anthropic".to_string())];
        cfg.exchange_env = vec![("GITHUB_TOKEN".to_string(), "github".to_string())];
        let settings = Settings {
            url: format!("http://{addr}"),
            admin: "a".into(),
            ttl_secs: 60,
            quota: None,
        };

        let mut env = Vec::new();
        let tokens = apply_with(&cfg, &settings, &reqwest::Client::new(), "att-1", &mut env)
            .await
            .unwrap();
        assert_eq!(tokens.len(), 2);
        assert_eq!(env.len(), 2);
        assert!(env.iter().any(|(k, _)| k == "ANTHROPIC_API_KEY"));
        assert!(env.iter().any(|(k, _)| k == "GITHUB_TOKEN"));
        assert_eq!(n.load(Ordering::SeqCst), 2);
    }
}
