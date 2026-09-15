//! Artifact storage (bytes + metadata, path-safe). Extracted from `store.rs`.

use super::{
    is_safe_artifact_name, is_safe_opaque_id, now_iso, sha256_bytes_hex, Store, StoreArtifactError,
};
use agentgrid_common::{ArtifactMeta, ArtifactUploadResponse, UploadArtifactRequest};
use anyhow::Result;
use sqlx::Row;
use uuid::Uuid;

impl Store {
    // ----- artifacts (Stage 2.8) -----

    /// Persist an artifact's bytes on the control-plane filesystem and record
    /// its metadata. `content` is treated as UTF-8 text (patches/logs).
    /// Resolve `attempt_id/name` to an absolute path inside the artifact root,
    /// rejecting traversal. Canonicalizes the parent (created lazily) and checks
    /// the final name is a single safe segment so a symlinked worktree dir or a
    /// `..`-laden name cannot escape the root (Stage 2.2 defense-in-depth).
    pub(crate) fn artifact_path(&self, attempt_id: &str, name: &str) -> Result<std::path::PathBuf> {
        // Hardening P0: validate attempt_id as a safe opaque ID before it is
        // joined into a filesystem path. IDs are UUIDv4 (36 hex+hyphens) but we
        // accept any short `[A-Za-z0-9_-]` token so future ID schemes stay safe.
        // This is defense-in-depth: handler-level ownership checks already reject
        // unknown IDs, but a malformed ID must never reach a path join.
        if !is_safe_opaque_id(attempt_id) {
            anyhow::bail!("invalid attempt_id");
        }
        if !is_safe_artifact_name(name) {
            anyhow::bail!("invalid artifact name");
        }
        let dir = self.artifact_root.join(attempt_id);
        // Reject a symlinked attempt dir before any canonical check — a
        // symlink pointing outside the root would otherwise escape even with
        // a safe name.
        if let Ok(md) = std::fs::symlink_metadata(&dir) {
            if md.file_type().is_symlink() {
                anyhow::bail!("artifact dir is a symlink");
            }
        }
        // If the dir already exists, verify it is still inside the (canonical)
        // artifact root; if it does not exist yet (write path just created it
        // via create_dir_all, read path will 404 anyway) skip the canonical
        // dance — is_safe_opaque_id already guarantees `dir` is lexically
        // inside the root, so there is nothing to escape.
        let file_path = if let Ok(canon_dir) = dir.canonicalize() {
            let canon_root = self
                .artifact_root
                .canonicalize()
                .unwrap_or_else(|_| self.artifact_root.clone());
            if !canon_dir.starts_with(&canon_root) {
                anyhow::bail!("artifact dir escapes root");
            }
            canon_dir.join(name)
        } else {
            // dir does not exist yet — lexical join is safe (opaque id + safe
            // name cannot contain separators).
            dir.join(name)
        };
        // Hardening P0: the resolved file itself must not be a symlink.
        if let Ok(md) = std::fs::symlink_metadata(&file_path) {
            if md.file_type().is_symlink() {
                anyhow::bail!("artifact file is a symlink");
            }
        }
        Ok(file_path)
    }

    pub async fn save_artifact(
        &self,
        attempt_id: &str,
        req: &UploadArtifactRequest,
    ) -> Result<ArtifactUploadResponse, StoreArtifactError> {
        self.save_artifact_bytes(
            attempt_id,
            &req.name,
            req.content.as_bytes(),
            req.media_type.as_deref(),
            req.sha256.as_deref(),
        )
        .await
    }

