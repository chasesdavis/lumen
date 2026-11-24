//! Local SQLite metadata index. Powers instant filename search, tree
//! browsing, and offline metadata operation for the FS adapter and MCP
//! server - none of which should ever block on network.

use crate::error::{LumenError, Result};
use crate::metadata::{ItemType, RemoteItem};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

pub struct MetadataIndex {
    conn: Mutex<Connection>,
}

impl MetadataIndex {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path).map_err(|e| LumenError::Storage(e.to_string()))?;
        Self::init(conn)
    }

    /// In-memory index; used by tests and the MCP server when running stateless.
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().map_err(|e| LumenError::Storage(e.to_string()))?;
        Self::init(conn)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS items (
                id TEXT PRIMARY KEY,
                parent_id TEXT,
                name TEXT NOT NULL,
                item_type TEXT NOT NULL,
                size INTEGER NOT NULL DEFAULT 0,
                content_version INTEGER NOT NULL DEFAULT 0,
                metadata_version INTEGER NOT NULL DEFAULT 0,
                chunks_json TEXT NOT NULL DEFAULT '[]',
                created_at INTEGER NOT NULL,
                modified_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_items_parent ON items(parent_id);
            CREATE INDEX IF NOT EXISTS idx_items_name ON items(name);
            CREATE TABLE IF NOT EXISTS content_index (
                item_id TEXT NOT NULL,
                body TEXT NOT NULL,
                PRIMARY KEY (item_id)
            );
            CREATE INDEX IF NOT EXISTS idx_content_body ON content_index(body);",
        )
        .map_err(|e| LumenError::Storage(e.to_string()))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn upsert(&self, item: &RemoteItem) -> Result<()> {
        let chunks =
            serde_json::to_string(&item.chunks).map_err(|e| LumenError::Storage(e.to_string()))?;
        let conn = self.conn.lock().expect("index poisoned");
        conn.execute(
            "INSERT INTO items (id, parent_id, name, item_type, size, content_version, metadata_version, chunks_json, created_at, modified_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) ON CONFLICT(id) DO UPDATE SET parent_id=?2, name=?3, size=?5, content_version=?6, metadata_version=?7, chunks_json=?8, modified_at=?10",
            params![
                item.id.to_string(),
                item.parent_id.map(|p| p.to_string()),
                item.name,
                if matches!(item.item_type, ItemType::Folder) { "folder" } else { "file" },
                item.size as i64, item.content_version, item.metadata_version,
                chunks,
                item.created_at.timestamp_millis(), item.modified_at.timestamp_millis(),
            ],
        ).map_err(|e| LumenError::Storage(e.to_string()))?;
        Ok(())
    }

    pub fn remove(&self, id: &uuid::Uuid) -> Result<()> {
        let conn = self.conn.lock().expect("index poisoned");
        conn.execute("DELETE FROM items WHERE id = ?1", params![id.to_string()])
            .map_err(|e| LumenError::Storage(e.to_string()))?;
        conn.execute(
            "DELETE FROM content_index WHERE item_id = ?1",
            params![id.to_string()],
        )
        .map_err(|e| LumenError::Storage(e.to_string()))?;
        Ok(())
    }

    /// Index the searchable text of a file for content search. The MCP
    /// layer supplies lossy UTF-8 of chosen pages under a per-file cap;
    /// we store none of it when empty so folders cost nothing.
    /// Plaintext-resident local index: encryption boundary is at rest on
    /// the object store, the local SQLite file uses OS-level protection.
    pub fn index_content(&self, id: &uuid::Uuid, body: &str) -> Result<()> {
        let mut conn = self.conn.lock().expect("index poisoned");
        let tx = conn
            .transaction()
            .map_err(|e| LumenError::Storage(e.to_string()))?;
        if body.is_empty() {
            tx.execute(
                "DELETE FROM content_index WHERE item_id = ?1",
                params![id.to_string()],
            )
            .map_err(|e| LumenError::Storage(e.to_string()))?;
        } else {
            tx.execute(
                "INSERT INTO content_index (item_id, body) VALUES (?1, ?2) ON CONFLICT(item_id) DO UPDATE SET body=?2",
                params![id.to_string(), body],
            ).map_err(|e| LumenError::Storage(e.to_string()))?;
        }
        tx.commit().map_err(|e| LumenError::Storage(e.to_string()))
    }

    pub fn search_names(&self, pattern: &str, limit: usize) -> Result<Vec<RemoteItem>> {
        let conn = self.conn.lock().expect("index poisoned");
        let pat = format!("%{}%", pattern.replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, name, item_type, size, content_version, metadata_version, chunks_json, created_at, modified_at FROM items WHERE name LIKE ?1 ESCAPE '\\' ORDER BY modified_at DESC LIMIT ?2",
        ).map_err(|e| LumenError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map(params![pat, limit as i64], row_to_item)
            .map_err(|e| LumenError::Storage(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| LumenError::Storage(e.to_string()))
    }

    pub fn children(&self, parent: Option<&uuid::Uuid>) -> Result<Vec<RemoteItem>> {
        let conn = self.conn.lock().expect("index poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, name, item_type, size, content_version, metadata_version, chunks_json, created_at, modified_at FROM items WHERE parent_id IS ?1 ORDER BY name ASC",
        ).map_err(|e| LumenError::Storage(e.to_string()))?;
        let pid = parent.map(|p| p.to_string());
        let rows = stmt
            .query_map(params![pid], row_to_item)
            .map_err(|e| LumenError::Storage(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| LumenError::Storage(e.to_string()))
    }

    /// Fetch a single item by id. Powers MCP file_metadata lookups.
    pub fn get_item(&self, id: &uuid::Uuid) -> Result<RemoteItem> {
        let conn = self.conn.lock().expect("index poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, parent_id, name, item_type, size, content_version, metadata_version, chunks_json, created_at, modified_at FROM items WHERE id = ?1",
        ).map_err(|e| LumenError::Storage(e.to_string()))?;
        let mut rows = stmt
            .query_map(params![id.to_string()], row_to_item)
            .map_err(|e| LumenError::Storage(e.to_string()))?;
        match rows.next() {
            Some(item) => item.map_err(|e| LumenError::Storage(e.to_string())),
            None => Err(LumenError::NotFound(format!("item {id} not found"))),
        }
    }

    /// Content search over locally indexed plaintext bodies. Returns the
    /// owning items ordered by modified_at. Powers MCP search_files with
    /// kind=content without any remote round-trip.
    pub fn search_content(&self, pattern: &str, limit: usize) -> Result<Vec<RemoteItem>> {
        let conn = self.conn.lock().expect("index poisoned");
        let pat = format!("%{}%", pattern);
        let mut stmt = conn.prepare(
            "SELECT i.id, i.parent_id, i.name, i.item_type, i.size, i.content_version, i.metadata_version, i.chunks_json, i.created_at, i.modified_at \
             FROM items i JOIN content_index c ON c.item_id = i.id \
             WHERE c.body LIKE ?1 ESCAPE '\\' ORDER BY i.modified_at DESC LIMIT ?2",
        ).map_err(|e| LumenError::Storage(e.to_string()))?;
        let rows = stmt
            .query_map(params![pat, limit as i64], row_to_item)
            .map_err(|e| LumenError::Storage(e.to_string()))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| LumenError::Storage(e.to_string()))
    }

    /// Every storage key referenced by any live item's chunk list. The GC
    /// reference scan: keys present in the store but absent from this set
    /// are orphans left behind by deletions and content rewrites. Read
    /// directly from chunks_json so a huge store stays O(store) here, not
    /// O(rows * items).
    pub fn referenced_chunk_keys(&self) -> Result<std::collections::HashSet<String>> {
        let conn = self.conn.lock().expect("index poisoned");
        let mut stmt = conn
            .prepare("SELECT chunks_json FROM items")
            .map_err(|e| LumenError::Storage(e.to_string()))?;
        let raw: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .map_err(|e| LumenError::Storage(e.to_string()))?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| LumenError::Storage(e.to_string()))?;
        drop(stmt);
        // Parse outside the mutex: chunk lists can be large and holding the
        // index lock across JSON decode would stall lookups on big drives.
        Ok(raw
            .into_iter()
            .flat_map(|json| {
                serde_json::from_str::<Vec<crate::metadata::ChunkRef>>(&json)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|c| c.key)
                    .collect::<Vec<_>>()
            })
            .collect())
    }
}

