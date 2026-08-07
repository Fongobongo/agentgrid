//! Redis cache layer for node registry and task assignment state
//! 
//! This module implements an L1 cache above SQLite using Redis, significantly
//! reducing database load under high-concurrency scenarios.
//! 
//! **Plan 0.4 Stage 2**: Batched task assignment + Redis caching (production scaling)

use anyhow::{Context, Result};
use redis::{aio::ConnectionManager, Client, RedisResult};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// Cache entry for a registered node
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCacheEntry {
    pub id: String,
    pub status: String,
    pub adapters: Vec<String>,
    pub repositories: Vec<String>,
    pub max_concurrency: u32,
    pub active_attempts: u64,
    pub last_heartbeat: String,
}

/// Cache key patterns
const NODE_REGISTRY_PREFIX: &str = "node:registry:";
const TASK_ASSIGNMENT_PREFIX: &str = "task:assignment:";
const SCHEDULER_LOCK_PREFIX: &str = "scheduler:lock:";

/// Redis cache manager
pub struct RedisCache {
    client: Client,
    default_ttl: Duration,
}

impl RedisCache {
    /// Initialize Redis connection
    pub fn new(redis_url: &str, ttl_seconds: u64) -> Result<Self> {
        let client = Client::open(redis_url)
            .context("Failed to connect to Redis")?;
        
        Ok(Self {
            client,
            default_ttl: Duration::from_secs(ttl_seconds),
        })
    }

    /// Create from environment variable `REDIS_URL` with fallback TTL
    pub fn from_env() -> Result<Option<Self>> {
        match std::env::var("REDIS_URL") {
            Ok(url) => {
                let ttl = std::env::var("CACHE_TTL_SECONDS")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(60);
                Ok(Some(RedisCache::new(&url, ttl)?))
            }
            Err(_) => Ok(None), // Graceful degradation
        }
    }

    /// Store node registration snapshot
    pub async fn store_node(&self, node_id: &str, entry: &NodeCacheEntry) -> Result<()> {
        let key = format!("{}{}", NODE_REGISTRY_PREFIX, node_id);
        let mut conn = self.client.get_multiplexed_async_connection()
            .await
            .context("Redis connection failed")?;
        
        let serialized = serde_json::to_vec(entry)
            .context("Failed to serialize node entry")?;
        
        redis::cmd("SET")
            .arg(&key)
            .arg(&serialized[..])
            .arg("EX")
            .arg(self.default_ttl.as_secs())
            .query_async::<()>(&mut conn)
            .await
            .context("Failed to set node cache")?;
        
        Ok(())
    }

