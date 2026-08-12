//! opencode profile apply engine (feature "opencode profiles").
//!
//! The node writes the active profile to `~/.config/opencode/opencode.json`
//! atomically (tmp + fsync + rename), keeps a `.agentgrid.bak` backup, and
//! records each application through `POST /v1/node/opencode-config/audit`.
//!
//! Trigger vocabulary (must match the audit `trigger` column):
//!   "ws_push"        — CP multicast `ConfigUpdate` on the WS channel
//!   "startup"        — applied once when the node daemon starts
//!   "error_threshold" — pulled because an attempt hit N config-class errors
//!   "interval"       — pulled on the opt-in interval timer (OFF by default)
//!
//! Why atomic rename: a half-written `opencode.json` while the config file
//! is being written would corrupt the agent's next process spawn. The
//! rename is atomic on every filesystem we support (ext4, xfs, btrfs,
//! overlayfs in containers), so a crash mid-write leaves either the old
//! config or the new one, never a torn file.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::Digest;
use std::sync::Mutex;

/// Track the most recently applied profile hash so the heartbeat can report
/// it back. The CP compares against the assigned profile's hash — drift
/// (manual edit, mid-flight rollback the daemon hasn't caught yet) shows
/// up on the dashboard as a degraded node until the next pull/reapply.
static APPLIED_HASH: Mutex<Option<String>> = Mutex::new(None);

pub fn set_applied_hash(hash: String) {
    *APPLIED_HASH.lock().unwrap() = Some(hash);
}

#[allow(dead_code)]
pub fn clear_applied_hash() {
    *APPLIED_HASH.lock().unwrap() = None;
}

pub fn applied_hash() -> Option<String> {
    APPLIED_HASH.lock().unwrap().clone()
}

/// Where the profiled opencode config lives. `~/.config/opencode/opencode.json`
/// is the documented global-config location; per-attempt overrides go via
/// `OPENCODE_CONFIG_CONTENT` and never touch this file.
#[cfg(not(test))]
pub fn opencode_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    Path::new(&home).join(".config/opencode/opencode.json")
}

/// Test hook: isolated profile root under TMPDIR via `AG_OPENCODE_HOME`.
#[cfg(test)]
pub fn opencode_config_path() -> PathBuf {
    let home = std::env::var("AG_OPENCODE_HOME")
        .or_else(|_| std::env::var("HOME").map(|h| format!("{h}-test")))
        .unwrap_or_else(|_| "/tmp".into());
    Path::new(&home).join(".config/opencode/opencode.json")
}

/// Fetch the node's active opencode profile from the CP and apply it when
/// the hash differs from the on-disk copy. `trigger` labels the audit row
/// (ws_push | error_threshold | interval | startup). The pushed hash (when
/// present) is only a hint; the server's reading is authoritative so a
/// stale push from a restarted CP cannot roll the node back.
pub async fn pull_and_apply(
    cfg: &crate::config::Config,
    client: &reqwest::Client,
    cred: &crate::config::SavedCredential,
    trigger: &str,
    _pushed_hash: Option<&str>,
) -> Result<()> {
    let url = format!("{}/v1/node/opencode-config/active", cfg.server);
    let resp = client
        .get(&url)
        .bearer_auth(&cred.credential)
        .send()
        .await?;
    if !resp.status().is_success() {
        tracing::warn!(status = %resp.status(), "opencode profile pull failed");
        return Ok(());
    }
    let active: agentgrid_common::ActiveOpencodeConfigResponse = resp.json().await?;
    let target_hash = active.hash.as_deref();
    if current_hash().await.as_deref() == target_hash {
        return Ok(()); // already applied; no-op
    }
    let Some(config) = active.config else {
        // Profile unassigned or deleted: leave the on-disk config alone —
        // falling back to "no config" would surprise operators far more than
        // keeping the last-known-good one.
        tracing::info!(
            trigger,
            "no opencode profile assigned; keeping on-disk config"
        );
        return Ok(());
    };
    let json = serde_json::to_string(&config)?;
    let new_hash = apply_config(&json).await?;
    tracing::info!(
        profile_id = ?active.profile_id,
        hash = %new_hash,
        trigger,
        "opencode profile applied"
    );
    set_applied_hash(new_hash.clone());
    // Audit trail — best-effort; apply happened even if POST failed.
    let audit_url = format!("{}/v1/node/opencode-config/audit", cfg.server);
    let body = serde_json::json!({
        "profile_id": active.profile_id,
        "hash": new_hash,
        "trigger": trigger,
        "verify": oracle_flag_for_audit(),
    });
    let _ = client
        .post(&audit_url)
        .bearer_auth(&cred.credential)
        .json(&body)
        .send()
        .await;
    Ok(())
}

