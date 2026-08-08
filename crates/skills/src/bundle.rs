//! Stage 4.2: skill trust gate + bundles + materialization + profile revisions.
//!
//! ponytail: minimal but testable. Bundle sources are `filesystem`/`git` (git is
//! a pin record only — actual git fetch is the node's job). Materialization
//! copies the original `SKILL.md` verbatim (preserving content + hash) rather
//! than re-serializing the parsed struct. Revisions use a symlinked `active`
//! pointer for transactional flip + rollback (Linux-only, per project constraints).

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{DiscoveredSkill, SkillSource};

/// Deterministic sha256 hex of skill content (used for lock verification).
pub fn compute_skill_hash(content: &str) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    format!("{:x}", h.finalize())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BundleSource {
    Filesystem { path: PathBuf },
    Git { url: String, rev: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum SkillPin {
    Commit(String),
    Hash(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SkillRef {
    pub name: String,
    pub source: BundleSource,
    pub pin: SkillPin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LockEntry {
    pub name: String,
    pub hash: String,
}

/// A skill bundle manifest: where skills come from and what hashes are pinned.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SkillBundle {
    pub skills: Vec<SkillRef>,
    pub lock: Vec<LockEntry>,
}

/// Decides which skills may activate. Project skills are untrusted by default
/// (malicious-repo protection); user/managed skills are trusted.
#[derive(Debug, Clone, Default)]
pub struct TrustStore {
    trusted: HashSet<(SkillSource, String)>,
}

impl TrustStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn trust(&mut self, source: SkillSource, name: &str) {
        self.trusted.insert((source, name.to_string()));
    }

    pub fn is_trusted(&self, source: SkillSource, name: &str) -> bool {
        match source {
            SkillSource::Project => self.trusted.contains(&(source, name.to_string())),
            SkillSource::User | SkillSource::Managed => true,
        }
    }
}

#[derive(Debug, Error, PartialEq)]
pub enum MaterializeError {
    #[error("untrusted project skill '{0}' not materialized")]
    Untrusted(String),
    #[error("io error for '{0}': {1}")]
    Io(String, String),
    #[error("hash mismatch for '{0}': expected {1}, got {2}")]
    HashMismatch(String, String, String),
}

/// A skill written to disk during materialization.
#[derive(Debug, Clone, PartialEq)]
pub struct MaterializedSkill {
    pub name: String,
    pub path: PathBuf,
    pub hash: String,
}

/// Copy each discovered skill's original `SKILL.md` into `<dest>/<name>/SKILL.md`.
///
/// Project skills absent from `trust` are skipped (and reported in the returned
/// `skipped` list) — they never reach an agent. When `expected` is `Some`, the
/// written content's hash is checked against the lock and a mismatch is fatal.
pub fn materialize(
    skills: &[DiscoveredSkill],
    dest: &Path,
    trust: &TrustStore,
    expected: Option<&HashMap<String, String>>,
) -> Result<(Vec<MaterializedSkill>, Vec<String>), MaterializeError> {
    let mut written = Vec::new();
    let mut skipped = Vec::new();

    for ds in skills {
        let name = &ds.skill.name;
        if ds.source == SkillSource::Project && !trust.is_trusted(ds.source, name) {
            skipped.push(name.clone());
            continue;
        }
        let content = std::fs::read_to_string(&ds.path)
            .map_err(|e| MaterializeError::Io(ds.path.display().to_string(), e.to_string()))?;
        let hash = compute_skill_hash(&content);

        if let Some(exp) = expected {
            if let Some(want) = exp.get(name) {
                if want != &hash {
                    return Err(MaterializeError::HashMismatch(
                        name.clone(),
                        want.clone(),
                        hash,
                    ));
                }
            }
        }

        let out_dir = dest.join(name);
        std::fs::create_dir_all(&out_dir)
            .map_err(|e| MaterializeError::Io(out_dir.display().to_string(), e.to_string()))?;
        let out_file = out_dir.join("SKILL.md");
        std::fs::write(&out_file, &content)
            .map_err(|e| MaterializeError::Io(out_file.display().to_string(), e.to_string()))?;
        written.push(MaterializedSkill {
            name: name.clone(),
            path: out_file,
            hash,
        });
    }

    Ok((written, skipped))
}

/// Manages immutable profile revisions under `<root>/revisions/<id>` with an
/// `active` symlink that is flipped transactionally (and can be rolled back).
pub struct RevisionStore {
    root: PathBuf,
}

impl RevisionStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn revisions_dir(&self) -> PathBuf {
        self.root.join("revisions")
    }

    fn active_link(&self) -> PathBuf {
        self.root.join("active")
    }

    /// Currently-active revision id, if any.
    pub fn active(&self) -> Option<String> {
        let link = self.active_link();
        let target = std::fs::read_link(&link).ok()?;
        target.file_name()?.to_str().map(String::from)
    }

    /// Materialize `src` into a new immutable revision `<id>` and flip `active`
    /// to it. Returns the previous active id (for rollback), if any.
    pub fn activate(&self, id: &str, src: &Path) -> Result<Option<String>, MaterializeError> {
        let prev = self.active();
        let rev_dir = self.revisions_dir().join(id);
        if rev_dir.exists() {
            std::fs::remove_dir_all(&rev_dir)
                .map_err(|e| MaterializeError::Io(rev_dir.display().to_string(), e.to_string()))?;
        }
        copy_dir(src, &rev_dir)
            .map_err(|e| MaterializeError::Io(rev_dir.display().to_string(), e.to_string()))?;

        let link = self.active_link();
        if link.exists() || link.is_symlink() {
            std::fs::remove_file(&link)
                .map_err(|e| MaterializeError::Io(link.display().to_string(), e.to_string()))?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&rev_dir, &link)
            .map_err(|e| MaterializeError::Io(link.display().to_string(), e.to_string()))?;
        #[cfg(not(unix))]
        std::fs::write(&link, rev_dir.to_string_lossy().as_bytes())
            .map_err(|e| MaterializeError::Io(link.display().to_string(), e.to_string()))?;

        Ok(prev)
    }

    /// Flip `active` back to an earlier revision id (must already exist).
    pub fn rollback(&self, prev_id: &str) -> Result<(), MaterializeError> {
        let rev_dir = self.revisions_dir().join(prev_id);
        if !rev_dir.exists() {
            return Err(MaterializeError::Io(
                rev_dir.display().to_string(),
                "revision does not exist".into(),
            ));
        }
        let link = self.active_link();
        if link.exists() || link.is_symlink() {
            std::fs::remove_file(&link)
                .map_err(|e| MaterializeError::Io(link.display().to_string(), e.to_string()))?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&rev_dir, &link)
            .map_err(|e| MaterializeError::Io(link.display().to_string(), e.to_string()))?;
        #[cfg(not(unix))]
        std::fs::write(&link, rev_dir.to_string_lossy().as_bytes())
            .map_err(|e| MaterializeError::Io(link.display().to_string(), e.to_string()))?;
        Ok(())
    }
}

fn copy_dir(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir(&path, &target)?;
        } else {
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Skill;
    use std::io::Write;

    fn discovered(name: &str, source: SkillSource, body: &str) -> DiscoveredSkill {
        let dir =
            std::env::temp_dir().join(format!("ag_sk_{}_{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("SKILL.md");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(f, "---\nname: {}\ndescription: d\n---\n{}", name, body).unwrap();
        DiscoveredSkill {
            skill: Skill {
                name: name.into(),
                description: "d".into(),
                license: None,
                compatibility: None,
                allowed_tools: vec![],
                metadata: Default::default(),
                body: body.into(),
            },
            source,
            path: p,
        }
    }

    #[test]
    fn hash_is_deterministic() {
        assert_eq!(compute_skill_hash("abc"), compute_skill_hash("abc"));
        assert_ne!(compute_skill_hash("abc"), compute_skill_hash("abd"));
    }

    #[test]
    fn project_untrusted_by_default_user_trusted() {
        let t = TrustStore::new();
        assert!(!t.is_trusted(SkillSource::Project, "x"));
        assert!(t.is_trusted(SkillSource::User, "x"));
        assert!(t.is_trusted(SkillSource::Managed, "x"));
        let mut t2 = TrustStore::new();
        t2.trust(SkillSource::Project, "x");
        assert!(t2.is_trusted(SkillSource::Project, "x"));
    }

    #[test]
    fn materialize_skips_untrusted_and_writes_trusted() {
        let skills = vec![
            discovered("p1", SkillSource::Project, "project"),
            discovered("u1", SkillSource::User, "user"),
        ];
        let u1_src = skills[1].path.clone();
        let dest = std::env::temp_dir().join(format!("ag_mat_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        let (written, skipped) = materialize(&skills, &dest, &TrustStore::new(), None).unwrap();
        assert_eq!(skipped, vec!["p1".to_string()]);
        assert_eq!(written.len(), 1);
        assert_eq!(written[0].name, "u1");
        // Byte-fidelity: the written file must equal the original SKILL.md
        // verbatim, not a re-serialized parsed struct. If `materialize` ever
        // switches to round-tripping through `Skill`, this fails while the
        // hash tests stay circular — see tests/e2e/run-skill-bundle.sh header.
        let out = dest.join("u1").join("SKILL.md");
        assert!(out.exists());
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            std::fs::read_to_string(&u1_src).unwrap(),
            "materialize must copy the original SKILL.md byte-for-byte"
        );
        assert!(!dest.join("p1").exists());
        let _ = std::fs::remove_dir_all(&dest);
        for s in &skills {
            if let Some(p) = s.path.parent() {
                let _ = std::fs::remove_dir_all(p);
            }
        }
    }

    #[test]
    fn materialize_verifies_lock_hashes() {
        let skills = vec![discovered("u2", SkillSource::User, "user")];
        let dest = std::env::temp_dir().join(format!("ag_mat2_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dest);
        let (written, _) = materialize(&skills, &dest, &TrustStore::new(), None).unwrap();
        let hash = written[0].hash.clone();
        // correct hash -> ok
        let exp = HashMap::from([("u2".to_string(), hash.clone())]);
        let r = materialize(&skills, &dest, &TrustStore::new(), Some(&exp));
        assert!(r.is_ok());
        // wrong hash -> fatal
        let bad = HashMap::from([("u2".to_string(), "deadbeef".into())]);
        let r2 = materialize(&skills, &dest, &TrustStore::new(), Some(&bad));
        assert!(matches!(r2, Err(MaterializeError::HashMismatch(_, _, _))));
        let _ = std::fs::remove_dir_all(&dest);
        for s in &skills {
            if let Some(p) = s.path.parent() {
                let _ = std::fs::remove_dir_all(p);
            }
        }
    }

    #[test]
    fn revision_activate_and_rollback() {
        let src =
            std::env::temp_dir().join(format!("ag_rev_src_{}", std::process::id()));
        let root = std::env::temp_dir().join(format!("ag_rev_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(src.join("s1")).unwrap();
        std::fs::write(src.join("s1").join("SKILL.md"), b"v1").unwrap();

        let rs = RevisionStore::new(root.clone());
        let prev = rs.activate("r1", &src).unwrap();
        assert!(prev.is_none());
        assert_eq!(rs.active(), Some("r1".into()));
        // new revision
        std::fs::write(src.join("s1").join("SKILL.md"), b"v2").unwrap();
        let prev = rs.activate("r2", &src).unwrap();
        assert_eq!(prev, Some("r1".into()));
        assert_eq!(rs.active(), Some("r2".into()));
        // rollback
        rs.rollback("r1").unwrap();
        assert_eq!(rs.active(), Some("r1".into()));
        // r1 still has v1
        let v = std::fs::read(root.join("active").join("s1").join("SKILL.md")).unwrap();
        assert_eq!(&v[..], b"v1");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&root);
    }
}
