//! Server-side ordered changelog backing the /v1/changes API.
//!
//! Every mutation (upsert or removal) receives a monotonic sequence
//! number exactly once. Clients resume from their persisted cursor and
//! pull forward-only deltas. The log is the single source of truth for
//! what changed and in which order.

use crate::error::Result;
use crate::metadata::RemoteItem;
use crate::sync::{ChangeFeed, ChangePage};
use std::sync::Mutex;
use uuid::Uuid;

#[derive(Debug, Clone)]
struct LogEntry {
    seq: i64,
    change: Change,
}

#[derive(Debug, Clone)]
enum Change {
    Upsert(RemoteItem),
    Removal(Uuid),
}

pub const DEFAULT_CAPACITY: usize = 100_000;

pub struct ChangeLog {
    inner: Mutex<Inner>,
}

struct Inner {
    next_seq: i64,
    entries: Vec<LogEntry>,
    capacity: usize,
}

impl Default for ChangeLog {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl ChangeLog {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                next_seq: 1,
                entries: Vec::new(),
                capacity,
            }),
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().expect("changelog poisoned").entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn earliest_seq(&self) -> i64 {
        self.inner
            .lock()
            .expect("changelog poisoned")
            .entries
            .first()
            .map(|e| e.seq)
            .unwrap_or(0)
    }

    pub fn latest_seq(&self) -> i64 {
        self.inner.lock().expect("changelog poisoned").next_seq - 1
    }

    pub fn record_upsert(&self, item: RemoteItem) -> Result<i64> {
        let mut inner = self.inner.lock().expect("changelog poisoned");
        Ok(inner.push(Change::Upsert(item)))
    }

    pub fn record_removal(&self, id: Uuid) -> Result<i64> {
        let mut inner = self.inner.lock().expect("changelog poisoned");
        Ok(inner.push(Change::Removal(id)))
    }

    /// Append a batch of upserts under a single lock acquisition. This is
    /// the commit path for CommitBatcher: N queued file upserts (chunk
    /// uploads already settled in storage by the time they land here)
    /// enter the log in arrival order with one lock cycle instead of N.
    /// Returns the seq of the last entry in the batch.
    pub fn append_batch(&self, items: Vec<RemoteItem>) -> Result<i64> {
        if items.is_empty() {
            // No-op flushes must not burn sequence numbers; returning
            // latest_seq keeps cursor math consistent for callers.
            return Ok(self.latest_seq());
        }
        let mut inner = self.inner.lock().expect("changelog poisoned");
        let mut last = 0;
        for item in items {
            last = inner.push(Change::Upsert(item));
        }
        Ok(last)
    }

    /// Next delta page strictly after 'cursor'. When there are no further
    /// entries the returned page echoes the caller cursor with more=false,
    /// so clients never move backwards waiting for new data.
    pub fn changes_since(&self, cursor: i64, limit: usize) -> Result<ChangePage> {
        let limit = limit.max(1);
        let inner = self.inner.lock().expect("changelog poisoned");
        let start = inner.entries.partition_point(|e| e.seq <= cursor);
        if start >= inner.entries.len() {
            return Ok(ChangePage {
                cursor,
                more: false,
                upserts: Vec::new(),
                removals: Vec::new(),
            });
        }
        let end = (start + limit).min(inner.entries.len());
        let slice = &inner.entries[start..end];
        let last_seq = slice.last().map(|e| e.seq).unwrap_or(cursor);
        let mut upserts = Vec::new();
        let mut removals = Vec::new();
        for e in slice {
            match &e.change {
                Change::Upsert(item) => upserts.push(item.clone()),
                Change::Removal(id) => removals.push(*id),
            }
        }
        Ok(ChangePage {
            cursor: last_seq,
            more: end < inner.entries.len(),
            upserts,
            removals,
        })
    }
}

impl Inner {
    fn push(&mut self, change: Change) -> i64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push(LogEntry { seq, change });
        while self.entries.len() > self.capacity && self.capacity > 0 {
            self.entries.remove(0);
        }
        seq
    }
}

