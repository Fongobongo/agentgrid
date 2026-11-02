//! `agentgrid-node-daemon probe` — what can *this* host reach?
//!
//! The control plane's view of the fleet (`ag nodes ls`) answers "who is
//! talking to me". It cannot answer the question that matters when a host is
//! not talking: **can this host reach the control plane, directly or through a
//! sibling?** Only a process on that host can, and the one binary that is on
//! every host is this daemon.
//!
//! So the probe is a subcommand here rather than in `ag`: it needs no
//! credential, no enrollment and no new deployment — the same binary that would
//! run as the node answers the question *before* the node exists, which is
//! exactly when the answer is needed (choosing a relay for `ag nodes add
//! --via`).
//!
//! What each check proves, and what it does not:
//!
//! - **Control plane** — `GET /v1/version` is public and unauthenticated, and
//!   its body names the control plane version and the node protocol. A `200`
//!   from something that is not AgentGrid fails the parse, so "reachable" here
//!   means "reachable *and* it is this product".
//! - **Relay** — the same request through the relay, plus the hop count the
//!   control plane echoes back (`x-agentgrid-relay-hops`). A relay that
//!   forwards stamps `1`; a `0` means the URL answered but nothing relayed it,
//!   which is the difference between "the relay works" and "something is
//!   listening there". A plain HTTP proxy or an L4 balancer lands in that case:
//!   they answer, and they do not stamp.
//! - **Gateway** — `GET /health` on the credential gateway. The daemon needs it
//!   for every attempt that uses a `@gateway` entry, so an unreachable gateway
//!   is a node that will fail those attempts and nothing else.
//! - **Host** — a TCP connect. Nothing more: a sibling's relay port either
//!   accepts or it does not, and that is the whole question when you are
//!   choosing a stepping stone.
//!
//! Every check is a separate target with its own timeout, so one dead relay
//! does not hide the rest.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;

/// Header a relay stamps and the control plane echoes (`crates/edge`).
const HOPS_HEADER: &str = "x-agentgrid-relay-hops";

/// Default per-check timeout. Long enough for a WAN round trip through a busy
/// relay, short enough that a black-holed address does not hold up the report.
const DEFAULT_TIMEOUT_SECS: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    ControlPlane,
    Relay,
    Gateway,
    Host,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::ControlPlane => "control plane",
            Kind::Relay => "relay",
            Kind::Gateway => "gateway",
            Kind::Host => "host",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub kind: Kind,
    pub target: String,
    pub ok: bool,
    /// One line, in the language of the check: what answered, or what failed.
    pub detail: String,
    pub millis: u128,
    /// The hop count the control plane echoed, for control-plane and relay
    /// checks. `None` = the answer carried no hop header (a control plane too
    /// old to echo, or a target that is not AgentGrid at all).
    pub hops: Option<u32>,
}

#[derive(Debug, Default)]
pub struct ProbeArgs {
    pub servers: Vec<String>,
    pub relays: Vec<String>,
    pub gateways: Vec<String>,
    pub hosts: Vec<String>,
    pub timeout: Duration,
    pub json: bool,
    /// Forward proxy to dial through, when the operator knows this host needs
    /// one. `None` = no proxy at all, **including** the environment's
    /// `HTTP_PROXY`: see [`run`].
    pub proxy: Option<String>,
}

pub const USAGE: &str = "\
usage: agentgrid-node-daemon probe [options]

  --server <url>     control plane to test (repeatable; default: AGENTGRID_SERVER
                     and AGENTGRID_SERVERS from the environment)
  --relay <url>      relay to test *through* (repeatable). A relay that forwards
                     reports hops=1; a URL that answers with hops=0 is listening
                     but is not relaying
  --gateway <url>    credential gateway to test (repeatable; default:
                     AGENTGRID_CRED_GATEWAY_URL)
  --host <host:port> raw TCP reachability of a host (repeatable) — a sibling's
                     relay port, an SSH endpoint, anything
  --proxy <url>      dial through this forward proxy. By default the probe
                     ignores HTTP_PROXY/HTTPS_PROXY: the question is what *this
                     host* can reach, and a proxy answers a different one
  --timeout <secs>   per-check timeout (default 5)
  --json             machine-readable output
  -h, --help         this text

