//! opencode-config profiles (feature "opencode profiles").
//!
//! A profile is a named bundle of opencode settings (model, small_model,
//! provider blocks, plugin npm refs, inline skills). Stored as opaque JSON —
//! the config schema belongs to opencode, not to us; the CP validates
//! syntax + a small key allowlist and lets node-side `opencode debug config`
//! be the final oracle.
//!
//! Hot paths:
//!   1. PUT/POST routes write rows + bump `hash` (sha256 of canonical JSON)
//!      and multicast `NodeWsMsg::ConfigUpdate` to every node subscribed to
//!      that profile (or every connected node on profile rename/delete where
//!      assignment state changed).
//!   2. Node pulls `active_config(node_id)` on `config_update` push or after
//!      `AGENTGRID_CONFIG_PULL_AFTER_ERRORS` (default 3) config-class errors;
//!      the response is cached server-side since the error path is hot.
//!   3. `apply_audit` records each application (ws_push / error_threshold /
//!      interval / startup) so dashboards can answer "who's on which
//!      profile" and "when did node X last apply".

use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

use super::{now_iso, Store};
use agentgrid_common::{OpencodeConfigAuditEntry, OpencodeProfile};

/// Feature "opencode profiles": allowlist of top-level keys accepted into a
/// stored profile. Intentionally conservative — anything outside the list is
/// dropped server-side rather than letting a buggy client persist garbage
/// the node later fails to parse. The node-side `OPENCODE_CONFIG_CONTENT`
/// merge keeps the escape hatch: anything not in this list can still be
/// injected per-attempt (it never lands on disk).
const ALLOWED_TOP_LEVEL: &[&str] = &[
    "model",
    "small_model",
    "default_agent",
    "provider",
    "plugin",
    "mcp",
    "agent",
    "permission",
    "enabled_providers",
    "disabled_providers",
    "share",
    "snapshot",
    "autoupdate",
];

/// Primitive per-key shape contract. Anything outside the allowlist is
/// silently stripped (allows server to hide typos without producing a
/// broken config); a key ON the list with the wrong JSON type is rejected
/// loudly — otherwise the node would ship it to opencode and the binary
/// would either crash or start silently with a fallback.
fn check_shape(key: &str, v: &serde_json::Value) -> Result<()> {
    let ok = match key {
        "model" | "small_model" | "default_agent" => v.is_string(),
        "share" | "snapshot" | "autoupdate" => v.is_boolean(),
        "enabled_providers" | "disabled_providers" | "plugin" => v.is_array(),
        "provider" | "mcp" | "agent" | "permission" => v.is_object() || v.is_null(),
        _ => true,
    };
    if !ok {
        anyhow::bail!("opencode profile key '{key}' has the wrong JSON shape");
    }
    Ok(())
}

/// Canonical JSON encoding of a pinned-skills set: a sorted JSON array of
/// strings (dedup). `None`/empty -> `None` (stored as SQL NULL, surfaces as
/// an empty pin set in the view). Two runs with the same names hash the same.
fn canonicalize_pinned(names: &[String]) -> Option<String> {
    let mut s: Vec<String> = names
        .iter()
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .collect();
    s.sort();
    s.dedup();
    if s.is_empty() {
        return None;
    }
    serde_json::to_string(&s).ok()
}

/// Strip config keys outside the allowlist. Returns the canonical JSON
/// string (sorted-within-object via serde's deterministic order) so two
/// uploads with the same semantics produce the same hash.
fn normalize_config(v: serde_json::Value) -> Result<(String, String)> {
    let mut v = v;
    if !v.is_object() {
        anyhow::bail!("profile config must be a JSON object");
    }
    if let Some(obj) = v.as_object_mut() {
        let keys: Vec<(String, serde_json::Value)> =
            obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for (k, shape_v) in keys {
            if ALLOWED_TOP_LEVEL.contains(&k.as_str()) {
                check_shape(&k, &shape_v)?;
            }
        }
        obj.retain(|k, _| ALLOWED_TOP_LEVEL.contains(&k.as_str()));
    }
    let json = serde_json::to_string(&v)?;
    let hash = agentgrid_common::sha256_hex(json.as_bytes());
    Ok((json, hash))
}

