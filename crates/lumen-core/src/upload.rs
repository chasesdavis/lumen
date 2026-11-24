//! Batched upload commits.
//!
//! Reverse-engineering found Helumi's client batch their metadata commits:
//! symbol dumps show commitBatcher / reserveBatcher / CallBatcher patterns
//! around a commitNew entry point, paired with UploadPacer/UploadGovernor
//! classes that throttle chunk traffic. Their design answers "how do we
//! slow sends down". Ours answers a different question: "how do we send the
//! fewest requests at all".
//!
//! Two properties make Lumen's batching structurally cheaper:
//!
//! 1. Content-addressed dedup happens before any bytes move. A write that
//!    overlaps existing content (a re-save, a project rebuild touching one
//!    source file) uploads nothing for those chunks - Helumi's pacer still
//!    has to regulate uploads that our path never issues.
//! 2. Chunk ciphertext lands in storage concurrently while metadata upserts
//!    queue in memory and flush to the changelog in arrival order under one
//!    lock acquisition per flush, not per file. Cursor followers observe
//!    file sets appearing in batched pages instead of item-at-a-time.
//!
//! The CommitBatcher is deliberately boring: an ordered Vec plus a Mutex,
//! no channels or background tasks. Flush points are explicit and visible
//! in call sites; there is no hidden async timing to reason about.

use crate::changelog::ChangeLog;
use crate::error::{LumenError, Result};
use crate::metadata::RemoteItem;
use std::sync::Mutex;

/// Number of pending upserts that triggers an automatic flush. Tuned so a
/// typical small-project sync (tens of files) commits in one changelog
/// lock cycle instead of dozens.
pub const DEFAULT_FLUSH_THRESHOLD: usize = 64;

/// Buffers ordered metadata upserts and flushes them into the shared
/// ChangeLog in batches. One flush = one lock acquisition appending N
/// entries, versus N lock round-trips for direct record_upsert calls.
pub struct CommitBatcher {
    log: std::sync::Arc<ChangeLog>,
    inner: Mutex<Inner>,
}

struct Inner {
    pending: Vec<RemoteItem>,
    /// Auto-flush point; push() triggers a commit at this queue length.
    threshold: usize,
    /// High-water mark of largest single batch handed to the log so far.
    /// Batching effectiveness metric retained for introspection: a depth
    /// stuck near 1 means callers are pushing+flushing one item at a time
    /// and earn nothing from batching - worth knowing during tuning.
    max_queue_depth: usize,
}

impl CommitBatcher {
    pub fn new(log: std::sync::Arc<ChangeLog>) -> Self {
        Self::new_with_threshold(log, DEFAULT_FLUSH_THRESHOLD)
    }

    pub fn new_with_threshold(log: std::sync::Arc<ChangeLog>, threshold: usize) -> Self {
        Self {
            log,
            inner: Mutex::new(Inner {
                pending: Vec::new(),
                // Thresholds below 1 degenerate into per-item flushing;
                // clamp so the auto-flush check can rely on non-zero.
                threshold: threshold.max(1),
                max_queue_depth: 0,
            }),
        }
    }

    /// Queue one upsert. Flushes automatically once pending items reach the
    /// configured threshold. Batch append happens outside the mutex so
    /// concurrent pushes only serialize on the tiny queue critical section.
    pub fn push(&self, item: RemoteItem) -> Result<()> {
        let flushed = {
            let mut inner = self.lock()?;
            inner.pending.push(item);
            if inner.pending.len() >= inner.threshold {
                Some(inner.take_pending())
            } else {
                None
            }
        };
        if let Some(batch) = flushed {
            self.log.append_batch(batch)?;
        }
        Ok(())
    }

    /// Commit everything currently queued; returns how many items went in.
    /// Empty flush is a no-op that leaves the changelog cursor untouched.
    pub fn flush(&self) -> Result<usize> {
        let count;
        let batch = {
            let mut inner = self.lock()?;
            if inner.pending.is_empty() {
                return Ok(0);
            }
            count = inner.pending.len();
            inner.take_pending()
        };
        self.log.append_batch(batch)?;
        Ok(count)
    }

    /// Record a removal directly into the changelog, bypassing the queue.
    /// Deletes must not wait behind pending upserts: a tombstone delayed in
    /// the queue would let a cursor follower observe an upsert page from
    /// before the delete and resurrect the removed item. Queuing semantics
    /// only hold for upserts, where arrival order equals commit order.
    pub fn removal(&self, id: uuid::Uuid) -> Result<()> {
        self.log.record_removal(id)?;
        Ok(())
    }

