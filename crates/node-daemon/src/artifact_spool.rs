//! Durable artifact spool (Hardening P1 item 11).
//!
//! An artifact produced by an attempt (e.g. `changes.patch`) is staged here
//! BEFORE upload so a control-plane outage mid-upload cannot lose it: the
//! worktree is deleted after the attempt, but the staged copy survives and is
//! retried on the next daemon startup. Files live at
//! `<root>/<attempt_id>/<name>`; the attempt id and name are sanitized to
//! safe path segments so nothing can escape the spool root.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

/// Sanitize a path segment to `[A-Za-z0-9_-]` (UUID-ish ids and artifact names
/// like `changes.patch` are safe; anything else is scrubbed). Empty results
/// are refused by the callers.
fn safe_segment(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
        .collect()
}

fn entry_path(root: &Path, attempt_id: &str, name: &str) -> Result<PathBuf> {
    let aid = safe_segment(attempt_id);
    if aid.is_empty() || aid == "." || aid == ".." || aid.starts_with('.') {
        anyhow::bail!("unsafe attempt_id segment");
    }
    let nm = safe_segment(name);
    if nm.is_empty() || nm == "." || nm == ".." || nm.starts_with('.') {
        anyhow::bail!("unsafe artifact name segment");
    }
    Ok(root.join(aid).join(nm))
}

/// Copy `path` into the spool atomically (temp sibling + rename). Idempotent:
/// re-staging the same (attempt_id, name) replaces the previous copy.
pub fn stage(root: &Path, attempt_id: &str, name: &str, path: &Path) -> Result<PathBuf> {
    let dest = entry_path(root, attempt_id, name)?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create spool dir {}", parent.display()))?;
    }
    let tmp = dest.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("create spool temp {}", tmp.display()))?;
        let mut src = std::fs::File::open(path)
            .with_context(|| format!("open source artifact {}", path.display()))?;
        std::io::copy(&mut src, &mut f)
            .with_context(|| format!("copy artifact into spool {}", dest.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync spool temp {}", tmp.display()))?;
    }
    std::fs::rename(&tmp, &dest)
        .with_context(|| format!("publish spool entry {}", dest.display()))?;
    Ok(dest)
}

/// All staged files, as `(attempt_id, name, path)`, in attempt-id order.
pub fn pending(root: &Path) -> Result<Vec<(String, String, PathBuf)>> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for attempt in entries {
        let attempt = attempt?;
        if !attempt.file_type()?.is_dir() {
            continue;
        }
        let aid = attempt.file_name().to_string_lossy().to_string();
        for file in std::fs::read_dir(attempt.path())? {
            let file = file?;
            if !file.file_type()?.is_file() {
                continue;
            }
            let name = file.file_name().to_string_lossy().to_string();
            out.push((aid.clone(), name, file.path()));
        }
    }
    Ok(out)
}

