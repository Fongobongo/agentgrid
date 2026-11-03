//! Egress proxy store: CP-managed proxy lists pushed to nodes.
//!
//! Rows with `node_id IS NULL` form the global pool (used by every node);
//! rows scoped to a node are appended after the global ones so the node
//! still has the global pool as fallback.

use anyhow::Result;
use serde::Serialize;

use super::Store;

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct ProxyView {
    pub id: i64,
    /// Password redacted (finding 8). The panel needs to recognise which
    /// proxy is configured, not the credential it uses; nodes get the real
    /// URL from [`Store::proxy_urls_for`], which reads the table directly.
    pub url: String,
    pub node_id: Option<String>,
    pub created_at: String,
}

/// Replace the password in a proxy URL's userinfo with `***`.
///
/// `scheme://user:pass@host:port` is the shape that matters; a URL with no
/// userinfo, or with a bare `user@host`, is returned unchanged. The split at
/// `://` and the search for `@` stop at the first `/` so a `@` in the path
/// cannot be mistaken for the userinfo separator.
pub fn redact_proxy_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find('/').unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let Some(at) = authority.rfind('@') else {
        return url.to_string();
    };
    let userinfo = &authority[..at];
    let Some(colon) = userinfo.find(':') else {
        return url.to_string();
    };
    format!(
        "{scheme}://{}:***@{}{tail}",
        &userinfo[..colon],
        &authority[at + 1..]
    )
}

impl Store {
    /// URLs for a node: global pool first, then node-scoped rows.
    pub async fn proxy_urls_for(&self, node_id: &str) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT url FROM proxies \
             WHERE node_id IS NULL OR node_id = ? \
             ORDER BY (node_id IS NULL) DESC, id",
        )
        .bind(node_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    pub async fn list_proxies(&self) -> Result<Vec<ProxyView>> {
        let rows = sqlx::query_as::<_, ProxyView>(
            "SELECT id, url, node_id, created_at FROM proxies \
             ORDER BY (node_id IS NULL) DESC, id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|mut r| {
                r.url = redact_proxy_url(&r.url);
                r
            })
            .collect())
    }

    pub async fn add_proxy(&self, url: &str, node_id: Option<&str>) -> Result<i64> {
        let r = self
            .write_one(
                sqlx::query("INSERT INTO proxies (url, node_id) VALUES (?, ?)")
                    .bind(url)
                    .bind(node_id),
            )
            .await?;
        Ok(r.last_insert_rowid())
    }

    pub async fn remove_proxy(&self, id: i64) -> Result<u64> {
        Ok(self
            .write_one(sqlx::query("DELETE FROM proxies WHERE id = ?").bind(id))
            .await?
            .rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::redact_proxy_url;

    #[test]
    fn a_proxy_password_is_redacted_and_the_rest_survives() {
        assert_eq!(
            redact_proxy_url("http://alice:hunter2@proxy.example:3128"),
            "http://alice:***@proxy.example:3128"
        );
        // A path or query after the authority must survive the rewrite.
        assert_eq!(
            redact_proxy_url("socks5://alice:pw@10.0.0.5:1080/path?x=1"),
            "socks5://alice:***@10.0.0.5:1080/path?x=1"
        );
    }

    #[test]
    fn a_proxy_url_without_a_password_is_left_alone() {
        for url in [
            "http://proxy.example:3128",
            "http://alice@proxy.example:3128",
            "not a url",
        ] {
            assert_eq!(redact_proxy_url(url), url);
        }
        // An `@` in the path is not userinfo.
        assert_eq!(
            redact_proxy_url("http://proxy.example/a@b"),
            "http://proxy.example/a@b"
        );
    }
}
