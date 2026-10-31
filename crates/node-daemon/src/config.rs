//! Node daemon configuration: env parsing, defaults, adapter specs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use crate::outbox::CompletionOutbox;
use crate::sandbox;
use agentgrid_common::policy::AutonomyLevel;

/// How an adapter is driven: a legacy wrapper binary (stdout-parsed) or an
/// ACP-speaking agent (JSON-RPC 2.0 over stdio). Both coexist in the registry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdapterProtocol {
    Wrapper,
    Acp,
}

impl AdapterProtocol {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "acp" => AdapterProtocol::Acp,
            _ => AdapterProtocol::Wrapper,
        }
    }
}

/// Node↔CP transport selection (plan 0.3 2.3 / ADR 0009). `Auto` prefers the
/// WebSocket control channel and falls back to long polling when the WS
/// connection repeatedly fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Poll,
    Ws,
    Auto,
}

impl Transport {
    pub fn parse(v: Option<String>) -> Self {
        match v
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref()
        {
            Some("poll") => Transport::Poll,
            Some("ws") => Transport::Ws,
            _ => Transport::Auto,
        }
    }
}

#[derive(Clone, Debug)]
pub struct AdapterSpec {
    pub id: String,
    pub protocol: AdapterProtocol,
}

/// Parse `AGENTGRID_ADAPTERS=mock,claude,opencode:acp` into specs. An entry
/// with no `:protocol` suffix defaults to `Wrapper` (backward compatible).
pub fn parse_adapters(s: &str) -> Vec<AdapterSpec> {
    s.split(',')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once(':') {
            Some((id, proto)) => AdapterSpec {
                id: id.trim().to_string(),
                protocol: AdapterProtocol::parse(proto),
            },
            None => AdapterSpec {
                id: p.to_string(),
                protocol: AdapterProtocol::Wrapper,
            },
        })
        .collect()
}

impl Config {
    /// Control-plane URL to use for the next request. HA: the currently
    /// selected replica (leader hints / rotation update it in place).
    pub fn base(&self) -> String {
        let current = self.endpoints.current();
        if current.is_empty() {
            self.server.clone()
        } else {
            current
        }
    }
}

/// SavedCredential for the running process, stashed once at startup by
/// `load_or_enroll`. Needed by code paths (error threshold, per-attempt
/// overrides) that don't take the credential through their hot loop.
static PROCESS_CRED: std::sync::RwLock<Option<SavedCredential>> = std::sync::RwLock::new(None);

pub fn stash_credential(cred: &SavedCredential) {
    *PROCESS_CRED.write().unwrap() = Some(cred.clone());
}

pub fn process_credential() -> Option<SavedCredential> {
    PROCESS_CRED.read().unwrap().clone()
}

/// Node identity persisted to disk after enrollment (never re-sent in plaintext).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SavedCredential {
    pub node_id: String,
    pub credential: String,
}

/// How many consecutive gateway errors (see [`is_gateway_status`]) one endpoint
/// may produce before the daemon moves to the next one.
///
/// Not one: a single `502` is a load balancer hiccup or a proxy blip, and
/// rotating on it would shuffle every node in the fleet on any bad minute. Not
/// ten either: at poll backoff (3 s, 6 s, …) ten failures is over a minute of a
/// node that is visibly talking to something that cannot reach the control
/// plane. Three is one "did you mean it" plus a real failure, and with the
/// counter shared by poll/heartbeat/ws it is ~10 s in poll mode, ~3 s over ws.
pub const GATEWAY_FAILURES_BEFORE_ROTATING: usize = 3;

/// The whole point of the constant is "not on the first one", so a future edit
/// that sets it to 1 must not compile rather than quietly restore the behaviour
/// this exists to avoid.
const _: () = assert!(
    GATEWAY_FAILURES_BEFORE_ROTATING > 1,
    "a single bad gateway must not move a node"
);

