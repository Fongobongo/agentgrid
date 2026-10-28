//! Snapshot replication (RPO = snapshot interval).
//!
//! The leader writes a consistent copy with the existing `VACUUM INTO` backup
//! path, hashes the *file*, and advertises the snapshot in `/v1/ha/status`.
//! A standby downloads it, verifies size + SHA-256 + `quick_check`, then
//! applies it in place (see `store::ha`) and records the applied sequence.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use super::protocol::{HaStatus, SnapshotManifest};
use super::role::HaRuntime;
use super::shared::HaShared;
use super::transport;
use crate::store::Store;

/// Snapshots kept on disk (current + one fallback for a slow standby).
const KEEP_SNAPSHOTS: usize = 2;

pub fn snapshot_id(epoch: u64, seq: u64) -> String {
    format!("ha-snap-{epoch}-{seq}")
}

pub fn db_file_name(id: &str) -> String {
    format!("{id}.db")
}

pub fn manifest_file_name(id: &str) -> String {
    format!("{id}.json")
}

/// `[a-zA-Z0-9._-]` only; the id is joined into a filesystem path.
pub fn is_safe_snapshot_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 96
        && id.starts_with("ha-snap-")
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_')
}

fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let mut f =
        std::fs::File::open(path).with_context(|| format!("open snapshot {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        hasher.update(&buf[..n]);
    }
    Ok((hex(&hasher.finalize()), size))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Leader: write `ha-snap-<epoch>-<seq>.db` + manifest and advertise it.
pub async fn create(
    store: &Store,
    shared: &HaShared,
    cluster_id: &str,
    epoch: u64,
    term: u64,
) -> Result<SnapshotManifest> {
    let dir = store.data_dir().to_path_buf();
    std::fs::create_dir_all(&dir)?;
    let seq = store.next_repl_seq().await?;
    let id = snapshot_id(epoch, seq);
    let db_name = db_file_name(&id);
    // `backup_to` refuses to overwrite; write a unique temp name then rename so
    // a leftover file from a crashed previous attempt cannot wedge replication.
    let tmp_name = format!("{db_name}.tmp-{}", uuid::Uuid::new_v4());
    store.backup_to(&tmp_name).await?;
    let db_path = dir.join(&db_name);
    std::fs::rename(dir.join(&tmp_name), &db_path)?;
    let (sha256, size) = {
        let p = db_path.clone();
        tokio::task::spawn_blocking(move || sha256_file(&p))
            .await
            .context("snapshot hash task panicked")??
    };
    let manifest = SnapshotManifest {
        cluster_id: cluster_id.to_string(),
        snapshot_id: id.clone(),
        epoch,
        term,
        seq,
        schema_version: store.schema_version().await?,
        sha256,
        size,
        created_unix: chrono::Utc::now().timestamp(),
    };
    let mpath = dir.join(manifest_file_name(&id));
    let tmp = mpath.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&manifest)?)?;
    std::fs::rename(&tmp, &mpath)?;
    shared.set_snapshot(Some(id), manifest.created_unix);
    prune(&dir);
    Ok(manifest)
}

pub fn read_manifest(dir: &Path, id: &str) -> Result<SnapshotManifest> {
    if !is_safe_snapshot_id(id) {
        anyhow::bail!("invalid snapshot id {id:?}");
    }
    let path = dir.join(manifest_file_name(id));
    let bytes = std::fs::read(&path)
        .with_context(|| format!("read snapshot manifest {}", path.display()))?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub fn snapshot_path(dir: &Path, id: &str) -> Result<PathBuf> {
    if !is_safe_snapshot_id(id) {
        anyhow::bail!("invalid snapshot id {id:?}");
    }
    Ok(dir.join(db_file_name(id)))
}

fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.starts_with("ha-snap-") && (name.ends_with(".db") || name.ends_with(".json"))
        })
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().ok()?, e.path()))
        })
        .collect();
    if files.len() <= KEEP_SNAPSHOTS * 2 {
        return;
    }
    files.sort_by_key(|(t, _)| *t);
    let drop = files.len() - KEEP_SNAPSHOTS * 2;
    for (_, path) in files.into_iter().take(drop) {
        let _ = std::fs::remove_file(path);
    }
}

/// Standby: fetch and apply the leader's advertised snapshot when it is newer
/// than what this node already has. Returns true when a snapshot was applied.
pub async fn apply_from(rt: &HaRuntime, store: &Store, status: &HaStatus) -> Result<bool> {
    let Some(id) = status.snapshot_id.as_deref() else {
        return Ok(false);
    };
    if status.applied_seq <= rt.shared.applied_seq()
        && status.snapshot_created_unix <= rt.shared.snapshot_created_unix()
    {
        return Ok(false);
    }
    let Some(leader_url) = status.leader_url.as_deref() else {
        return Ok(false);
    };
    // One restore at a time; re-check freshness under the lock because the
    // apply loop and an operator catch-up can race for the same snapshot.
    let _apply = rt.shared.apply_lock.lock().await;
    if status.applied_seq <= rt.shared.applied_seq()
        && status.snapshot_created_unix <= rt.shared.snapshot_created_unix()
    {
        return Ok(false);
    }
    let manifest = transport::manifest(rt, leader_url, id).await?;
    if manifest.cluster_id != rt.config.cluster_id {
        anyhow::bail!(
            "snapshot {} belongs to cluster {:?}, not {:?}",
            manifest.snapshot_id,
            manifest.cluster_id,
            rt.config.cluster_id
        );
    }
    let dir = store.data_dir().to_path_buf();
    let incoming = dir.join(format!("incoming-{id}.db"));
    let downloaded = transport::download(rt, leader_url, id, &incoming).await?;
    if downloaded != manifest.size {
        let _ = tokio::fs::remove_file(&incoming).await;
        anyhow::bail!(
            "snapshot {id} size mismatch: got {downloaded}, manifest {}",
            manifest.size
        );
    }
    let (sha, size) = {
        let p = incoming.clone();
        tokio::task::spawn_blocking(move || sha256_file(&p))
            .await
            .context("snapshot hash task panicked")??
    };
    if size != manifest.size || sha != manifest.sha256 {
        let _ = tokio::fs::remove_file(&incoming).await;
        anyhow::bail!("snapshot {id} integrity check failed (sha/size mismatch)");
    }
    let check = Store::verify_snapshot_file(&incoming).await?;
    if check.schema_version != manifest.schema_version {
        let _ = tokio::fs::remove_file(&incoming).await;
        anyhow::bail!(
            "snapshot {id} schema {} != manifest {}",
            check.schema_version,
            manifest.schema_version
        );
    }
    store.restore_snapshot(incoming.clone()).await?;
    let _ = tokio::fs::remove_file(&incoming).await;
    rt.shared.set_applied_seq(manifest.seq);
    rt.shared
        .set_snapshot(Some(manifest.snapshot_id.clone()), manifest.created_unix);
    rt.gate.set_epoch(store.ha_epoch().await?);
    rt.shared.set_leader(&status.node_id, leader_url);
    tracing::info!(snapshot = %manifest.snapshot_id, seq = manifest.seq, "applied HA snapshot");
    Ok(true)
}
