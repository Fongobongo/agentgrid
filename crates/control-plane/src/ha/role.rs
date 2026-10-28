//! Runtime role and the write-fencing gate.
//!
//! The gate is the single in-process authority for "may this node accept a
//! write right now". It is consulted before every `BEGIN IMMEDIATE`; HA also
//! re-checks the replicated epoch inside the transaction. A standby, or a
//! leader past its quorum lease, is fenced with a typed error that the HTTP
//! layer maps to 503.

use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;

use super::backend::S3Client;
use super::config::{HaConfig, HaMode, NodeRole, ReplBackend};
use super::shared::HaShared;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum HaRole {
    Single = 0,
    Standby = 1,
    Leader = 2,
    Witness = 3,
}

impl HaRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Standby => "standby",
            Self::Leader => "leader",
            Self::Witness => "witness",
        }
    }

    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Standby,
            2 => Self::Leader,
            3 => Self::Witness,
            _ => Self::Single,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FencedError {
    pub role: HaRole,
    pub epoch: u64,
}

impl fmt::Display for FencedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "control plane is not writable (role={}, epoch={})",
            self.role.as_str(),
            self.epoch
        )
    }
}

impl std::error::Error for FencedError {}

static MONO_START: OnceLock<Instant> = OnceLock::new();