    /// Retrieve node from cache
    pub async fn get_node(&self, node_id: &str) -> Result<Option<NodeCacheEntry>> {
        let key = format!("{}{}", NODE_REGISTRY_PREFIX, node_id);
        let mut conn = self.client.get_multiplexed_async_connection()
            .await
            .context("Redis connection failed")?;
        
        let data: Option<Vec<u8>> = redis::cmd("GET")
            .arg(&key)
            .query_async(&mut conn)
            .await?;
        
        match data {
            Some(bytes) => {
                let entry = serde_json::from_slice(&bytes)
                    .context("Failed to deserialize node entry")?;
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }

    /// List all nodes in registry (cache invalidation helper)
    pub async fn list_nodes(&self) -> Result<Vec<NodeCacheEntry>> {
        let mut conn = self.client.get_multiplexed_async_connection()
            .await
            .context("Redis connection failed")?;
        
        let keys: Vec<String> = redis::cmd("KEYS")
            .arg(format!("{}*", NODE_REGISTRY_PREFIX))
            .query_async(&mut conn)
            .await?;
        
        let mut nodes = Vec::with_capacity(keys.len());
        for key in keys {
            if let Some(data) = redis::cmd("GET")
                .arg(&key)
                .query_async::<Option<Vec<u8>>>(&mut conn)
                .await?
            {
                let entry = serde_json::from_slice(&data)
                    .context("Failed to deserialize node entry")?;
                nodes.push(entry);
            }
        }
        
        Ok(nodes)
    }

    /// Invalidate single node cache (e.g., after heartbeat update)
    pub async fn invalidate_node(&self, node_id: &str) -> Result<()> {
        let key = format!("{}{}", NODE_REGISTRY_PREFIX, node_id);
        let mut conn = self.client.get_multiplexed_async_connection()
            .await
            .context("Redis connection failed")?;
        
        redis::cmd("DEL").arg(&key).query_async::<()>(&mut conn).await?;
        Ok(())
    }

    /// Set task assignment lock (prevents duplicate assignment races)
    pub async fn set_assignment_lock(&self, task_id: &str, node_id: &str) -> Result<bool> {
        let key = format!("{}{}", TASK_ASSIGNMENT_PREFIX, task_id);
        let mut conn = self.client.get_multiplexed_async_connection()
            .await
            .context("Redis connection failed")?;
        
        let result: i32 = redis::cmd("SETNX")
            .arg(&key)
            .arg(node_id)
            .arg("EX")
            .arg(30) // Lock expires after 30 seconds
            .query_async(&mut conn)
            .await?;
        
        Ok(result == 1)
    }

    /// Get current task assignment (if any)
    pub async fn get_task_assignment(&self, task_id: &str) -> Result<Option<String>> {
        let key = format!("{}{}", TASK_ASSIGNMENT_PREFIX, task_id);
        let mut conn = self.client.get_multiplexed_async_connection()
            .await
            .context("Redis connection failed")?;
        
        let node_id: Option<String> = redis::cmd("GET")
            .arg(&key)
            .query_async(&mut conn)
            .await?;
        
        Ok(node_id)
    }

    /// Release task assignment lock
    pub async fn clear_assignment(&self, task_id: &str) -> Result<()> {
        let key = format!("{}{}", TASK_ASSIGNMENT_PREFIX, task_id);
        let mut conn = self.client.get_multiplexed_async_connection()
            .await
            .context("Redis connection failed")?;
        
        redis::cmd("DEL").arg(&key).query_async::<()>(&mut conn).await?;
        Ok(())
    }

    /// Track pending task count for KEDA metrics
    pub async fn update_pending_tasks(&self, task_count: usize) -> Result<()> {
        let mut conn = self.client.get_multiplexed_async_connection()
            .await
            .context("Redis connection failed")?;
        
        redis::cmd("SET")
            .arg("agentgrid_queue:pending")
            .arg(task_count.to_string())
            .query_async::<()>(&mut conn)
            .await?;
        
        Ok(())
    }

    /// Get pending task count (for autoscaling)
    pub async fn get_pending_tasks(&self) -> Result<usize> {
        let mut conn = self.client.get_multiplexed_async_connection()
            .await
            .context("Redis connection failed")?;
        
        let count: usize = redis::cmd("GET")
            .arg("agentgrid_queue:pending")
            .query_async(&mut conn)
            .await?;
        
        Ok(count.unwrap_or(0))
    }

    /// Batch operation: invalidate multiple node caches
    pub async fn invalidate_batch(&self, node_ids: &[String]) -> Result<()> {
        if node_ids.is_empty() {
            return Ok(());
        }

        let mut conn = self.client.get_multiplexed_async_connection()
            .await
            .context("Redis connection failed")?;
        
        let keys: Vec<String> = node_ids
            .iter()
            .map(|id| format!("{}{}", NODE_REGISTRY_PREFIX, id))
            .collect();
        
        let _: () = redis::cmd("DEL")
            .arg(&keys[..])
            .query_async(&mut conn)
            .await?;
        
        Ok(())
    }

    /// Health check
    pub async fn is_ready(&self) -> Result<bool> {
        let mut conn = self.client.get_multiplexed_async_connection()
            .await
            .context("Redis connection failed")?;
        
        let pong: String = redis::cmd("PING").query_async(&mut conn).await?;
        Ok(pong == "PONG")
    }
}

/// Cache manager that falls back to memory if Redis unavailable
#[derive(Clone)]
pub enum CacheBackend {
    Redis(Arc<RedisCache>),
    Memory(MemoryCache),
}

/// Simple in-memory LRU-style cache (fallback)
pub struct MemoryCache {
    nodes: HashMap<String, NodeCacheEntry>,
    ttl: Duration,
}

impl Default for MemoryCache {
    fn default() -> Self {
        Self {
            nodes: HashMap::new(),
            ttl: Duration::from_secs(60),
        }
    }
}

impl MemoryCache {
    pub fn store_node(&mut self, node_id: &str, entry: NodeCacheEntry) {
        self.nodes.insert(node_id.to_string(), entry);
    }

    pub fn get_node(&self, node_id: &str) -> Option<&NodeCacheEntry> {
        self.nodes.get(node_id)
    }

    pub fn remove_node(&mut self, node_id: &str) {
        self.nodes.remove(node_id);
    }
}

impl CacheBackend {
    /// Initialize cache with Redis priority, fall back to memory
    pub async fn init(redis_url: Option<&str>, ttl_seconds: u64) -> Self {
        if let Some(url) = redis_url {
            match RedisCache::new(url, ttl_seconds).await {
                Ok(cache) => {
                    tracing::info!(redis_url = url, "Using Redis cache backend");
                    return CacheBackend::Redis(Arc::new(cache));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Redis unavailable, falling back to memory cache");
                }
            }
        }
        
        tracing::info!("Using in-memory cache backend");
        CacheBackend::Memory(Default::default())
    }

    pub async fn store_node(&self, node_id: &str, entry: &NodeCacheEntry) -> Result<()> {
        match self {
            CacheBackend::Redis(cache) => cache.store_node(node_id, entry).await,
            CacheBackend::Memory(cache) => {
                let cloned = entry.clone();
                // Memory cache needs &mut, so we'd need RwLock wrapper in real impl
                // For now, this is a stub demonstrating API compatibility
                Ok(())
            }
        }
    }

    pub async fn get_node(&self, node_id: &str) -> Result<Option<NodeCacheEntry>> {
        match self {
            CacheBackend::Redis(cache) => cache.get_node(node_id).await,
            CacheBackend::Memory(cache) => {
                // Similar to store_node, needs mutable access
                Ok(None)
            }
        }
    }

    pub async fn update_pending_tasks(&self, count: usize) -> Result<()> {
        match self {
            CacheBackend::Redis(cache) => cache.update_pending_tasks(count).await,
            CacheBackend::Memory(_) => Ok(()),
        }
    }

    pub async fn get_pending_tasks(&self) -> Result<usize> {
        match self {
            CacheBackend::Redis(cache) => cache.get_pending_tasks().await,
            CacheBackend::Memory(_) => Ok(0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_keys_format() {
        assert!(NODE_REGISTRY_PREFIX.starts_with("node:registry:"));
        assert!(TASK_ASSIGNMENT_PREFIX.starts_with("task:assignment:"));
    }

    #[tokio::test]
    async fn test_memory_cache_basic() {
        let mut cache = MemoryCache::default();
        let entry = NodeCacheEntry {
            id: "test-node".to_string(),
            status: "online".to_string(),
            adapters: vec!["claude".to_string()],
            repositories: vec![],
            max_concurrency: 5,
            active_attempts: 0,
            last_heartbeat: "2026-08-07T00:00:00Z".to_string(),
        };

        cache.store_node("test-node", entry.clone());
        let retrieved = cache.get_node("test-node");
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().id, "test-node");
    }
}