/// Current on-disk hash. None when no profile has been applied.
pub async fn current_hash() -> Option<String> {
    let path = opencode_config_path();
    let bytes = tokio::fs::read(&path).await.ok()?;
    Some(format!("{:x}", sha2::Sha256::digest(bytes)))
}

/// Apply the profile: write `<hash>.opencode.json.tmp` → fsync → rename over
/// `opencode.json`. Returns the new hash.
pub async fn apply_config(config_json: &str) -> Result<String> {
    let path = opencode_config_path();
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let new_hash = format!("{:x}", sha2::Sha256::digest(config_json.as_bytes()));
    // Skip no-op writes: the disk-side hash matches what we were about to
    // write. Profiles are PUT-idempotent server-side, this is the read side.
    if current_hash().await.as_deref() == Some(new_hash.as_str()) {
        return Ok(new_hash);
    }
    let tmp = path.with_extension("json.tmp");
    // Backup the previous config so a bad new profile can be reverted
    // manually. We intentionally do NOT auto-revert: opencode validates its
    // own config on load and falls back to defaults, so an actually broken
    // profile surfaces as adapter errors the node can react to.
    if let Ok(old) = tokio::fs::read(&path).await {
        let bak = path.with_extension("json.agentgrid.bak");
        let _ = tokio::fs::write(&bak, &old).await;
    }
    tokio::fs::write(&tmp, config_json).await?;
    let f = std::fs::File::open(&tmp)?;
    f.sync_all().ok();
    tokio::fs::rename(&tmp, &path)
        .await
        .context("atomic rename of opencode.json failed")?;
    // `opencode debug config` as the final oracle: the new file is on disk,
    // ask the binary whether it parses cleanly. Failure doesn't revert the
    // apply (we explicitly trust the server's allow-list instead of the
    // binary in the common case) — instead the audit row carries an
    // `extras.verify_outcome` flag so the dashboard can flag profiles
    // whose bodies parse on the CP but make the bin barf on the node.
    let verify = debug_config_oracle().await;
    let outcome = oracle_flag(&verify);
    set_oracle(outcome);
    tracing::info!(
        hash = %new_hash,
        verified = matches!(&verify, Ok(Some(_))),
        outcome,
        "opencode debug config oracle"
    );
    Ok(new_hash)
}

/// Run `opencode debug config` against the freshly-landed config. Returns
/// `Ok(stdout)` on success, Err with the binary's stderr on failure.
/// The binary is best-effort — when `opencode` isn't in PATH (e.g. only
/// mock adapters run today) the check is skipped, not failed.
pub async fn debug_config_oracle() -> Result<Option<String>> {
    match tokio::process::Command::new("opencode")
        .args(["debug", "config"])
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            Ok(Some(String::from_utf8_lossy(&out.stdout).into_owned()))
        }
        Ok(out) => anyhow::bail!(
            "opencode debug config failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(_) => Ok(None), // binary not installed — skip oracle
    }
}

/// Parse oracle outcome into a compact flag the audit row can carry.
pub fn oracle_flag(result: &Result<Option<String>>) -> &'static str {
    match result {
        Ok(Some(_)) => "verified",
        Ok(None) => "skipped_no_binary",
        Err(_) => "verify_failed",
    }
}