/// Overrides [`GATEWAY_FAILURES_BEFORE_ROTATING`] for one node.
///
/// A constant was the wrong shape: one fleet's "the relay lost its route" and
/// another's "the load balancer hiccups every minute" want different numbers,
/// and only the operator knows which. The default stays, so a node that does
/// not set this behaves exactly as before.
pub const GATEWAY_FAILURES_ENV: &str = "AGENTGRID_GATEWAY_FAILURES_BEFORE_ROTATING";

/// Resolve the rotation threshold from a raw environment value.
///
/// Anything that is not an integer **above 1** falls back to the default,
/// deliberately without an error path: a single `502` is a blip, and a node
/// that refuses to start because someone typed `AGENTGRID_GATEWAY_FAILURES_
/// BEFORE_ROTATING=1` (or `three`) would turn a typo into an outage. Refusing
/// `1` here is the same invariant the `const _: () = assert!` above enforces
/// on the default — this is where it has to be enforced at runtime instead.
pub fn parse_gateway_threshold(raw: Option<&str>) -> usize {
    match raw.map(str::trim).and_then(|v| v.parse::<usize>().ok()) {
        Some(n) if n > 1 => n,
        _ => GATEWAY_FAILURES_BEFORE_ROTATING,
    }
}

/// `502 Bad Gateway` / `504 Gateway Timeout`: the endpoint answered, and its
/// own upstream did not. This is the shape of a relay whose route to the
/// control plane died, and of any egress proxy in front of a dead control
/// plane — both look like a *healthy* endpoint from the outside, which is why
/// `is_connect()` never sees them.
pub fn is_gateway_status(code: u16) -> bool {
    code == 502 || code == 504
}

/// What [`Endpoints::note_gateway_failure`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum GatewayOutcome {
    /// Counted, still on the same endpoint. `seen` consecutive so far.
    Counted(usize),
    /// The threshold was reached; the daemon is now on this endpoint.
    Rotated(String),
    /// The threshold was reached but there is nowhere to go.
    Nowhere,
}

/// Ordered control-plane endpoints. HA deployments list every replica
/// (`AGENTGRID_SERVERS=a,b,c`); the daemon follows `X-AgentGrid-Leader`
/// hints when they name a configured host and otherwise rotates in order.
#[derive(Debug)]
pub struct Endpoints {
    urls: Vec<String>,
    current: std::sync::atomic::AtomicUsize,
    /// Consecutive gateway errors seen on the current endpoint, shared by every
    /// loop (poll, heartbeat, ws) on purpose: they are all talking to the same
    /// endpoint, so three failures spread across them mean the same thing as
    /// three in one. Cleared by any successful answer and by every move
    /// (`advance` / `follow_hint`), so it only ever counts a *run*.
    gateway_failures: std::sync::Mutex<usize>,
    /// How many of those in a row rotate. [`GATEWAY_FAILURES_BEFORE_ROTATING`]
    /// unless [`GATEWAY_FAILURES_ENV`] said otherwise.
    gateway_threshold: usize,
}

impl Endpoints {
    pub fn new(urls: Vec<String>) -> Arc<Self> {
        Self::with_threshold(urls, GATEWAY_FAILURES_BEFORE_ROTATING)
    }

    /// [`Endpoints::new`] with an explicit rotation threshold. Below 2 is
    /// raised to 2 rather than rejected: `parse_gateway_threshold` already
    /// maps nonsense to the default, and a constructor that panics on a
    /// caller's arithmetic is a worse failure than a floor.
    pub fn with_threshold(urls: Vec<String>, threshold: usize) -> Arc<Self> {
        let urls: Vec<String> = urls
            .into_iter()
            .map(|u| u.trim().trim_end_matches('/').to_string())
            .filter(|u| !u.is_empty())
            .collect();
        Arc::new(Self {
            urls,
            current: std::sync::atomic::AtomicUsize::new(0),
            gateway_failures: std::sync::Mutex::new(0),
            gateway_threshold: threshold.max(2),
        })
    }