    /// Stage 2.2 binary-safe artifact write: raw bytes + optional media type
    /// and hex SHA-256. Idempotent per (attempt_id, name). The legacy text
    /// endpoint forwards here with `content.as_bytes()`.
    ///
    /// Plan 6.7 (zstd log compression): log-class artifacts (`.log` names /
    /// text/plain) whose content compresses by at least 30% are stored as
    /// `<name>.zst` with the row flagging `stored_compressed`; everything else
    /// (patches, binaries, small logs) stays plain. `size_bytes` always keeps
    /// the UNCOMPRESSED length — that is the logical size every API/Range
    /// speaks; the on-disk shape is an internal detail.
    pub async fn save_artifact_bytes(
        &self,
        attempt_id: &str,
        name: &str,
        bytes: &[u8],
        media_type: Option<&str>,
        sha256: Option<&str>,
    ) -> Result<ArtifactUploadResponse, StoreArtifactError> {
        // Hardening P0: validate attempt_id before any filesystem path join.
        if !is_safe_opaque_id(attempt_id) {
            return Err(StoreArtifactError::InvalidAttemptId);
        }
        // Hardening P0 (artifact integrity): always compute the server-side
        // SHA-256 of the uploaded bytes. If the caller supplied a sha256 hint
        // (JSON `sha256` field or raw `x-artifact-sha256` header) and it
        // disagrees, reject with `HashMismatch` (handler -> 422). We store
        // only the computed server-side hash, never the client value.
        let computed = sha256_bytes_hex(bytes);
        if let Some(expected) = sha256 {
            let expected = expected.trim().to_ascii_lowercase();
            if !expected.is_empty() && expected != computed {
                return Err(StoreArtifactError::HashMismatch { expected, computed });
            }
        }
        let dir = self.artifact_root.join(attempt_id);
        tokio::fs::create_dir_all(&dir).await?;
        let path = self.artifact_path(attempt_id, name)?;
        // Plan 6.7: compress log-class artifacts when it pays. The 30% floor
        // keeps small/garbage-ish logs plain (a .zst that saves nothing just
        // adds a decode cost to every download). The compressed candidate is
        // built in a blocking task — zstd level 3 on a multi-MiB log is CPU
        // work, not async work.
        let log_class =
            name.ends_with(".log") || media_type.map(|m| m.starts_with("text/")).unwrap_or(false);
        let (write_bytes, stored_compressed, on_disk_name) = if log_class && bytes.len() >= 4096 {
            let src = bytes.to_vec();
            let name_probe = name.to_string();
            let compressed = tokio::task::spawn_blocking(move || {
                compress_if_worthwhile(&name_probe, &src, 0.30)
            })
            .await
            .unwrap_or(None);
            match compressed {
                Some(z) => (z, true, format!("{name}.zst")),
                // Incompressible or < 30% win: keep the plain shape.
                None => (bytes.to_vec(), false, name.to_string()),
            }
        } else {
            (bytes.to_vec(), false, name.to_string())
        };
        // Hardening P0 (crash safety): write to a sibling temp file then
        // atomic rename, so a crash between write and metadata commit cannot
        // leave a half-written published artifact. Same dir => same fs rename.
        // The uuid suffix keeps concurrent uploads of the same artifact (an
        // in-flight retry racing the original request) from clobbering each
        // other's tmp file — last-rename-wins bytes could mismatch the sha
        // row the other writer committed, and on Windows the second rename
        // onto an existing target fails outright.
        let disk_path = path.with_file_name(&on_disk_name);
        let tmp = disk_path.with_extension(format!("tmp.upload-{}", Uuid::new_v4()));
        tokio::fs::write(&tmp, write_bytes).await?;
        tokio::fs::rename(&tmp, &disk_path).await?;
        // A shape change (plain -> .zst or back) must not leave the previous
        // shape behind as a shadow both readers and retention would miss.
        if on_disk_name != name {
            let _ = tokio::fs::remove_file(&path).await;
        }
        let size = bytes.len() as i64;
        let id = Uuid::new_v4().to_string();
        let now = now_iso();
        sqlx::query(
            "INSERT INTO artifacts (id, attempt_id, name, size_bytes, stored_at, media_type, sha256, stored_compressed) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(attempt_id, name) DO UPDATE SET \
                size_bytes = excluded.size_bytes, \
                stored_at = excluded.stored_at, \
                media_type = excluded.media_type, \
                sha256 = excluded.sha256, \
                stored_compressed = excluded.stored_compressed",
        )
        .bind(&id)
        .bind(attempt_id)
        .bind(name)
        .bind(size)
        .bind(&now)
        .bind(media_type)
        .bind(&computed)
        .bind(stored_compressed as i64)
        .execute(&self.pool)
        .await?;
        Ok(ArtifactUploadResponse {
            name: name.to_string(),
            size_bytes: size,
            media_type: media_type.map(|s| s.to_string()),
            sha256: computed,
        })
    }