fn row_to_item(row: &rusqlite::Row) -> rusqlite::Result<RemoteItem> {
    let id: String = row.get(0)?;
    let parent: Option<String> = row.get(1)?;
    let name: String = row.get(2)?;
    let ty: String = row.get(3)?;
    Ok(RemoteItem {
        id: uuid::Uuid::parse_str(&id).unwrap_or_default(),
        parent_id: parent.and_then(|p| uuid::Uuid::parse_str(&p).ok()),
        name,
        item_type: if ty == "folder" {
            ItemType::Folder
        } else {
            ItemType::File
        },
        size: row.get::<_, i64>(4)? as u64,
        content_version: row.get(5)?,
        metadata_version: row.get(6)?,
        chunks: serde_json::from_str(&row.get::<_, String>(7)?).unwrap_or_default(),
        created_at: chrono::DateTime::from_timestamp_millis(row.get::<_, i64>(8)?)
            .unwrap_or_default(),
        modified_at: chrono::DateTime::from_timestamp_millis(row.get::<_, i64>(9)?)
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_search_remove_roundtrip() {
        let ix = MetadataIndex::open_memory().unwrap();
        let mut it = RemoteItem {
            id: uuid::Uuid::new_v4(),
            parent_id: None,
            name: "lumen-notes.md".into(),
            item_type: ItemType::File,
            size: 42,
            content_version: 1,
            metadata_version: 1,
            chunks: vec![],
            created_at: chrono::Utc::now(),
            modified_at: chrono::Utc::now(),
        };
        ix.upsert(&it).unwrap();
        let hits = ix.search_names("notes", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "lumen-notes.md");
        // update path
        it.name = "renamed.md".into();
        it.content_version += 1;
        ix.upsert(&it).unwrap();
        assert_eq!(ix.search_names("renamed", 10).unwrap().len(), 1);
        assert_eq!(ix.search_names("notes", 10).unwrap().len(), 0);
        ix.remove(&it.id).unwrap();
        assert!(ix.search_names("renamed", 10).unwrap().is_empty());
    }

    #[test]
    fn content_index_and_search_roundtrip() {
        let ix = MetadataIndex::open_memory().unwrap();
        let now = chrono::Utc::now();
        let a = RemoteItem {
            id: uuid::Uuid::new_v4(),
            parent_id: None,
            name: "a.txt".into(),
            item_type: ItemType::File,
            size: 10,
            content_version: 1,
            metadata_version: 1,
            chunks: vec![],
            created_at: now,
            modified_at: now,
        };
        let b = RemoteItem {
            id: uuid::Uuid::new_v4(),
            parent_id: None,
            name: "b.txt".into(),
            item_type: ItemType::File,
            size: 10,
            content_version: 1,
            metadata_version: 1,
            chunks: vec![],
            created_at: now,
            modified_at: now,
        };
        ix.upsert(&a).unwrap();
        ix.upsert(&b).unwrap();

        // No hits before any content is indexed.
        assert!(ix.search_content("needle", 10).unwrap().is_empty());

        ix.index_content(&a.id, "hello needle one").unwrap();
        ix.index_content(&b.id, "hello world two").unwrap();
        let hits = ix.search_content("needle", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, a.id);

        // LIKE wildcards in the pattern are literal, not SQL metacharacters:
        // a body containing a raw percent must not break the query.
        ix.index_content(&b.id, "cost is 50 pct of total").unwrap();
        let literal = ix.search_content("% of", 10).unwrap();
        assert!(literal.iter().all(|it| it.id == b.id));

        // Removal cascades the content row too.
        ix.remove(&a.id).unwrap();
        assert!(ix.search_content("needle", 10).unwrap().is_empty());

        // Empty body clears the index entry for the file.
        ix.index_content(&b.id, "").unwrap();
        assert!(ix.search_content("sure", 10).unwrap().is_empty());
    }

    /// Original test plan gate: SQLite index integrity under concurrent
    /// writes. One shared MetadataIndex behind Arc, many writer threads
    /// interleaving upsert/index while a reader thread searches mid-churn.
    #[test]
    fn concurrent_writers_keep_index_intact() {
        let ix = std::sync::Arc::new(MetadataIndex::open_memory().unwrap());
        const WRITERS: usize = 8;
        const ITEMS_PER_WRITER: usize = 200;
        let mut handles = Vec::new();
        for w in 0..WRITERS {
            let ix = ix.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..ITEMS_PER_WRITER {
                    let now = chrono::Utc::now();
                    let id = uuid::Uuid::new_v4();
                    ix.upsert(&RemoteItem {
                        id,
                        parent_id: None,
                        name: format!("w{} item {}", w, i),
                        item_type: ItemType::File,
                        size: i as u64,
                        content_version: 1,
                        metadata_version: 1,
                        chunks: vec![],
                        created_at: now,
                        modified_at: now,
                    })
                    .unwrap();
                    if i % 2 == 0 {
                        let body = format!("writer {} body {}", w, i);
                        ix.index_content(&id, &body).unwrap();
                    }
                }
            }));
        }
        let reader_ix = ix.clone();
        let reader = std::thread::spawn(move || {
            for _ in 0..300 {
                assert!(
                    reader_ix.search_names("item", 1000).unwrap().len()
                        <= WRITERS * ITEMS_PER_WRITER
                );
                assert!(
                    reader_ix.search_content("body", 1000).unwrap().len()
                        <= (WRITERS * ITEMS_PER_WRITER) / 2
                );
            }
        });
        for h in handles {
            h.join().expect("writer panicked");
        }
        reader.join().expect("reader panicked");
        // After the storm: exact counts prove nothing was lost or doubled.
        assert_eq!(
            ix.search_names("item", 2000).unwrap().len(),
            WRITERS * ITEMS_PER_WRITER
        );
        assert_eq!(
            ix.search_content("body", 2000).unwrap().len(),
            (WRITERS * ITEMS_PER_WRITER) / 2
        );
    }
}
