//! Peer relay: a node that can reach the control plane carries a peer's
//! traffic.
//!
//! The topology this exists for is the one where two hosts cannot talk
//! directly but a third one can reach both: a node behind a firewall, a NAT or
//! a segment that has no route to the control plane, while a sibling node in
//! the same rack has one. Installing a forwarder on the sibling was the
//! answer; this makes the sibling — which is already running our software —
//! the forwarder.
//!
//! `AGENTGRID_RELAY_LISTEN=0.0.0.0:7900` on node A starts an HTTP/SSE/WS
//! forwarder that sends everything to A's *current* control-plane endpoint.
//! Node B then points at it instead of at the control plane:
//!
//! ```text
//! # node B — knows only A, never the control plane
//! AGENTGRID_SERVER=http://node-a.internal:7900
//! ```
//!
//! Two properties are worth stating plainly, because both are easy to get
//! wrong:
//!
//! - **No cluster token and no discovery.** The standalone `agentgrid-cp-edge`
//!   finds the leader by asking `/v1/ha/status` with the cluster token. A node
//!   has no cluster token, but it does not need one: its transport already
//!   follows `X-AgentGrid-Leader` hints and updates its endpoint list, so the
//!   relay simply asks the daemon where the control plane is *now*. A leader
//!   move is picked up on the next request.
//! - **Not an authorisation bypass.** The relay forwards `Authorization`
//!   verbatim, so the control plane still authenticates the peer's own node
//!   credential. What it *does* bypass is network segmentation — that is the
//!   whole point — which is why it is opt-in per node and why
//!   `AGENTGRID_RELAY_ALLOW` exists.

use std::net::SocketAddr;

use anyhow::{Context, Result};

use crate::config::Config;

/// Relay settings, or `None` when the operator did not ask for a relay.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    pub listen: SocketAddr,
    /// CIDR prefixes allowed to use this relay. Empty = any source; the
    /// daemon warns about that at startup because a relay widens who can
    /// reach the control plane.
    pub allow: Vec<agentgrid_edge::Cidr>,
    /// How many relays a request may cross before one of them answers `508`.
    ///
    /// Relays chain without any extra configuration — node C points at node
    /// B's relay and B is itself pointed at A's — which is convenient and is
    /// also how two operators can accidentally build a loop. The static
    /// self-loop check only sees a direct self-reference, so the chain needs a
    /// counter that travels with the request.
    pub max_hops: u32,
    /// The URL other nodes should dial, when one can be stated. See
    /// [`announced_url`].
    pub announce: Option<String>,
}

/// Default `AGENTGRID_RELAY_MAX_HOPS`: enough for the chain an operator
/// actually builds (C → B → A → control plane is three), small enough that a
/// loop is cut in milliseconds rather than by a timeout.
pub const DEFAULT_MAX_HOPS: u32 = 3;

/// `AGENTGRID_RELAY_ADVERTISE` — the address peers should dial.
pub const ADVERTISE_ENV: &str = "AGENTGRID_RELAY_ADVERTISE";

/// The announcement the heartbeat carries, read from the environment.
///
/// Infallible on purpose: the heartbeat must not be the place a configuration
/// error surfaces (it would repeat every ten seconds), and [`RelayConfig`]
/// already refuses to start the daemon on a malformed value. `None` here means
/// "nothing to announce", which is what a non-relaying node sends.
pub fn announce_from_env() -> Option<String> {
    let listen = std::env::var("AGENTGRID_RELAY_LISTEN").ok();
    let advertise = std::env::var(ADVERTISE_ENV).ok();
    announced_url(listen.as_deref(), advertise.as_deref())
        .ok()
        .flatten()
}

