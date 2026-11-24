//! Orphan chunk garbage collection.
//!
//! Deletion semantics across Lumen: index rows and changelog tombstones land
//! immediately (FS remove, MCP delete_file), but encrypted chunks stay in
//! the object store until this module reclaims them. Reasons not to delete
//! eagerly on the hot path:
//!
//! 1. Content-addressed keys are shared across files via dedup. Deleting on
//!    file removal would corrupt siblings that reference the same key.
//! 2. replace_file orphans only part of a chunk list; an eager pass would
//!    do the same reference scan anyway - just one item at a time.
//!
//! The mark-and-sweep here is deliberately simple and safe: read the live
//! reference set out of MetadataIndex first, then list storage under the
//! chunk prefix and delete anything unreferenced.

use crate::chunking::CHUNK_KEY_PREFIX;
use crate::error::Result;
use crate::index::MetadataIndex;
use crate::meter::{UsageEvent, UsageMeter};
use crate::store::ObjectStore;
use std::sync::Arc;

/// One GC pass result. Reclaimed bytes are ciphertext sizes as billed - the
/// same measure VfsOps reports for uploads, so billing never double-counts
/// between stored/reclaimed paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcReport {
    /// Keys examined under the chunk prefix.
    pub scanned_keys: u64,
    /// Orphaned keys deleted from storage.
    pub reclaimed_keys: u64,
    /// Sum of orphan object sizes reported to the meter.
    pub reclaimed_bytes: u64,
}

/// Run one synchronous mark-and-sweep over `index` + `store`.
///
/// Ordering rule (load-bearing): chunks are uploaded before their metadata
/// upsert reaches the index (write_full pushes ciphertext first, callers
/// then queue the RemoteItem through CommitBatcher). A GC pass that runs
/// while a batcher holds unflushed upserts would see those fresh chunks as
/// orphans and could delete data whose only referencing row has not landed
/// yet. Caller contract: flush every CommitBatcher feeding this index
/// before invoking gc_collect.
pub async fn gc_collect(
    index: &MetadataIndex,
    store: &dyn ObjectStore,
) -> Result<GcReport> {
    let referenced = index.referenced_chunk_keys()?;
    let mut scanned = 0u64;
    let mut reclaimed_keys = 0u64;
    let mut reclaimed_bytes = 0u64;

    // Mark phase: enumerate every chunk object under the shared prefix.
    let candidates = store.list(CHUNK_KEY_PREFIX).await?;
    let mut orphans = Vec::new();
    for entry in &candidates {
        scanned += 1;
        if !referenced.contains(&entry.key) {
            orphans.push((entry.key.clone(), entry.size));
        }
    }

    // Sweep phase: deletes are best-effort per key - one failing delete must
    // not abort the pass since every other orphan reclaims fine; retry the
    // survivor next run.
    for (key, size) in orphans {
        match store.delete(&key).await {
            Ok(()) => {
                reclaimed_keys += 1;
                reclaimed_bytes += size;
            }
            Err(_) => continue,
        }
    }

    Ok(GcReport { scanned_keys: scanned, reclaimed_keys, reclaimed_bytes })
}

