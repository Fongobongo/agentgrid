//! Stage 11 (CTX): a context provider produces a compact "context pack" — a
//! repo-level digest the agent gets alongside the prompt so it doesn't
//! re-index the repository per attempt. The first concrete implementation is
//! CTX (an external indexer); `NoopContextProvider` is the graceful fallback
//! when no provider is configured (empty pack, no error).
//!
//! Cache key: `(repo, base_commit, provider_version, config_hash)`. A node that
//! re-uses a cached pack for the same key skips re-indexing (Stage 11 exit
//! criterion). The cache itself lives on the node; this trait only produces the
//! pack.

use serde::{Deserialize, Serialize};

/// A compact context pack for a repository at a given base commit. The `body`
/// is appended to the agent prompt (like the skills block); `bytes_in` is the
/// raw repo size the indexer read, `bytes_out` the pack size (metrics).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextPack {
    pub provider: String,
    pub repo: String,
    pub base_commit: String,
    /// Cache key the provider used (or would use): `(repo, base_commit,
    /// provider_version, config_hash)` joined. Two packs with the same key are
    /// interchangeable.
    pub cache_key: String,
    /// Whether this pack came from a warm cache (no re-index). Metrics use it
    /// for the cache-hit rate.
    pub cache_hit: bool,
    /// Bytes the indexer read from the repo (metrics: before).
    pub bytes_in: usize,
    /// Pack body size in bytes (metrics: after).
    pub bytes_out: usize,
    /// Indexing wall time in ms (0 for a cache hit).
    pub index_ms: u64,
    /// The text appended to the prompt. Empty for Noop.
    pub body: String,
}

impl ContextPack {
    /// A pack that contributes nothing (no provider configured). The agent
    /// simply doesn't get a context digest; the task is unaffected.
    pub fn noop(repo: &str, base_commit: &str) -> Self {
        ContextPack {
            provider: "noop".into(),
            repo: repo.into(),
            base_commit: base_commit.into(),
            cache_key: format!("noop:{repo}:{base_commit}"),
            cache_hit: true,
            bytes_in: 0,
            bytes_out: 0,
            index_ms: 0,
            body: String::new(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.body.is_empty()
    }
}

/// Contract a context provider implements. `build` is invoked per attempt with
/// the repo + base commit the attempt branches from; the provider may serve a
/// cached pack (same `cache_key`) without re-indexing.
pub trait ContextProvider: Send + Sync {
    /// Human-readable provider id (e.g. `"ctx"`, `"noop"`).
    fn id(&self) -> &str;

    /// Produce a context pack for `(repo, base_commit)`. The provider is free
    /// to read a warm cache keyed by `cache_key_for`; on a miss it indexes and
    /// publishes under that key.
    fn build(&self, repo: &str, base_commit: &str) -> Result<ContextPack, ContextError>;

    /// Probe whether the provider is available (e.g. the CTX binary exists).
    /// Unavailable → the node falls back to `NoopContextProvider`.
    fn available(&self) -> bool;
}

/// Error from a context provider (e.g. the indexer binary is missing).
#[derive(Debug, Clone)]
pub struct ContextError(pub String);

impl std::fmt::Display for ContextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ContextError {}

/// The canonical cache key for a pack: `(repo, base_commit, provider_version,
/// config_hash)`. Exported so providers and the cache agree on the shape.
pub fn cache_key_for(
    repo: &str,
    base_commit: &str,
    provider_version: &str,
    config_hash: &str,
) -> String {
    format!("{repo}:{base_commit}:{provider_version}:{config_hash}")
}

/// Graceful fallback: produces an empty pack. Used when no provider is
/// configured or a probed provider is unavailable (Stage 11 graceful fallback).
pub struct NoopContextProvider;

impl ContextProvider for NoopContextProvider {
    fn id(&self) -> &str {
        "noop"
    }
    fn build(&self, repo: &str, base_commit: &str) -> Result<ContextPack, ContextError> {
        Ok(ContextPack::noop(repo, base_commit))
    }
    fn available(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_is_empty_and_cached() {
        let p = NoopContextProvider.build("myrepo", "abc123").unwrap();
        assert!(p.is_empty());
        assert!(p.cache_hit, "noop never re-indexes");
        assert_eq!(p.provider, "noop");
        assert_eq!(p.bytes_in, 0);
        assert_eq!(p.bytes_out, 0);
    }

    #[test]
    fn cache_key_is_deterministic() {
        let a = cache_key_for("repo", "commit", "ctx-1.0", "cfg-hash");
        let b = cache_key_for("repo", "commit", "ctx-1.0", "cfg-hash");
        assert_eq!(a, b);
        // Any differing component changes the key (forces a re-index).
        assert_ne!(a, cache_key_for("repo", "commit2", "ctx-1.0", "cfg-hash"));
        assert_ne!(a, cache_key_for("repo", "commit", "ctx-2.0", "cfg-hash"));
        assert_ne!(a, cache_key_for("repo2", "commit", "ctx-1.0", "cfg-hash"));
    }
}