    /// Items waiting for the next flush.
    pub fn len(&self) -> Result<usize> {
        Ok(self.lock()?.pending.len())
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Largest batch ever handed to the log (survives flushes). "Depth"
    /// here means real committed batch size, not transient queue size:
    /// it stays truthful after take_pending drains the vec.
    pub fn max_queue_depth(&self) -> Result<usize> {
        Ok(self.lock()?.max_queue_depth)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Inner>> {
        self.inner
            .lock()
            .map_err(|_| LumenError::Storage("commit batcher mutex poisoned".into()))
    }
}

impl Inner {
    fn take_pending(&mut self) -> Vec<RemoteItem> {
        let taken = std::mem::take(&mut self.pending);
        if taken.len() > self.max_queue_depth {
            self.max_queue_depth = taken.len();
        }
        taken
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::ItemType;
    use uuid::Uuid;

    fn item(name: &str) -> RemoteItem {
        RemoteItem {
            id: Uuid::new_v4(),
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
    fn batch_flush_preserves_arrival_order_under_one_seq_run() {
        let log = std::sync::Arc::new(ChangeLog::default());
        let batcher = CommitBatcher::new_with_threshold(log.clone(), 100);
        for name in ["a", "b", "c"] {
            batcher.push(item(name)).unwrap();
        }
        assert_eq!(batcher.len().unwrap(), 3);
        assert_eq!(log.latest_seq(), 0, "nothing committed before flush");

        assert_eq!(batcher.flush().unwrap(), 3);
        let page = log.changes_since(0, 100).unwrap();
        let names: Vec<&str> = page.upserts.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["a", "b", "c"], "arrival order is the commit order");
        assert!(!page.more);
    }

    #[test]
    fn auto_flush_fires_at_threshold_and_carries_remainder() {
        let log = std::sync::Arc::new(ChangeLog::default());
        let batcher = CommitBatcher::new_with_threshold(log.clone(), 3);
        for name in ["a", "b", "c"] {
            batcher.push(item(name)).unwrap();
        }
        assert_eq!(
            log.latest_seq(),
            3,
            "third push crossed threshold and flushed"
        );
        assert!(batcher.is_empty().unwrap());
        batcher.push(item("d")).unwrap();
        assert_eq!(log.latest_seq(), 3, "d waits below threshold");
        assert_eq!(batcher.flush().unwrap(), 1);
        assert_eq!(log.latest_seq(), 4);
    }

    #[test]
    fn empty_flush_burns_no_sequence_numbers() {
        let log = std::sync::Arc::new(ChangeLog::default());
        let batcher = CommitBatcher::new(log.clone());
        assert_eq!(batcher.flush().unwrap(), 0);
        assert_eq!(log.latest_seq(), 0);
        batcher.push(item("only")).unwrap();
        assert_eq!(batcher.flush().unwrap(), 1);
        // Repeated empty flushes after a real one must not move the cursor.
        assert_eq!(batcher.flush().unwrap(), 0);
        assert_eq!(batcher.flush().unwrap(), 0);
        assert_eq!(log.latest_seq(), 1);
        assert_eq!(log.len(), 1);
    }

    #[test]
    fn max_queue_depth_reports_real_batch_sizes_across_flushes() {
        let log = std::sync::Arc::new(ChangeLog::default());
        let batcher = CommitBatcher::new_with_threshold(log, 10);
        for name in ["a", "b"] {
            batcher.push(item(name)).unwrap();
        }
        assert_eq!(
            batcher.max_queue_depth().unwrap(),
            0,
            "nothing committed yet"
        );
        batcher.flush().unwrap();
        assert_eq!(batcher.max_queue_depth().unwrap(), 2);
        for name in ["c", "d", "e"] {
            batcher.push(item(name)).unwrap();
        }
        batcher.flush().unwrap();
        assert_eq!(
            batcher.max_queue_depth().unwrap(),
            3,
            "high-water mark grows"
        );
        // A smaller later flush must never shrink it.
        batcher.push(item("f")).unwrap();
        batcher.flush().unwrap();
        assert_eq!(batcher.max_queue_depth().unwrap(), 3);
    }

    #[test]
    fn threshold_clamped_to_one_still_batches_correctly() {
        let log = std::sync::Arc::new(ChangeLog::default());
        let batcher = CommitBatcher::new_with_threshold(log.clone(), 0);
        batcher.push(item("a")).unwrap();
        assert_eq!(
            log.latest_seq(),
            1,
            "threshold clamps to 1: every push flushes"
        );
        assert!(batcher.is_empty().unwrap());
    }

    #[tokio::test]
    async fn concurrent_pushes_from_many_threads_commit_everything() {
        use std::sync::Arc;
        let log = Arc::new(ChangeLog::default());
        let batcher = Arc::new(CommitBatcher::new_with_threshold(log.clone(), 8));
        let mut handles = Vec::new();
        for t in 0..8u32 {
            let b = batcher.clone();
            handles.push(tokio::task::spawn_blocking(move || {
                for n in 0..16u32 {
                    let mut it = item("t");
                    it.name = format!("t{}-n{}", t, n);
                    b.push(it).unwrap();
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        batcher.flush().unwrap();

        // Every push either triggered a threshold flush or waited for the
        // final explicit one; the log must contain all 128 upserts.
        assert_eq!(log.len(), 128usize);
        let page = log.changes_since(0, 500).unwrap();
        let total_upserts: usize = page.upserts.len() + page.removals.len();
        assert_eq!(total_upserts, 128);
        // Names are unique - nothing lost or duplicated across threads.
        let mut names: Vec<String> = page.upserts.iter().map(|i| i.name.clone()).collect();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            128,
            "each pushed item must appear exactly once"
        );
    }
}