    /// List a task's artifacts (latest attempt) with metadata. Empty when the
    /// task has no attempts or none uploaded artifacts. Plan 1.11 (#8) SDK
    /// `artifacts()` uses this.
    pub async fn list_artifacts(&self, task_id: &str) -> Result<Vec<ArtifactMeta>> {
        let Some(attempt_id) = self.latest_attempt_id(task_id).await? else {
            return Ok(vec![]);
        };
        let rows = sqlx::query(
            "SELECT name, size_bytes, media_type, sha256 FROM artifacts \
             WHERE attempt_id = ? ORDER BY name",
        )
        .bind(&attempt_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| ArtifactMeta {
                name: r.try_get("name").unwrap_or_default(),
                size_bytes: r.try_get::<i64, _>("size_bytes").unwrap_or(0),
                media_type: r.try_get::<Option<String>, _>("media_type").ok().flatten(),
                sha256: r.try_get::<Option<String>, _>("sha256").ok().flatten(),
            })
            .collect())
    }

    /// Read a stored artifact's metadata by task id + name (latest attempt).
    pub async fn read_artifact_meta(
        &self,
        task_id: &str,
        name: &str,
    ) -> Result<Option<ArtifactMeta>> {
        let Some(attempt_id) = self.latest_attempt_id(task_id).await? else {
            return Ok(None);
        };
        let row = sqlx::query(
            "SELECT size_bytes, media_type, sha256 FROM artifacts WHERE attempt_id = ? AND name = ?",
        )
        .bind(&attempt_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| ArtifactMeta {
            name: name.to_string(),
            size_bytes: r.try_get::<i64, _>("size_bytes").unwrap_or(0),
            media_type: r.try_get::<Option<String>, _>("media_type").ok().flatten(),
            sha256: r.try_get::<Option<String>, _>("sha256").ok().flatten(),
        }))
    }

    /// Read a stored artifact's raw bytes by task id + name (latest attempt).
    pub async fn read_artifact_bytes(&self, task_id: &str, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(attempt_id) = self.latest_attempt_id(task_id).await? else {
            return Ok(None);
        };
        let path = match self.artifact_path(&attempt_id, name) {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        let (path, compressed) = resolve_on_disk(&path, &attempt_id, name, &self.pool).await;
        match tokio::fs::read(&path).await {
            Ok(b) => Ok(Some(if compressed {
                decompress_blocking(b).await
            } else {
                b
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Resolve the latest attempt id for a task (artifacts are per-attempt).
    pub async fn latest_attempt_id(&self, task_id: &str) -> Result<Option<String>> {
        let row =
            sqlx::query("SELECT id FROM attempts WHERE task_id = ? ORDER BY number DESC LIMIT 1")
                .bind(task_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|r| r.try_get::<String, _>("id")).transpose()?)
    }

    /// Read a stored artifact's content by task id + name (latest attempt).
    pub async fn read_artifact(&self, task_id: &str, name: &str) -> Result<Option<String>> {
        let Some(attempt_id) = self.latest_attempt_id(task_id).await? else {
            return Ok(None);
        };
        self.read_artifact_for_attempt(&attempt_id, name).await
    }

    /// Read a stored artifact's content by exact attempt id + name (any
    /// attempt, not just the latest). Plan 2.5 (#22b) needs this to fetch
    /// eval-case contents for historical attempts when a retry runs.
    pub async fn read_artifact_for_attempt(
        &self,
        attempt_id: &str,
        name: &str,
    ) -> Result<Option<String>> {
        let path = match self.artifact_path(attempt_id, name) {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        let (path, compressed) = resolve_on_disk(&path, attempt_id, name, &self.pool).await;
        match tokio::fs::read_to_string(&path).await {
            Ok(s) => Ok(Some(if compressed {
                // Compressed logs are bytes on disk; decode then re-read as
                // UTF-8 (lossy keeps the String contract of this method).
                let b = tokio::fs::read(&path).await.unwrap_or_default();
                String::from_utf8_lossy(&decompress_blocking(b).await).to_string()
            } else {
                s
            })),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Plan 6.7 (streaming downloads): resolve a stored artifact to an
    /// open file handle + its byte length, WITHOUT reading the content into
    /// RAM. `Range` handling (start/end) is the caller's — this returns the
    /// full-file handle so a serve path can seek anywhere. `None` when the
    /// task has no attempts / the artifact is missing (same semantics as
    /// `read_artifact_bytes`, so handlers keep a single 404 path).
    ///
    /// Plan 6.7 (zstd): the returned struct flags a compressed backing
    /// file — the serve path streams a decode of it (Content-Length stays
    /// the logical uncompressed length) and ignores Range per RFC 9110
    /// §14.2 (a server may ignore a Range header; it must then send the
    /// full representation).
    pub async fn open_artifact(&self, task_id: &str, name: &str) -> Result<Option<OpenArtifact>> {
        let Some(attempt_id) = self.latest_attempt_id(task_id).await? else {
            return Ok(None);
        };
        let path = match self.artifact_path(&attempt_id, name) {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        let (disk_path, compressed) = resolve_on_disk(&path, &attempt_id, name, &self.pool).await;
        match tokio::fs::File::open(&disk_path).await {
            Ok(file) => {
                // Logical length: the DB row's uncompressed size_bytes.
                let len = sqlx::query_scalar::<_, i64>(
                    "SELECT size_bytes FROM artifacts WHERE attempt_id = ? AND name = ?",
                )
                .bind(&attempt_id)
                .bind(name)
                .fetch_optional(&self.pool)
                .await?
                .unwrap_or(0) as u64;
                Ok(Some(OpenArtifact {
                    file,
                    len,
                    compressed,
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// Plan 6.7: an artifact opened for streaming. `len` is the logical
/// (uncompressed) byte length; `compressed` says the file handle holds a
/// zstd frame that must be decoded on the wire.
pub struct OpenArtifact {
    pub file: tokio::fs::File,
    pub len: u64,
    pub compressed: bool,
}

/// Test-visible alias for the pure compression decision (routes tests prove
/// the 30% floor + round-trip without touching the store internals).
pub fn compress_if_worthwhile_pub(name: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    compress_if_worthwhile(name, bytes, 0.30)
}

/// Pure decision + compression for Plan 6.7: return the zstd frame when the
/// content compresses by at least `min_ratio` (0.30 = 30% smaller), else None.
/// Runs inside `spawn_blocking` at the call site.
pub(crate) fn compress_if_worthwhile(name: &str, bytes: &[u8], min_ratio: f64) -> Option<Vec<u8>> {
    use std::io::Write;
    let mut enc =
        zstd::stream::Encoder::new(Vec::new(), 3).expect("zstd encoder to memory cannot fail");
    // write_all to a Vec writer cannot fail except for the source read,
    // which is a slice.
    if enc.write_all(bytes).is_err() {
        return None;
    }
    let z = enc.finish().ok()?;
    let win = 1.0 - (z.len() as f64 / bytes.len() as f64);
    if win >= min_ratio {
        tracing::debug!(
            artifact = name,
            raw = bytes.len(),
            zst = z.len(),
            "artifact stored zstd-compressed"
        );
        Some(z)
    } else {
        None
    }
}

/// Decode a zstd frame off the async runtime (multi-MiB logs are CPU work).
async fn decompress_blocking(bytes: Vec<u8>) -> Vec<u8> {
    tokio::task::spawn_blocking(move || {
        zstd::stream::decode_all(bytes.as_slice()).unwrap_or_else(|e| {
            tracing::warn!("artifact zstd decode failed: {e}");
            Vec::new()
        })
    })
    .await
    .unwrap_or_default()
}

/// Plan 6.7 (zstd): pick the on-disk shape for an artifact. The row's
/// `stored_compressed` flag is authoritative; a missing/zero flag falls back
/// to the plain name (legacy rows + pre-migration files). Also heals a race
/// where the flag says .zst but only the plain file exists (an in-flight
/// re-upload between shapes): prefer whichever file is actually present.
async fn resolve_on_disk(
    plain_path: &std::path::Path,
    attempt_id: &str,
    name: &str,
    pool: &sqlx::SqlitePool,
) -> (std::path::PathBuf, bool) {
    let flagged: Option<i64> = sqlx::query_scalar::<_, i64>(
        "SELECT stored_compressed FROM artifacts WHERE attempt_id = ? AND name = ?",
    )
    .bind(attempt_id)
    .bind(name)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let zst = plain_path.with_file_name(format!("{name}.zst"));
    match flagged {
        Some(1) => {
            if tokio::fs::try_exists(&zst).await.unwrap_or(false) {
                (zst, true)
            } else {
                // Flag says compressed but the frame is gone (crash between
                // rename and commit is impossible — both precede the row —
                // but an operator restore may have dropped it): serve the
                // plain file if it exists, else the missing .zst (→ 404).
                if tokio::fs::try_exists(plain_path).await.unwrap_or(false) {
                    (plain_path.to_path_buf(), false)
                } else {
                    (zst, true)
                }
            }
        }
        _ => (plain_path.to_path_buf(), false),
    }
}
