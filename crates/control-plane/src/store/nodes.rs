//! Node enrollment, lifecycle, sessions and audit. Extracted from `store.rs`.

use super::{
    audit_from_row, iso_plus_secs, lose_node_attempts, node_status_str, now_iso, sha256_hex,
    AuditEvent, Store,
};
use agentgrid_common::{EnrollRequest, EnrollResponse, HeartbeatRequest, NodeStatus};
use anyhow::Result;
use sqlx::sqlite::Sqlite;
use sqlx::Row;
use uuid::Uuid;

impl Store {
    // ----- enrollment + node auth (Stage 2.3) -----

    /// Issue a one-time enrollment token (TTL 10 min). Only its hash is stored.
    pub async fn create_enrollment_token(&self) -> Result<(String, String)> {
        let token = Uuid::new_v4().to_string();
        let hash = sha256_hex(&token);
        let expires_at = iso_plus_secs(600);
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        sqlx::query(
            "INSERT INTO enrollment_tokens (id, token_hash, expires_at, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&hash)
        .bind(&expires_at)
        .bind(&now)
        .execute(&self.pool)
        .await?;
        Ok((token, expires_at))
    }

    /// Exchange a valid (unused, unexpired) token for a permanent node credential.
    pub async fn enroll_node(&self, req: &EnrollRequest) -> Result<Option<EnrollResponse>> {
        let mut tx = self.write_txn().await?;
        let hash = sha256_hex(&req.token);
        let tok = sqlx::query(
            "SELECT id, expires_at, used_at FROM enrollment_tokens WHERE token_hash = ?",
        )
        .bind(&hash)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(tok) = tok else {
            let _ = tx.rollback().await;
            return Ok(None);
        };
        let expires: String = tok.try_get("expires_at")?;
        let used: Option<String> = tok.try_get("used_at")?;
        if used.is_some() || expires < now_iso() {
            let _ = tx.rollback().await;
            return Ok(None);
        }
        let node_id = Uuid::new_v4().to_string();
        let credential = Uuid::new_v4().to_string();
        let cred_hash = sha256_hex(&credential);
        let now = now_iso();
        let adapters = serde_json::to_string(&req.adapters)?;
        let repos = serde_json::to_string(&req.repositories)?;
        sqlx::query(
            "INSERT INTO nodes (id, name, status, agent_version, max_concurrency, adapters, repositories, active_attempts, last_heartbeat_at, credential_hash, created_at, repo_cache_bytes, workspace_bytes) \
             VALUES (?, ?, 'online', ?, ?, ?, ?, 0, ?, ?, ?, 0, 0)",
        )
        .bind(&node_id)
        .bind(&req.name)
        .bind(&req.agent_version)
        .bind(req.max_concurrency as i64)
        .bind(&adapters)
        .bind(&repos)
        .bind(&now)
        .bind(&cred_hash)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE enrollment_tokens SET used_at = ? WHERE id = ?")
            .bind(&now)
            .bind(tok.try_get::<String, _>("id")?)
            .execute(&mut *tx)
            .await?;
        self.audit_tx(&mut tx, "node", Some(&node_id), "enroll", None, None)
            .await?;
        tx.commit().await?;
        Ok(Some(EnrollResponse {
            node_id,
            credential,
        }))
    }

    /// Resolve a node credential to its node id, or None if unknown or revoked.
    pub async fn node_id_for_credential(&self, credential: &str) -> Result<Option<String>> {
        let hash = sha256_hex(credential);
        let row = sqlx::query("SELECT id, status FROM nodes WHERE credential_hash = ?")
            .bind(&hash)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match row {
            Some(r) => {
                let status: String = r.try_get("status")?;
                if status == "revoked" {
                    None
                } else {
                    Some(r.try_get("id")?)
                }
            }
            None => None,
        })
    }

