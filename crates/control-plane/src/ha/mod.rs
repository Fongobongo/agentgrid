//! Control-plane high availability (active-passive).
//!
//! SQLite is single-writer, so HA is leader/standby: exactly one data node
//! accepts mutations per epoch, standbys apply snapshots and take over after
//! quorum election. See `docs/decisions/0021-control-plane-high-availability.md`.
//!
//! Single mode is the default and keeps the pre-HA code path: no HA sockets,
//! no HA background tasks, and an always-open [`HaGate`].

pub mod artifacts;
pub mod config;
pub mod election;
pub mod protocol;
pub mod quorum;
pub mod role;
pub mod routes;
pub mod shared;
pub mod snapshot;
pub mod state_file;
pub mod transport;
pub mod witness;

pub use config::{
    ArtifactSync, HaConfig, HaMode, HaPeer, NodeRole, ReplBackend, RpoMode, S3Config,
};
pub use role::{FencedError, HaGate, HaRole, HaRuntime};
pub use shared::HaShared;

#[cfg(test)]
mod tests;