/// Core-side reader over a shared server log: the same object the API
/// handlers mutate, handed to SyncEngine behind an Arc.
#[async_trait::async_trait]
impl ChangeFeed for std::sync::Arc<ChangeLog> {
    async fn changes_since(&self, cursor: i64, limit: usize) -> Result<ChangePage> {
        (**self).changes_since(cursor, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::LumenError;
    use crate::metadata::ItemType;

    fn item(id: u128, name: &str) -> RemoteItem {
        RemoteItem {
            id: Uuid::from_u128(id),
            parent_id: None,
            name: name.into(),
            item_type: ItemType::File,
            size: 10,
            content_version: 1,
            metadata_version: 1,
            chunks: vec![],
            created_at: chrono::Utc::now(),
            modified_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn assigns_monotonic_sequences_and_pages_forward() {
        let log = ChangeLog::default();
        assert_eq!(log.latest_seq(), 0);
        log.record_upsert(item(1, "a.md")).unwrap();
        let rm_id = Uuid::from_u128(2);
        log.record_removal(rm_id).unwrap();
        log.record_upsert(item(3, "c.md")).unwrap();
        assert_eq!(log.latest_seq(), 3);

        let p1 = log.changes_since(0, 1).unwrap();
        assert!(p1.more);
        assert_eq!(p1.cursor, 1);
        assert_eq!(p1.upserts.len(), 1);
        let p2 = log.changes_since(p1.cursor, 1).unwrap();
        assert!(p2.more);
        assert_eq!(p2.cursor, 2);
        assert!(p2.upserts.is_empty());
        assert_eq!(p2.removals, vec![rm_id]);
        let p3 = log.changes_since(p2.cursor, 1).unwrap();
        assert!(!p3.more);
        assert_eq!(p3.cursor, 3);

        let p4 = log.changes_since(3, 100).unwrap();
        assert_eq!((p4.cursor, p4.more), (3, false));
    }

    #[test]
    fn lower_bound_is_respected_so_entries_are_never_replayed() {
        let log = ChangeLog::default();
        log.record_upsert(item(1, "a.md")).unwrap();
        log.record_upsert(item(2, "b.md")).unwrap();
        let p = log.changes_since(1, 100).unwrap();
        assert_eq!(p.upserts.len(), 1);
        assert_eq!(p.upserts[0].name, "b.md");
    }

    #[test]
    fn append_batch_commits_in_arrival_order_and_skips_seq_on_empty() {
        let log = ChangeLog::default();
        assert_eq!(
            log.append_batch(Vec::new()).unwrap(),
            0,
            "empty batch is a no-op"
        );
        assert_eq!(log.latest_seq(), 0);
        let mut batch = Vec::new();
        for name in ["x.md", "y.md", "z.md"] {
            let mut i = item(5u128, name);
            i.name = name.into();
            batch.push(i);
        }
        let last = log.append_batch(batch).unwrap();
        assert_eq!(last, 3);
        assert_eq!(log.latest_seq(), 3);
        // Arrival order holds; one page reads the whole batch contiguously.
        let p = log.changes_since(0, 100).unwrap();
        let names: Vec<&str> = p.upserts.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["x.md", "y.md", "z.md"]);
        // Empty flush after real ones keeps sequence numbers unburned.
        assert_eq!(log.append_batch(Vec::new()).unwrap(), 3);
        assert_eq!(log.latest_seq(), 3);
    }

    #[test]
    fn removal_pages_surface_ids_and_cursors_move_forward() {
        let log = ChangeLog::default();
        let id = Uuid::from_u128(9);
        log.record_removal(id).unwrap();
        let p = log.changes_since(0, 100).unwrap();
        assert_eq!(p.removals, vec![id]);
        assert_eq!(p.cursor, 1);
        assert!(!p.more);
    }

    #[tokio::test]
    async fn feeds_sync_engine_end_to_end_through_shared_arc() {
        use crate::index::MetadataIndex;
        use crate::sync::SyncEngine;

        let log = std::sync::Arc::new(ChangeLog::default());
        let id = Uuid::from_u128(42);
        log.record_upsert(item(42, "note.md")).unwrap();
        log.record_removal(id).unwrap();

        let index = std::sync::Arc::new(MetadataIndex::open_memory().unwrap());
        let engine = SyncEngine::new(log.clone()).with_shared_index(index.clone());
        engine.pull_all(16).await.unwrap();

        assert!(matches!(index.get_item(&id), Err(LumenError::NotFound(_))));
    }
}
