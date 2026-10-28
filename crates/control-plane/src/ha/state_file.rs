//! Durable election state (term + vote) kept outside SQLite so a standby can
//! persist votes before it ever opens/restores a database.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PersistedVote {
    pub term: u64,
    pub voted_for: Option<String>,
}

pub fn load(path: &Path) -> PersistedVote {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Atomic write with fsync so a crash cannot resurrect a vote for another
/// candidate in the same term.
pub fn save(path: &Path, v: &PersistedVote) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(v)?)?;
    if let Ok(f) = std::fs::File::open(&tmp) {
        let _ = f.sync_all();
    }
    std::fs::rename(&tmp, path)?;
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}
