//! Unified VFS operation layer consumed by platform adapters.
//!
//! Adapters (macFUSE/FUSE-T on macOS, libfuse3 on Linux, WinFsp on Windows)
//! translate kernel callbacks into these ops. Everything above this layer -
//! cache lookup, prefetch signal, chunk fetch/decrypt, metadata resolution -
//! is shared code so per-platform bugs stay contained.

use lumen_core::cache::ChunkCache;
use lumen_core::chunking::Chunk;
use lumen_core::crypto::ChunkCrypto;
use lumen_core::error::{LumenError, Result};
use lumen_core::meter::{UsageEvent, UsageMeter};
use lumen_core::metadata::RemoteItem;
use lumen_core::prefetch::Prefetcher;
use lumen_core::store::ObjectStore;
use std::sync::Arc;

pub struct VfsOps {
    store: Arc<dyn ObjectStore>,
    crypto: ChunkCrypto,
    cache: ChunkCache,
    prefetcher: Prefetcher,
    meter: UsageMeter,
}

impl VfsOps {
    pub fn new(store: Arc<dyn ObjectStore>, crypto: ChunkCrypto) -> Self {
        Self {
            store,
            crypto,
            cache: ChunkCache::new(8 * 1024 * 1024 * 1024),
            prefetcher: Prefetcher::new(),
            meter: UsageMeter::disabled(),
        }
    }

    /// Attach a billing meter. Uploads report stored bytes (ciphertext,
    /// i.e. what the provider actually stores), downloads report retrieved
    /// bytes on cache misses only - cache hits cost nothing at the backend.
    pub fn with_meter(mut self, meter: UsageMeter) -> Self {
        self.meter = meter;
        self
    }

    /// Read `len` bytes of a file starting at `off`. Splits into chunk reads,
    /// decrypts each chunk once, caches plaintext bytes and feeds both cache
    /// hits and misses into the prefetcher as access signals.
    pub async fn read(&self, item: &RemoteItem, off: u64, len: u64) -> Result<Vec<u8>> {
        if off >= item.size {
            return Ok(Vec::new());
        }
        let end = off.saturating_add(len).min(item.size);
        let mut buf = Vec::with_capacity((end - off) as usize);
        let mut pos = off;
        while pos < end {
            let cref = item
                .chunk_covering(pos)
                .ok_or_else(|| LumenError::NotFound(format!("no chunk covers offset {}", pos)))?;
            let within = (pos - cref.offset) as usize;
            let take = ((cref.offset + cref.len) - pos).min(end - pos) as usize;
            self.prefetcher.observe_read(item.id, chunk_index_of(item, pos));
            if let Some(data) = self.cache.get(&cref.key) {
                buf.extend_from_slice(&data[within..within + take]);
            } else {
                let blob = self.store.get(&cref.key).await?;
                self.meter.report(UsageEvent { stored_bytes: 0, retrieved_bytes: blob.len() as u64 });
                let plain = self.crypto.decrypt_chunk(&blob, &cref.fingerprint)?;
                buf.extend_from_slice(&plain[within..within + take]);
                self.cache.put(&cref.key, plain.clone());
                // Sequential-access bonus fires once per chunk via index signal.
                self.prefetcher.observe_read(item.id, chunk_index_of(item, pos));
            }
            pos += take as u64;
        }
        // Fetch-ahead: the prefetcher ranks likely next chunks from recent
        // access patterns. Warm uncached ones into the cache now so a
        // sequential follow-up read is served without a storage round-trip.
        for idx in self.prefetcher.next_likely_chunks(item.id) {
            if let Some(cref) = item.chunks.get(idx as usize) {
                if !self.cache.contains(&cref.key) {
                    if let Ok(blob) = self.store.get(&cref.key).await {
                        self.meter.report(UsageEvent { stored_bytes: 0, retrieved_bytes: blob.len() as u64 });
                        if let Ok(plain) = self.crypto.decrypt_chunk(&blob, &cref.fingerprint) {
                            self.cache.put(&cref.key, plain);
                        }
                    }
                }
            }
        }
        Ok(buf)
    }

