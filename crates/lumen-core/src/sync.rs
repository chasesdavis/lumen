//! Cursor-based delta sync, modeled after Helumi's changes(since:) API
//! surface but with local SQLite persistence so cold starts resume from
//! the exact cursor instead of doing full tree fetches.

use crate::error::Result;
use crate::metadata::DriveTree;
use crate::metadata::RemoteItem;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangePage {
    /// Server-side monotonic sequence number this page ends at.
    pub cursor: i64,
    pub more: bool,
    pub upserts: Vec<RemoteItem>,
    /// IDs removed since last seen cursor.
    pub removals: Vec<uuid::Uuid>,
}

/// Source of change pages. Implemented by lumen-api-server's client and
/// in tests by fixtures. The engine only knows this trait - keeping core
/// fully decoupled from HTTP.
#[async_trait::async_trait]
pub trait ChangeFeed: Send + Sync {
    async fn changes_since(&self, cursor: i64, limit: usize) -> Result<ChangePage>;
}

pub struct SyncEngine<F: ChangeFeed> {
    feed: F,
    tree: Mutex<DriveTree>,
    cursor: Mutex<i64>,
    index: Option<std::sync::Arc<crate::index::MetadataIndex>>,
}

impl<F: ChangeFeed> SyncEngine<F> {
    pub fn new(feed: F) -> Self {
        Self { feed, tree: Mutex::new(DriveTree::new()), cursor: Mutex::new(0), index: None }
    }

    /// Attach a MetadataIndex so every applied delta updates both the
    /// in-memory DriveTree and the SQLite index (the store powering MCP
    /// tools and offline search) in one pass. Call before first pull.
    pub fn with_index(mut self, index: crate::index::MetadataIndex) -> Self {
        self.index = Some(std::sync::Arc::new(index));
        self
    }

    /// Shared-handle variant: keep your own Arc to the same index the sync
    /// engine writes into - callers pass it on to the MCP server or FS
    /// adapter afterwards.
    pub fn with_shared_index(mut self, index: std::sync::Arc<crate::index::MetadataIndex>) -> Self {
        self.index = Some(index);
        self
    }

    /// Pull all pending pages until exhausted. Returns number of applied changes.
    pub async fn pull_all(&self, limit: usize) -> Result<usize> {
        let mut applied = 0;
        loop {
            let cur = *self.cursor.lock().await;
            let page = self.feed.changes_since(cur, limit).await?;
            let mut tree = self.tree.lock().await;
            for item in &page.upserts {
                tree.upsert(item.clone());
                if let Some(ix) = &self.index {
                    ix.upsert(item)?;
                }
                applied += 1;
            }
            for id in &page.removals {
                tree.remove(id);
                if let Some(ix) = &self.index {
                    ix.remove(id)?;
                }
                applied += 1;
            }
            *self.cursor.lock().await = page.cursor;
            if !page.more { break; }
        }
        Ok(applied)
    }

    pub async fn cursor(&self) -> i64 { *self.cursor.lock().await }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{ItemType, RemoteItem};

    struct FakeFeed { data: Vec<(i64, RemoteItem)> }

    #[async_trait::async_trait]
    impl ChangeFeed for FakeFeed {
        async fn changes_since(&self, cursor: i64, _limit: usize) -> Result<ChangePage> {
            let ups: Vec<RemoteItem> = self.data.iter()
                .filter(|(seq, _)| *seq > cursor)
                .map(|(_, it)| it.clone()).collect();
            Ok(ChangePage { cursor: self.data.last().map(|(s, _)| *s).unwrap_or(cursor), more: false, upserts: ups, removals: vec![] })
        }
    }

    #[tokio::test]
    async fn applies_changes_from_zero() {
        let id = uuid::Uuid::new_v4();
        let item = RemoteItem {
            id, parent_id: None, name: "a.txt".into(), item_type: ItemType::File,
            size: 3, content_version: 1, metadata_version: 1, chunks: vec![],
            created_at: chrono::Utc::now(), modified_at: chrono::Utc::now(),
        };
        let f = FakeFeed { data: vec![(5, item)] };
        let se = SyncEngine::new(f);
        assert_eq!(se.pull_all(100).await.unwrap(), 1);
        assert_eq!(se.cursor().await, 5);
        let again = se.pull_all(100).await.unwrap();
        assert_eq!(again, 0);
    }

    #[tokio::test]
    async fn attached_index_receives_upserts_and_removals() {
        use crate::index::MetadataIndex;

        struct TwoPhaseFeed;
        #[async_trait::async_trait]
        impl ChangeFeed for TwoPhaseFeed {
            async fn changes_since(&self, cursor: i64, _limit: usize) -> Result<ChangePage> {
                // seq 1: create file; seq 2: remove it. Page by cursor.
                if cursor == 0 {
                    let item = RemoteItem {
                        id: uuid::Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap(),
                        parent_id: None,
                        name: "g.md".into(),
                        item_type: ItemType::File,
                        size: 2,
                        content_version: 1,
                        metadata_version: 1,
                        chunks: vec![],
                        created_at: chrono::Utc::now(),
                        modified_at: chrono::Utc::now(),
                    };
                    Ok(ChangePage { cursor: 1, more: true, upserts: vec![item], removals: vec![] })
                } else {
                    Ok(ChangePage {
                        cursor: 2,
                        more: false,
                        upserts: vec![],
                        removals: vec![uuid::Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap()],
                    })
                }
            }
        }

        let index = MetadataIndex::open_memory().unwrap();
        let index = std::sync::Arc::new(index);
        let se = SyncEngine::new(TwoPhaseFeed).with_shared_index(index.clone());
        assert_eq!(se.pull_all(100).await.unwrap(), 2);
        assert_eq!(se.cursor().await, 2);

        // After the full two-page cycle the item was upserted then removed:
        // the index reflects the removal (NotFound), proving deltas flow
        // through to the SQLite side, not just the DriveTree.
        let missing = index.get_item(&uuid::Uuid::parse_str("00000000-0000-4000-8000-000000000001").unwrap());
        assert!(matches!(missing, Err(crate::error::LumenError::NotFound(_))));
    }
}