Exit code is 1 when any check fails: a probe that reports a dead control plane
and exits 0 is a probe nobody can put in a script.
";

/// Parse the arguments after `probe`.
///
/// Returns `Ok(None)` for `--help`, which the caller prints and exits on. Hand
/// rolled rather than clap: this crate has no argument parser, and adding one
/// for a diagnostic subcommand would be the only CLI surface in the daemon.
pub fn parse_args(args: &[String]) -> Result<Option<ProbeArgs>> {
    let mut out = ProbeArgs {
        timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        ..Default::default()
    };
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut take = |what: &str| -> Result<String> {
            it.next()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
                .with_context(|| format!("{what} needs a value"))
        };
        match arg.as_str() {
            "-h" | "--help" => return Ok(None),
            "--json" => out.json = true,
            "--server" => out.servers.push(take("--server")?),
            "--relay" => out.relays.push(take("--relay")?),
            "--gateway" => out.gateways.push(take("--gateway")?),
            "--host" => out.hosts.push(take("--host")?),
            "--proxy" => out.proxy = Some(take("--proxy")?),
            "--timeout" => {
                let raw = take("--timeout")?;
                let secs: u64 = raw
                    .parse()
                    .with_context(|| format!("--timeout={raw:?} is not a number of seconds"))?;
                if secs == 0 {
                    anyhow::bail!("--timeout=0 would fail every check before it started");
                }
                out.timeout = Duration::from_secs(secs);
            }
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }
    Ok(Some(out))
}

/// Fill in the targets the environment already knows about.
///
/// Split from the parsing so a test can assert the defaults without mutating
/// the process environment, which every other test in this binary shares.
pub fn with_env_defaults(mut args: ProbeArgs, env: &EnvSnapshot) -> ProbeArgs {
    if args.servers.is_empty() {
        if let Some(list) = env.servers.as_ref() {
            args.servers.extend(list.iter().cloned());
        }
    }
    if args.gateways.is_empty() {
        if let Some(gw) = env.gateway.as_deref() {
            args.gateways.push(gw.to_string());
        }
    }
    args
}

/// The environment values the probe reads, captured once.
#[derive(Debug, Default, Clone)]
pub struct EnvSnapshot {
    /// `AGENTGRID_SERVERS` (comma list) if set, else `AGENTGRID_SERVER`.
    pub servers: Option<Vec<String>>,
    pub gateway: Option<String>,
    /// `AGENTGRID_RELAY_LISTEN`, for the "this host is a relay" line.
    pub relay_listen: Option<String>,
    /// The announcement other hosts are told to dial, if any.
    pub relay_announce: Option<String>,
}

impl EnvSnapshot {
    pub fn from_env() -> Self {
        let servers = std::env::var("AGENTGRID_SERVERS")
            .ok()
            .map(|raw| {
                raw.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|v: &Vec<String>| !v.is_empty())
            .or_else(|| {
                std::env::var("AGENTGRID_SERVER")
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .map(|s| vec![s])
            });
        Self {
            servers,
            gateway: std::env::var("AGENTGRID_CRED_GATEWAY_URL")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            relay_listen: std::env::var("AGENTGRID_RELAY_LISTEN")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            relay_announce: crate::relay::announce_from_env(),
        }
    }
}

/// Run every check and return the outcomes in the order they were asked for.
pub async fn run(args: &ProbeArgs) -> Result<Vec<Outcome>> {
    // `no_proxy()` is not a detail. The daemon's HTTP client honours
    // `HTTP_PROXY`/`HTTPS_PROXY`, which is right for its own transport and
    // wrong here: with a proxy in the environment every check would measure
    // the *proxy's* path to the target, and a control plane that this host
    // cannot reach would come back `200` because the proxy can. The question
    // this command exists to answer is what this host can reach, so the
    // environment's proxy is ignored — and `--proxy` is there for the host
    // that genuinely has to go through one.
    let mut builder = reqwest::Client::builder().timeout(args.timeout).no_proxy();
    if let Some(proxy) = args.proxy.as_deref() {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy)
                .with_context(|| format!("--proxy={proxy:?} is not a proxy URL"))?,
        );
    }
    let client = builder.build().context("build the probe HTTP client")?;
    let mut out = Vec::new();
    for base in &args.servers {
        out.push(probe_control_plane(&client, base).await);
    }
    for base in &args.relays {
        out.push(probe_relay(&client, base).await);
    }
    for base in &args.gateways {
        out.push(probe_gateway(&client, base).await);
    }
    for host in &args.hosts {
        out.push(probe_host(host, args.timeout).await);
    }
    Ok(out)
}