/// `sanitize_config` runs the same allowlist the PUT path applies and
/// returns the resulting Value (used by `?dry_run=true` to surface the
/// preview + the dropped-keys list).
///
/// The dropped-key tracking is thread-local because the call sites are
/// synchronous (no awaiting a mutex) and the only consumer is the same
/// request thread that called us — no cross-request leaks.
pub fn sanitize_config(v: &serde_json::Value) -> Result<serde_json::Value> {
    LAST_DROPPED.with(|c| c.borrow_mut().clear());
    let mut v = v.clone();
    if !v.is_object() {
        anyhow::bail!("profile config must be a JSON object");
    }
    if let Some(obj) = v.as_object_mut() {
        // Shape-check allowed keys before stripping; even though the dry-run
        // only reports on dropped keys, feeding material-for-allowed-keys a
        // wrong-typed value should fail at the same place the real PUT
        // would.
        for (k, shape_v) in obj.iter() {
            if ALLOWED_TOP_LEVEL.contains(&k.as_str()) {
                check_shape(k, shape_v)?;
            }
        }
        let mut dropped: Vec<String> = Vec::new();
        let allowed: std::collections::HashSet<&str> = ALLOWED_TOP_LEVEL.iter().copied().collect();
        let keys: Vec<String> = obj.keys().cloned().collect();
        for k in keys {
            if !allowed.contains(k.as_str()) {
                obj.remove(&k);
                dropped.push(k);
            }
        }
        dropped.sort();
        LAST_DROPPED.with(|c| *c.borrow_mut() = dropped);
    }
    Ok(v)
}

/// Keys dropped on the most recent `sanitize_config` call on this thread.
/// The route's dry-run path reads this right after `sanitize_config`.
pub fn last_dropped_keys() -> Vec<String> {
    LAST_DROPPED.with(|c| c.borrow().clone())
}