/// Where other nodes should dial this node's relay.
///
/// `AGENTGRID_RELAY_LISTEN=0.0.0.0:7900` says "listen everywhere", which is not
/// an address anyone can dial. The control plane needs the *dialable* one — it
/// is what `ag nodes add --via` hands to the joining host — and only the
/// operator knows it, so it is stated separately:
///
/// ```text
/// AGENTGRID_RELAY_LISTEN=0.0.0.0:7900
/// AGENTGRID_RELAY_ADVERTISE=http://node-a.internal:7900
/// ```
///
/// With a concrete listen address the advertise value is redundant and is
/// derived. With `0.0.0.0` and no advertise value nothing is announced: a
/// guessed hostname fails on the *other* machine, which is the worst possible
/// place to discover a wrong address — the joining host has no shell there and
/// the operator has no control-plane view of it yet.
pub fn announced_url(listen: Option<&str>, advertise: Option<&str>) -> Result<Option<String>> {
    let listen = listen.map(str::trim).filter(|s| !s.is_empty());
    let advertise = advertise.map(str::trim).filter(|s| !s.is_empty());
    let Some(listen) = listen else {
        if advertise.is_some() {
            anyhow::bail!(
                "AGENTGRID_RELAY_ADVERTISE is set but AGENTGRID_RELAY_LISTEN is not — \
                 this node does not relay, so there is nothing to announce"
            );
        }
        return Ok(None);
    };
    if let Some(raw) = advertise {
        return Ok(Some(normalise_advertise(raw)?));
    }
    let addr: SocketAddr = listen.parse().with_context(|| {
        format!("AGENTGRID_RELAY_LISTEN={listen:?} is not an address:port pair")
    })?;
    if addr.ip().is_unspecified() {
        // Listening on every interface. Say so once at the caller (which knows
        // whether it is startup or a heartbeat) and announce nothing.
        return Ok(None);
    }
    // `SocketAddr`'s Display brackets an IPv6 address, which is exactly what a
    // URL authority needs.
    Ok(Some(format!("http://{addr}")))
}

/// `host:port` → `http://host:port`; a path or a non-HTTP scheme is refused.
///
/// A path would be silently swallowed: every use of this URL appends
/// `/v1/node/join.sh` (or a bundle path) to it, so `http://a:7900/relay` would
/// become `http://a:7900/relay/v1/node/join.sh` and 404 on a host that cannot
/// ask anyone why. Refusing is louder and cheaper than debugging that.
///
/// Parsed by hand rather than with a URL crate: this is the only place in the
/// workspace that needs one, and the grammar it accepts is deliberately tiny —
/// scheme, host, optional port, nothing else.
fn normalise_advertise(raw: &str) -> Result<String> {
    let raw = raw.trim();
    // A single trailing slash is the same address written differently, and the
    // store trims it anyway; two would be a path.
    let body = raw.strip_suffix('/').unwrap_or(raw);
    let (scheme, authority) = match body.split_once("://") {
        Some((s, rest)) => (s, rest),
        // Bare `host:port`, which is what an operator copies out of `ip a`.
        None => ("http", body),
    };
    if scheme != "http" && scheme != "https" {
        anyhow::bail!(
            "AGENTGRID_RELAY_ADVERTISE={raw:?} must be http:// or https:// — a relay speaks \
             plain HTTP and the joining host has no way to negotiate anything else"
        );
    }
    if authority.contains('/') {
        anyhow::bail!(
            "AGENTGRID_RELAY_ADVERTISE={raw:?} must not carry a path: every URL built from it \
             appends its own (/v1/node/join.sh, /v1/node/bundle), so a path here breaks both"
        );
    }
    // `host` or `host:port`, where the port has to be a port. A bare `http://`
    // lands here as `authority == "http:"` (the scheme's own colon), which the
    // port parse refuses — otherwise the most obvious typo would be accepted as
    // the host "http".
    let host = match authority.rsplit_once(':') {
        Some((h, p)) => {
            p.parse::<u16>().with_context(|| {
                format!(
                    "AGENTGRID_RELAY_ADVERTISE={raw:?} has a port that is not a number \
                     (write http://host:7900, and bracket an IPv6 address)"
                )
            })?;
            h
        }
        None => authority,
    };
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if bare.is_empty() || bare.contains(':') || bare.contains(' ') {
        anyhow::bail!(
            "AGENTGRID_RELAY_ADVERTISE={raw:?} has no usable host — write http://host:7900 \
             (bracket an IPv6 address: http://[fd00::1]:7900)"
        );
    }
    Ok(format!("{scheme}://{authority}"))
}

