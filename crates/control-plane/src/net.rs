//! Client address resolution.
//!
//! The login limiter (finding 12) has to key on *who is asking*, not only on
//! which account is being guessed at: a per-account bucket alone lets anyone
//! lock the real user out, and does nothing against spraying one password
//! across many accounts. That needs the peer address, which axum only puts in
//! the request when the server was started with connect info — so both
//! listeners (plaintext and TLS) are served through
//! `into_make_service_with_connect_info::<PeerAddr>()` and handlers read the
//! address with the [`ClientAddr`] extractor.
//!
//! `X-Forwarded-For` is honoured only when the direct peer is a configured
//! trusted proxy (`AGENTGRID_TRUSTED_PROXIES`): otherwise any client could
//! pick its own bucket per request and the limiter would be decorative.

use std::net::{IpAddr, SocketAddr};

use axum::extract::connect_info::Connected;
use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::request::Parts,
    serve::IncomingStream,
};

/// The direct peer address, wrapped so the `Connected` impls below can be
/// written here (they are local to this crate; `SocketAddr` itself is not).
#[derive(Clone, Copy, Debug)]
pub struct PeerAddr(pub SocketAddr);

impl<'a> Connected<IncomingStream<'a, tokio::net::TcpListener>> for PeerAddr {
    fn connect_info(stream: IncomingStream<'a, tokio::net::TcpListener>) -> Self {
        Self(*stream.remote_addr())
    }
}

impl<'a> Connected<IncomingStream<'a, crate::tls::TlsListener>> for PeerAddr {
    fn connect_info(stream: IncomingStream<'a, crate::tls::TlsListener>) -> Self {
        Self(*stream.remote_addr())
    }
}

/// The client IP as far as the control plane can honestly tell, or `None` when
/// the server was built without connect info (in-process tests, `oneshot`).
#[derive(Clone, Copy, Debug)]
pub struct ClientAddr(pub Option<IpAddr>);

impl<S> FromRequestParts<S> for ClientAddr
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<PeerAddr>>()
            .map(|ConnectInfo(PeerAddr(addr))| addr.ip());
        let forwarded = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok());
        Ok(ClientAddr(peer.map(|peer| {
            resolve_client(peer, forwarded, &trusted_proxies())
        })))
    }
}

/// Trusted proxies from `AGENTGRID_TRUSTED_PROXIES` (comma-separated IPs).
/// Empty by default: no proxy is trusted, so `X-Forwarded-For` is ignored and
/// the direct peer is the client.
pub fn trusted_proxies() -> Vec<IpAddr> {
    std::env::var("AGENTGRID_TRUSTED_PROXIES")
        .ok()
        .map(|v| {
            v.split(',')
                .filter_map(|s| s.trim().parse::<IpAddr>().ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Pick the address to rate-limit on.
///
/// The rightmost `X-Forwarded-For` entry is the one our trusted proxy appended
/// (it is what that proxy saw); everything to its left is client-supplied and
/// therefore worthless for limiting. The header is ignored entirely unless the
/// peer itself is trusted.
fn resolve_client(peer: IpAddr, forwarded_for: Option<&str>, trusted: &[IpAddr]) -> IpAddr {
    if !trusted.contains(&peer) {
        return peer;
    }
    forwarded_for
        .and_then(|v| v.rsplit(',').next())
        .and_then(|s| s.trim().parse::<IpAddr>().ok())
        .unwrap_or(peer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn a_forwarded_header_from_an_untrusted_peer_is_ignored() {
        // Without this the header is a free per-request bucket selector.
        let got = resolve_client(ip("203.0.113.9"), Some("1.2.3.4"), &[ip("10.0.0.1")]);
        assert_eq!(got, ip("203.0.113.9"));
    }

    #[test]
    fn a_trusted_proxy_passes_the_rightmost_forwarded_entry() {
        // "6.6.6.6" is what the client claimed; "1.2.3.4" is what the proxy
        // appended. Only the latter is trustworthy.
        let got = resolve_client(ip("10.0.0.1"), Some("6.6.6.6, 1.2.3.4"), &[ip("10.0.0.1")]);
        assert_eq!(got, ip("1.2.3.4"));
    }

    #[test]
    fn a_trusted_proxy_without_the_header_falls_back_to_the_peer() {
        assert_eq!(
            resolve_client(ip("10.0.0.1"), None, &[ip("10.0.0.1")]),
            ip("10.0.0.1")
        );
        // Garbage in the header must not silently disable limiting either.
        assert_eq!(
            resolve_client(ip("10.0.0.1"), Some("not-an-ip"), &[ip("10.0.0.1")]),
            ip("10.0.0.1")
        );
    }
}
