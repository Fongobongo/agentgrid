//! Snapshot replication (RPO = snapshot interval).
//!
//! The leader writes a consistent copy with the existing `VACUUM INTO` backup
//! path, hashes the *file*, and advertises the snapshot in `/v1/ha/status`.
//! A standby downloads it from the leader or object storage, verifies size +
//! SHA-256 + `quick_check`, then applies it in place (see `store::ha`) and
//! records the applied sequence.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use super::protocol::{HaStatus, SnapshotManifest};
use super::role::HaRuntime;
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

/// Leader: write `ha-snap-<epoch>-<seq>.db` + manifest, advertise it and (in
/// S3 backend mode) publish both to object storage.
///
/// In incremental modes the snapshot is cut at the current replication
/// sequence under the write gate, so it is a consistent base for the changesets
/// that follow. When nothing has committed since the advertised snapshot, that
/// snapshot is reused instead of writing an identical copy (and republished to
/// S3, which repairs a previous failed upload).
pub async fn create(rt: &HaRuntime, store: &Store) -> Result<SnapshotManifest> {
    let dir = store.data_dir().to_path_buf();
    std::fs::create_dir_all(&dir)?;
    if let Some(id) = rt.shared.snapshot_id() {
        if rt.shared.snapshot_seq() == store.ha_repl_seq().await? {
            if let Ok(m) = read_manifest(&dir, &id) {
                publish_s3(rt, &dir, &m).await?;
                return Ok(m);
            }
        }
    }
    // `backup_to` refuses to overwrite; write a unique temp name then rename so
    // a leftover file from a crashed previous attempt cannot wedge replication.
    let tmp_name = format!("ha-snap-tmp-{}.db", uuid::Uuid::new_v4());
    let seq = if store.replicator().is_some() {
        store.snapshot_to(&tmp_name).await?
    } else {
        let seq = store.next_repl_seq().await?;
        store.backup_to(&tmp_name).await?;
        seq
    };
    let id = snapshot_id(rt.gate.epoch(), seq);
    let db_name = db_file_name(&id);
    let db_path = dir.join(&db_name);
    std::fs::rename(dir.join(&tmp_name), &db_path)?;
    let (sha256, size) = {
        let p = db_path.clone();
        tokio::task::spawn_blocking(move || sha256_file(&p))
            .await
            .context("snapshot hash task panicked")??
    };
    let manifest = SnapshotManifest {
        cluster_id: rt.config.cluster_id.clone(),
        snapshot_id: id.clone(),
        epoch: rt.gate.epoch(),
        term: rt.shared.term(),
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
    rt.shared
        .set_snapshot(Some(id), manifest.seq, manifest.created_unix);
    prune(&dir);
    publish_s3(rt, &dir, &manifest).await?;
    Ok(manifest)
}

async fn publish_s3(rt: &HaRuntime, dir: &Path, manifest: &SnapshotManifest) -> Result<()> {
    let Some(s3) = &rt.s3 else {
        return Ok(());
    };
    let db_path = snapshot_path(dir, &manifest.snapshot_id)?;
    s3.put_snapshot(
        &manifest.snapshot_id,
        &db_path,
        &serde_json::to_vec_pretty(manifest)?,
    )
    .await
    .context("publish HA snapshot to object storage")?;
    // Everything at or below `manifest.seq` is inside the snapshot we just
    // published, so the older changeset batches are dead weight — without
    // this the bucket grows one object per second forever and every standby
    // LIST gets slower. A standby that still needed a pruned batch detects
    // the gap and restores this snapshot instead (documented behaviour).
    let keep = rt.config.changeset_retention;
    if keep > 0 && rt.is_leader() {
        match s3.prune_changesets(manifest.seq, keep).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(
                deleted = n,
                seq = manifest.seq,
                keep,
                "pruned object-storage changesets"
            ),
            // Pruning is housekeeping: never fail a good snapshot over it.
            Err(e) => tracing::warn!("changeset prune failed: {e}"),
        }
    }
    Ok(())
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

fn validate_manifest(rt: &HaRuntime, manifest: &SnapshotManifest) -> Result<()> {
    if manifest.cluster_id != rt.config.cluster_id {
        anyhow::bail!(
            "snapshot {} belongs to cluster {:?}, not {:?}",
            manifest.snapshot_id,
            manifest.cluster_id,
            rt.config.cluster_id
        );
    }
    if !is_safe_snapshot_id(&manifest.snapshot_id) {
        anyhow::bail!("invalid snapshot id {:?}", manifest.snapshot_id);
    }
    Ok(())
}

/// A snapshot is needed when this replica was demoted (its local copy may hold
/// unacknowledged writes), or when the advertised snapshot is newer than both
/// the applied position and the last snapshot this replica restored.
fn is_stale(rt: &HaRuntime, seq: u64, created_unix: i64) -> bool {
    rt.shared.need_full_snapshot()
        || (seq > rt.shared.applied_seq() && created_unix > rt.shared.snapshot_created_unix())
}

/// Standby: fetch and apply the leader's advertised snapshot when it is newer
/// than what this node already has, or always when this node demoted and its
/// local copy may hold unacknowledged writes. Returns true when a snapshot was
/// applied.
pub async fn apply_from(rt: &HaRuntime, store: &Store, status: &HaStatus) -> Result<bool> {
    let Some(id) = status.snapshot_id.as_deref() else {
        return Ok(false);
    };
    let Some(leader_url) = status.leader_url.as_deref() else {
        return Ok(false);
    };
    let schema_differs = status.schema_version != store.schema_version().await?;
    if !is_stale(rt, status.snapshot_seq, status.snapshot_created_unix) && !schema_differs {
        return Ok(false);
    }
    // A download can take minutes; changesets keep applying meanwhile, so only
    // the swap itself takes the restore lock. This lock dedupes parallel
    // downloads (apply loop vs. operator catch-up).
    let Ok(_snapshot) = rt.shared.snapshot_lock.try_lock() else {
        return Ok(false);
    };
    let schema_differs = status.schema_version != store.schema_version().await?;
    if !is_stale(rt, status.snapshot_seq, status.snapshot_created_unix) && !schema_differs {
        return Ok(false);
    }
    let manifest = transport::manifest(rt, leader_url, id).await?;
    validate_manifest(rt, &manifest)?;
    let dir = store.data_dir().to_path_buf();
    let incoming = dir.join(format!("incoming-{}.db", manifest.snapshot_id));
    transport::download(rt, leader_url, id, &incoming).await?;
    let applied = install_snapshot(rt, store, &manifest, &incoming).await?;
    if applied {
        rt.shared.set_leader(&status.node_id, leader_url);
    }
    Ok(applied)
}

/// Standby in S3 backend mode: poll the `latest.json` pointer directly. This
/// must work when the old leader is unreachable, which is exactly when a new
/// standby needs a base snapshot most.
pub async fn apply_from_s3(rt: &HaRuntime, store: &Store) -> Result<bool> {
    let Some(s3) = &rt.s3 else {
        return Ok(false);
    };
    let Some(bytes) = s3.latest_snapshot_manifest().await? else {
        return Ok(false);
    };
    let manifest: SnapshotManifest =
        serde_json::from_slice(&bytes).context("parse latest S3 snapshot manifest")?;
    validate_manifest(rt, &manifest)?;
    let schema_differs = manifest.schema_version != store.schema_version().await?;
    if !is_stale(rt, manifest.seq, manifest.created_unix) && !schema_differs {
        return Ok(false);
    }
    let Ok(_snapshot) = rt.shared.snapshot_lock.try_lock() else {
        return Ok(false);
    };
    let schema_differs = manifest.schema_version != store.schema_version().await?;
    if !is_stale(rt, manifest.seq, manifest.created_unix) && !schema_differs {
        return Ok(false);
    }
    let dir = store.data_dir().to_path_buf();
    let incoming = dir.join(format!("incoming-{}.db", manifest.snapshot_id));
    s3.download_snapshot(&manifest.snapshot_id, &incoming)
        .await?;
    install_snapshot(rt, store, &manifest, &incoming).await
}

/// Verify and restore a downloaded snapshot. The caller holds
/// `snapshot_lock`; this function takes `apply_lock` only for the swap.
async fn install_snapshot(
    rt: &HaRuntime,
    store: &Store,
    manifest: &SnapshotManifest,
    incoming: &Path,
) -> Result<bool> {
    let (sha, size) = {
        let p = incoming.to_path_buf();
        tokio::task::spawn_blocking(move || sha256_file(&p))
            .await
            .context("snapshot hash task panicked")??
    };
    if size != manifest.size || sha != manifest.sha256 {
        let _ = tokio::fs::remove_file(incoming).await;
        anyhow::bail!(
            "snapshot {} integrity check failed (sha/size mismatch)",
            manifest.snapshot_id
        );
    }
    let check = Store::verify_snapshot_file(incoming).await?;
    if check.schema_version != manifest.schema_version {
        let _ = tokio::fs::remove_file(incoming).await;
        anyhow::bail!(
            "snapshot {} schema {} != manifest {}",
            manifest.snapshot_id,
            check.schema_version,
            manifest.schema_version
        );
    }
    let schema_mismatch = check.schema_version != store.schema_version().await?;
    let applied = {
        let _apply = rt.shared.apply_lock.lock().await;
        let forced = rt.shared.need_full_snapshot() || schema_mismatch;
        // Increments may have caught up past the snapshot while it downloaded;
        // restoring would roll them back.
        let local_seq = store.ha_repl_seq().await?;
        if !forced && local_seq >= manifest.seq {
            if rt.shared.applied_seq() < local_seq {
                rt.shared.set_applied_seq(local_seq);
            }
            let _ = tokio::fs::remove_file(incoming).await;
            return Ok(false);
        }
        if rt.shared.need_full_snapshot() {
            let _ = store.quarantine(rt.gate.epoch()).await;
        }
        store.restore_snapshot(incoming.to_path_buf()).await?;
        true
    };
    let _ = tokio::fs::remove_file(incoming).await;
    rt.shared.set_applied_seq(manifest.seq);
    rt.shared.set_snapshot(
        Some(manifest.snapshot_id.clone()),
        manifest.seq,
        manifest.created_unix,
    );
    rt.gate.set_epoch(store.ha_epoch().await?);
    rt.shared.clear_need_full_snapshot();
    tracing::info!(
        snapshot = %manifest.snapshot_id,
        seq = manifest.seq,
        "applied HA snapshot"
    );
    Ok(applied)
}