impl RelayConfig {
    /// Parse the `AGENTGRID_RELAY_*` values.
    ///
    /// Split out from [`from_env`] so it can be tested without mutating the
    /// process environment (which races across tests). A hop limit below 1 is
    /// refused rather than clamped: `0` would mean "refuse every request",
    /// which is a relay that is listening and useless, and the operator would
    /// be looking at a `508` storm to find that out. The announce address is
    /// validated here for the same reason — a relay nobody can name is a relay
    /// only its own host knows about.
    pub fn parse(
        listen: Option<&str>,
        allow: Option<&str>,
        max_hops: Option<&str>,
        advertise: Option<&str>,
    ) -> Result<Option<Self>> {
        let listen = listen.map(str::trim).filter(|s| !s.is_empty());
        let Some(listen) = listen else {
            // An allowlist without a listen address is a config mistake worth
            // saying out loud: the operator expected a relay to exist.
            if allow.map(str::trim).is_some_and(|a| !a.is_empty()) {
                anyhow::bail!(
                    "AGENTGRID_RELAY_ALLOW is set but AGENTGRID_RELAY_LISTEN is not — \
                     nothing would listen"
                );
            }
            if max_hops.map(str::trim).is_some_and(|h| !h.is_empty()) {
                anyhow::bail!(
                    "AGENTGRID_RELAY_MAX_HOPS is set but AGENTGRID_RELAY_LISTEN is not — \
                     nothing would relay"
                );
            }
            if advertise.map(str::trim).is_some_and(|a| !a.is_empty()) {
                anyhow::bail!(
                    "AGENTGRID_RELAY_ADVERTISE is set but AGENTGRID_RELAY_LISTEN is not — \
                     this node does not relay, so there is nothing to announce"
                );
            }
            return Ok(None);
        };
        let raw_listen = listen;
        let listen: SocketAddr = listen.parse().with_context(|| {
            format!("AGENTGRID_RELAY_LISTEN={listen:?} is not an address:port pair")
        })?;
        let allow = agentgrid_edge::parse_allowlist(allow)
            .context("AGENTGRID_RELAY_ALLOW is not a comma-separated CIDR list")?;
        let max_hops = match max_hops.map(str::trim).filter(|s| !s.is_empty()) {
            None => DEFAULT_MAX_HOPS,
            Some(raw) => {
                let n: u32 = raw.parse().with_context(|| {
                    format!("AGENTGRID_RELAY_MAX_HOPS={raw:?} is not a positive integer")
                })?;
                if n == 0 {
                    anyhow::bail!(
                        "AGENTGRID_RELAY_MAX_HOPS=0 would refuse every request — a relay that \
                         listens and answers 508 to everything"
                    );
                }
                n
            }
        };
        // Validated (not just stored) at startup: a malformed announce address
        // is a relay that no other host can be told about, and the operator
        // would otherwise only learn it from a join command that never
        // appears.
        let announce = announced_url(Some(raw_listen), advertise)?;
        Ok(Some(Self {
            listen,
            allow,
            max_hops,
            announce,
        }))
    }

    pub fn from_env() -> Result<Option<Self>> {
        let listen = std::env::var("AGENTGRID_RELAY_LISTEN").ok();
        let allow = std::env::var("AGENTGRID_RELAY_ALLOW").ok();
        let max_hops = std::env::var("AGENTGRID_RELAY_MAX_HOPS").ok();
        let advertise = std::env::var(ADVERTISE_ENV).ok();
        Self::parse(
            listen.as_deref(),
            allow.as_deref(),
            max_hops.as_deref(),
            advertise.as_deref(),
        )
    }
}