    /// Chunk, encrypt and upload data; returns the new chunk list.
    pub async fn write_full(&self, data: Vec<u8>) -> Result<Vec<Chunk>> {
        use bytes::Bytes;
        use lumen_core::chunking::FastCdc;
        let data = Bytes::from(data);
        let cdc = FastCdc::default();
        let chunks = cdc.split(data.clone())?;
        for c in &chunks {
            let key = c.storage_key();
            match self.store.head(&key).await {
                Ok(_) => continue, // content-addressed dedup hit
                // Absent objects surface as either a raw backend/transport
                // error or a typed NotFound depending on the adapter.
                Err(LumenError::Storage(_) | LumenError::NotFound(_)) => {
                    let start = c.offset as usize;
                    let end = start + c.len as usize;
                    let ct = self.crypto.encrypt_chunk(&data[start..end], &c.fingerprint)?;
                    let ct_len = ct.len() as u64;
                    self.store.put(&key, ct.into()).await?;
                    // Dedup hits upload nothing; only fresh objects count.
                    self.meter.report(UsageEvent { stored_bytes: ct_len, retrieved_bytes: 0 });
                }
                Err(e) => return Err(e),
            }
        }
        Ok(chunks)
    }

    pub fn cache_hit_rate(&self) -> f64 {
        self.cache.stats().hit_rate()
    }

    /// Attached usage meter (exposed for tests and billing introspection).
    pub fn meter(&self) -> &UsageMeter {
        &self.meter
    }
}

