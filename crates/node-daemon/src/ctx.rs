//! Stage 11 (CTX): external context-indexer provider + on-disk index cache.
//!
//! The node can prepend a compact repository digest to the agent prompt
//! (`compose_context_block` in `skills.rs`). The provider is selected from
//! env; without one the attempt proceeds pack-less (graceful fallback —
//! context is a hint, never a hard dependency).
//!
//! **External provider contract** (`AGENTGRID_CTX_BIN=<path|name>`;
//! `tests/e2e/ctx-stub.sh` is a conformant stub used by E2E):
//! - `<bin> --version` → first stdout line is the provider version (recorded
//!   in the cache key, so an upgrade re-indexes instead of serving stale
//!   packs). Bounded 10s.
//! - `<bin> index --repo <repo> --base <commit> --format json` → stdout JSON
//!   `{"body": "<digest>", "bytes_in"?: n, "bytes_out"?: m, "index_ms"?: ms}`.
//!   Non-zero exit / unparseable output / timeout (`AGENTGRID_CTX_TIMEOUT_SECS`,
//!   default 120) → build error → the attempt runs without a pack. The child
//!   runs with a cleared env plus the operator's `CTX_*` namespace (the
//!   indexer's own config); node secrets never reach it.
//!
//! **On-disk cache** (`CtxCache`): keyed by
//! `cache_key_for(repo, base_commit, provider_version, config_hash)` where
//! `config_hash` pins the provider identity (binary path). Atomic publish
//! (tmp file + rename — a crash never leaves a partial pack), quota
//! (`AGENTGRID_CTX_CACHE_MB`, default 512; oldest-mtime eviction), corrupt
//! entries re-index (fail-safe delete + miss). A repeat attempt on the same
//! `(repo, base)` hits the cache: no subprocess spawn, `cache_hit = true` in
//! the `context_pack` status event (Stage 11 exit criterion).
//!
//! Capability stance: the probe runs at daemon startup (logged) and before
//! every build (missing binary → Noop, never a task failure). The provider
//! id rides the `context_pack` event, so operators see which indexer fed
//! each attempt; no CP wire change (context stays a node-local hint).

use std::path::{Path, PathBuf};

use agentgrid_common::{cache_key_for, sha256_hex, ContextError, ContextPack, ContextProvider};

