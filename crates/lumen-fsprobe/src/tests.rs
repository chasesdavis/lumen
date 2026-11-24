//! Shared test doubles for the fsprobe VFS layer.

use lumen_core::error::{LumenError, Result};
use lumen_core::store::{ListEntry, ObjectStore};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// In-memory object store implementing the full trait surface with call
/// counters - lets VFS tests assert fetch counts (cache behavior) without
/// network round-trips or a local MinIO deployment.
#[derive(Default)]
pub struct MockObjectStore {
    pub map: Mutex<HashMap<String, bytes::Bytes>>,
    pub get_calls: AtomicU64,
    pub put_calls: AtomicU64,
    pub head_calls: AtomicU64,
}

#[async_trait::async_trait]
impl ObjectStore for MockObjectStore {
    async fn put(&self, key: &str, body: bytes::Bytes) -> Result<()> {
        self.put_calls.fetch_add(1, Ordering::Relaxed);
        self.map
            .lock()
            .map_err(|_| LumenError::Storage("store poisoned".into()))?
            .insert(key.to_string(), body);
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<bytes::Bytes> {
        self.get_calls.fetch_add(1, Ordering::Relaxed);
        self.map
            .lock()
            .map_err(|_| LumenError::Storage("store poisoned".into()))?
            .get(key)
            .cloned()
            .ok_or_else(|| LumenError::NotFound(key.to_string()))
    }

    async fn get_range(&self, key: &str, offset: u64, len: u64) -> Result<bytes::Bytes> {
        let all = self.get(key).await?;
        let s = offset.min(all.len() as u64) as usize;
        let e = (offset + len).min(all.len() as u64) as usize;
        if s > e || e > all.len() {
            return Err(LumenError::Storage("bad range".into()));
        }
        Ok(all.slice(s..e))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.map
            .lock()
            .map_err(|_| LumenError::Storage("store poisoned".into()))?
            .remove(key);
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>> {
        let map = self
            .map
            .lock()
            .map_err(|_| LumenError::Storage("store poisoned".into()))?;
        Ok(map
            .iter()
            .filter(|(k, _)| k.starts_with(prefix))
            .map(|(k, v)| ListEntry {
                key: k.clone(),
                size: v.len() as u64,
                etag: None,
            })
            .collect())
    }

    async fn head(&self, key: &str) -> Result<u64> {
        self.head_calls.fetch_add(1, Ordering::Relaxed);
        self.map
            .lock()
            .map_err(|_| LumenError::Storage("store poisoned".into()))?
            .get(key)
            .map(|v| v.len() as u64)
            .ok_or_else(|| LumenError::NotFound(key.to_string()))
    }
}

impl MockObjectStore {
    /// Helper for tests that need an Arc-wrapped instance directly.
    pub fn arc() -> Arc<Self> {
        Arc::new(Self::default())
    }
}
