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
    pub url: String,
    pub node_id: Option<String>,
    pub created_at: String,
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
        Ok(sqlx::query_as::<_, ProxyView>(
            "SELECT id, url, node_id, created_at FROM proxies \
             ORDER BY (node_id IS NULL) DESC, id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn add_proxy(&self, url: &str, node_id: Option<&str>) -> Result<i64> {
        let r = sqlx::query("INSERT INTO proxies (url, node_id) VALUES (?, ?)")
            .bind(url)
            .bind(node_id)
            .execute(&self.pool)
            .await?;
        Ok(r.last_insert_rowid())
    }

    pub async fn remove_proxy(&self, id: i64) -> Result<u64> {
        Ok(sqlx::query("DELETE FROM proxies WHERE id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }
}
