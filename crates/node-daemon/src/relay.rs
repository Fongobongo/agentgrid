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
}

/// Default `AGENTGRID_RELAY_MAX_HOPS`: enough for the chain an operator
/// actually builds (C → B → A → control plane is three), small enough that a
/// loop is cut in milliseconds rather than by a timeout.
pub const DEFAULT_MAX_HOPS: u32 = 3;

impl RelayConfig {
    /// Parse `AGENTGRID_RELAY_LISTEN` / `AGENTGRID_RELAY_ALLOW` values.
    ///
    /// Split out from [`from_env`] so it can be tested without mutating the
    /// process environment (which races across tests).
    /// Parse `AGENTGRID_RELAY_LISTEN` / `AGENTGRID_RELAY_ALLOW` /
    /// `AGENTGRID_RELAY_MAX_HOPS` values.
    ///
    /// Split out from [`from_env`] so it can be tested without mutating the
    /// process environment (which races across tests). A value below 1 is
    /// refused rather than clamped: `0` would mean "refuse every request",
    /// which is a relay that is listening and useless, and the operator would
    /// be looking at a `508` storm to find that out.
    pub fn parse(
        listen: Option<&str>,
        allow: Option<&str>,
        max_hops: Option<&str>,
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
            return Ok(None);
        };
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
        Ok(Some(Self {
            listen,
            allow,
            max_hops,
        }))
    }

    pub fn from_env() -> Result<Option<Self>> {
        let listen = std::env::var("AGENTGRID_RELAY_LISTEN").ok();
        let allow = std::env::var("AGENTGRID_RELAY_ALLOW").ok();
        let max_hops = std::env::var("AGENTGRID_RELAY_MAX_HOPS").ok();
        Self::parse(listen.as_deref(), allow.as_deref(), max_hops.as_deref())
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
        assert!(RelayConfig::parse(None, None, None).unwrap().is_none());
        assert!(RelayConfig::parse(Some("  "), None, None)
            .unwrap()
            .is_none());
    }

    #[test]
    fn allow_without_listen_is_a_loud_mistake() {
        let err = RelayConfig::parse(None, Some("10.0.0.0/8"), None).unwrap_err();
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
        let err = RelayConfig::parse(None, None, Some("5")).unwrap_err();
        assert!(
            format!("{err}").contains("AGENTGRID_RELAY_LISTEN"),
            "the message must name the missing variable: {err}"
        );
    }

    #[test]
    fn listen_parses_with_and_without_an_allowlist() {
        let r = RelayConfig::parse(Some("0.0.0.0:7900"), None, None)
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
        )
        .unwrap()
        .expect("relay");
        assert_eq!(r.allow.len(), 2);
        assert_eq!(r.max_hops, 5);
    }

    #[test]
    fn zero_hops_is_refused_not_clamped() {
        // 0 would mean "508 for every request" — a relay that listens and is
        // useless, which the operator would diagnose from a 508 storm.
        let err = RelayConfig::parse(Some("0.0.0.0:7900"), None, Some("0")).unwrap_err();
        assert!(
            format!("{err}").contains("508"),
            "the message must say what 0 would do: {err}"
        );
    }

    #[test]
    fn a_nonsense_hop_limit_is_refused() {
        for bad in ["-1", "three", "1.5", " "] {
            let r = RelayConfig::parse(Some("0.0.0.0:7900"), None, Some(bad));
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
        assert!(RelayConfig::parse(Some("7900"), None, None).is_err());
        assert!(RelayConfig::parse(Some("localhost:7900"), None, None).is_err());
        assert!(RelayConfig::parse(Some("127.0.0.1:7900"), Some("nope"), None).is_err());
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
}
