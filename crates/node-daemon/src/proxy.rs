//! Egress proxy pool with failover.
//!
//! Sources (first wins): env `AGENTGRID_PROXY_URLS=url1,url2` (manual
//! override, never replaced) else the CP-managed list delivered in every
//! `PollResponse.proxy_urls` (global pool first, then node-scoped rows).
//!
//! Failover: `current()` returns the first non-dead URL in list order. On a
//! transport error the caller marks the URL dead (`mark_dead`); it stays out
//! of rotation for `DEAD_TTL` and then gets retried automatically. With no
//! proxies configured everything behaves as before (direct egress).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const DEAD_TTL: Duration = Duration::from_secs(300);

pub struct ProxyPool {
    inner: Mutex<Inner>,
}

struct Inner {
    env_urls: Vec<String>,
    cp_urls: Vec<String>,
    dead: HashMap<String, Instant>,
}

impl ProxyPool {
    /// `env_urls` = parsed `AGENTGRID_PROXY_URLS`. Empty = CP-managed mode.
    pub fn new(env_urls: Vec<String>) -> Self {
        // Drop empties — `split_csv` maps an unset var to one empty string,
        // which would otherwise masquerade as a configured proxy.
        let env_urls: Vec<String> = env_urls.into_iter().filter(|u| !u.is_empty()).collect();
        Self {
            inner: Mutex::new(Inner {
                env_urls,
                cp_urls: Vec::new(),
                dead: HashMap::new(),
            }),
        }
    }

    /// Replace the CP-managed list. No-op when env override is configured.
    ///
    /// `complete` says whether `urls` is the node's whole pool. Without it an
    /// empty delivery is ambiguous, and the old code resolved it as "nothing
    /// to say" (audit X-N1b: legacy polls carry `[]` on every cycle). That
    /// made removing the last proxy a no-op — the node kept egressing through
    /// a URL the operator had deleted. A control plane that sets `complete`
    /// means it: empty is then "you have none".
    pub fn update_from_cp(&self, urls: Vec<String>, complete: bool) {
        let mut g = self.inner.lock().unwrap();
        if !g.env_urls.is_empty() {
            return;
        }
        if urls.is_empty() && !complete {
            return;
        }
        // Keep liveness state for URLs that survive the update.
        g.dead.retain(|u, _| urls.contains(u));
        g.cp_urls = urls;
    }

    fn urls(g: &Inner) -> &[String] {
        if g.env_urls.is_empty() {
            &g.cp_urls
        } else {
            &g.env_urls
        }
    }

    /// First non-dead proxy URL, if any.
    pub fn current(&self) -> Option<String> {
        let g = self.inner.lock().unwrap();
        let now = Instant::now();
        Self::urls(&g).iter().find_map(|u| match g.dead.get(u) {
            Some(t) if now.duration_since(*t) < DEAD_TTL => None,
            _ => Some(u.clone()),
        })
    }

    /// Mark a URL dead (transport error). Rejoins the pool after DEAD_TTL.
    pub fn mark_dead(&self, url: &str) {
        self.inner
            .lock()
            .unwrap()
            .dead
            .insert(url.to_string(), Instant::now());
    }

    /// Revive a URL immediately (prober saw it reachable).
    pub fn revive(&self, url: &str) {
        self.inner.lock().unwrap().dead.remove(url);
    }