    /// Resolve the owning node id of an attempt, or None if no such attempt.
    /// Used by node handlers to enforce cross-node isolation (hardening P0):
    /// a node may only mutate attempts assigned to it.
    pub async fn attempt_owner(&self, attempt_id: &str) -> Result<Option<String>> {
        let row = sqlx::query("SELECT node_id FROM attempts WHERE id = ?")
            .bind(attempt_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| r.try_get::<String, _>("node_id")).transpose()?)
    }

    /// Record a heartbeat: refresh capabilities/load and last-seen time.
    pub async fn heartbeat(&self, node_id: &str, req: &HeartbeatRequest) -> Result<bool> {
        let status = req.status.unwrap_or(NodeStatus::Online);
        let adapters = serde_json::to_string(&req.adapters)?;
        let repos = serde_json::to_string(&req.repositories)?;
        let now = now_iso();
        let affected = sqlx::query(
            "UPDATE nodes SET name = ?, \
               status = CASE WHEN status = 'revoked' THEN 'revoked' ELSE ? END, \
               agent_version = ?, max_concurrency = ?, adapters = ?, repositories = ?, \
               load_avg = ?, free_disk_mb = ?, last_heartbeat_at = ?, \
               unsafe_active = ?, permission_interception = ?, \
               outbox_bytes = ?, artifact_spool_bytes = ?, \
               outbox_rows = ?, outbox_oldest_pending_age_ms = ?, outbox_corruption_count = ?, outbox_completion_rows = ?, repo_lock_wait_ms = ?, sandbox_backend = ?, enforced_limits = ?, repo_cache_bytes = ?, workspace_bytes = ?, network_mode = ?, active_rss_mib = ?, max_rss_mib = CASE WHEN ? > 0 THEN ? ELSE max_rss_mib END \
             WHERE id = ?",
        )
        // active_attempts is intentionally not heartbeat-settable: it is the
        // CP-authoritative quota counter (assign/complete + reconcile), and a
        // stale or lying node report could break tasks_max enforcement.
        .bind(&req.name)
        .bind(node_status_str(status))
        .bind(&req.agent_version)
        .bind(req.max_concurrency as i64)
        .bind(&adapters)
        .bind(&repos)
        .bind(req.load_avg)
        .bind(req.free_disk_mb as i64)
        .bind(&now)
        .bind(req.unsafe_active as i64)
        .bind(&req.permission_interception)
        .bind(req.outbox_bytes as i64)
        .bind(req.artifact_spool_bytes as i64)
        .bind(req.outbox_rows as i64)
        .bind(req.outbox_oldest_pending_age_ms as i64)
        .bind(req.outbox_corruption_count as i64)
        .bind(req.outbox_completion_rows as i64)
        .bind(req.repo_lock_wait_ms as i64)
        .bind(&req.sandbox_backend)
        .bind(req.enforced_limits as i64)
        .bind(req.repo_cache_bytes as i64)
        .bind(req.workspace_bytes as i64)
        .bind(&req.network_mode)
        .bind(req.active_rss_mib as i64)
        .bind(req.max_rss_mib as i64)
        .bind(req.max_rss_mib as i64)
        .bind(node_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if affected == 1 && status == NodeStatus::Offline {
            let mut t = self.write_txn().await?;
            lose_node_attempts(&mut t, node_id).await?;
            t.commit().await?;
        }
        // Opencode-config drift detector: when the node reports an applied
        // hash that doesn't match its assigned profile's hash, log+wite an
        // audit row so the UI/CLI can surface "this node drifted". We do
        // NOT mark the node degraded — drift typically heals within one
        // heartbeat when the next pull lands and the apply pipeline sets
        // the hash to match.
        if let (Some(applied), Some(profile_row)) = (
            &req.applied_opencode_hash,
            self.node_opencode_profile(node_id).await?,
        ) {
            if applied != &profile_row.hash {
                tracing::warn!(
                    node_id = node_id,
                    applied = %applied,
                    expected = %profile_row.hash,
                    "opencode config drift — node applied hash mismatches assigned profile"
                );
                let detail = serde_json::json!({
                    "applied": applied,
                    "expected": profile_row.hash,
                    "profile_id": profile_row.id,
                })
                .to_string();
                let _ = self
                    .audit("node", Some(node_id), "opencode.drift", None, Some(&detail))
                    .await;
            }
        }
        Ok(affected == 1)
    }

    /// Revoke a node: reject its credential immediately, mark `revoked`, and
    /// lose any in-flight attempts (Stage 1.2).
    pub async fn revoke_node(&self, node_id: &str) -> Result<bool> {
        let now = now_iso();
        let affected =
            sqlx::query("UPDATE nodes SET status = 'revoked', revoked_at = ? WHERE id = ?")
                .bind(&now)
                .bind(node_id)
                .execute(&self.pool)
                .await?
                .rows_affected();
        if affected == 1 {
            self.audit("node", Some(node_id), "revoke", None, None)
                .await?;
            let mut t = self.write_txn().await?;
            lose_node_attempts(&mut t, node_id).await?;
            t.commit().await?;
        }
        Ok(affected == 1)
    }

    /// Hardening P2 item 37: mark a node drained (stop NEW assignments) or
    /// undrained. In-flight attempts are untouched; the heartbeat keeps the
    /// node online so maintenance can drain gracefully.
    pub async fn set_node_drained(&self, node_id: &str, drained: bool) -> Result<bool> {
        let affected = sqlx::query("UPDATE nodes SET drained = ? WHERE id = ?")
            .bind(drained as i64)
            .bind(node_id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        if affected == 1 {
            self.audit(
                "user",
                None,
                if drained {
                    "node.drain"
                } else {
                    "node.undrain"
                },
                Some(node_id),
                None,
            )
            .await?;
        }
        Ok(affected == 1)
    }

    /// Check if a session (by jti) has been revoked.
    pub async fn is_session_revoked(&self, jti: &str) -> Result<bool> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT jti FROM revoked_sessions WHERE jti = ?")
                .bind(jti)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.is_some())
    }

    /// Revoke a user session by jti (JWT ID).
    pub async fn revoke_session(&self, jti: &str, username: &str) -> Result<bool> {
        let now = now_iso();
        let affected = sqlx::query(
            "INSERT OR IGNORE INTO revoked_sessions (jti, username, revoked_at) VALUES (?, ?, ?)",
        )
        .bind(jti)
        .bind(username)
        .bind(&now)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(affected == 1)
    }

    /// Mark a node offline (unless already revoked) and lose its in-flight
    /// attempts. Triggered by stale-heartbeat maintenance, a self-reported
    /// offline status, or an explicit admin action (Stage 1.2).
    /// Race-safe (hardening P0 item 7): CAS `online`/non-pending -> `offline`
    /// under `BEGIN IMMEDIATE` and only lose attempts for a node we actually
    /// flipped, so a concurrent heartbeat can't re-online a node mid-lose.
    pub async fn mark_node_offline(&self, node_id: &str) -> Result<bool> {
        let now = now_iso();
        let mut tx = self.write_txn().await?;
        let affected = sqlx::query(
            "UPDATE nodes SET status = 'offline', last_heartbeat_at = ? \
             WHERE id = ? AND status NOT IN ('offline','pending','revoked')",
        )
        .bind(&now)
        .bind(node_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if affected == 1 {
            lose_node_attempts(&mut tx, node_id).await?;
        }
        tx.commit().await?;
        Ok(affected == 1)
    }

    pub async fn audit(
        &self,
        actor_type: &str,
        actor_id: Option<&str>,
        action: &str,
        subject: Option<&str>,
        payload: Option<&str>,
    ) -> Result<()> {
        let mut tx = self.write_txn().await?;
        self.audit_tx(&mut tx, actor_type, actor_id, action, subject, payload)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    async fn audit_tx(
        &self,
        tx: &mut sqlx::SqliteConnection,
        actor_type: &str,
        actor_id: Option<&str>,
        action: &str,
        subject: Option<&str>,
        payload: Option<&str>,
    ) -> Result<()> {
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        sqlx::query(
            "INSERT INTO audit_events (id, actor_type, actor_id, action, subject, payload, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(actor_type)
        .bind(actor_id)
        .bind(action)
        .bind(subject)
        .bind(payload)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

    /// Most-recent audit events (newest first), optionally filtered by action.
    pub async fn list_audit(&self, action: Option<&str>, limit: i64) -> Result<Vec<AuditEvent>> {
        let rows = match action {
            Some(a) => {
                sqlx::query(
                    "SELECT id, actor_type, actor_id, action, subject, payload, created_at \
                     FROM audit_events WHERE action = ? ORDER BY created_at DESC LIMIT ?",
                )
                .bind(a)
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT id, actor_type, actor_id, action, subject, payload, created_at \
                     FROM audit_events ORDER BY created_at DESC LIMIT ?",
                )
                .bind(limit)
                .fetch_all(&self.pool)
                .await?
            }
        };
        Ok(rows.iter().map(audit_from_row).collect())
    }

    /// Change-detection fingerprint for the UI SSE change stream (plan 3.2):
    /// status counts for tasks, nodes and workflow runs. Cheap aggregate
    /// queries; the UI refetches full lists only when this value changes.
    pub async fn status_fingerprint(
        &self,
    ) -> Result<std::collections::BTreeMap<String, std::collections::BTreeMap<String, i64>>> {
        async fn counts(
            pool: &sqlx::Pool<Sqlite>,
            table: &'static str,
        ) -> Result<std::collections::BTreeMap<String, i64>> {
                        let rows = sqlx::query(sqlx::AssertSqlSafe(&format!( /* audited: clauses are compile-time constants; every value is a bound parameter */
                "SELECT status, COUNT(*) AS n FROM {table} GROUP BY status"
            )))
            .fetch_all(pool)
            .await?;
            let mut m = std::collections::BTreeMap::new();
            for r in rows {
                let status: String = r.try_get("status").unwrap_or_default();
                let n: i64 = r.try_get("n").unwrap_or(0);
                m.insert(status, n);
            }
            Ok(m)
        }
        let mut fp = std::collections::BTreeMap::new();
        fp.insert("tasks".to_string(), counts(&self.pool, "tasks").await?);
        fp.insert("nodes".to_string(), counts(&self.pool, "nodes").await?);
        fp.insert(
            "workflow_runs".to_string(),
            counts(&self.pool, "workflow_runs").await?,
        );
        Ok(fp)
    }
}