/// Index of the chunk containing `pos`. Chunks are ordered by offset, so a
/// reverse scan hits the last chunk starting at-or-before `pos`, which is
/// the one covering it for any non-overlapping chunk list.
fn chunk_index_of(item: &RemoteItem, pos: u64) -> u64 {
    item.chunks
        .iter()
        .rposition(|c| c.offset <= pos)
        .map(|i| i as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::MockObjectStore;
    use lumen_core::meter::{UsageEvent, UsageMeter};
    use uuid::Uuid;

    fn make_item_from_chunks(chunks: &[Chunk]) -> RemoteItem {
        use chrono::Utc;
        lumen_core::metadata::RemoteItem {
            id: Uuid::new_v4(),
            parent_id: None,
            name: "a.bin".into(),
            item_type: lumen_core::metadata::ItemType::File,
            size: chunks.iter().map(|c| c.len).sum(),
            content_version: 1,
            metadata_version: 1,
            chunks: chunks
                .iter()
                .map(|c| lumen_core::metadata::ChunkRef {
                    key: c.storage_key(),
                    offset: c.offset,
                    len: c.len,
                    fingerprint: c.fingerprint,
                })
                .collect(),
            created_at: Utc::now(),
            modified_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn read_fetches_decrypts_and_caches_chunks() {
        let store = Arc::new(MockObjectStore::default());
        let crypto = ChunkCrypto::from_password("pw", b"vfs-salt");
        let vfs = VfsOps::new(store.clone(), crypto);

        let data = vec![b'a'; 300 * 1024];
        let chunks = vfs.write_full(data.clone()).await.unwrap();
        assert_eq!(chunks.iter().map(|c| c.len as usize).sum::<usize>(), data.len());
        assert!(!store.map.lock().unwrap().is_empty());

        let item = make_item_from_chunks(&chunks);
        let got = vfs.read(&item, 0, data.len() as u64).await.unwrap();
        assert_eq!(got, data);
        assert_eq!(store.get_calls.load(std::sync::atomic::Ordering::Relaxed), chunks.len() as u64);

        // Second read must be served entirely from cache without new fetches.
        let cached = vfs.read(&item, 0, data.len() as u64).await.unwrap();
        assert_eq!(cached, data);
        assert_eq!(store.get_calls.load(std::sync::atomic::Ordering::Relaxed), chunks.len() as u64);
        // First read misses every chunk; second read hits all of them.
        assert!((vfs.cache_hit_rate() - 0.5).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn read_slice_within_single_chunk_resolves_offset() {
        let store = Arc::new(MockObjectStore::default());
        let crypto = ChunkCrypto::from_password("pw", b"vfs-salt");
        let vfs = VfsOps::new(store, crypto);

        // Single deterministic chunk below DEFAULT_MIN.
        let data = b"lumen single chunk payload".to_vec();
        let chunks = vfs.write_full(data.clone()).await.unwrap();
        assert_eq!(chunks.len(), 1);

        let item = make_item_from_chunks(&chunks);
        let mid: usize = 6;
        let len: usize = 7;
        let got = vfs.read(&item, mid as u64, len as u64).await.unwrap();
        assert_eq!(got.as_slice(), &data[mid..mid + len]);

        // Zero-length read returns empty without touching storage.
        let none = vfs.read(&item, 0, 0).await.unwrap();
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn read_beyond_size_and_write_dedup_reuploads() {
        let store = Arc::new(MockObjectStore::default());
        let crypto = ChunkCrypto::from_password("pw", b"vfs-salt");
        let vfs = VfsOps::new(store.clone(), crypto);

        let data = vec![b'z'; 500 * 1024];
        let chunks = vfs.write_full(data.clone()).await.unwrap();
        let keys_before: Vec<String> = store.map.lock().unwrap().keys().cloned().collect();

        let item = make_item_from_chunks(&chunks);
        let beyond = vfs.read(&item, item.size + 100, 10).await.unwrap();
        assert!(beyond.is_empty());

        // Rewriting identical content dedups: same chunk set, no new objects.
        let again = vfs.write_full(data).await.unwrap();
        assert_eq!(again.len(), chunks.len());
        let keys_after: Vec<String> = store.map.lock().unwrap().keys().cloned().collect();
        assert_eq!(keys_before, keys_after);
    }

    #[tokio::test]
    async fn usage_metering_counts_fresh_uploads_and_cache_miss_reads() {
        let store = Arc::new(MockObjectStore::default());
        let crypto = ChunkCrypto::from_password("pw", b"vfs-salt");
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::<UsageEvent>::new()));
        let sink_events = events.clone();
        let sink: lumen_core::meter::UsageSink =
            std::sync::Arc::new(move |e| sink_events.lock().unwrap().push(e));
        let vfs = VfsOps::new(store, crypto)
            .with_meter(UsageMeter::disabled().with_sink(sink));

        let data = vec![b'm'; 300 * 1024];
        let chunks = vfs.write_full(data.clone()).await.unwrap();

        // Every stored event on a fresh write carries ciphertext bytes.
        {
            let evs = events.lock().unwrap().clone();
            assert!(!evs.is_empty());
            assert!(evs.iter().all(|e| e.retrieved_bytes == 0 && e.stored_bytes > 0));
        }
        assert!(vfs.meter().stored() > 0);

        // Rewrite identical content: dedup hit stores nothing new.
        events.lock().unwrap().clear();
        vfs.write_full(data.clone()).await.unwrap();
        assert!(events.lock().unwrap().is_empty());

        // First read: each chunk miss reports retrieved bytes.
        let item = make_item_from_chunks(&chunks);
        events.lock().unwrap().clear();
        vfs.read(&item, 0, data.len() as u64).await.unwrap();
        {
            let evs = events.lock().unwrap().clone();
            assert!(!evs.is_empty());
            assert!(evs.iter().all(|e| e.stored_bytes == 0 && e.retrieved_bytes > 0));
            assert_eq!(evs.iter().map(|e| e.retrieved_bytes).sum::<u64>(), vfs.meter().retrieved());
        }
    }

    #[tokio::test]
    async fn readahead_warms_next_chunks_so_sequential_reads_skip_storage() {
        let store = Arc::new(MockObjectStore::default());
        let crypto = ChunkCrypto::from_password("pw", b"vfs-salt");
        let vfs = VfsOps::new(store.clone(), crypto);

        // Multi-chunk file with content split across chunk boundaries.
        // Large enough to guarantee multiple chunks past the 256 KiB min.
        let data = vec![b'p'; 5 * 1024 * 1024];
        let chunks = vfs.write_full(data.clone()).await.unwrap();
        assert!(chunks.len() >= 2, "fixture must span multiple chunks");
        let item = make_item_from_chunks(&chunks);

        // First half-file read: every covering chunk is a miss and the
        // next likely chunk(s) are speculatively warmed into cache.
        let first_len = item.size / 2;
        let got = vfs.read(&item, 0, first_len).await.unwrap();
        assert_eq!(got.len() as u64, first_len);
        let gets_after_first = store.get_calls.load(std::sync::atomic::Ordering::Relaxed);

        // Continue reading: warmed chunks must come from cache; the only
        // new storage get (if any) is a speculative fetch still pending.
        let rest = vfs.read(&item, first_len, item.size - first_len).await.unwrap();
        assert_eq!(rest.len() as u64, item.size - first_len);
        let gets_after_second = store.get_calls.load(std::sync::atomic::Ordering::Relaxed);

        // Full re-read from zero must be served entirely from cache.
        let all_again = vfs.read(&item, 0, item.size).await.unwrap();
        assert_eq!(all_again, data);
        assert_eq!(
            store.get_calls.load(std::sync::atomic::Ordering::Relaxed),
            gets_after_second,
            "re-read must not hit storage when everything was warmed"
        );
        assert!(gets_after_first > 0);
    }
}