/// `GET /v1/version` — public, and it names the product.
async fn probe_control_plane(client: &reqwest::Client, base: &str) -> Outcome {
    let base = base.trim_end_matches('/');
    let started = Instant::now();
    match client.get(format!("{base}/v1/version")).send().await {
        Err(e) => Outcome {
            kind: Kind::ControlPlane,
            target: base.to_string(),
            ok: false,
            detail: describe_error(&e),
            millis: started.elapsed().as_millis(),
            hops: None,
        },
        Ok(resp) => {
            let millis = started.elapsed().as_millis();
            let hops = hops_of(resp.headers());
            let status = resp.status();
            let body: Option<Value> = resp.json().await.ok();
            match (status.is_success(), body) {
                (true, Some(v)) if v.get("control_plane").is_some() => Outcome {
                    kind: Kind::ControlPlane,
                    target: base.to_string(),
                    ok: true,
                    detail: version_line(&v, hops),
                    millis,
                    hops,
                },
                // Something answered, and it is not (or not only) an AgentGrid
                // control plane. A reverse proxy's default page lands here, and
                // saying so is the whole value of the check.
                (true, _) => Outcome {
                    kind: Kind::ControlPlane,
                    target: base.to_string(),
                    ok: false,
                    detail: format!("answered HTTP {status} but not as an AgentGrid control plane"),
                    millis,
                    hops,
                },
                (false, _) => Outcome {
                    kind: Kind::ControlPlane,
                    target: base.to_string(),
                    ok: false,
                    detail: format!("HTTP {status} from /v1/version"),
                    millis,
                    hops,
                },
            }
        }
    }
}

/// The same request through a relay, where the hop count is the answer.
async fn probe_relay(client: &reqwest::Client, base: &str) -> Outcome {
    let mut outcome = probe_control_plane(client, base).await;
    outcome.kind = Kind::Relay;
    relay_verdict(outcome)
}

/// Turn "it answered" into "it relayed", which are different claims.
///
/// Split out from [`probe_relay`] so the rule can be tested without a server,
/// and so the one place that decides it stays one place.
fn relay_verdict(mut outcome: Outcome) -> Outcome {
    if !outcome.ok {
        return outcome;
    }
    outcome.detail = match outcome.hops {
        // The relay forwarded: the count it stamped came back.
        Some(n) if n > 0 => format!("forwards (hops={n}); {}", outcome.detail),
        // It answered, and nothing relayed the request. This is the case the
        // check exists for: a plain HTTP proxy, an L4 balancer or a stale
        // address all look healthy from the outside.
        Some(_) => "answered but stamped no hop: this URL is not relaying (a plain HTTP \
                    proxy or an L4 balancer, or the control plane itself)"
            .to_string(),
        None => "answered, but the control plane did not echo a hop count (it is older than \
                 the echo, so this check cannot tell a relay from a direct connection)"
            .to_string(),
    };
    if outcome.hops == Some(0) {
        outcome.ok = false;
    }
    outcome
}

async fn probe_gateway(client: &reqwest::Client, base: &str) -> Outcome {
    let base = base.trim_end_matches('/');
    let started = Instant::now();
    match client.get(format!("{base}/health")).send().await {
        Err(e) => Outcome {
            kind: Kind::Gateway,
            target: base.to_string(),
            ok: false,
            detail: describe_error(&e),
            millis: started.elapsed().as_millis(),
            hops: None,
        },
        Ok(resp) => {
            let millis = started.elapsed().as_millis();
            let status = resp.status();
            let body: Option<Value> = resp.json().await.ok();
            let healthy = body
                .as_ref()
                .and_then(|v| v.get("ok"))
                .and_then(|v| v.as_bool())
                == Some(true);
            Outcome {
                kind: Kind::Gateway,
                target: base.to_string(),
                ok: status.is_success() && healthy,
                detail: if status.is_success() && healthy {
                    "healthy".to_string()
                } else {
                    format!("HTTP {status}, /health did not report ok")
                },
                millis,
                hops: None,
            }
        }
    }
}

