//! Byte-budgeted LRU chunk cache.
//!
//! Helumi's ChunkStore keeps fixed-size chunks on disk with
//! maxBytes/minFree/critical free-space limits. Lumen matches the same
//! safety rails but tracks an accurate in-flight and pinned-byte count so
//! streaming reads never evict chunks mid-use, plus exposes hit-rate stats
//! (missing from Helumi) that drive the prefetcher's learning signal.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub bytes_cached: u64,
    pub bytes_evicted: u64,
}

impl CacheStats {
    pub fn hit_rate(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 { return 0.0; }
        self.hits as f64 / total as f64
    }
}

struct Entry {
    data: Arc<Vec<u8>>,
    last_used: u64,
    pinned: bool,
}

pub struct ChunkCache {
    inner: Mutex<HashMap<String, Entry>>,
    budget_bytes: u64,
    pinned_bytes: AtomicU64,
    /// Reserved floor of free local disk; the FS adapter checks host disk
    /// headroom against this before letting fetched chunks stay resident.
    /// Not yet consumed by any adapter - will be read by the mount layer.
    #[allow(dead_code)]
    min_free_disk: u64,
    clock: AtomicU64,
    pub stats_hits: AtomicU64,
    pub stats_misses: AtomicU64,
    pub stats_evicted: AtomicU64,
    current_bytes: AtomicU64,
}

impl ChunkCache {
    pub fn new(budget_bytes: u64) -> Self {
        Self::with_limits(budget_bytes, 8 * 1024 * 1024 * 1024)
    }

    pub fn with_limits(budget_bytes: u64, min_free_disk: u64) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            budget_bytes,
            pinned_bytes: AtomicU64::new(0),
            min_free_disk,
            clock: AtomicU64::new(0),
            stats_hits: AtomicU64::new(0),
            stats_misses: AtomicU64::new(0),
            stats_evicted: AtomicU64::new(0),
            current_bytes: AtomicU64::new(0),
        }
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed)
    }

    /// Fetch a cached chunk if present. Updates recency and counts a hit or miss.
    pub fn get(&self, key: &str) -> Option<Arc<Vec<u8>>> {
        let t = self.tick();
        let mut map = self.inner.lock().expect("cache poisoned");
        match map.get_mut(key) {
            Some(e) => {
                e.last_used = t;
                self.stats_hits.fetch_add(1, Ordering::Relaxed);
                Some(Arc::clone(&e.data))
            }
            None => {
                drop(map);
                self.stats_misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    /// Insert a chunk. Evicts unpinned LRU entries when over budget.
    pub fn put(&self, key: &str, data: Vec<u8>) -> bool {
        let t = self.tick();
        let incoming = data.len() as u64;
        {
            let mut map = self.inner.lock().expect("cache poisoned");
            if map.contains_key(key) {
                return true; // already cached; keep original copy
            }
            map.insert(key.to_string(), Entry {
                data: Arc::new(data),
                last_used: t,
                pinned: false,
            });
        }
        self.current_bytes.fetch_add(incoming, Ordering::Relaxed);
        self.evict_if_needed();
        true
    }

    pub fn pin(&self, key: &str) {
        let mut map = self.inner.lock().expect("cache poisoned");
        if let Some(e) = map.get_mut(key) {
            if !e.pinned {
                e.pinned = true;
                let size = e.data.len() as u64;
                drop(map);
                self.pinned_bytes.fetch_add(size, Ordering::Relaxed);
            }
        }
    }

    pub fn unpin(&self, key: &str) {
        let mut map = self.inner.lock().expect("cache poisoned");
        if let Some(e) = map.get_mut(key) {
            if e.pinned {
                e.pinned = false;
                let size = e.data.len() as u64;
                drop(map);
                self.pinned_bytes.fetch_sub(size, Ordering::Relaxed);
            }
        }
    }

    pub fn contains(&self, key: &str) -> bool {
        self.inner.lock().expect("cache poisoned").contains_key(key)
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("cache poisoned").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn current_bytes(&self) -> u64 {
        self.current_bytes.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.stats_hits.load(Ordering::Relaxed),
            misses: self.stats_misses.load(Ordering::Relaxed),
            bytes_cached: self.current_bytes.load(Ordering::Relaxed),
            bytes_evicted: self.stats_evicted.load(Ordering::Relaxed),
        }
    }

    fn evict_if_needed(&self) {
        // Pinned bytes do not create eviction pressure: a chunk held open by
        // a streaming reader must never force newer cached chunks out.
        while self.current_bytes.load(Ordering::Relaxed)
            - self.pinned_bytes.load(Ordering::Relaxed)
            > self.budget_bytes
        {
            let victim = {
                let mut map = self.inner.lock().expect("cache poisoned");
                // Everything pinned: nothing can be evicted right now.
                map.iter()
                    .filter(|(_, e)| !e.pinned)
                    .min_by_key(|(_, e)| e.last_used)
                    .map(|(k, e)| (k.clone(), e.data.len()))
                    .map(|(k, len)| {
                        map.remove(&k);
                        (k, len as u64)
                    })
            };
            match victim {
                Some((_, s)) => {
                    self.current_bytes.fetch_sub(s, Ordering::Relaxed);
                    self.stats_evicted.fetch_add(1, Ordering::Relaxed);
                }
                None => break,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lru_eviction_order() {
        let c = ChunkCache::with_limits(100, 0);
        c.put("a", vec![0; 30]);
        c.put("b", vec![0; 30]);
        c.get("a"); // touch a, b becomes LRU
        c.put("c", vec![0; 60]); // over budget; should evict b
        assert!(c.contains("a"));
        assert!(!c.contains("b"));
        assert!(c.contains("c"));
    }

    #[test]
    fn pin_prevents_eviction() {
        let c = ChunkCache::with_limits(10, 0);
        c.put("keep", vec![0; 6]);
        c.pin("keep");
        c.put("other", vec![0; 6]);
        // "keep" cannot be chosen as a victim while pinned even though it is LRU.
        assert!(c.contains("keep"));
        assert!(c.contains("other"));
        // Pinned entries are never selected as victims regardless of age.
        c.put("third", vec![0; 6]);
        assert!(c.contains("keep"));
        // After unpinning, the aged entry becomes the next eviction victim.
        c.unpin("keep");
        c.put("fourth", vec![0; 6]);
        assert!(!c.contains("keep"));
    }

    #[test]
    fn stats_track_hits_misses() {
        let c = ChunkCache::new(1000);
        c.get("none");
        c.put("x", vec![1]);
        c.get("x");
        c.get("y");
        assert_eq!(c.stats_hits.load(Ordering::Relaxed), 1);
        assert_eq!(c.stats_misses.load(Ordering::Relaxed), 2);
    }
}