/// Remove a staged artifact once it has been uploaded and acked.
pub fn remove(root: &Path, attempt_id: &str, name: &str) -> Result<()> {
    let p = entry_path(root, attempt_id, name)?;
    match std::fs::remove_file(&p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Remove an entire attempt directory (all artifacts for that attempt).
fn remove_attempt_dir(root: &Path, attempt_id: &str) -> Result<()> {
    let aid = safe_segment(attempt_id);
    if aid.is_empty() || aid == "." || aid == ".." || aid.starts_with('.') {
        anyhow::bail!("unsafe attempt_id segment");
    }
    let attempt_dir = root.join(aid);
    match std::fs::remove_dir_all(&attempt_dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Recover orphaned artifact spool entries.
///
/// Removes attempt directories that have not been modified for longer than
/// `max_age`. This cleans up artifacts from attempts that were cancelled,
/// failed, or otherwise abandoned and will never be uploaded.
/// Returns the number of attempt directories removed.
pub fn recover_orphans(root: &Path, max_age: Duration) -> Result<usize> {
    let now = SystemTime::now();
    let mut removed = 0;

    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };

    for attempt in entries {
        let attempt = attempt?;
        if !attempt.file_type()?.is_dir() {
            continue;
        }
        let aid = attempt.file_name().to_string_lossy().to_string();

        // Check the directory's modified time
        let metadata = match attempt.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let modified = match metadata.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };

        if now.duration_since(modified).unwrap_or(Duration::ZERO) > max_age {
            tracing::info!(
                attempt_id = %aid,
                age_hours = now.duration_since(modified).unwrap_or_default().as_secs() / 3600,
                "removing orphaned artifact spool entry"
            );
            if remove_attempt_dir(root, &aid).is_ok() {
                removed += 1;
            }
        }
    }

    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ag-artspool-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn stage_pending_remove_round_trip() {
        let root = tmpdir("roundtrip");
        let src = root.join("src.txt");
        std::fs::write(&src, b"hello artifact").unwrap();

        let staged = stage(&root, "attempt-1", "changes.patch", &src).unwrap();
        assert!(staged.ends_with("attempt-1/changes.patch"));

        let pend = pending(&root).unwrap();
        assert_eq!(pend.len(), 1);
        assert_eq!(pend[0].0, "attempt-1");
        assert_eq!(pend[0].1, "changes.patch");
        assert_eq!(std::fs::read(&pend[0].2).unwrap(), b"hello artifact");

        remove(&root, "attempt-1", "changes.patch").unwrap();
        assert!(pending(&root).unwrap().is_empty());
    }

    #[test]
    fn restage_overwrites() {
        let root = tmpdir("restage");
        let a = root.join("a.txt");
        let b = root.join("b.txt");
        std::fs::write(&a, b"v1").unwrap();
        std::fs::write(&b, b"v2-longer").unwrap();
        stage(&root, "a1", "f.bin", &a).unwrap();
        stage(&root, "a1", "f.bin", &b).unwrap();
        let pend = pending(&root).unwrap();
        assert_eq!(pend.len(), 1);
        assert_eq!(std::fs::read(&pend[0].2).unwrap(), b"v2-longer");
    }

    #[test]
    fn traversal_attempt_id_is_refused() {
        let root = tmpdir("traversal");
        let src = root.join("src.txt");
        std::fs::write(&src, b"x").unwrap();
        assert!(stage(&root, "../evil", "f.bin", &src).is_err());
    }

    #[test]
    fn remove_is_scoped_to_attempt_and_name() {
        let root = tmpdir("rm-scope");
        let src = root.join("s.txt");
        std::fs::write(&src, b"x").unwrap();
        stage(&root, "a1", "one.txt", &src).unwrap();
        stage(&root, "a1", "two.txt", &src).unwrap();
        stage(&root, "a2", "one.txt", &src).unwrap();
        // Removing one name from one attempt leaves the rest untouched.
        remove(&root, "a1", "one.txt").unwrap();
        let pend = pending(&root).unwrap();
        let names: Vec<(&str, &str)> = pend
            .iter()
            .map(|(a, n, _)| (a.as_str(), n.as_str()))
            .collect();
        assert!(names.contains(&("a1", "two.txt")));
        assert!(names.contains(&("a2", "one.txt")));
        assert!(!names.contains(&("a1", "one.txt")));
    }

    #[test]
    fn recover_orphans_removes_old_entries() {
        let root = tmpdir("orphans");
        let src = root.join("s.txt");
        std::fs::write(&src, b"x").unwrap();

        // Create old attempt directory
        stage(&root, "old-attempt", "file.txt", &src).unwrap();

        // Sleep > 1 second to ensure mtime difference (filesystem resolution)
        sleep(Duration::from_secs(2));

        // Create new attempt directory (more recent)
        stage(&root, "new-attempt", "file.txt", &src).unwrap();

        // Recover with 1 second max_age - only old-attempt should be removed
        let removed = recover_orphans(&root, Duration::from_secs(1)).unwrap();
        assert_eq!(removed, 1);

        let pend = pending(&root).unwrap();
        assert_eq!(pend.len(), 1);
        assert_eq!(pend[0].0, "new-attempt");
    }

    #[test]
    fn recover_orphans_respects_max_age() {
        let root = tmpdir("orphans-age");
        let src = root.join("s.txt");
        std::fs::write(&src, b"x").unwrap();

        stage(&root, "attempt-1", "file.txt", &src).unwrap();

        // Max age 1 hour - should NOT remove
        let removed = recover_orphans(&root, Duration::from_secs(3600)).unwrap();
        assert_eq!(removed, 0);

        let pend = pending(&root).unwrap();
        assert_eq!(pend.len(), 1);
    }
}