    /// The threshold in force — the default unless the environment overrode it.
    pub fn gateway_threshold(&self) -> usize {
        self.gateway_threshold
    }

    pub fn current(&self) -> String {
        if self.urls.is_empty() {
            return String::new();
        }
        let i = self.current.load(std::sync::atomic::Ordering::Relaxed) % self.urls.len();
        self.urls[i].clone()
    }

    pub fn urls(&self) -> &[String] {
        &self.urls
    }

    /// Follow a leader hint only when it matches a configured replica: the
    /// hint comes from our CP, but a redirect to an unknown host is refused.
    pub fn follow_hint(&self, hint: Option<&str>) -> bool {
        let Some(hint) = hint.map(|h| h.trim().trim_end_matches('/')) else {
            return false;
        };
        let Some(i) = self.urls.iter().position(|u| u == hint) else {
            return false;
        };
        self.current.store(i, std::sync::atomic::Ordering::Relaxed);
        self.reset_gateway_failures();
        true
    }

    /// Advance to the next replica and return it.
    pub fn advance(&self) -> String {
        if !self.urls.is_empty() {
            self.current
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        self.reset_gateway_failures();
        self.current()
    }

    /// A successful answer: whatever went wrong before, this endpoint reaches
    /// the control plane, so the run is over.
    pub fn note_success(&self) {
        self.reset_gateway_failures();
    }

    /// Count a gateway error from the current endpoint and rotate once the
    /// threshold ([`GATEWAY_FAILURES_BEFORE_ROTATING`] by default) of them have
    /// happened in a row.
    ///
    /// A `502`/`504` never surfaces as a transport error, so the connect/timeout
    /// rotation cannot see it: without this, a node pointed at a relay that lost
    /// its own route — or at any proxy in front of a dead control plane — sits
    /// on that endpoint forever, backing off, while a working endpoint sits
    /// unused in `AGENTGRID_SERVERS`.
    ///
    /// A `Mutex`, not an atomic: counting and rotating must be one step. With
    /// `fetch_add` two loops could both see "3" and rotate twice, skipping an
    /// endpoint. The guard is never held across an `.await`.
    pub fn note_gateway_failure(&self) -> GatewayOutcome {
        if self.urls.len() < 2 {
            return GatewayOutcome::Nowhere;
        }
        let mut seen = self.gateway_failures.lock().unwrap();
        *seen += 1;
        if *seen < self.gateway_threshold {
            return GatewayOutcome::Counted(*seen);
        }
        // Clear the run before rotating: `advance` takes the same mutex, so
        // holding the guard across it would deadlock.
        *seen = 0;
        drop(seen);
        GatewayOutcome::Rotated(self.advance())
    }

    fn reset_gateway_failures(&self) {
        *self.gateway_failures.lock().unwrap() = 0;
    }
}

#[derive(Clone)]
pub struct Config {
    pub server: String,
    pub endpoints: Arc<Endpoints>,
    pub node_name: String,
    pub workspace_root: PathBuf,
    pub max_concurrency: u32,
    pub agent_version: String,
    pub adapters: Vec<AdapterSpec>,
    pub repositories: Vec<String>,
    pub heartbeat_secs: u64,
    pub enroll_token: Option<String>,
    pub credential_path: PathBuf,
    pub env_file: Option<PathBuf>,
    pub repository_root: PathBuf,
    pub secrets: Vec<String>,
    pub adapter_env: Vec<(String, String)>,
    pub sandbox: sandbox::SandboxKind,
    pub outbox_root: PathBuf,
    pub artifact_spool_root: PathBuf,
    pub max_artifact_size: u64,
    pub completion_outbox: Arc<CompletionOutbox>,
    pub autonomy: AutonomyLevel,
    pub adapter_versions: HashMap<String, Option<String>>,
    pub network_mode: String,
    pub transport: Transport,
    /// Plan 1.2 (#4): command guard config. Deny/allow CSV patterns matched
    /// against tool_call commands. Deny is the safety net; allow (when
    /// non-empty) is a quiet whitelist. See `command_guard.rs`.
    pub guard_deny: Vec<String>,
    pub guard_allow: Vec<String>,
    /// Plan 1.8 (#15): account pool for adapter provider failover. Each entry
    /// is one credential env-var (e.g. `ANTHROPIC_API_KEY`) and a list of
    /// tokens to rotate through on a 429 — extra tokens are cycling backups.
    /// Parsed from `AGENTGRID_ACCOUNTS="ENV=tok1,tok2;ENV2=tok3"`.
    /// Empty (default) = no pool, the single token in `adapter_env` is used.
    pub accounts: Vec<AccountConfig>,
    /// Competitor-gap feature (GitHub write-back): PAT used to push the agent
    /// branch, open a PR and comment on the linked issue. Never logged or
    /// forwarded to the adapter; None = write-back disabled (tasks with
    /// `github_push` emit a log event explaining the missing token).
    pub github_token: Option<String>,
    /// Egress proxy pool with failover (env override `AGENTGRID_PROXY_URLS`
    /// or CP-pushed list). Shared across poll/ws/attempts so one pool state
    /// drives every egress surface.
    pub proxies: std::sync::Arc<crate::proxy::ProxyPool>,
    /// CP-pushed adapter environment (custom endpoints/keys, e.g.
    /// ANTHROPIC_BASE_URL). Updated on every poll; local AGENTGRID_ADAPTER_ENV
    /// values win on key collision.
    pub managed_adapter_env:
        std::sync::Arc<std::sync::Mutex<Vec<agentgrid_common::AdapterEnvEntry>>>,
}

/// One credential env-var and the tokens that back it for failover rotation.
#[derive(Clone, Debug)]
pub struct AccountConfig {
    /// The env var the adapter reads as its API key (e.g. `ANTHROPIC_API_KEY`).
    pub env: String,
    /// Ordered pool of tokens; rotation picks tokens past the first on 429.
    pub tokens: Vec<String>,
}

/// Parse `AGENTGRID_ACCOUNTS` into a per-env token pool. Format:
/// `ENV1=tok1,tok2;ENV2=tok3` — `;` separates env vars, `,` separates tokens.
/// Empty / unset → no pool.
pub fn parse_accounts(s: &str) -> Vec<AccountConfig> {
    s.split(';')
        .map(|entry| entry.trim())
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let (env, rest) = entry.split_once('=')?;
            let tokens: Vec<String> = rest
                .split(',')
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
                .collect();
            if tokens.is_empty() {
                return None;
            }
            Some(AccountConfig {
                env: env.trim().to_string(),
                tokens,
            })
        })
        .collect()
}