/// Last oracle outcome (for the audit POST — apply_config runs before
/// pull_and_apply has a chance to bind the flag, so we keep it on a
/// thread-local static).
static LAST_ORACLE: Mutex<Option<&'static str>> = Mutex::new(None);

pub fn set_oracle(outcome: &'static str) {
    *LAST_ORACLE.lock().unwrap() = Some(outcome);
}

pub fn last_oracle() -> Option<&'static str> {
    *LAST_ORACLE.lock().unwrap()
}

fn oracle_flag_for_audit() -> &'static str {
    last_oracle().unwrap_or("unknown")
}

/// Build the merged config used when launching the adapter:
///   base = on-disk profiled config (or `{}`)
///   plan = per-attempt override (model / small_model / partial config)
///
/// Shallow-merge semantics: each top-level key in the override wins. The
/// abuse vector (an override overriding the whole provider block) is fine —
/// the override lives only as long as the adapter process; the on-disk
/// profile is not modified.
pub async fn build_override_env(
    override_cfg: Option<&agentgrid_common::OpencodeOverride>,
) -> Option<String> {
    let o = override_cfg?;
    let base: serde_json::Value = match tokio::fs::read(&opencode_config_path()).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    };
    let mut merged = base.as_object().cloned().unwrap_or_default();
    if let Some(cfg) = &o.config {
        if let Some(obj) = cfg.as_object() {
            for (k, v) in obj {
                merged.insert(k.clone(), v.clone());
            }
        }
    }
    if let Some(m) = &o.model {
        merged.insert("model".into(), serde_json::json!(m));
    }
    if let Some(m) = &o.small_model {
        merged.insert("small_model".into(), serde_json::json!(m));
    }
    serde_json::to_string(&serde_json::Value::Object(merged)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests must not race on the process-wide AG_OPENCODE_HOME env var;
    // a process-local mutex serializes them.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[allow(dead_code)]
    struct TempHomeGuard(std::sync::MutexGuard<'static, ()>, tempfile::TempDir);

    fn temp_home_guard() -> TempHomeGuard {
        let g = HOME_LOCK.lock().unwrap();
        let td = tempfile::tempdir().unwrap();
        std::env::set_var("AG_OPENCODE_HOME", td.path().to_str().unwrap());
        TempHomeGuard(g, td)
    }

    fn temp_path() -> std::path::PathBuf {
        let root = std::env::var("AG_OPENCODE_HOME").unwrap();
        Path::new(&root).join(".config/opencode/opencode.json")
    }

    #[tokio::test]
    async fn apply_writes_file_and_hash_roundtrips() {
        let _g = temp_home_guard();
        let path = temp_path();
        let hash = apply_config("{\"model\":\"m1\"}").await.unwrap();
        assert!(path.exists());
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(format!("{:x}", sha2::Sha256::digest(&bytes)), hash);
        assert_eq!(current_hash().await.as_deref(), Some(hash.as_str()));
    }

    #[tokio::test]
    async fn apply_is_idempotent_under_no_change() {
        let _g = temp_home_guard();
        let h1 = apply_config("{\"model\":\"m1\"}").await.unwrap();
        let h2 = apply_config("{\"model\":\"m1\"}").await.unwrap();
        assert_eq!(h1, h2);
    }

    #[tokio::test]
    async fn build_override_merges_over_on_disk_base() {
        let _g = temp_home_guard();
        apply_config("{\"model\":\"m1\",\"snapshot\":true}")
            .await
            .unwrap();
        let o = agentgrid_common::OpencodeOverride {
            model: Some("m2".into()),
            small_model: None,
            config: Some(serde_json::json!({"temperature": 0.0})),
        };
        let env = build_override_env(Some(&o)).await.unwrap();
        let v: serde_json::Value = serde_json::from_str(&env).unwrap();
        assert_eq!(v.get("model").and_then(|m| m.as_str()), Some("m2"));
        assert_eq!(
            v.get("snapshot").and_then(|s| s.as_bool()),
            Some(true),
            "on-disk base key survives the merge"
        );
        assert_eq!(v.get("temperature").and_then(|t| t.as_f64()), Some(0.0));
    }
}
