//! Drive metadata model: tree of RemoteItems mirroring Helumi's
//! RemoteItem/DriveTree/ChangePage concepts but with explicit cursor sync.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItemType {
    File,
    Folder,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkRef {
    pub key: String,      // storage key (content-addressed)
    pub offset: u64,
    pub len: u64,
    pub fingerprint: [u8; 16],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteItem {
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub name: String,     // encrypted WireName envelope when private files on
    pub item_type: ItemType,
    pub size: u64,        // plaintext size for files
    pub content_version: i64,  // bumped per content change (optimistic concurrency)
    pub metadata_version: i64, // bumped per rename/move
    pub chunks: Vec<ChunkRef>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub modified_at: chrono::DateTime<chrono::Utc>,
}

impl RemoteItem {
    pub fn chunk_covering(&self, off: u64) -> Option<&ChunkRef> {
        self.chunks.iter().find(|c| off >= c.offset && off < c.offset + c.len)
    }
}

/// In-memory tree view over the synced items. Kept fresh by SyncEngine.
#[derive(Debug, Default)]
pub struct DriveTree {
    items: std::collections::HashMap<Uuid, RemoteItem>,
}

impl DriveTree {
    pub fn new() -> Self { Self::default() }

    pub fn upsert(&mut self, item: RemoteItem) { self.items.insert(item.id, item); }

    pub fn remove(&mut self, id: &Uuid) { self.items.remove(id); }

    pub fn get(&self, id: &Uuid) -> Option<&RemoteItem> { self.items.get(id) }

    pub fn children_of(&self, parent: Option<Uuid>) -> Vec<&RemoteItem> {
        let mut v: Vec<&RemoteItem> = self.items.values()
            .filter(|i| i.parent_id == parent && !matches!(item_state(i), State::Tombstone))
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Resolve a path like /projects/lumen/README.md to an item.
    pub fn resolve(&self, path: &str) -> Option<&RemoteItem> {
        let mut cur = None;
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            cur = self.items.values()
                .find(|i| i.parent_id == cur.map(|c: &RemoteItem| c.id) && i.name == seg);
            cur?;
        }
        cur
    }
}

#[derive(PartialEq)]
#[allow(dead_code)]
enum State { Active, Tombstone }
fn item_state(_i: &RemoteItem) -> State { State::Active }
