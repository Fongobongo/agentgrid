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

    /// Answer every open `issued` row for a host that has now enrolled.
    ///
    /// The in-memory job list is capped at twenty entries, so a row can fall out
    /// of it while still sitting in the table as `issued`. Closing only what
    /// happens to be in memory would leave the database asserting "a token went
    /// out and nothing came back" about a host that is enrolled and
    /// heartbeating — the opposite of what this table is for. The table is the
    /// record, so the close happens here, matched by name: the only string the
    /// mint and the enrollment share.
    ///
    /// Idempotent — the `WHERE state = 'issued'` guard means a row already
    /// closed by the write-through path is left alone, step text and all.
    pub async fn close_issued_installs(&self, name: &str, node_id: &str) -> Result<usize> {
        let rows =
            sqlx::query("SELECT id, steps FROM node_installs WHERE state = 'issued' AND name = ?")
                .bind(name)
                .fetch_all(&self.pool)
                .await?;
        let finished_at = chrono::Utc::now().to_rfc3339();
        let mut closed = 0;
        for row in &rows {
            let id: String = row.try_get("id").unwrap_or_default();
            let mut steps: Vec<String> = row
                .try_get::<String, _>("steps")
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            steps.push(format!("enrolled as {node_id}"));
            self.write_one(
                sqlx::query(
                    "UPDATE node_installs SET state = 'succeeded', steps = ?, finished_at = ? \
                     WHERE id = ? AND state = 'issued'",
                )
                .bind(serde_json::to_string(&steps)?)
                .bind(&finished_at)
                .bind(&id),
            )
            .await?;
            closed += 1;
        }
        Ok(closed)
    }

    /// Record an open intent for a host, unless one is already open.
    ///
    /// Returns `true` when this call created the row. `ON CONFLICT DO NOTHING`
    /// without a target covers the partial unique index from migration 0101, so
    /// the check and the insert are one statement: two control planes — or one
    /// whose job list has aged the original out of its twenty-entry window —
    /// cannot both create an intent for the same host.
    ///
    /// `state`, `error`, `server_url` and `finished_at` are written literally
    /// rather than bound: an intent is by definition open, and letting a caller
    /// pass those through would make this method a second, weaker
    /// [`Self::put_install`].
    pub async fn record_install_intent(&self, r: &InstallRecord) -> Result<bool> {
        let res = self
            .write_one(
                sqlx::query(
                    "INSERT INTO node_installs \
                     (id, host, name, state, steps, error, server_url, started_at, finished_at) \
                     VALUES (?, ?, ?, 'issued', ?, NULL, NULL, ?, NULL) \
                     ON CONFLICT DO NOTHING",
                )
                .bind(&r.id)
                .bind(&r.host)
                .bind(&r.name)
                .bind(serde_json::to_string(&r.steps)?)
                .bind(&r.started_at),
            )
            .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Remove one attempt. `false` when there was no such row.
    ///
    /// This is the counterpart to retention: an `issued` row never ages out, so
    /// a host that was configured and then decommissioned would otherwise sit in
    /// the table — and in the panel — forever, removable only by hand-written
    /// SQL. Deleting a `running` row is refused one layer up, at the HTTP
    /// boundary, because that is where the difference between "an old record"
    /// and "a job in flight" is known.
    pub async fn delete_install(&self, id: &str) -> Result<bool> {
        let res = self
            .write_one(sqlx::query("DELETE FROM node_installs WHERE id = ?").bind(id))
            .await?;
        Ok(res.rows_affected() == 1)
    }

    /// Drop finished attempts past the retention window.
    ///
    /// Only `succeeded` and `failed` are eligible. An `issued` row answers
    /// "was this host ever configured?", and a `running` row is the only trace
    /// of an install whose outcome was never learned — deleting either by age
    /// would erase the fact this table exists to keep, so both are retained
    /// indefinitely. That is also what keeps the table bounded: one open intent
    /// per host that has not arrived, however old it is.
    ///
    /// `retention_hours <= 0` deletes nothing, which is the operator escape
    /// hatch ("keep every attempt") and what a deployment that treats this as
    /// an audit log would set.
    ///
    /// Timestamps are `to_rfc3339()` UTC throughout, so lexicographic order is
    /// chronological order and the cutoff needs no parsing.
    pub async fn prune_installs(&self, retention_hours: i64) -> Result<u64> {
        if retention_hours <= 0 {
            return Ok(0);
        }
        let cutoff = (chrono::Utc::now() - chrono::Duration::hours(retention_hours)).to_rfc3339();
        let res = self
            .write_one(
                sqlx::query(
                    "DELETE FROM node_installs \
                     WHERE state IN ('succeeded', 'failed') \
                       AND COALESCE(finished_at, started_at) < ?",
                )
                .bind(&cutoff),
            )
            .await?;
        Ok(res.rows_affected())
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
