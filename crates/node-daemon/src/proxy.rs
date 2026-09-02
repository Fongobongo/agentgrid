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
    pub fn update_from_cp(&self, urls: Vec<String>) {
        let mut g = self.inner.lock().unwrap();
        if !g.env_urls.is_empty() {
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

    /// All configured URLs, dead or not (health checks / status reporting).
    #[allow(dead_code)]
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
        p.update_from_cp(vec!["http://x:1".into()]);
        assert_eq!(p.current().as_deref(), Some("http://x:1"));

        let env_p = ProxyPool::new(vec!["http://mine:1".into()]);
        env_p.update_from_cp(vec!["http://x:1".into()]);
        assert_eq!(env_p.current().as_deref(), Some("http://mine:1"));
    }
}