fn split_csv(env: &str, default: &str) -> Vec<String> {
    std::env::var(env)
        .ok()
        .and_then(|v| {
            let items: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            if items.is_empty() {
                None
            } else {
                Some(items)
            }
        })
        .unwrap_or_else(|| vec![default.to_string()])
}

fn parse_env_pairs(env: &str) -> Vec<(String, String)> {
    std::env::var(env)
        .ok()
        .map(|v| {
            v.split([' ', ',', '\n'])
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .filter_map(|s| {
                    let (k, val) = s.split_once('=')?;
                    Some((k.trim().to_string(), val.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn hostname() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Stage 9.1: parse the command-policy autonomy level from an env string
/// like `l0`..`l4` (case-insensitive). Unknown / missing → default (`L2`).
fn parse_autonomy(v: Option<String>) -> AutonomyLevel {
    let Some(v) = v else {
        return AutonomyLevel::default();
    };
    serde_json::from_value(serde_json::Value::String(v.to_lowercase())).unwrap_or_default()
}

/// Endpoints plus their rotation threshold, from the pieces
/// [`config_from_env`] reads.
///
/// Split out so the env → counter wiring is testable without mutating the
/// process environment: every test in this binary shares it, and a
/// `set_var`/`remove_var` pair around a live poll loop is a race waiting for a
/// slow CI box.
fn build_endpoints(
    server: &str,
    servers: Vec<String>,
    raw_threshold: Option<&str>,
) -> Arc<Endpoints> {
    let threshold = parse_gateway_threshold(raw_threshold);
    if let Some(raw) = raw_threshold {
        // Complain only when the value was ignored: a correct override must not
        // produce noise on every boot, and a bad one must not be silent — the
        // operator set a number and got a different one.
        if raw.trim().parse::<usize>().ok() != Some(threshold) {
            tracing::warn!(
                value = raw,
                using = threshold,
                "{} must be an integer above 1; using the default",
                GATEWAY_FAILURES_ENV
            );
        }
    }
    Endpoints::with_threshold(
        if servers.is_empty() {
            vec![server.to_string()]
        } else {
            servers
        },
        threshold,
    )
}

pub fn config_from_env() -> Config {
    let data_dir =
        std::env::var("AGENTGRID_DATA_DIR").unwrap_or_else(|_| "./agentgrid-data".into());
    let server = std::env::var("AGENTGRID_SERVER")
        .unwrap_or_else(|_| "http://127.0.0.1:7800".into())
        .trim_end_matches('/')
        .to_string();
    let servers: Vec<String> = std::env::var("AGENTGRID_SERVERS")
        .ok()
        .map(|raw| raw.split(',').map(str::trim).map(str::to_string).collect())
        .unwrap_or_default();
    let endpoints = build_endpoints(
        &server,
        servers,
        std::env::var(GATEWAY_FAILURES_ENV).ok().as_deref(),
    );
    Config {
        server,
        endpoints,
        node_name: std::env::var("AGENTGRID_NODE_NAME")
            .unwrap_or_else(|_| hostname().unwrap_or_else(|| "node".into())),
        workspace_root: PathBuf::from(
            std::env::var("AGENTGRID_WORKSPACE_ROOT")
                .unwrap_or_else(|_| "./agentgrid-workspace".into()),
        ),
        max_concurrency: std::env::var("AGENTGRID_MAX_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(2),
        agent_version: std::env::var("AGENTGRID_AGENT_VERSION")
            .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").into()),
        adapters: parse_adapters(
            &std::env::var("AGENTGRID_ADAPTERS").unwrap_or_else(|_| "mock".into()),
        ),
        repositories: split_csv("AGENTGRID_REPOSITORIES", "*"),
        heartbeat_secs: std::env::var("AGENTGRID_HEARTBEAT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10),
        enroll_token: std::env::var("AGENTGRID_ENROLL_TOKEN").ok(),
        credential_path: PathBuf::from(&data_dir).join("credential.json"),
        env_file: std::env::var("AGENTGRID_ENV_FILE").ok().map(PathBuf::from),
        repository_root: PathBuf::from(
            std::env::var("AGENTGRID_REPOSITORY_ROOT")
                .unwrap_or_else(|_| "./agentgrid-repos".into()),
        ),
        secrets: split_csv("AGENTGRID_SECRETS", ""),
        adapter_env: parse_env_pairs("AGENTGRID_ADAPTER_ENV"),
        sandbox: sandbox::SandboxKind::from_env(),
        outbox_root: PathBuf::from(&data_dir).join("outbox"),
        artifact_spool_root: PathBuf::from(&data_dir).join("artifact-spool"),
        max_artifact_size: std::env::var("AGENTGRID_MAX_ARTIFACT_SIZE_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .map(|mb| mb * 1024 * 1024)
            .unwrap_or(100 * 1024 * 1024),
        completion_outbox: Arc::new({
            let dir = PathBuf::from(&data_dir).join("outbox");
            CompletionOutbox::open(&dir).unwrap_or_else(|e| {
                tracing::warn!("completion outbox open failed: {e}; events may be lost on kill");
                // Neither the data dir nor the system temp dir is writable:
                // crash durability is impossible and every other disk-writing
                // daemon subsystem is about to fail the same way. Fail fast
                // with the real cause instead of a bare unwrap backtrace.
                CompletionOutbox::open(&std::env::temp_dir()).expect(
                    "completion outbox unusable: neither the data dir nor the system temp dir opened",
                )
            })
        }),
        autonomy: parse_autonomy(std::env::var("AGENTGRID_AUTONOMY").ok()),
        adapter_versions: HashMap::new(),
        network_mode: std::env::var("AGENTGRID_NETWORK_MODE")
            .ok()
            .filter(|v| v == "none" || v == "restricted" || v == "unrestricted")
            .unwrap_or_else(|| "none".into()),
        transport: Transport::parse(std::env::var("AGENTGRID_TRANSPORT").ok()),
        guard_deny: std::env::var("AGENTGRID_GUARD_DENY")
            .ok()
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        guard_allow: std::env::var("AGENTGRID_GUARD_ALLOW")
            .ok()
            .map(|v| {
                v.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
        accounts: parse_accounts(&std::env::var("AGENTGRID_ACCOUNTS").unwrap_or_default()),
        github_token: std::env::var("AGENTGRID_GITHUB_TOKEN")
            .ok()
            .filter(|t| !t.trim().is_empty()),
        proxies: std::sync::Arc::new(crate::proxy::ProxyPool::new(split_csv(
            "AGENTGRID_PROXY_URLS",
            "",
        ))),
        managed_adapter_env: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
    }
}

/// Determine permission interception mode for an adapter spec.
pub fn adapter_permission_interception(a: &AdapterSpec) -> String {
    match a.protocol {
        AdapterProtocol::Acp => "structured".into(),
        AdapterProtocol::Wrapper => "wrapper".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_adapters_basic() {
        let specs = parse_adapters("mock,claude:acp");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].id, "mock");
        assert_eq!(specs[0].protocol, AdapterProtocol::Wrapper);
        assert_eq!(specs[1].id, "claude");
        assert_eq!(specs[1].protocol, AdapterProtocol::Acp);
    }

    #[test]
    fn parse_adapters_empty() {
        let specs = parse_adapters("");
        assert_eq!(specs.len(), 0);
    }

    #[test]
    fn parse_transport_values() {
        assert_eq!(Transport::parse(None), Transport::Auto);
        assert_eq!(Transport::parse(Some("poll".into())), Transport::Poll);
        assert_eq!(Transport::parse(Some("WS".into())), Transport::Ws);
        assert_eq!(Transport::parse(Some("garbage".into())), Transport::Auto);
    }

    #[test]
    fn parse_accounts_basic() {
        let accs = parse_accounts("ANTHROPIC_API_KEY=k1,k2;OPENAI_API_KEY=k3");
        assert_eq!(accs.len(), 2);
        assert_eq!(accs[0].env, "ANTHROPIC_API_KEY");
        assert_eq!(accs[0].tokens, vec!["k1".to_string(), "k2".to_string()]);
        assert_eq!(accs[1].env, "OPENAI_API_KEY");
        assert_eq!(accs[1].tokens, vec!["k3".to_string()]);
    }

    #[test]
    fn parse_accounts_empty_and_skips_blank() {
        assert!(parse_accounts("").is_empty());
        // No tokens after `=` → entry dropped.
        assert!(parse_accounts("FOO=").is_empty());
        // Whitespace tolerated.
        let accs = parse_accounts("  FOO = k1 , k2  ");
        assert_eq!(accs.len(), 1);
        assert_eq!(accs[0].env, "FOO");
        assert_eq!(accs[0].tokens, vec!["k1".to_string(), "k2".to_string()]);
    }

    #[test]
    fn endpoints_follow_configured_hint_and_rotate() {
        let e = Endpoints::new(vec!["http://cp-a:7800/".into(), "http://cp-b:7800".into()]);
        assert_eq!(e.current(), "http://cp-a:7800");
        // Unknown hosts are never followed (no redirect off the configured set).
        assert!(!e.follow_hint(Some("http://evil:7800")));
        assert_eq!(e.current(), "http://cp-a:7800");
        assert!(e.follow_hint(Some("http://cp-b:7800/")));
        assert_eq!(e.current(), "http://cp-b:7800");
        assert_eq!(e.advance(), "http://cp-a:7800");
        assert_eq!(e.advance(), "http://cp-b:7800");
    }

    fn pair() -> Arc<Endpoints> {
        Endpoints::new(vec![
            "http://relay-a:7900".into(),
            "http://relay-b:7900".into(),
        ])
    }

    /// The whole point of the counter: a relay that answers `502` because *its*
    /// route died is not a transport error, so nothing rotates on it. Two in a
    /// row must still not move the node — a single bad gateway is a blip.
    #[test]
    fn gateway_failures_rotate_only_after_the_threshold() {
        let e = pair();
        for n in 1..GATEWAY_FAILURES_BEFORE_ROTATING {
            assert_eq!(e.note_gateway_failure(), GatewayOutcome::Counted(n));
            assert_eq!(e.current(), "http://relay-a:7900", "rotated too early");
        }
        assert_eq!(
            e.note_gateway_failure(),
            GatewayOutcome::Rotated("http://relay-b:7900".into())
        );
        assert_eq!(e.current(), "http://relay-b:7900");
    }

    /// The threshold is a knob now, so the two things that must never be
    /// configurable are the floor (one blip moves a node) and the ability of a
    /// typo to stop the daemon from starting.
    #[test]
    fn the_gateway_threshold_is_a_knob_with_a_floor() {
        assert_eq!(
            parse_gateway_threshold(None),
            GATEWAY_FAILURES_BEFORE_ROTATING
        );
        assert_eq!(parse_gateway_threshold(Some("5")), 5);
        assert_eq!(parse_gateway_threshold(Some(" 7 ")), 7);
        assert_eq!(parse_gateway_threshold(Some("2")), 2);
        // Below 2, not a number, empty, negative: the default, never a panic.
        for bad in ["1", "0", "", "abc", "three", "-4"] {
            assert_eq!(
                parse_gateway_threshold(Some(bad)),
                GATEWAY_FAILURES_BEFORE_ROTATING,
                "value {bad:?} must fall back to the default"
            );
        }
    }

    /// A configured threshold is what the counter actually uses — the point of
    /// the change. Two endpoints, threshold 2: the second `502` rotates.
    #[test]
    fn a_configured_threshold_rotates_sooner() {
        let e = Endpoints::with_threshold(
            vec!["http://relay-a:7900".into(), "http://relay-b:7900".into()],
            2,
        );
        assert_eq!(e.gateway_threshold(), 2);
        assert_eq!(e.note_gateway_failure(), GatewayOutcome::Counted(1));
        assert_eq!(e.current(), "http://relay-a:7900");
        assert_eq!(
            e.note_gateway_failure(),
            GatewayOutcome::Rotated("http://relay-b:7900".into())
        );
    }

    /// `with_threshold(.., 1)` would restore the behaviour the counter exists
    /// to prevent, so the constructor raises it instead of trusting the caller.
    #[test]
    fn a_threshold_below_two_is_raised_not_honoured() {
        let e = Endpoints::with_threshold(vec!["http://a:1".into(), "http://b:1".into()], 1);
        assert_eq!(e.gateway_threshold(), 2);
        assert_eq!(e.note_gateway_failure(), GatewayOutcome::Counted(1));
        assert_eq!(e.current(), "http://a:1", "rotated on a single bad gateway");
        // And the default constructor still uses the constant.
        assert_eq!(
            Endpoints::new(vec!["http://a:1".into()]).gateway_threshold(),
            GATEWAY_FAILURES_BEFORE_ROTATING
        );
    }

    /// The env value has to reach the endpoints the loops actually hold. The
    /// parser and the constructor are covered above; a wiring mistake between
    /// them would leave the knob dead while every unit test stayed green.
    #[test]
    fn the_env_threshold_reaches_the_endpoints() {
        let e = build_endpoints("http://a:1", vec![], Some("2"));
        assert_eq!(e.gateway_threshold(), 2);
        assert_eq!(e.current(), "http://a:1");

        // `AGENTGRID_SERVERS` still wins over `AGENTGRID_SERVER`.
        let e = build_endpoints(
            "http://ignored:1",
            vec!["http://x:1".into(), "http://y:1".into()],
            Some("9"),
        );
        assert_eq!(e.gateway_threshold(), 9);
        assert_eq!(e.current(), "http://x:1");

        // No env: the constant, so an unconfigured node behaves exactly as it
        // did before the knob existed.
        assert_eq!(
            build_endpoints("http://a:1", vec![], None).gateway_threshold(),
            GATEWAY_FAILURES_BEFORE_ROTATING
        );
        // A value the parser refuses must not leak into the endpoint list.
        assert_eq!(
            build_endpoints("http://a:1", vec![], Some("1")).gateway_threshold(),
            GATEWAY_FAILURES_BEFORE_ROTATING
        );
    }

    /// One good answer proves the endpoint reaches the control plane, so the
    /// run is over. Without this, a node with an intermittent gateway would
    /// accumulate failures over hours and rotate on a healthy endpoint.
    #[test]
    fn a_successful_answer_clears_the_run() {
        let e = pair();
        for _ in 1..GATEWAY_FAILURES_BEFORE_ROTATING {
            assert!(matches!(
                e.note_gateway_failure(),
                GatewayOutcome::Counted(_)
            ));
        }
        e.note_success();
        // The run starts over: the next two are "1 of 3", "2 of 3" again.
        assert_eq!(e.note_gateway_failure(), GatewayOutcome::Counted(1));
        assert_eq!(e.current(), "http://relay-a:7900");
    }

    /// Moving to another endpoint ends the run: the failures belonged to the
    /// one we left, and counting them against the new one would rotate again
    /// on its first hiccup.
    #[test]
    fn moving_clears_the_run() {
        let e = pair();
        for _ in 1..GATEWAY_FAILURES_BEFORE_ROTATING {
            e.note_gateway_failure();
        }
        assert!(e.follow_hint(Some("http://relay-b:7900")));
        assert_eq!(e.note_gateway_failure(), GatewayOutcome::Counted(1));
    }

    /// With one endpoint there is nowhere to go, and pretending otherwise
    /// would report a rotation that never happened.
    #[test]
    fn gateway_failures_with_a_single_endpoint_are_not_a_rotation() {
        let e = Endpoints::new(vec!["http://only:7900".into()]);
        for _ in 0..10 {
            assert_eq!(e.note_gateway_failure(), GatewayOutcome::Nowhere);
        }
        assert_eq!(e.current(), "http://only:7900");
    }

    /// `504` is the same failure with a slower upstream, so it counts too.
    #[test]
    fn gateway_statuses_are_the_two_proxy_timeouts() {
        for code in [502u16, 504] {
            assert!(is_gateway_status(code), "{code} should count as a gateway");
        }
        // 503 is *not* one: it means "I am a standby" and carries a leader
        // hint, which is handled by following the hint, not by counting.
        for code in [500u16, 503, 404, 401, 200] {
            assert!(!is_gateway_status(code), "{code} is not a gateway error");
        }
    }
}
