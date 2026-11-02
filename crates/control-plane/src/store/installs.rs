//! Durable install attempts (`POST /v1/node-installs`).
//!
//! The panel's job list used to live in a `HashMap` on the control plane, which
//! had one consequence worth fixing: a restart erased the only record that
//! anyone had tried to bring a host up. The node table cannot carry that fact —
//! it is keyed by enrollment, so a host whose daemon never enrolled is absent
//! by definition — which made "configured but not reachable" the one question
//! the panel could not answer after a restart.
//!
//! The row is written through on every transition rather than at the end: an
//! install that is killed halfway is exactly the one whose record matters.

use super::Store;
use anyhow::Result;
use sqlx::Row;

/// One install attempt, as stored.
///
/// Deliberately plain strings for `state` and `steps`: this module is below the
/// HTTP layer and must not depend on the route's enums, and a row written by an
/// older binary has to stay readable.
#[derive(Clone, Debug)]
pub struct InstallRecord {
    pub id: String,
    pub host: String,
    pub name: String,
    pub state: String,
    pub steps: Vec<String>,
    pub error: Option<String>,
    pub server_url: Option<String>,
    pub started_at: String,
    pub finished_at: Option<String>,
}

impl Store {
    /// Insert or replace one attempt.
    pub async fn put_install(&self, r: &InstallRecord) -> Result<()> {
        let steps = serde_json::to_string(&r.steps)?;
        self.write_one(
            sqlx::query(
                "INSERT INTO node_installs \
                 (id, host, name, state, steps, error, server_url, started_at, finished_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
                 ON CONFLICT(id) DO UPDATE SET \
                   host = excluded.host, name = excluded.name, state = excluded.state, \
                   steps = excluded.steps, error = excluded.error, \
                   server_url = excluded.server_url, finished_at = excluded.finished_at",
            )
            .bind(&r.id)
            .bind(&r.host)
            .bind(&r.name)
            .bind(&r.state)
            .bind(&steps)
            .bind(&r.error)
            .bind(&r.server_url)
            .bind(&r.started_at)
            .bind(&r.finished_at),
        )
        .await?;
        Ok(())
    }

    /// Newest first, capped by the caller.
    pub async fn list_installs(&self, limit: i64) -> Result<Vec<InstallRecord>> {
        let rows = sqlx::query(
            "SELECT id, host, name, state, steps, error, server_url, started_at, finished_at \
             FROM node_installs ORDER BY started_at DESC LIMIT ?",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(row_to_install).collect())
    }

    pub async fn get_install(&self, id: &str) -> Result<Option<InstallRecord>> {
        let row = sqlx::query(
            "SELECT id, host, name, state, steps, error, server_url, started_at, finished_at \
             FROM node_installs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.as_ref().map(row_to_install))
    }
}

fn row_to_install(r: &sqlx::sqlite::SqliteRow) -> InstallRecord {
    InstallRecord {
        id: r.try_get("id").unwrap_or_default(),
        host: r.try_get("host").unwrap_or_default(),
        name: r.try_get("name").unwrap_or_default(),
        state: r.try_get("state").unwrap_or_else(|_| "failed".into()),
        // A corrupt blob degrades to "no steps" rather than failing the list:
        // the row still answers the question it exists for.
        steps: r
            .try_get::<String, _>("steps")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        error: r.try_get("error").ok().flatten(),
        server_url: r.try_get("server_url").ok().flatten(),
        started_at: r.try_get("started_at").unwrap_or_default(),
        finished_at: r.try_get("finished_at").ok().flatten(),
    }
}