/// Refuse a relay that forwards to itself.
///
/// A node whose own control-plane URL *is* its relay would answer every
/// forwarded request by forwarding it again, forever — a loop that looks like
/// a hang and burns the whole event loop. Checked at startup because the
/// configuration is static: the endpoint can move between replicas, but it
/// never becomes the relay by itself.
pub fn relays_to_itself(base: &str, listen: SocketAddr) -> bool {
    let Some((host, port)) = host_port(base) else {
        return false;
    };
    if port != listen.port() {
        return false;
    }
    let listen_ip = listen.ip();
    if listen_ip.is_unspecified() {
        // 0.0.0.0:7900 says nothing about which host, so the only safe reading
        // is "could be me".
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => ip == listen_ip,
        Err(_) => false,
    }
}

fn host_port(url: &str) -> Option<(String, u16)> {
    let (scheme, rest) = url.split_once("://").unwrap_or(("http", url));
    let authority = rest.rsplit('@').next()?.split('/').next()?;
    if authority.is_empty() {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse().ok()?),
        None => (authority, if scheme == "https" { 443 } else { 80 }),
    };
    Some((host.trim_matches(['[', ']']).to_string(), port))
}

/// Start the relay, or do nothing when it is not configured.
///
/// Returns once the listener is bound, so a port that is already in use stops
/// the daemon at startup instead of leaving a relay that silently never ran.
pub async fn start(cfg: &Config, relay: RelayConfig) -> Result<()> {
    let base = cfg.base();
    if relays_to_itself(&base, relay.listen) {
        anyhow::bail!(
            "AGENTGRID_RELAY_LISTEN={} would forward to itself (control plane is {base}); \
             a relay must point at a different control plane",
            relay.listen
        );
    }
    if relay.allow.is_empty() {
        tracing::warn!(
            listen = %relay.listen,
            "peer relay has no AGENTGRID_RELAY_ALLOW: any host that can reach this port \
             can reach the control plane through this node"
        );
    }
    match &relay.announce {
        Some(url) => tracing::info!(
            listen = %relay.listen,
            announce = %url,
            "peer relay announces {url}: a host that cannot reach the control plane can be \
             joined through this node (`ag nodes add --via`)"
        ),
        None => tracing::warn!(
            listen = %relay.listen,
            "peer relay announces no address: AGENTGRID_RELAY_LISTEN is 0.0.0.0 and \
             AGENTGRID_RELAY_ADVERTISE is unset, so no other host can be told where this \
             relay is — set AGENTGRID_RELAY_ADVERTISE=http://<this-host>:{}",
            relay.listen.port()
        ),
    }
    let edge = agentgrid_edge::Edge::new(agentgrid_edge::EdgeConfig {
        // Re-read per request: `base()` already moved if a leader hint
        // arrived, so the relay follows the control plane without a TTL.
        upstream: agentgrid_edge::Upstream::Fixed(std::sync::Arc::new({
            let cfg = cfg.clone();
            move || cfg.base()
        })),
        upstream_timeout: std::time::Duration::from_secs(30),
        allowed_sources: relay.allow.clone(),
        max_hops: Some(relay.max_hops),
    })?;
    let listener = tokio::net::TcpListener::bind(relay.listen)
        .await
        .with_context(|| format!("bind the peer relay on {}", relay.listen))?;
    tracing::info!(
        listen = %relay.listen,
        upstream = %base,
        allowed = relay.allow.len(),
        "peer relay listening: this node can carry other nodes' control-plane traffic"
    );
    tokio::spawn(async move {
        if let Err(e) = agentgrid_edge::serve(edge, listener).await {
            tracing::error!(error = %e, "peer relay stopped");
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_listen_means_no_relay() {
        assert!(RelayConfig::parse(None, None, None, None)
            .unwrap()
            .is_none());
        assert!(RelayConfig::parse(Some("  "), None, None, None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn allow_without_listen_is_a_loud_mistake() {
        let err = RelayConfig::parse(None, Some("10.0.0.0/8"), None, None).unwrap_err();
        assert!(
            format!("{err}").contains("AGENTGRID_RELAY_LISTEN"),
            "the message must name the missing variable: {err}"
        );
    }

    #[test]
    fn max_hops_without_listen_is_a_loud_mistake() {
        // Same reasoning as the allowlist: the operator configured a relay
        // limit, so a daemon that starts with no relay is not what they asked
        // for and must not be silently accepted.
        let err = RelayConfig::parse(None, None, Some("5"), None).unwrap_err();
        assert!(
            format!("{err}").contains("AGENTGRID_RELAY_LISTEN"),
            "the message must name the missing variable: {err}"
        );
    }

    #[test]
    fn listen_parses_with_and_without_an_allowlist() {
        let r = RelayConfig::parse(Some("0.0.0.0:7900"), None, None, None)
            .unwrap()
            .expect("relay");
        assert_eq!(r.listen.port(), 7900);
        assert!(r.allow.is_empty(), "no list = any source, caller warns");
        assert_eq!(
            r.max_hops, DEFAULT_MAX_HOPS,
            "an unset limit must still bound a loop"
        );

        let r = RelayConfig::parse(
            Some("10.0.0.5:7900"),
            Some("10.0.0.0/8, 192.168.1.7"),
            Some("5"),
            None,
        )
        .unwrap()
        .expect("relay");
        assert_eq!(r.allow.len(), 2);
        assert_eq!(r.max_hops, 5);
        assert_eq!(
            r.announce.as_deref(),
            Some("http://10.0.0.5:7900"),
            "a concrete listen address is dialable, so it is announced as-is"
        );
    }

    #[test]
    fn zero_hops_is_refused_not_clamped() {
        // 0 would mean "508 for every request" — a relay that listens and is
        // useless, which the operator would diagnose from a 508 storm.
        let err = RelayConfig::parse(Some("0.0.0.0:7900"), None, Some("0"), None).unwrap_err();
        assert!(
            format!("{err}").contains("508"),
            "the message must say what 0 would do: {err}"
        );
    }

    #[test]
    fn a_nonsense_hop_limit_is_refused() {
        for bad in ["-1", "three", "1.5", " "] {
            let r = RelayConfig::parse(Some("0.0.0.0:7900"), None, Some(bad), None);
            // Whitespace means "unset" and falls back to the default; anything
            // else must be an error rather than a silent default.
            if bad.trim().is_empty() {
                assert_eq!(r.unwrap().expect("relay").max_hops, DEFAULT_MAX_HOPS);
            } else {
                assert!(r.is_err(), "{bad:?} must be refused");
            }
        }
    }

    #[test]
    fn bad_addresses_are_rejected() {
        assert!(RelayConfig::parse(Some("7900"), None, None, None).is_err());
        assert!(RelayConfig::parse(Some("localhost:7900"), None, None, None).is_err());
        assert!(RelayConfig::parse(Some("127.0.0.1:7900"), Some("nope"), None, None).is_err());
    }

    #[test]
    fn a_relay_pointed_at_itself_is_refused() {
        let listen: SocketAddr = "127.0.0.1:7900".parse().unwrap();
        assert!(relays_to_itself("http://127.0.0.1:7900", listen));
        // Unspecified listen says nothing about the host, so same port is a
        // possible self-loop and must be refused.
        let any: SocketAddr = "0.0.0.0:7900".parse().unwrap();
        assert!(relays_to_itself("http://10.0.0.5:7900", any));
        // Different port or a different concrete host is fine.
        assert!(!relays_to_itself("http://127.0.0.1:7800", listen));
        let other: SocketAddr = "127.0.0.1:7900".parse().unwrap();
        assert!(!relays_to_itself("http://10.0.0.5:7900", other));
    }

    #[test]
    fn host_port_reads_scheme_credentials_and_default_ports() {
        assert_eq!(
            host_port("http://cp.internal:7800/base"),
            Some(("cp.internal".into(), 7800))
        );
        assert_eq!(
            host_port("https://user:pw@cp.internal"),
            Some(("cp.internal".into(), 443))
        );
        assert_eq!(host_port("http://[::1]:7900"), Some(("::1".into(), 7900)));
    }

    /// The announcement is what makes `ag nodes add --via` possible: without a
    /// dialable address the control plane cannot name this relay, and the
    /// operator has to hand out the address out of band — which is the state
    /// this field exists to end.
    #[test]
    fn a_concrete_listen_address_is_announced_as_is() {
        assert_eq!(
            announced_url(Some("10.0.0.5:7900"), None)
                .unwrap()
                .as_deref(),
            Some("http://10.0.0.5:7900")
        );
        // IPv6 needs the brackets a URL authority requires; `SocketAddr`'s
        // Display already writes them.
        assert_eq!(
            announced_url(Some("[fd00::1]:7900"), None)
                .unwrap()
                .as_deref(),
            Some("http://[fd00::1]:7900")
        );
    }

    /// `0.0.0.0` says "listen everywhere", which is not an address. Guessing
    /// the host's own name produces a URL that fails on the *other* machine —
    /// the one place the operator cannot see the error — so nothing is
    /// announced and `start()` warns with the variable to set.
    #[test]
    fn listening_everywhere_without_an_advertise_value_announces_nothing() {
        assert_eq!(announced_url(Some("0.0.0.0:7900"), None).unwrap(), None);
        assert_eq!(announced_url(Some("[::]:7900"), None).unwrap(), None);
        // Whitespace is "unset", not an address.
        assert_eq!(
            announced_url(Some("0.0.0.0:7900"), Some("  ")).unwrap(),
            None
        );
    }

    #[test]
    fn an_explicit_advertise_value_wins_over_the_listen_address() {
        assert_eq!(
            announced_url(Some("0.0.0.0:7900"), Some("http://node-a.internal:7900"))
                .unwrap()
                .as_deref(),
            Some("http://node-a.internal:7900")
        );
        // A bare host:port is the common case (the operator copied it from
        // `ip a`), so the scheme is filled in rather than refused.
        assert_eq!(
            announced_url(Some("0.0.0.0:7900"), Some("node-a.internal:7900"))
                .unwrap()
                .as_deref(),
            Some("http://node-a.internal:7900")
        );
    }

    /// Every URL built from the announcement appends its own path. A path here
    /// would 404 on a host that cannot ask anyone why, so it is refused at
    /// startup instead.
    #[test]
    fn an_advertise_value_with_a_path_is_refused() {
        let err = announced_url(Some("0.0.0.0:7900"), Some("http://a:7900/relay"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("path"), "the message must say why: {err}");

        for bad in ["http://", "://a:7900", "http://a:7900/relay"] {
            assert!(
                announced_url(Some("0.0.0.0:7900"), Some(bad)).is_err(),
                "{bad:?} must be refused"
            );
        }
        // A scheme that is not HTTP cannot be spoken by the joining host.
        assert!(announced_url(Some("0.0.0.0:7900"), Some("ftp://a:7900")).is_err());
        // One trailing slash is the same address written differently, not a
        // path — refusing it would fail an install over punctuation.
        assert_eq!(
            announced_url(Some("0.0.0.0:7900"), Some("https://a:7900/"))
                .unwrap()
                .as_deref(),
            Some("https://a:7900")
        );
    }

    #[test]
    fn advertising_without_relaying_is_a_loud_mistake() {
        let err = RelayConfig::parse(None, None, None, Some("http://a:7900"))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("AGENTGRID_RELAY_LISTEN"),
            "the message must name the missing variable: {err}"
        );
    }

    /// The heartbeat path must never be where a config error surfaces: it runs
    /// every ten seconds, and `RelayConfig` has already refused to start.
    #[test]
    fn announce_from_env_is_silent_without_a_listen_address() {
        // Not `set_var`: this test asserts the unset path, and the process
        // environment is shared with every test running beside it.
        if std::env::var("AGENTGRID_RELAY_LISTEN").is_err() {
            assert_eq!(announce_from_env(), None);
        }
    }
}
