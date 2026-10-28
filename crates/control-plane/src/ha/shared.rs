//! Mutable consensus state shared by the election loop and the HA routes.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use anyhow::Result;

use super::role::mono_ms;
use super::state_file::{self, PersistedVote};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaderInfo {
    pub id: String,
    pub url: String,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Shared, non-DB consensus state. Guarded sections are short and never await.
pub struct HaShared {
    term: AtomicU64,
    voted_for: Mutex<Option<String>>,
    vote_path: Mutex<Option<PathBuf>>,
    leader: Mutex<Option<LeaderInfo>>,
    last_contact_ms: AtomicU64,
    applied_seq: AtomicU64,
    snapshot_id: Mutex<Option<String>>,
    snapshot_created_unix: AtomicI64,
    last_error: Mutex<Option<String>>,
    /// Serialises manual/automatic promotion attempts.
    pub promote_lock: tokio::sync::Mutex<()>,
    /// Serialises snapshot restores: the apply loop and an operator-driven
    /// catch-up must never rewrite the live database file at the same time.
    pub apply_lock: tokio::sync::Mutex<()>,
    pub manual_promote: tokio::sync::Notify,
}

impl Default for HaShared {
    fn default() -> Self {
        Self {
            term: AtomicU64::new(0),
            voted_for: Mutex::new(None),
            vote_path: Mutex::new(None),
            leader: Mutex::new(None),
            last_contact_ms: AtomicU64::new(mono_ms()),
            applied_seq: AtomicU64::new(0),
            snapshot_id: Mutex::new(None),
            snapshot_created_unix: AtomicI64::new(0),
            last_error: Mutex::new(None),
            promote_lock: tokio::sync::Mutex::new(()),
            apply_lock: tokio::sync::Mutex::new(()),
            manual_promote: tokio::sync::Notify::new(),
        }
    }
}

impl HaShared {
    pub fn term(&self) -> u64 {
        self.term.load(Ordering::Acquire)
    }

    pub fn set_term(&self, term: u64) {
        self.term.store(term, Ordering::Release);
    }

    pub fn voted_for(&self) -> Option<String> {
        lock(&self.voted_for).clone()
    }

    pub fn set_vote_path(&self, path: PathBuf) {
        *lock(&self.vote_path) = Some(path);
    }

    pub fn set_term_and_vote(&self, term: u64, vote: Option<&str>) -> Result<()> {
        self.term.store(term, Ordering::Release);
        *lock(&self.voted_for) = vote.map(str::to_string);
        self.persist_vote()
    }

    pub fn persist_vote(&self) -> Result<()> {
        let path = lock(&self.vote_path).clone();
        if let Some(path) = path {
            state_file::save(
                &path,
                &PersistedVote {
                    term: self.term(),
                    voted_for: self.voted_for(),
                },
            )?;
        }
        Ok(())
    }

    pub fn leader(&self) -> Option<LeaderInfo> {
        lock(&self.leader).clone()
    }

    pub fn leader_url(&self) -> Option<String> {
        lock(&self.leader).as_ref().map(|l| l.url.clone())
    }

    pub fn set_leader(&self, id: &str, url: &str) {
        *lock(&self.leader) = Some(LeaderInfo {
            id: id.to_string(),
            url: url.to_string(),
        });
    }

    pub fn clear_leader(&self) {
        *lock(&self.leader) = None;
    }

    pub fn touch_leader(&self) {
        self.last_contact_ms.store(mono_ms(), Ordering::Release);
    }

    pub fn last_contact_ms(&self) -> u64 {
        self.last_contact_ms.load(Ordering::Acquire)
    }

    pub fn applied_seq(&self) -> u64 {
        self.applied_seq.load(Ordering::Acquire)
    }

    pub fn set_applied_seq(&self, seq: u64) {
        self.applied_seq.store(seq, Ordering::Release);
    }

    pub fn snapshot_id(&self) -> Option<String> {
        lock(&self.snapshot_id).clone()
    }

    pub fn set_snapshot(&self, id: Option<String>, created_unix: i64) {
        *lock(&self.snapshot_id) = id;
        self.snapshot_created_unix
            .store(created_unix, Ordering::Release);
    }

    pub fn snapshot_created_unix(&self) -> i64 {
        self.snapshot_created_unix.load(Ordering::Acquire)
    }

    pub fn set_error(&self, err: Option<String>) {
        *lock(&self.last_error) = err;
    }

    pub fn last_error(&self) -> Option<String> {
        lock(&self.last_error).clone()
    }
}