    /// All configured URLs, dead or not (health checks / status reporting).
    #[allow(dead_code)]
    pub fn all(&self) -> Vec<String> {
        let g = self.inner.lock().unwrap();
        Self::urls(&g).to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failover_order_and_ttl() {
        let p = ProxyPool::new(vec!["http://a:1".into(), "http://b:2".into()]);
        assert_eq!(p.current().as_deref(), Some("http://a:1"));
        p.mark_dead("http://a:1");
        assert_eq!(p.current().as_deref(), Some("http://b:2"));
        p.mark_dead("http://b:2");
        assert_eq!(p.current(), None, "all dead -> direct egress");
    }

    #[test]
    fn cp_list_updates_unless_env_override() {
        let p = ProxyPool::new(vec![]);
        p.update_from_cp(vec!["http://x:1".into()], true);
        assert_eq!(p.current().as_deref(), Some("http://x:1"));

        let env_p = ProxyPool::new(vec!["http://mine:1".into()]);
        env_p.update_from_cp(vec!["http://x:1".into()], true);
        assert_eq!(env_p.current().as_deref(), Some("http://mine:1"));
    }

    /// A legacy poll carries `[]` every cycle, so an empty *incomplete*
    /// delivery must keep the pool — otherwise a pre-0.9 control plane would
    /// wipe it on the first poll after the node learned one over WS.
    #[test]
    fn empty_incomplete_keeps_the_pool() {
        let p = ProxyPool::new(vec![]);
        p.update_from_cp(vec!["http://x:1".into()], true);
        p.update_from_cp(vec![], false);
        assert_eq!(p.current().as_deref(), Some("http://x:1"));
    }

    /// Removing the last proxy is what an empty *complete* delivery means.
    /// Before this, `ag proxy rm` of the final URL left every node egressing
    /// through a proxy the control plane no longer knew about.
    #[test]
    fn empty_complete_clears_the_pool() {
        let p = ProxyPool::new(vec![]);
        p.update_from_cp(vec!["http://x:1".into()], true);
        assert_eq!(p.current().as_deref(), Some("http://x:1"));
        p.update_from_cp(vec![], true);
        assert_eq!(
            p.current(),
            None,
            "a removed proxy must not stay in rotation"
        );
        // `current()` alone is not proof: a `mark_dead` would hide a pool that
        // still holds the URL. The pool itself must be empty.
        assert!(
            p.all().is_empty(),
            "the pool must be empty, not merely quarantined: {:?}",
            p.all()
        );
        // And it stays cleared for a later empty delivery.
        p.update_from_cp(vec![], false);
        assert!(p.all().is_empty());
    }

    /// The env override is the node admin's decision: it survives a complete
    /// (even empty) delivery from the control plane.
    #[test]
    fn env_override_survives_a_complete_empty_delivery() {
        let p = ProxyPool::new(vec!["http://mine:1".into()]);
        p.update_from_cp(vec![], true);
        assert_eq!(p.current().as_deref(), Some("http://mine:1"));
    }
}

/// Background health prober: every `interval` try a TCP connect to every
/// pool entry. Reachable => revive (clears the dead-TTL so the node returns
/// to it before the full quarantine expires); unreachable => mark the dead
/// window again. Keeps the pool honest without load on the CP.
///
/// Best-effort and deliberately shallow: a TCP handshake proves the proxy
/// host answers, not that the upstream destination is reachable.
pub async fn probe_loop(pool: std::sync::Arc<ProxyPool>, interval: Duration) {
    // Never return early on an empty pool: the CP-pushed list may arrive
    // after startup. Just sleep until there is something to probe.
    loop {
        tokio::time::sleep(interval).await;
        for url in pool.all() {
            match probe_one(&url).await {
                true => pool.revive(&url),
                false => pool.mark_dead(&url),
            }
        }
    }
}

async fn probe_one(url: &str) -> bool {
    let Some(addr) = host_port(url) else {
        return false;
    };
    tokio::time::timeout(Duration::from_secs(2), tokio::net::TcpStream::connect(addr))
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
}

fn host_port(url: &str) -> Option<String> {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = after_scheme.rsplit('@').next()?.split('/').next()?;
    if authority.is_empty() {
        None
    } else {
        Some(authority.to_string())
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;

    #[test]
    fn host_port_parses_common_forms() {
        assert_eq!(
            host_port("http://127.0.0.1:8080").unwrap(),
            "127.0.0.1:8080"
        );
        assert_eq!(
            host_port("http://user:pw@proxy.local:3128/ignored").unwrap(),
            "proxy.local:3128"
        );
        assert_eq!(
            host_port("socks5://10.0.0.5:1080").unwrap(),
            "10.0.0.5:1080"
        );
        assert!(host_port("http://").is_none());
    }

    #[tokio::test]
    async fn dead_proxy_revives_when_reachable() {
        // Bind a throwaway listener — that's a reachable TCP endpoint.
        let ln = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = ln.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}");
        let pool = std::sync::Arc::new(ProxyPool::new(vec![url.clone()]));
        pool.mark_dead(&url);
        assert_eq!(pool.current(), None);
        assert!(probe_one(&url).await);
        pool.revive(&url);
        assert_eq!(pool.current().as_deref(), Some(url.as_str()));
        drop(ln);
    }
}