/// A bare TCP connect. The question "can this host reach that port" has no
/// application-level answer when the port may be a relay, an SSH daemon or
/// nothing at all.
async fn probe_host(target: &str, timeout: Duration) -> Outcome {
    let addr = normalise_host(target);
    let started = Instant::now();
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&addr)).await {
        Ok(Ok(_)) => Outcome {
            kind: Kind::Host,
            target: addr,
            ok: true,
            detail: "tcp connect".to_string(),
            millis: started.elapsed().as_millis(),
            hops: None,
        },
        Ok(Err(e)) => Outcome {
            kind: Kind::Host,
            target: addr,
            ok: false,
            detail: describe_io_error(&e),
            millis: started.elapsed().as_millis(),
            hops: None,
        },
        Err(_) => Outcome {
            kind: Kind::Host,
            target: addr,
            ok: false,
            detail: format!("timed out after {}s", timeout.as_secs()),
            millis: started.elapsed().as_millis(),
            hops: None,
        },
    }
}

/// `node-a.internal:7900` and `http://node-a.internal:7900` are the same target
/// for a TCP connect; the scheme and any path are dropped rather than refused,
/// because an operator pasting a relay URL from `ag nodes ls` is the expected
/// use.
fn normalise_host(target: &str) -> String {
    let t = target.trim();
    let t = t
        .strip_prefix("http://")
        .or_else(|| t.strip_prefix("https://"))
        .unwrap_or(t);
    t.split('/').next().unwrap_or(t).to_string()
}

fn hops_of(headers: &reqwest::header::HeaderMap) -> Option<u32> {
    headers
        .get(HOPS_HEADER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u32>().ok())
}

fn version_line(v: &Value, hops: Option<u32>) -> String {
    let version = v
        .get("control_plane")
        .and_then(|x| x.as_str())
        .unwrap_or("?");
    let protocol = v
        .get("node_protocol")
        .and_then(|x| x.as_str())
        .unwrap_or("?");
    match hops {
        Some(n) => format!("v{version} (node protocol v{protocol}), hops={n}"),
        None => format!("v{version} (node protocol v{protocol})"),
    }
}

/// reqwest's Display for a transport error is a sentence with the URL in it;
/// the useful part is the cause, and the cause is what an operator can act on.
fn describe_error(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "timed out".to_string();
    }
    if e.is_connect() {
        return format!("cannot connect ({})", root_cause(e));
    }
    root_cause(e)
}

fn describe_io_error(e: &std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::ConnectionRefused => "connection refused".to_string(),
        std::io::ErrorKind::TimedOut => "timed out".to_string(),
        _ => e.to_string(),
    }
}

fn root_cause(e: &reqwest::Error) -> String {
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    let mut last = e.to_string();
    while let Some(err) = cur {
        last = err.to_string();
        cur = err.source();
    }
    last
}

/// The report, as text.
///
/// Grouped by kind rather than by target: the operator is answering one
/// question per group ("can I reach the control plane", "can I reach it through
/// a sibling"), and a flat list makes them re-sort it by eye.
pub fn render_text(outcomes: &[Outcome], env: &EnvSnapshot) -> String {
    let mut s = String::new();
    s.push_str("agentgrid reach — probed from this host\n");
    if let Some(listen) = env.relay_listen.as_deref() {
        match env.relay_announce.as_deref() {
            Some(url) => s.push_str(&format!(
                "  this host relays on {listen} and announces {url}\n"
            )),
            None => s.push_str(&format!(
                "  this host relays on {listen} but announces no address (AGENTGRID_RELAY_ADVERTISE), \
                 so no other host can be told to dial it\n"
            )),
        }
    }
    if outcomes.is_empty() {
        s.push_str("\nNothing to check: pass --server, --relay, --gateway or --host, or set\n");
        s.push_str("AGENTGRID_SERVER. See `probe --help`.\n");
        return s;
    }
    for kind in [Kind::ControlPlane, Kind::Relay, Kind::Gateway, Kind::Host] {
        let group: Vec<&Outcome> = outcomes.iter().filter(|o| o.kind == kind).collect();
        if group.is_empty() {
            continue;
        }
        s.push_str(&format!("\n{}:\n", kind.label().to_uppercase()));
        let width = group.iter().map(|o| o.target.len()).max().unwrap_or(0);
        for o in group {
            s.push_str(&format!(
                "  {:<width$}  {:<4} {:>6}  {}\n",
                o.target,
                if o.ok { "ok" } else { "FAIL" },
                format!("{} ms", o.millis),
                o.detail,
                width = width
            ));
        }
    }
    let failed = outcomes.iter().filter(|o| !o.ok).count();
    s.push_str(&format!(
        "\n{} of {} check(s) failed\n",
        failed,
        outcomes.len()
    ));
    s
}