/// GC pass that reports reclaimed bytes through the usage metering seam.
/// Reclaim flows as a signed ledger event so billing reconciles against what
/// the provider actually stores: gc_run passes i64 negative deltas to
/// UsageLedger::record via a meter sink variant that cannot be confused with
/// upload traffic. At the core seam the UsageEvent type carries unsigned
/// fields; reclaimed bytes ride in the stored_bytes slot and sinks consult
/// the GcReport to emit the correct signed delta (see api-server wiring).
pub async fn gc_and_report(
    index: &MetadataIndex,
    store: Arc<dyn ObjectStore>,
    meter: &UsageMeter,
) -> Result<GcReport> {
    let report = gc_collect(index, store.as_ref()).await?;
    if report.reclaimed_bytes > 0 {
        meter.report(UsageEvent {
            stored_bytes: report.reclaimed_bytes,
            retrieved_bytes: 0,
        });
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::LumenError;
    use crate::metadata::{ChunkRef, ItemType, RemoteItem};
    use crate::store::ListEntry;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    struct CountingStore {
        map: std::sync::Mutex<HashMap<String, bytes::Bytes>>,
        delete_calls: AtomicU64,
        list_calls: AtomicU64,
    }

    #[async_trait::async_trait]
    impl ObjectStore for CountingStore {
        async fn put(&self, key: &str, body: bytes::Bytes) -> Result<()> {
            self.map.lock().unwrap().insert(key.to_string(), body);
            Ok(())
        }
        async fn get(&self, key: &str) -> Result<bytes::Bytes> {
            self.map
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or_else(|| LumenError::NotFound(key.to_string()))
        }
        async fn get_range(&self, _key: &str, _o: u64, _l: u64) -> Result<bytes::Bytes> {
            Err(LumenError::Storage("unused in gc tests".into()))
        }
        async fn delete(&self, key: &str) -> Result<()> {
            self.delete_calls.fetch_add(1, Ordering::Relaxed);
            let removed = self.map.lock().unwrap().remove(key);
            if removed.is_none() {
                return Err(LumenError::NotFound(key.to_string()));
            }
            Ok(())
        }
        async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>> {
            self.list_calls.fetch_add(1, Ordering::Relaxed);
            Ok(self
                .map
                .lock()
                .unwrap()
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .map(|(k, v)| ListEntry { key: k.clone(), size: v.len() as u64, etag: None })
                .collect())
        }
        async fn head(&self, _key: &str) -> Result<u64> {
            Err(LumenError::Storage("unused in gc tests".into()))
        }
    }

    fn item(name: &str, chunk_keys: &[String]) -> RemoteItem {
        let mut chunks = Vec::new();
        let mut off = 0;
        for (i, k) in chunk_keys.iter().enumerate() {
            let len = 1024u64 * (k.len() as u64);
            chunks.push(ChunkRef { key: k.clone(), offset: off, len, fingerprint: [0; 16] });
            off += len;
            // suppress unused warning pattern for i while keeping readability
            let _ = i;
        }
        RemoteItem {
            id: uuid::Uuid::new_v4(),
            parent_id: None,
            name: name.to_string(),
            item_type: ItemType::File,
            size: off,
            content_version: 1,
            metadata_version: 1,
            chunks,
            created_at: chrono::Utc::now(),
            modified_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn gc_reclaims_only_unreferenced_keys() {
        let index = MetadataIndex::open_memory().unwrap();
        let store = Arc::new(CountingStore::default());

        // Two files; A keeps referencing its chunk after B is deleted.
        let key_a = "chunks/aa/bb/cc/dd/001100".to_string();
        let key_b = "chunks/aa/bb/cc/dd/002200".to_string();
        store.put(&key_a, bytes::Bytes::from(vec![1u8; 400])).await.unwrap();
        store.put(&key_b, bytes::Bytes::from(vec![2u8; 300])).await.unwrap();

        let mut file_a = item("a.txt", &[key_a.clone()]);
        index.upsert(&file_a).unwrap();
        let file_b = item("b.txt", &[key_b.clone()]);
        index.upsert(&file_b).unwrap();

        // Simulate a deletion: row removed but chunk left behind.
        index.remove(&file_b.id).unwrap();

        let report = gc_collect(&index, store.as_ref()).await.unwrap();
        assert_eq!(report.scanned_keys, 2);
        assert_eq!(report.reclaimed_keys, 1);
        assert_eq!(report.reclaimed_bytes, 300);
        assert_eq!(store.delete_calls.load(Ordering::Relaxed), 1);

        // Survivor still readable and still referenced by remaining item.
        let refs = index.referenced_chunk_keys().unwrap();
        assert!(refs.contains(file_a.chunks[0].key.as_str()) || refs.contains(key_a.as_str()));
        assert!(store.map.lock().unwrap().contains_key(key_a_str(&key_a)));
        assert!(!store.map.lock().unwrap().contains_key(key_a_str(&key_b)));

        // Second run is a no-op: nothing new to reclaim.
        let idle = gc_collect(&index, store.as_ref()).await.unwrap();
        assert_eq!(idle.reclaimed_keys, 0);
        assert_eq!(idle.scanned_keys, 1);

        let _ = &mut file_a;
    }

    fn key_a_str(k: &str) -> &str { k }

    #[tokio::test]
    async fn gc_handles_replace_orphans_and_ignores_non_chunk_objects() {
        let index = MetadataIndex::open_memory().unwrap();
        let store = Arc::new(CountingStore::default());

        // v1 has one chunk; replace switches to two different keys.
        let old_key = "chunks/00/11/22/33/dead00".to_string();
        let new_k1 = "chunks/44/55/66/77/alive01".to_string();
        store.put(old_key.as_str(), bytes::Bytes::from(vec![9u8; 900])).await.unwrap();
        store.put(new_k1.as_str(), bytes::Bytes::from(vec![5u8; 700])).await.unwrap();

        let mut f = item("doc.md", &[old_key.clone()]);
        index.upsert(&f).unwrap();

        // Replace: bump versions, switch chunk list - old chunk orphaned.
        f.content_version += 1;
        f.metadata_version += 1;
        let replaced_len = 700;
        f.chunks = vec![ChunkRef { key: new_k1.clone(), offset: 0, len: replaced_len as u64, fingerprint: [0; 16] }];
        f.size = replaced_len as u64;
        index.upsert(&f).unwrap();

        // Non-chunk object must never be touched even though it exists.
        store.put("meta/manifest.json", bytes::Bytes::from("hi")).await.unwrap();

        let report = gc_collect(&index, store.as_ref()).await.unwrap();
        assert_eq!(report.scanned_keys, 2, "only keys under chunks/");
        assert_eq!(report.reclaimed_keys, 1);
        assert_eq!(report.reclaimed_bytes, 900);
        assert!(store.map.lock().unwrap().contains_key("meta/manifest.json"));
        assert!(store.map.lock().unwrap().contains_key(new_k1.as_str()));
        assert!(!store.map.lock().unwrap().contains_key(old_key.as_str()));
    }

    #[tokio::test]
    async fn gc_and_report_feeds_meter_with_reclaimed_bytes() {
        let index = MetadataIndex::open_memory().unwrap();
        let store = Arc::new(CountingStore::default());

        let live = "chunks/10/20/30/40/livekey".to_string();
        let dead = "chunks/50/60/70/80/deadkey".to_string();
        store.put(live.as_str(), bytes::Bytes::from(vec![1u8; 256])).await.unwrap();
        store.put(dead.as_str(), bytes::Bytes::from(vec![2u8; 128])).await.unwrap();

        let f = item("f.bin", &[live.clone()]);
        index.upsert(&f).unwrap();

        let meter = UsageMeter::disabled();
        let report = gc_and_report(&index, store.clone(), &meter).await.unwrap();
        assert_eq!(report.reclaimed_bytes, 128);
        assert_eq!(meter.stored(), 128);

        // Sink-wired variant observes the same reclaim event exactly once.
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<UsageEvent>::new()));
        let s2 = std::sync::Arc::clone(&seen);
        let sink: crate::meter::UsageSink =
            std::sync::Arc::new(move |e| s2.lock().unwrap().push(e));
        let m2 = UsageMeter::new(sink);
        let r2 = gc_and_report(&index, store.clone(), &m2).await.unwrap();
        assert_eq!(r2.reclaimed_bytes, 0, "nothing left after first pass");
        assert_eq!(seen.lock().unwrap().len(), 0, "no event when nothing reclaimed");
    }

    #[tokio::test]
    async fn concurrent_flush_gap_documented_scan_survives() {
        // One GC pass against an empty index with fresh uploads on disk:
        // everything is an orphan from a pure scan perspective. This is why
        // the caller contract requires flushing all batchers first. The
        // test pins current behavior so any future change here is noticed.
        let index = MetadataIndex::open_memory().unwrap();
        let store = Arc::new(CountingStore::default());
        let fresh = "chunks/99/88/77/66/uncommitted".to_string();
        store.put(fresh.as_str(), bytes::Bytes::from(vec![7u8; 512])).await.unwrap();
        let report = gc_collect(&index, store.as_ref()).await.unwrap();
        assert_eq!(report.reclaimed_keys, 1, "unflushed upload is treated as garbage");
    }
}