pub(crate) fn mono_ms() -> u64 {
    MONO_START
        .get_or_init(Instant::now)
        .elapsed()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// Lease deadline `ttl` from now (monotonic milliseconds).
pub(crate) fn lease_deadline(ttl: Duration) -> u64 {
    mono_ms().saturating_add(ttl.as_millis().min(u128::from(u64::MAX)) as u64)
}

/// Fencing authority shared by the store, scheduler and HTTP middleware.
pub struct HaGate {
    role: AtomicU8,
    epoch: AtomicU64,
    /// Monotonic milliseconds; `u64::MAX` means "no lease required" (single).
    lease_until_ms: AtomicU64,
    fenced_writes: AtomicU64,
    /// Writes admitted while this node was leader, reset on every promotion.
    /// A leader that demotes without ever admitting a write has nothing the
    /// rest of the cluster could have missed, so it must not be condemned to
    /// a full snapshot (and with it, permanent ineligibility to campaign).
    admitted_writes: AtomicU64,
}

impl HaGate {
    pub fn single() -> Arc<Self> {
        Arc::new(Self {
            role: AtomicU8::new(HaRole::Single as u8),
            epoch: AtomicU64::new(0),
            lease_until_ms: AtomicU64::new(u64::MAX),
            fenced_writes: AtomicU64::new(0),
            admitted_writes: AtomicU64::new(0),
        })
    }

    pub fn new(role: HaRole, epoch: u64) -> Arc<Self> {
        let lease = if role == HaRole::Single { u64::MAX } else { 0 };
        Arc::new(Self {
            role: AtomicU8::new(role as u8),
            epoch: AtomicU64::new(epoch),
            lease_until_ms: AtomicU64::new(lease),
            fenced_writes: AtomicU64::new(0),
            admitted_writes: AtomicU64::new(0),
        })
    }

    pub fn role(&self) -> HaRole {
        HaRole::from_u8(self.role.load(Ordering::Acquire))
    }

    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub fn is_leader(&self) -> bool {
        matches!(self.role(), HaRole::Single | HaRole::Leader)
    }

    pub fn lease_until_ms(&self) -> u64 {
        self.lease_until_ms.load(Ordering::Acquire)
    }

    pub fn lease_remaining(&self) -> Duration {
        let until = self.lease_until_ms();
        if until == u64::MAX {
            return Duration::from_secs(u64::MAX / 1000);
        }
        Duration::from_millis(until.saturating_sub(mono_ms()))
    }

    pub fn assert_writable(&self) -> Result<(), FencedError> {
        let role = self.role();
        let ok = match role {
            HaRole::Single => true,
            HaRole::Leader => mono_ms() < self.lease_until_ms(),
            HaRole::Standby | HaRole::Witness => false,
        };
        if ok {
            self.admitted_writes.fetch_add(1, Ordering::Relaxed);
            Ok(())
        } else {
            self.fenced_writes.fetch_add(1, Ordering::Relaxed);
            Err(FencedError {
                role,
                epoch: self.epoch(),
            })
        }
    }

    /// Become leader for `epoch` with a lease valid until `lease_until_ms`.
    pub fn promote(&self, epoch: u64, lease_until_ms: u64) {
        self.epoch.store(epoch, Ordering::Release);
        self.lease_until_ms.store(lease_until_ms, Ordering::Release);
        self.admitted_writes.store(0, Ordering::Release);
        self.role.store(HaRole::Leader as u8, Ordering::Release);
    }

    pub fn promote_for(&self, epoch: u64, ttl: Duration) {
        self.promote(epoch, lease_deadline(ttl));
    }

    pub fn renew_lease(&self, lease_until_ms: u64) {
        self.lease_until_ms.store(lease_until_ms, Ordering::Release);
    }

    pub fn renew_for(&self, ttl: Duration) {
        self.renew_lease(lease_deadline(ttl));
    }

    /// True while this node may still serve as leader (single counts).
    pub fn lease_live(&self) -> bool {
        self.is_leader() && (self.role() == HaRole::Single || mono_ms() < self.lease_until_ms())
    }

    pub fn demote(&self) {
        self.lease_until_ms.store(0, Ordering::Release);
        self.role.store(HaRole::Standby as u8, Ordering::Release);
    }

    pub fn set_epoch(&self, epoch: u64) {
        self.epoch.store(epoch, Ordering::Release);
    }

    pub fn note_fenced_write(&self) {
        self.fenced_writes.fetch_add(1, Ordering::Relaxed);
    }

    pub fn fenced_writes(&self) -> u64 {
        self.fenced_writes.load(Ordering::Relaxed)
    }

    /// Writes admitted since the last promotion (see `admitted_writes`).
    pub fn admitted_writes(&self) -> u64 {
        self.admitted_writes.load(Ordering::Relaxed)
    }
}

/// HA configuration plus the shared gate. Present in every process; in single
/// mode it is inert.
pub struct HaRuntime {
    pub config: HaConfig,
    pub gate: Arc<HaGate>,
    pub shared: HaShared,
    /// Peer HTTP client. Built only in HA mode.
    pub client: Option<reqwest::Client>,
    /// Object-storage data plane, when `AGENTGRID_HA_REPL_BACKEND=s3`.
    pub s3: Option<S3Client>,
    /// SHA-256 of the shared JWT secret. Compared during peer handshakes so a
    /// host with a different secret cannot silently serve a split brain.
    jwt_fingerprint: Mutex<Option<String>>,
    /// The shared JWT secret itself, kept only for the opt-in one-time
    /// provisioning endpoint. Never persisted to SQLite.
    jwt_secret: Mutex<Option<String>>,
}

impl HaRuntime {
    pub fn single() -> Arc<Self> {
        Arc::new(Self {
            config: HaConfig::single(),
            gate: HaGate::single(),
            shared: HaShared::default(),
            client: None,
            s3: None,
            jwt_fingerprint: Mutex::new(None),
            jwt_secret: Mutex::new(None),
        })
    }

    pub fn from_env() -> Result<Arc<Self>> {
        Self::new(HaConfig::from_env()?)
    }

    pub fn new(config: HaConfig) -> Result<Arc<Self>> {
        let gate = match (config.mode, config.role) {
            (HaMode::Single, _) => HaGate::single(),
            // `config.role` is the topology role (data host); the runtime
            // starts passive until election promotes it.
            (HaMode::Ha, NodeRole::Data) => HaGate::new(HaRole::Standby, 0),
            (HaMode::Ha, NodeRole::Witness) => HaGate::new(HaRole::Witness, 0),
        };
        let shared = HaShared::default();
        if config.is_ha() && config.role == NodeRole::Witness {
            // A witness has no database; its durable vote lives next to the
            // witness directory from the first start.
            shared.set_vote_path(config.witness_dir.join("ha-witness-state.json"));
        }
        // No redirects: a peer header carries the cluster token and must not
        // be replayed to a different host.
        let client = if config.is_ha() {
            Some(
                reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()?,
            )
        } else {
            None
        };
        let s3 = match (&config.repl_backend, config.s3.as_ref()) {
            (ReplBackend::S3, Some(cfg)) => Some(S3Client::new(
                cfg,
                &config.cluster_id,
                config.allow_plaintext,
            )?),
            _ => None,
        };
        Ok(Arc::new(Self {
            config,
            gate,
            shared,
            client,
            s3,
            jwt_fingerprint: Mutex::new(None),
            jwt_secret: Mutex::new(None),
        }))
    }

    /// Load persisted term/vote from `path` and adopt it (data nodes only;
    /// the path is the DB directory, which is known after `AppState::open`).
    pub fn init_data_state(&self, path: std::path::PathBuf) -> Result<()> {
        let persisted = super::state_file::load(&path);
        self.shared.set_vote_path(path);
        self.shared
            .set_term_and_vote(persisted.term, persisted.voted_for.as_deref())?;
        Ok(())
    }

    pub fn is_ha(&self) -> bool {
        self.config.mode == HaMode::Ha
    }

    pub fn is_leader(&self) -> bool {
        self.gate.is_leader()
    }

    pub fn role(&self) -> HaRole {
        self.gate.role()
    }

    pub fn client(&self) -> Result<&reqwest::Client> {
        self.client
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("HA peer client is unavailable in single mode"))
    }

    /// Record the shared JWT secret and publish its fingerprint to peers.
    pub fn set_jwt_secret(&self, secret: &[u8]) {
        let fingerprint = agentgrid_common::sha256_hex(secret);
        *self
            .jwt_fingerprint
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(fingerprint);
        *self.jwt_secret.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(String::from_utf8_lossy(secret).into_owned());
    }

    pub fn jwt_fingerprint(&self) -> Option<String> {
        self.jwt_fingerprint
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Only the opt-in `GET /v1/ha/secret` handler reads this.
    pub fn jwt_secret(&self) -> Option<String> {
        self.jwt_secret
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// `None` in single mode: the store then skips lease/epoch fencing.
    pub fn store_gate(&self) -> Option<Arc<HaGate>> {
        self.is_ha().then(|| self.gate.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_fingerprint_tracks_the_secret() {
        let rt = HaRuntime::single();
        assert!(rt.jwt_fingerprint().is_none());
        let secret = b"0123456789abcdef0123456789abcdef";
        rt.set_jwt_secret(secret);
        assert_eq!(
            rt.jwt_fingerprint().unwrap(),
            agentgrid_common::sha256_hex(secret)
        );
        assert_eq!(rt.jwt_secret().unwrap(), String::from_utf8_lossy(secret));
        rt.set_jwt_secret(b"fedcba9876543210fedcba9876543210");
        assert_ne!(
            rt.jwt_fingerprint().unwrap(),
            agentgrid_common::sha256_hex(secret)
        );
    }

    #[test]
    fn single_is_always_writable() {
        let g = HaGate::single();
        assert_eq!(g.role(), HaRole::Single);
        assert!(g.assert_writable().is_ok());
        assert_eq!(g.fenced_writes(), 0);
    }

    #[test]
    fn standby_is_fenced() {
        let g = HaGate::new(HaRole::Standby, 4);
        let err = g.assert_writable().unwrap_err();
        assert_eq!(err.role, HaRole::Standby);
        assert_eq!(err.epoch, 4);
        assert_eq!(g.fenced_writes(), 1);
    }

    #[test]
    fn leader_needs_a_live_lease() {
        let g = HaGate::new(HaRole::Leader, 7);
        assert!(g.assert_writable().is_err());
        g.promote(7, u64::MAX - 1);
        assert!(g.assert_writable().is_ok());
        g.demote();
        assert!(g.assert_writable().is_err());
    }

    #[test]
    fn promotion_advances_epoch() {
        let g = HaGate::new(HaRole::Standby, 0);
        g.promote(3, u64::MAX - 1);
        assert_eq!(g.epoch(), 3);
        assert_eq!(g.role(), HaRole::Leader);
    }

    #[test]
    fn admitted_writes_are_per_leadership_term() {
        // `demote` uses this to decide whether the local copy is trustworthy:
        // a node that won an election and lost a same-term race milliseconds
        // later admitted nothing, so it must stay electable.
        let g = HaGate::new(HaRole::Standby, 1);
        g.promote(2, u64::MAX - 1);
        assert_eq!(g.admitted_writes(), 0);
        assert!(g.assert_writable().is_ok());
        assert!(g.assert_writable().is_ok());
        assert_eq!(g.admitted_writes(), 2);
        g.demote();
        assert_eq!(
            g.admitted_writes(),
            2,
            "demote keeps the count for the check"
        );
        g.promote(3, u64::MAX - 1);
        assert_eq!(g.admitted_writes(), 0, "a new term starts clean");
    }

    #[test]
    fn a_leader_without_a_lease_admits_nothing() {
        let g = HaGate::new(HaRole::Leader, 5);
        assert!(g.assert_writable().is_err());
        assert_eq!(g.admitted_writes(), 0);
    }
}