thread_local! {
    static LAST_DROPPED: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

pub struct OpencodeProfileRow {
    pub id: String,
    pub name: String,
    pub config_json: String,
    pub hash: String,
    pub prev_config_json: Option<String>,
    pub prev_hash: Option<String>,
    pub expires_at: Option<String>,
    pub pinned_skills_json: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn row_to_profile(r: &sqlx::sqlite::SqliteRow) -> OpencodeProfileRow {
    OpencodeProfileRow {
        id: r.get("id"),
        name: r.get("name"),
        config_json: r.get("config_json"),
        hash: r.get("hash"),
        prev_config_json: r.get("prev_config_json"),
        prev_hash: r.get("prev_hash"),
        expires_at: r.get("expires_at"),
        pinned_skills_json: r.get("pinned_skills_json"),
        created_at: r.get("created_at"),
        updated_at: r.get("updated_at"),
    }
}

impl OpencodeProfileRow {
    pub fn into_view(self) -> Result<OpencodeProfile> {
        let config = serde_json::from_str(&self.config_json)?;
        let prev = match (self.prev_config_json, self.prev_hash) {
            (Some(json), Some(hash)) => {
                let prev_config = serde_json::from_str(&json)?;
                Some(Box::new(agentgrid_common::OpencodeProfileRevision {
                    hash,
                    config: prev_config,
                    updated_at: String::new(), // populated by rollback when the swap fires
                }))
            }
            _ => None,
        };
        Ok(OpencodeProfile {
            id: self.id,
            name: self.name,
            config,
            hash: self.hash,
            prev,
            expires_at: self.expires_at,
            apply_count: None,
            pinned_skills: self
                .pinned_skills_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok()),
            created_at: self.created_at,
            updated_at: self.updated_at,
        })
    }
}

impl Store {
    pub async fn list_opencode_profiles(&self) -> Result<Vec<OpencodeProfile>> {
        let rows = sqlx::query(
            "SELECT id, name, config_json, hash, prev_config_json, prev_hash, expires_at, pinned_skills_json, created_at, updated_at,
                    (SELECT COUNT(*) FROM opencode_config_audit a WHERE a.profile_id = opencode_profiles.id) AS apply_count
             FROM opencode_profiles ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|r| {
                let mut p = row_to_profile(&r).into_view()?;
                p.apply_count = Some(r.try_get("apply_count").unwrap_or(0));
                Ok(p)
            })
            .collect()
    }

    pub async fn get_opencode_profile(&self, name: &str) -> Result<Option<OpencodeProfile>> {
        let row = sqlx::query(
            "SELECT id, name, config_json, hash, prev_config_json, prev_hash, expires_at, pinned_skills_json, created_at, updated_at
             FROM opencode_profiles WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| row_to_profile(&r).into_view()).transpose()
    }

    /// PUT semantics: create-or-replace. Returns the final row (with the
    /// recomputed hash). Existing nodes currently assigned to the old row
    /// keep their `opencode_profile_id` (same primary key when the update
    /// was by-name; new row only when it was a fresh insert).
    pub async fn upsert_opencode_profile(
        &self,
        name: &str,
        config: serde_json::Value,
        expires_at: Option<String>,
        pinned_skills: Option<Vec<String>>,
    ) -> Result<OpencodeProfile> {
        let (json, hash) = normalize_config(config)?;
        let now = now_iso();
        let pinned_json = canonicalize_pinned(pinned_skills.as_deref().unwrap_or(&[]));
        // UPSERT on the unique `name`. The id is stable across updates so
        // foreign keys (nodes.opencode_profile_id) survive. `expires_at` is
        // replaced wholesale (None clears a previous TTL).
        let row = sqlx::query(
            "INSERT INTO opencode_profiles (id, name, config_json, hash, expires_at, pinned_skills_json, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(name) DO UPDATE SET
                prev_config_json = opencode_profiles.config_json,
                prev_hash = opencode_profiles.hash,
                config_json = excluded.config_json,
                hash = excluded.hash,
                expires_at = excluded.expires_at,
                pinned_skills_json = excluded.pinned_skills_json,
                updated_at = excluded.updated_at
             RETURNING id, name, config_json, hash, prev_config_json, prev_hash, expires_at, pinned_skills_json, created_at, updated_at",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(name)
        .bind(&json)
        .bind(&hash)
        .bind(&expires_at)
        .bind(&pinned_json)
        .bind(&now)
        .bind(&now)
        .fetch_one(&self.pool)
        .await?;
        let final_row = row_to_profile(&row);
        // Append the pre-PUT body as a revision so rollback can walk back
        // more than one step. The row's `prev_*` column is the fast path
        // for the most recent revision; this table adds deeper history.
        if let (Some(prev_json), Some(prev_hash)) =
            (&final_row.prev_config_json, &final_row.prev_hash)
        {
            let _ = sqlx::query(
                "INSERT INTO opencode_profile_revisions (profile_id, config_json, hash, saved_at)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(&final_row.id)
            .bind(prev_json)
            .bind(prev_hash)
            .bind(&now)
            .execute(&self.pool)
            .await;
        }
        final_row.into_view()
    }

    /// Roll the profile one step back: swap cur→prev, drop the old prev.
    /// Nodes holding the profile pick the new hash over the next WS push
    /// (`ConfigUpdate` ships after commit). The audit route's trigger
    /// vocabulary accepts "rollback" so the node-side entry stays honest
    /// about why the file turned.
    ///
    /// `steps` walks the revision stack (prev column = revision #1, plus
    /// rows from `opencode_profile_revisions`); the body that ends up on
    /// top replaces the live row and mid-walk snapshots re-land as new
    /// revisions so the next rollback to the *previous* state stays cheap.
    /// Returns None when the requested history depth doesn't exist.
    pub async fn rollback_opencode_profile(
        &self,
        name: &str,
        steps: u32,
    ) -> Result<Option<OpencodeProfile>> {
        let mut tx = self.pool.begin().await?;
        type ProfileRow = (String, String, String, Option<String>, Option<String>);
        let row: Option<ProfileRow> = sqlx::query_as(
            "SELECT id, config_json, hash, prev_config_json, prev_hash
             FROM opencode_profiles WHERE name = ?",
        )
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((profile_id, mut cur_json, mut cur_hash, prev_json, prev_hash)) = row else {
            tx.rollback().await?;
            return Ok(None);
        };

        let mut stack: Vec<(String, String)> = Vec::new();
        if let (Some(pj), Some(ph)) = (prev_json, prev_hash) {
            stack.push((pj, ph));
        }
        let extra = sqlx::query_as::<_, (String, String)>(
            "SELECT config_json, hash FROM opencode_profile_revisions
             WHERE profile_id = ? ORDER BY id ASC",
        )
        .bind(&profile_id)
        .fetch_all(&mut *tx)
        .await?;
        for e in extra {
            stack.push(e);
        }

        let steps_n = steps.max(1) as usize;
        if stack.len() < steps_n {
            tx.rollback().await?;
            return Ok(None);
        }

        let now = now_iso();
        for _ in 0..steps_n {
            let (new_json, new_hash) = stack.pop().unwrap();
            let _ = sqlx::query(
                "INSERT INTO opencode_profile_revisions (profile_id, config_json, hash, saved_at)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(&profile_id)
            .bind(&cur_json)
            .bind(&cur_hash)
            .bind(&now)
            .execute(&mut *tx)
            .await;
            cur_json = new_json;
            cur_hash = new_hash;
        }
        let (prev_j, prev_h) = stack
            .pop()
            .map(|(j, h)| (Some(j), Some(h)))
            .unwrap_or((None, None));
        sqlx::query(
            "UPDATE opencode_profiles
             SET config_json = ?, hash = ?, prev_config_json = ?, prev_hash = ?, updated_at = ?
             WHERE id = ?",
        )
        .bind(&cur_json)
        .bind(&cur_hash)
        .bind(&prev_j)
        .bind(&prev_h)
        .bind(&now)
        .bind(&profile_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.get_opencode_profile(name).await
    }

    pub async fn delete_opencode_profile(&self, name: &str) -> Result<bool> {
        let r = sqlx::query("DELETE FROM opencode_profiles WHERE name = ?")
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(r.rows_affected() > 0)
    }

    /// A/B percent split: redistribute the nodes currently on either arm
    /// (`keep_id` / `other_id`) so that `percent`% of them land on `keep_id`
    /// and the rest on `other_id`. Deterministic (ordered by node id) so
    /// re-running with the same percent is stable. Returns the per-node
    /// `(node_id, profile_id)` so the route can push the matching hash and
    /// stretch — only nodes on the two arms move, the rest of the fleet is
    /// left untouched.
    pub async fn assign_percent_between(
        &self,
        keep_id: &str,
        other_id: &str,
        percent: u8,
    ) -> Result<Vec<(String, String)>> {
        let rows =
            sqlx::query("SELECT id FROM nodes WHERE opencode_profile_id IN (?, ?) ORDER BY id")
                .bind(keep_id)
                .bind(other_id)
                .fetch_all(&self.pool)
                .await?;
        let node_ids: Vec<String> = rows.iter().map(|r| r.get("id")).collect();
        let total = node_ids.len();
        let keep_n = total * percent as usize / 100;
        let mut out = Vec::with_capacity(node_ids.len());
        let mut tx = self.pool.begin().await?;
        for (i, node_id) in node_ids.iter().enumerate() {
            let target = if i < keep_n { keep_id } else { other_id };
            sqlx::query("UPDATE nodes SET opencode_profile_id = ? WHERE id = ?")
                .bind(target)
                .bind(node_id)
                .execute(&mut *tx)
                .await?;
            out.push((node_id.clone(), target.to_string()));
        }
        tx.commit().await?;
        Ok(out)
    }

    /// Janitor sweep: delete every profile whose `expires_at` has passed.
    /// Behaves exactly like a manual DELETE — nodes are re-pointed off via
    /// ON DELETE SET NULL; the caller wakes them with a ConfigUpdate clear
    /// push. Returns `(profile_name, affected_node_ids)` per expired profile
    /// so the caller can push + the audit can stay honest.
    pub async fn expire_opencode_profiles(&self) -> Result<Vec<(String, Vec<String>)>> {
        let now = now_iso();
        let expired = sqlx::query(
            "SELECT id, name FROM opencode_profiles WHERE expires_at IS NOT NULL AND expires_at <= ?",
        )
        .bind(&now)
        .fetch_all(&self.pool)
        .await?;
        let mut out = Vec::new();
        for row in &expired {
            let id: String = row.get("id");
            let name: String = row.get("name");
            let nodes = self.list_nodes_for_profile(&id).await.unwrap_or_default();
            if let Err(e) = self.delete_opencode_profile(&name).await {
                tracing::warn!("opencode profile {name} expire delete failed: {e}");
                continue;
            }
            let detail = serde_json::json!({ "profile_name": name }).to_string();
            let _ = self
                .audit("system", None, "opencode.expire", None, Some(&detail))
                .await;
            out.push((name, nodes));
        }
        Ok(out)
    }

    /// Delete a profile while atomically re-pointing every node currently
    /// assigned to it onto a fallback profile. Single txn, so a node can't
    /// observe a half-state (reassigned but profile still alive / profile
    /// gone but nodes still pointing at it). Returns false when the target
    /// profile does not exist.
    pub async fn delete_opencode_profile_with_fallback(
        &self,
        name: &str,
        fallback_id: &str,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "UPDATE nodes SET opencode_profile_id = ?
             WHERE opencode_profile_id = (SELECT id FROM opencode_profiles WHERE name = ?)",
        )
        .bind(fallback_id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
        let r = sqlx::query("DELETE FROM opencode_profiles WHERE name = ?")
            .bind(name)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(r.rows_affected() > 0)
    }

    /// Resolve the profile assigned to a node (or None if unassigned / the
    /// profile was deleted and ON DELETE SET NULL fired).
    pub async fn node_opencode_profile(&self, node_id: &str) -> Result<Option<OpencodeProfile>> {
        let row = sqlx::query(
            "SELECT p.id, p.name, p.config_json, p.hash, p.prev_config_json, p.prev_hash, p.expires_at, p.pinned_skills_json, p.created_at, p.updated_at
             FROM opencode_profiles p
             JOIN nodes n ON n.opencode_profile_id = p.id
             WHERE n.id = ?",
        )
        .bind(node_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| row_to_profile(&r).into_view()).transpose()
    }

    /// Assign (`Some(id)`) or clear (`None`) the profile applied by a node.
    /// Returns (applied, profile_id, hash) — the caller uses the tuple to
    /// build the ConfigUpdate push. `applied=false` when the node id does
    /// not exist.
    pub async fn assign_opencode_profile(
        &self,
        node_id: &str,
        profile_id: Option<&str>,
    ) -> Result<(bool, Option<String>, Option<String>)> {
        let r = sqlx::query("UPDATE nodes SET opencode_profile_id = ? WHERE id = ?")
            .bind(profile_id)
            .bind(node_id)
            .execute(&self.pool)
            .await?;
        if r.rows_affected() == 0 {
            return Ok((false, None, None));
        }
        if let Some(pid) = profile_id {
            let row = sqlx::query("SELECT hash FROM opencode_profiles WHERE id = ?")
                .bind(pid)
                .fetch_optional(&self.pool)
                .await?;
            let hash: Option<String> = row.and_then(|r| r.try_get("hash").ok());
            Ok((true, Some(pid.to_string()), hash))
        } else {
            Ok((true, None, None))
        }
    }

    pub async fn list_nodes_for_profile(&self, profile_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT id FROM nodes WHERE opencode_profile_id = ?")
            .bind(profile_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(|r| r.get("id")).collect())
    }

    /// Record an application. Trigger vocabulary: ws_push | error_threshold
    /// | interval | startup. Best-effort; callers log the error, never fail
    /// the apply path on audit failure.
    pub async fn record_opencode_apply(
        &self,
        node_id: &str,
        profile_id: Option<&str>,
        hash: &str,
        trigger: &str,
        verify: Option<&str>,
        pinned_untrusted: Option<&[String]>,
    ) -> Result<()> {
        let pinned_json = match pinned_untrusted {
            Some(names) => canonicalize_pinned(names),
            None => None,
        };
        sqlx::query(
            "INSERT INTO opencode_config_audit (at, node_id, profile_id, hash, trigger, verify, pinned_untrusted_json)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(now_iso())
        .bind(node_id)
        .bind(profile_id)
        .bind(hash)
        .bind(trigger)
        .bind(verify)
        .bind(&pinned_json)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_opencode_audit(
        &self,
        node_id: &str,
        limit: u32,
    ) -> Result<Vec<OpencodeConfigAuditEntry>> {
        let rows = sqlx::query(
            "SELECT at, profile_id, hash, trigger, verify, pinned_untrusted_json
             FROM opencode_config_audit
             WHERE node_id = ?
             ORDER BY at DESC
             LIMIT ?",
        )
        .bind(node_id)
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let pinned_untrusted: Option<Vec<String>> = r
                    .try_get::<Option<String>, _>("pinned_untrusted_json")
                    .ok()
                    .flatten()
                    .and_then(|s| serde_json::from_str(&s).ok());
                OpencodeConfigAuditEntry {
                    at: r.get("at"),
                    profile_id: r.get("profile_id"),
                    hash: r.get("hash"),
                    trigger: r.get("trigger"),
                    verify: r.get("verify"),
                    pinned_untrusted,
                }
            })
            .collect())
    }
}