/// The report, as JSON, for a script or a bug report.
pub fn render_json(outcomes: &[Outcome], env: &EnvSnapshot) -> String {
    let checks: Vec<Value> = outcomes
        .iter()
        .map(|o| {
            serde_json::json!({
                "kind": o.kind.label(),
                "target": o.target,
                "ok": o.ok,
                "ms": o.millis,
                "hops": o.hops,
                "detail": o.detail,
            })
        })
        .collect();
    let body = serde_json::json!({
        "relay_listen": env.relay_listen,
        "relay_announce": env.relay_announce,
        "checks": checks,
        "failed": outcomes.iter().filter(|o| !o.ok).count(),
        "ok": outcomes.iter().all(|o| o.ok),
    });
    serde_json::to_string_pretty(&body).unwrap_or_else(|_| "{}".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Result<Option<ProbeArgs>> {
        let owned: Vec<String> = v.iter().map(|s| s.to_string()).collect();
        parse_args(&owned)
    }

    #[test]
    fn help_is_not_an_error() {
        assert!(args(&["--help"]).unwrap().is_none());
        assert!(args(&["-h"]).unwrap().is_none());
    }

    #[test]
    fn every_target_flag_is_repeatable() {
        let a = args(&[
            "--server",
            "http://cp:7800",
            "--relay",
            "http://a:7900",
            "--relay",
            "http://b:7900",
            "--gateway",
            "http://gw:7845",
            "--host",
            "10.0.0.5:7900",
        ])
        .unwrap()
        .unwrap();
        assert_eq!(a.servers, vec!["http://cp:7800"]);
        assert_eq!(a.relays, vec!["http://a:7900", "http://b:7900"]);
        assert_eq!(a.gateways, vec!["http://gw:7845"]);
        assert_eq!(a.hosts, vec!["10.0.0.5:7900"]);
        assert_eq!(a.timeout, Duration::from_secs(DEFAULT_TIMEOUT_SECS));
    }

    #[test]
    fn a_flag_without_a_value_is_refused_by_name() {
        let err = args(&["--relay"]).unwrap_err().to_string();
        assert!(err.contains("--relay"), "got: {err}");
        // And an unknown flag is refused rather than ignored: a typo must not
        // silently probe nothing.
        let err = args(&["--relays", "http://a:7900"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("--relays"), "got: {err}");
    }

    #[test]
    fn a_proxy_is_opt_in() {
        let a = args(&["--server", "http://cp:7800"]).unwrap().unwrap();
        assert_eq!(
            a.proxy, None,
            "the environment's proxy must not silently redirect the probe"
        );
        let a = args(&["--proxy", "http://p:3128"]).unwrap().unwrap();
        assert_eq!(a.proxy.as_deref(), Some("http://p:3128"));
    }

    #[test]
    fn a_zero_timeout_is_refused() {
        // 0 would fail every check before it started, which reads as "the
        // whole fleet is down".
        let err = args(&["--timeout", "0"]).unwrap_err().to_string();
        assert!(
            err.contains("508") || err.contains("before it started"),
            "got: {err}"
        );
    }

    /// The defaults are what makes the command usable on a node without
    /// arguments: the daemon's own configuration is already in the environment.
    #[test]
    fn the_environment_supplies_the_control_plane_and_the_gateway() {
        let env = EnvSnapshot {
            servers: Some(vec!["http://cp:7800".into()]),
            gateway: Some("http://gw:7845".into()),
            relay_listen: None,
            relay_announce: None,
        };
        let a = with_env_defaults(ProbeArgs::default(), &env);
        assert_eq!(a.servers, vec!["http://cp:7800"]);
        assert_eq!(a.gateways, vec!["http://gw:7845"]);

        // An explicit flag wins: the operator is asking about a specific path.
        let explicit = ProbeArgs {
            servers: vec!["http://other:7800".into()],
            ..Default::default()
        };
        let a = with_env_defaults(explicit, &env);
        assert_eq!(a.servers, vec!["http://other:7800"]);
    }

    #[test]
    fn a_host_target_tolerates_a_pasted_url() {
        // `ag nodes ls` prints the relay as a URL; pasting it into --host must
        // not become a DNS lookup for "http".
        assert_eq!(normalise_host("http://node-a:7900"), "node-a:7900");
        assert_eq!(normalise_host("https://node-a:7900/"), "node-a:7900");
        assert_eq!(normalise_host(" 10.0.0.5:7900 "), "10.0.0.5:7900");
    }

    /// A relay that answers but stamps nothing is the failure this check
    /// exists for: a plain HTTP proxy or an L4 balancer looks perfectly healthy
    /// from the outside and does not carry AgentGrid traffic.
    #[test]
    fn a_relay_that_stamped_no_hop_is_reported_as_not_relaying() {
        let base = Outcome {
            kind: Kind::Relay,
            target: "http://p:3128".into(),
            ok: true,
            detail: "v0.8.1 (node protocol v2)".into(),
            millis: 1,
            hops: None,
        };

        let answered = relay_verdict(Outcome {
            hops: Some(0),
            ..base.clone()
        });
        assert!(
            !answered.ok,
            "answering is not relaying: the operator would pick this as a stepping stone"
        );
        assert!(
            answered.detail.contains("not relaying"),
            "{}",
            answered.detail
        );

        let through = relay_verdict(Outcome {
            hops: Some(1),
            ..base.clone()
        });
        assert!(through.ok);
        assert!(through.detail.contains("hops=1"), "{}", through.detail);
        // The version line survives: "it forwards" and "it forwards to *our*
        // control plane" are both needed to trust the answer.
        assert!(through.detail.contains("v0.8.1"), "{}", through.detail);

        // A control plane too old to echo says so rather than guessing either
        // way — the alternative is calling every relay broken after a rollback.
        let unknown = relay_verdict(base);
        assert!(unknown.ok);
        assert!(
            unknown.detail.contains("did not echo"),
            "{}",
            unknown.detail
        );
    }

    #[test]
    fn the_report_groups_by_question_and_counts_failures() {
        let env = EnvSnapshot::default();
        let out = vec![
            Outcome {
                kind: Kind::ControlPlane,
                target: "http://cp:7800".into(),
                ok: true,
                detail: "v0.8.1 (node protocol v2), hops=0".into(),
                millis: 4,
                hops: Some(0),
            },
            Outcome {
                kind: Kind::Relay,
                target: "http://a:7900".into(),
                ok: false,
                detail: "connection refused".into(),
                millis: 1,
                hops: None,
            },
        ];
        let text = render_text(&out, &env);
        assert!(text.contains("CONTROL PLANE"));
        assert!(text.contains("RELAY"));
        assert!(text.contains("1 of 2 check(s) failed"), "{text}");
        // The order of the groups is the order of the questions, not of the
        // arguments.
        assert!(text.find("CONTROL PLANE").unwrap() < text.find("RELAY").unwrap());

        let json: Value = serde_json::from_str(&render_json(&out, &env)).unwrap();
        assert_eq!(json["failed"], 1);
        assert_eq!(json["ok"], false);
        assert_eq!(json["checks"][0]["kind"], "control plane");
    }

    #[test]
    fn an_empty_run_says_what_to_pass() {
        let text = render_text(&[], &EnvSnapshot::default());
        assert!(text.contains("--server"), "{text}");
    }

    #[test]
    fn a_local_relay_that_announces_nothing_is_called_out() {
        // The node relays, but no other host can be told where — the exact
        // state `ag nodes add --via` refuses to build a command from.
        let env = EnvSnapshot {
            relay_listen: Some("0.0.0.0:7900".into()),
            relay_announce: None,
            ..Default::default()
        };
        let text = render_text(&[], &env);
        assert!(text.contains("announces no address"), "{text}");
        assert!(text.contains("AGENTGRID_RELAY_ADVERTISE"), "{text}");
    }
}