/// Binary override env; unset/empty → no external indexer (Noop).
pub fn ctx_bin() -> Option<String> {
    std::env::var("AGENTGRID_CTX_BIN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Default cache dir: `AGENTGRID_CTX_CACHE_DIR`, else `ctx-cache` next to the
/// given dir (callers pass `artifact_spool_root`, whose parent is the node
/// data dir). Explicit env wins so tests/E2E can isolate.
pub fn default_cache_dir(anchor: &Path) -> PathBuf {
    if let Ok(d) = std::env::var("AGENTGRID_CTX_CACHE_DIR") {
        let d = d.trim().to_string();
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    anchor
        .parent()
        .map(|p| p.join("ctx-cache"))
        .unwrap_or_else(|| std::env::temp_dir().join("agentgrid-ctx-cache"))
}

/// Cache quota in bytes (`AGENTGRID_CTX_CACHE_MB`, default 512; 0 = bypass
/// the cache entirely — every build re-indexes).
pub fn cache_quota_bytes() -> u64 {
    std::env::var("AGENTGRID_CTX_CACHE_MB")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(|mb| mb * 1024 * 1024)
        .unwrap_or(512 * 1024 * 1024)
}

/// Stage 13 (RTK/Headroom, optional output optimizer): prompt-budget cap for
/// the injected pack body (`AGENTGRID_CTX_BUDGET_BYTES`, default 0 = off).
/// When set and the body exceeds it, the body is compressed (dup-line
/// collapse + newline-safe truncation) and the `context_pack` event records
/// `trimmed_bytes`; the full pack stays in the on-disk cache (raw evidence
/// preserved).
pub fn ctx_budget_bytes() -> usize {
    std::env::var("AGENTGRID_CTX_BUDGET_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
}

/// Index subprocess timeout (`AGENTGRID_CTX_TIMEOUT_SECS`, default 120,
/// clamped to 5..600 so a wedged indexer cannot park an attempt forever).
pub fn index_timeout_secs() -> u64 {
    std::env::var("AGENTGRID_CTX_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120)
        .clamp(5, 600)
}

/// External indexer: shells out to the configured binary per build
/// (ponytail: one process per attempt, matching `ExternalPolicyProvider`;
/// the on-disk cache keeps the steady-state rate at ~zero).
pub struct ExternalContextProvider {
    binary: String,
}

impl ExternalContextProvider {
    pub fn new(binary: impl Into<String>) -> Self {
        ExternalContextProvider {
            binary: binary.into(),
        }
    }

    /// From env (`AGENTGRID_CTX_BIN`). `None` → no external indexer.
    pub fn from_env() -> Option<Self> {
        ctx_bin().map(Self::new)
    }

    /// Capability probe: binary resolves and `--version` answers in 10s.
    /// Returns the version string. `None` → caller falls back to Noop.
    pub fn probe(&self) -> Option<String> {
        crate::capabilities::resolve_in_path(&self.binary)?;
        let out = std::process::Command::new(&self.binary)
            .arg("--version")
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let v = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if v.is_empty() {
            None
        } else {
            Some(v)
        }
    }

    /// Config hash pinning the provider identity into the cache key: the
    /// binary path (a different binary = a different index).
    pub fn config_hash(&self) -> String {
        sha256_hex(self.binary.as_bytes())[..16].to_string()
    }
}

#[derive(serde::Deserialize)]
struct IndexOutput {
    #[serde(default)]
    body: String,
    #[serde(default)]
    bytes_in: usize,
    #[serde(default)]
    bytes_out: usize,
    #[serde(default)]
    index_ms: u64,
}

/// Run `cmd` to completion with a wall-clock timeout (blocking — the
/// `ContextProvider` contract is sync). On expiry the child is killed and an
/// error returned, so a wedged indexer cannot park an attempt forever.
fn run_bounded(
    mut cmd: std::process::Command,
    timeout_secs: u64,
) -> Result<std::process::Output, String> {
    // wait_with_output does NOT pipe stdio by itself: without explicit
    // pipes the child inherits our stdout/stderr, its output lands in our
    // log, and `out.stdout` comes back empty (surfaced downstream as
    // "unparseable output: EOF"). Pipe explicitly; stdin is nulled so a
    // chatty indexer can never block reading our stdin.
    cmd.stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("spawn failed: {e}"))?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait().map_err(|e| format!("wait failed: {e}"))? {
            Some(_) => {
                return child
                    .wait_with_output()
                    .map_err(|e| format!("collect output failed: {e}"));
            }
            None => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {timeout_secs}s"));
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
    }
}

impl ContextProvider for ExternalContextProvider {
    fn id(&self) -> &str {
        "external-ctx"
    }

    fn build(&self, repo: &str, base_commit: &str) -> Result<ContextPack, ContextError> {
        // Fail-fast without a probe: a clear error (not a hang) when the
        // binary vanished after startup.
        if crate::capabilities::resolve_in_path(&self.binary).is_none() {
            return Err(ContextError(format!(
                "ctx binary not found: {}",
                self.binary
            )));
        }
        let mut cmd = std::process::Command::new(&self.binary);
        cmd.arg("index")
            .arg("--repo")
            .arg(repo)
            .arg("--base")
            .arg(base_commit)
            .arg("--format")
            .arg("json")
            .env_clear();
        // The indexer is operator-pinned config (like the binary path
        // itself), so its own `CTX_*` namespace passes through; everything
        // else is cleared so node secrets never reach the indexer process.
        for (k, v) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("CTX_") {
                cmd.env(k, v);
            }
        }
        let out = run_bounded(cmd, index_timeout_secs())
            .map_err(|e| ContextError(format!("ctx index failed: {e}")))?;
        if !out.status.success() {
            return Err(ContextError(format!(
                "ctx index exited {}: {}",
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        let parsed: IndexOutput = serde_json::from_slice(&out.stdout)
            .map_err(|e| ContextError(format!("ctx index unparseable output: {e}")))?;
        Ok(ContextPack {
            provider: self.id().to_string(),
            repo: repo.to_string(),
            base_commit: base_commit.to_string(),
            cache_key: cache_key_for(repo, base_commit, "?", "?"),
            cache_hit: false,
            bytes_in: parsed.bytes_in,
            bytes_out: if parsed.bytes_out != 0 {
                parsed.bytes_out
            } else {
                parsed.body.len()
            },
            index_ms: parsed.index_ms,
            body: parsed.body,
        })
    }

    fn available(&self) -> bool {
        self.probe().is_some()
    }
}

/// On-disk index cache. Keyed by the full `cache_key_for` string, stored as
/// `<sha256(key)>.json` (keys contain `/`/`:`/`*` — never used as file names
/// directly). Fail-safe: any IO/parse error degrades to miss (and deletes a
/// corrupt entry) — the cache never fails an attempt.
pub struct CtxCache {
    dir: PathBuf,
    quota_bytes: u64,
    disabled: bool,
}

impl CtxCache {
    /// Open (creating) `dir` with `quota_bytes`. A creation failure disables
    /// the cache (warn) instead of failing the caller — caching is an
    /// optimization, not a dependency.
    pub fn open(dir: PathBuf, quota_bytes: u64) -> Self {
        if quota_bytes == 0 {
            return CtxCache {
                dir,
                quota_bytes,
                disabled: true,
            };
        }
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!(dir = %dir.display(), error = %e, "ctx cache dir unavailable; caching disabled");
            return CtxCache {
                dir,
                quota_bytes,
                disabled: false,
            };
        }
        CtxCache {
            dir,
            quota_bytes,
            disabled: false,
        }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        self.dir
            .join(format!("{}.json", sha256_hex(key.as_bytes())))
    }

    /// Load a pack. Hit → `cache_hit = true`. Corrupt entry → deleted + miss.
    pub fn get(&self, key: &str) -> Option<ContextPack> {
        if self.disabled {
            return None;
        }
        let bytes = std::fs::read(self.path_for(key)).ok()?;
        match serde_json::from_slice::<ContextPack>(&bytes) {
            Ok(mut p) => {
                p.cache_hit = true;
                Some(p)
            }
            Err(_) => {
                let _ = std::fs::remove_file(self.path_for(key));
                None
            }
        }
    }

    /// Store a pack (atomic tmp + rename), then enforce the quota (oldest
    /// mtime first; the just-written entry is newest so it survives its own
    /// publish). Failures only warn.
    pub fn put(&self, key: &str, pack: &ContextPack) {
        if self.disabled {
            return;
        }
        let bytes = match serde_json::to_vec(pack) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "ctx cache serialize failed");
                return;
            }
        };
        let tmp = self.dir.join(format!(
            ".tmp-{}-{}.json",
            std::process::id(),
            &sha256_hex(key.as_bytes())[..16]
        ));
        if std::fs::write(&tmp, &bytes).is_err() {
            return;
        }
        if std::fs::rename(&tmp, self.path_for(key)).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        self.enforce_quota();
    }

    fn enforce_quota(&self) {
        let mut entries: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
        let mut total: u64 = 0;
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            total += md.len();
            entries.push((
                md.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH),
                md.len(),
                p,
            ));
        }
        if total <= self.quota_bytes {
            return;
        }
        entries.sort_by_key(|(m, _, _)| *m);
        for (_, len, p) in entries {
            if total <= self.quota_bytes {
                break;
            }
            if std::fs::remove_file(&p).is_ok() {
                total = total.saturating_sub(len);
            }
        }
    }
}

/// Cache-wrapping provider: serve warm packs without spawning the indexer;
/// miss → build → publish. The cache key pins `(repo, base, version,
/// config)` so upgrades and provider swaps re-index instead of serving
/// stale digests.
pub struct CachedProvider<P: ContextProvider> {
    inner: P,
    cache: CtxCache,
    version: String,
    config_hash: String,
}

impl<P: ContextProvider> CachedProvider<P> {
    pub fn new(inner: P, cache: CtxCache, version: String, config_hash: String) -> Self {
        CachedProvider {
            inner,
            cache,
            version,
            config_hash,
        }
    }
}

impl<P: ContextProvider> ContextProvider for CachedProvider<P> {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn build(&self, repo: &str, base_commit: &str) -> Result<ContextPack, ContextError> {
        let key = cache_key_for(repo, base_commit, &self.version, &self.config_hash);
        if let Some(hit) = self.cache.get(&key) {
            return Ok(hit);
        }
        let mut pack = self.inner.build(repo, base_commit)?;
        pack.cache_key = key.clone();
        pack.cache_hit = false;
        self.cache.put(&key, &pack);
        Ok(pack)
    }

    fn available(&self) -> bool {
        self.inner.available()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentgrid_common::NoopContextProvider;

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "ag-ctx-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn cache_round_trips_and_marks_hits() {
        let dir = tmpdir("roundtrip");
        let cache = CtxCache::open(dir.clone(), 10 * 1024 * 1024);
        assert!(cache.get("k").is_none(), "empty cache misses");
        let mut p = ContextPack::noop("r", "b");
        p.body = "digest".into();
        p.cache_key = "k".into();
        cache.put("k", &p);
        let hit = cache.get("k").expect("stored pack must hit");
        assert!(hit.cache_hit);
        assert_eq!(hit.body, "digest");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cache_key_never_touches_the_filesystem_verbatim() {
        // Keys contain `/`, `:` and `*` (repo names, `*` for plain-dir tasks)
        // — they must be hashed, never used as file names (path traversal).
        let dir = tmpdir("keyshape");
        let cache = CtxCache::open(dir.clone(), 10 * 1024 * 1024);
        let mut p = ContextPack::noop("r", "b");
        p.body = "x".into();
        cache.put("*:/../evil:0.:abcd", &p);
        assert!(cache.get("*:/../evil:0.:abcd").is_some());
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            names
                .iter()
                .all(|n| !n.contains('/') && !n.contains('*') && !n.contains(':')),
            "file names must be hashes: {names:?}"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn corrupt_entry_is_deleted_and_misses() {
        let dir = tmpdir("corrupt");
        let cache = CtxCache::open(dir.clone(), 10 * 1024 * 1024);
        let mut p = ContextPack::noop("r", "b");
        p.body = "v".into();
        cache.put("k", &p);
        std::fs::write(
            dir.join(format!("{}.json", sha256_hex("k".as_bytes()))),
            b"{oops",
        )
        .unwrap();
        assert!(cache.get("k").is_none(), "corrupt entry must miss");
        assert!(
            std::fs::read_dir(&dir).unwrap().count() == 0,
            "corrupt entry must be deleted"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn quota_evicts_oldest_first() {
        let dir = tmpdir("quota");
        // Seed one pack, measure its on-disk size, then size the quota to
        // hold ~2 packs: the third publish must evict the oldest.
        let seed = CtxCache::open(dir.clone(), u64::MAX);
        let pack = |k: &str| {
            let mut p = ContextPack::noop("r", "b");
            p.body = format!("body-{k}-with-padding-to-grow-the-file-xxxxx");
            p.cache_key = k.into();
            p
        };
        seed.put("a", &pack("a"));
        let sa = std::fs::metadata(seed.path_for("a")).unwrap().len();
        let cache = CtxCache::open(dir.clone(), 2 * sa + 10);
        for k in ["b", "c"] {
            std::thread::sleep(std::time::Duration::from_millis(15));
            cache.put(k, &pack(k));
        }
        assert!(cache.get("a").is_none(), "oldest must be evicted");
        assert!(cache.get("b").is_some());
        assert!(cache.get("c").is_some());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn zero_quota_disables_without_io() {
        let dir = tmpdir("zeroq");
        let cache = CtxCache::open(dir.join("never-created"), 0);
        let mut p = ContextPack::noop("r", "b");
        p.body = "v".into();
        cache.put("k", &p);
        assert!(cache.get("k").is_none());
        assert!(!dir.join("never-created").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    /// Counting in-memory provider: proves the cached wrapper skips rebuilds.
    struct CountingProvider {
        builds: std::sync::Mutex<u32>,
    }

    impl ContextProvider for CountingProvider {
        fn id(&self) -> &str {
            "counting"
        }
        fn build(&self, repo: &str, base: &str) -> Result<ContextPack, ContextError> {
            *self.builds.lock().unwrap() += 1;
            Ok(ContextPack {
                provider: "counting".into(),
                repo: repo.into(),
                base_commit: base.into(),
                cache_key: String::new(),
                cache_hit: false,
                bytes_in: 10,
                bytes_out: 5,
                index_ms: 7,
                body: "counted-digest".into(),
            })
        }
        fn available(&self) -> bool {
            true
        }
    }

    #[test]
    fn cached_provider_skips_rebuild_on_repeat() {
        let dir = tmpdir("cached");
        let cache = CtxCache::open(dir.clone(), 10 * 1024 * 1024);
        let inner = CountingProvider {
            builds: std::sync::Mutex::new(0),
        };
        let cached = CachedProvider::new(inner, cache, "1.0".into(), "cfg".into());
        let first = cached.build("repo", "abc").unwrap();
        assert!(!first.cache_hit);
        assert_eq!(first.body, "counted-digest");
        let second = cached.build("repo", "abc").unwrap();
        assert!(second.cache_hit);
        assert_eq!(*cached.inner.builds.lock().unwrap(), 1, "one build total");
        // A different base re-indexes (key pins base_commit).
        let third = cached.build("repo", "def").unwrap();
        assert!(!third.cache_hit);
        assert_eq!(*cached.inner.builds.lock().unwrap(), 2);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cached_provider_propagates_inner_errors() {
        struct Failing;
        impl ContextProvider for Failing {
            fn id(&self) -> &str {
                "failing"
            }
            fn build(&self, _: &str, _: &str) -> Result<ContextPack, ContextError> {
                Err(ContextError("boom".into()))
            }
            fn available(&self) -> bool {
                false
            }
        }
        let dir = tmpdir("failthru");
        let cached = CachedProvider::new(
            Failing,
            CtxCache::open(dir.clone(), 1024 * 1024),
            "1".into(),
            "c".into(),
        );
        assert!(cached.build("r", "b").is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn noop_selection_when_unset() {
        // No AGENTGRID_CTX_BIN in this process (tests must not depend on the
        // operator env) — selection falls back to Noop. Guard in case the
        // dev box exports one.
        if std::env::var("AGENTGRID_CTX_BIN").is_ok() {
            return;
        }
        assert!(ExternalContextProvider::from_env().is_none());
        let n = NoopContextProvider;
        assert!(n.build("r", "b").unwrap().is_empty());
    }

    fn ctx_test_assignment() -> agentgrid_common::Assignment {
        agentgrid_common::Assignment {
            attempt_id: "att-ctx".into(),
            fencing_token: String::new(),
            task_id: "t1".into(),
            repository: "*".into(),
            prompt: "do it".into(),
            adapter: "fake-acp".into(),
            number: 1,
            timeout_secs: 30,
            git_url: String::new(),
            default_branch: String::new(),
            validation_command: None,
            validation_timeout_secs: None,
            base_commit: None,
            parent_acp_session_id: None,
            network_mode: None,
            provenance: None,
            upstream_commits: vec![],
            upstream_task_ids: vec![],
            group_id: None,
            read_only: false,
            eval_cases: vec![],
            consensus_group_id: None,
            consensus_member: None,
            opencode_override: None,
            github_push: false,
            github_repo: None,
            github_issue: None,
            github_base_ref: None,
            github_create: false,
            workflow_role: None,
            max_tokens: None,
            max_cost_cents: None,
        }
    }

    fn ctx_test_sink(attempt: &str) -> std::sync::Arc<crate::event_sink::EventSink> {
        let dir = std::env::temp_dir().join(format!(
            "ag-ctx-sink-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let outbox = std::sync::Arc::new(crate::outbox::EventOutbox::open(&dir, attempt).unwrap());
        crate::event_sink::EventSink::new(
            attempt.into(),
            reqwest::Client::new(),
            "http://x".into(),
            String::new(),
            outbox,
        )
    }

    /// The full compose path: pack body lands in the prompt, the metrics
    /// event records the miss, and the repeat call serves the warm pack
    /// without rebuilding (cache_hit flips, one inner build total).
    #[tokio::test]
    async fn compose_injects_pack_and_serves_repeat_from_cache() {
        let dir = tmpdir("compose");
        let inner = CountingProvider {
            builds: std::sync::Mutex::new(0),
        };
        let cached = CachedProvider::new(
            inner,
            CtxCache::open(dir.clone(), 10 * 1024 * 1024),
            "9.9".into(),
            "cfg".into(),
        );
        let assignment = ctx_test_assignment();
        let sink = ctx_test_sink("att-ctx");
        let first = crate::skills::compose_context_block(&cached, &assignment, &sink, 0).await;
        assert_eq!(first, "counted-digest");
        let second = crate::skills::compose_context_block(&cached, &assignment, &sink, 0).await;
        assert_eq!(second, "counted-digest");
        assert_eq!(*cached.inner.builds.lock().unwrap(), 1, "one build total");
        let buf = sink.buffered_events().await;
        let packs: Vec<bool> = buf
            .iter()
            .filter(|e| e.payload.get("kind").and_then(|v| v.as_str()) == Some("context_pack"))
            .map(|e| {
                e.payload
                    .get("cache_hit")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true)
            })
            .collect();
        assert_eq!(packs, vec![false, true], "miss then hit, in order");
        std::fs::remove_dir_all(dir).ok();
    }

    /// Provider failure degrades to an empty block (no event, no error) —
    /// the attempt proceeds pack-less.
    #[tokio::test]
    async fn compose_falls_back_to_empty_on_provider_error() {
        struct Failing;
        impl ContextProvider for Failing {
            fn id(&self) -> &str {
                "failing"
            }
            fn build(&self, _: &str, _: &str) -> Result<ContextPack, ContextError> {
                Err(ContextError("boom".into()))
            }
            fn available(&self) -> bool {
                false
            }
        }
        let assignment = ctx_test_assignment();
        let sink = ctx_test_sink("att-ctx-fail");
        let block = crate::skills::compose_context_block(&Failing, &assignment, &sink, 0).await;
        assert!(block.is_empty());
        assert!(sink.buffered_events().await.is_empty());
    }

    /// Env hygiene: the indexer subprocess sees the operator's `CTX_*`
    /// namespace (its own config) but nothing else — node secrets must not
    /// reach it. Unix-only (needs `sh` as a contract-speaking stand-in).
    #[cfg(unix)]
    #[test]
    fn indexer_sees_ctx_namespace_only() {
        use std::io::Write;
        let dir = tmpdir("envhygiene");
        let script = dir.join("probe-indexer.sh");
        // Minimal contract speaker: answers --version, dumps the visible
        // CTX_/SNEAKY_ env on `index`, prints a pack.
        let mut f = std::fs::File::create(&script).unwrap();
        writeln!(f, "#!/bin/sh").unwrap();
        writeln!(
            f,
            "if [ \"$1\" = \"--version\" ]; then echo test-9.9; exit 0; fi"
        )
        .unwrap();
        writeln!(
            f,
            "env | grep -E '^(CTX_|SNEAKY_)' > \"$CTX_PROBE_OUT\" 2>/dev/null; echo '{{\"body\":\"hi\"}}'"
        )
        .unwrap();
        drop(f);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        std::env::set_var("CTX_PROBE_MARKER", "present");
        std::env::set_var("CTX_PROBE_OUT", dir.join("seen.env"));
        std::env::set_var("SNEAKY_MARKER", "must-not-arrive");
        let p = ExternalContextProvider::new(script.to_string_lossy().to_string());
        assert_eq!(p.probe().as_deref(), Some("test-9.9"));
        let pack = p.build("r", "b").expect("stub speaker must parse");
        assert_eq!(pack.body, "hi");
        let seen = std::fs::read_to_string(dir.join("seen.env")).unwrap();
        assert!(
            seen.contains("CTX_PROBE_MARKER=present"),
            "CTX_* passes: {seen}"
        );
        assert!(
            !seen.contains("SNEAKY_MARKER"),
            "non-CTX env blocked: {seen}"
        );
        std::env::remove_var("CTX_PROBE_MARKER");
        std::env::remove_var("CTX_PROBE_OUT");
        std::env::remove_var("SNEAKY_MARKER");
        std::fs::remove_dir_all(dir).ok();
    }

    /// Stage 13 optimizer: a body over budget is compressed (dup-collapse +
    /// newline-safe truncate), the event records `trimmed_bytes`, and the
    /// marker keeps the dropped count — raw evidence stays in the cache file.
    #[tokio::test]
    async fn compose_trims_body_over_budget_and_reports_saved_bytes() {
        struct Big;
        impl ContextProvider for Big {
            fn id(&self) -> &str {
                "big"
            }
            fn build(&self, repo: &str, base: &str) -> Result<ContextPack, ContextError> {
                // 50 distinct lines — dedup cannot collapse the run, so the
                // budget forces a newline-safe truncation with a marker.
                let body = (0..50)
                    .map(|i| format!("unique-line-{i:02}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(ContextPack {
                    provider: "big".into(),
                    repo: repo.into(),
                    base_commit: base.into(),
                    cache_key: "k".into(),
                    cache_hit: false,
                    bytes_in: 500,
                    bytes_out: body.len(),
                    index_ms: 1,
                    body,
                })
            }
            fn available(&self) -> bool {
                true
            }
        }
        let assignment = ctx_test_assignment();
        let sink = ctx_test_sink("att-ctx-trim");
        let block = crate::skills::compose_context_block(&Big, &assignment, &sink, 64).await;
        assert!(
            block.len() <= 64 + 64,
            "block must be capped near budget: {}",
            block.len()
        );
        assert!(
            block.contains("truncated"),
            "marker must state the cut: {block}"
        );
        let buf = sink.buffered_events().await;
        let trimmed: Vec<u64> = buf
            .iter()
            .filter(|e| e.payload.get("kind").and_then(|v| v.as_str()) == Some("context_pack"))
            .filter_map(|e| e.payload.get("trimmed_bytes").and_then(|v| v.as_u64()))
            .collect();
        assert_eq!(trimmed.len(), 1);
        assert!(trimmed[0] > 0, "saved bytes must be reported");
        // Budget 0 = off: full body passes through, trimmed_bytes = 0.
        let sink2 = ctx_test_sink("att-ctx-notrim");
        let full = crate::skills::compose_context_block(&Big, &assignment, &sink2, 0).await;
        assert!(full.contains("unique-line-49"));
        assert!(!full.contains("truncated"));
    }
}
