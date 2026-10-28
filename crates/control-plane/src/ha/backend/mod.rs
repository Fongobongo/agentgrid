//! Replication data-plane backends.
//!
//! Election always stays on the peer/witness transport; object storage is only
//! a durable place for snapshots, changesets and artifact bytes. The peer
//! backend is implemented by [`super::transport`]; this module adds the S3
//! compatible backend used by Supabase Storage, MinIO, R2 and AWS S3.

pub mod s3;

pub use s3::{ChangesetObject, S3Client};
