//! Cache abstraction layer for AgentGrid control plane
//! 
//! Provides Redis-backed caching with memory fallback for:
//! - Node registry (live node status snapshots)
//! - Task assignment locks (race condition prevention)
//! - Scheduler metrics (pending task counts for autoscaling)
//! 
//! **Plan 0.4 Stage 2**: Production scaling infrastructure

pub mod redis;

pub use redis::{CacheBackend, MemoryCache, NodeCacheEntry, RedisCache};
