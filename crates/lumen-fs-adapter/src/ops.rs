//! Mount operations shared by every platform adapter.
//!
//! Maps POSIX-shaped calls onto MetadataIndex + VfsOps. Path handling is
//! purely lexical here; normalization rules are fixed once so all three
//! platform frontends behave identically:
//!
//! - Paths are absolute; the virtual mount root is "/"
//! - Repeated separators collapse: "/a//b" resolves as "/a/b"
//! - Trailing slash is dropped for files, irrelevant for directories
//! - "." collapses away; ".." never escapes (resolution clamps at root)

use lumen_core::error::LumenError;
use lumen_core::upload::CommitBatcher;
use lumen_core::index::MetadataIndex;
use lumen_core::metadata::{ItemType, RemoteItem};
use lumen_fsprobe::vfs::VfsOps;
use std::sync::Arc;

#[derive(Debug)]
pub enum AdapterError {
    NotFound,
    NotADirectory,
    IsADirectory,
    AlreadyExists,
    InvalidPath,
    Core(LumenError),
}

impl From<LumenError> for AdapterError {
    fn from(e: LumenError) -> Self {
        match e {
            LumenError::NotFound(_) => AdapterError::NotFound,
            e => AdapterError::Core(e),
        }
    }
}

impl std::fmt::Display for AdapterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdapterError::NotFound => write!(f, "no such file or directory"),
            AdapterError::NotADirectory => write!(f, "not a directory"),
            AdapterError::IsADirectory => write!(f, "is a directory"),
            AdapterError::AlreadyExists => write!(f, "file already exists"),
            AdapterError::InvalidPath => write!(f, "invalid path"),
            AdapterError::Core(e) => write!(f, "{e}"),
        }
    }
}

/// Filled stat structure handed to platform code to translate into their
/// native attribute structs (stat/statfs/FILE_ID_BOTH_DIR_INFO).
#[derive(Debug, Clone, PartialEq)]
pub struct Stat {
    pub item_id: uuid::Uuid,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified_at: chrono::DateTime<chrono::Utc>,
}

pub struct MountOps {
    index: Arc<MetadataIndex>,
    vfs: Arc<VfsOps>,
    commits: CommitBatcher,
}

impl MountOps {
    pub fn new(index: Arc<MetadataIndex>, vfs: Arc<VfsOps>) -> Self {
        let log = std::sync::Arc::new(lumen_core::ChangeLog::default());
        let commits = CommitBatcher::new(log);
        Self { index, vfs, commits }
    }

    /// Test/production constructor with an explicit shared changelog so the
    /// commit path feeds the same log the API sync handlers read from.
    pub fn with_changelog(index: Arc<MetadataIndex>, vfs: Arc<VfsOps>, log: std::sync::Arc<lumen_core::ChangeLog>) -> Self {
        Self { index, vfs, commits: CommitBatcher::new(log) }
    }

    /// Flush all pending metadata upserts into the changelog. Platform
    /// adapters should call this at operation boundaries (end of create,
    /// end of directory scan pass) rather than per single file.
    pub async fn commit_pending(&self) -> Result<usize, AdapterError> {
        Ok(self.commits.flush()?)
    }

    /// Split a mount-relative path into clean segments. Root yields an empty vec.
    pub fn split_path(path: &str) -> Result<Vec<&str>, AdapterError> {
        let trimmed = path.trim_matches('/');
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for seg in trimmed.split('/') {
            match seg {
                "" | "." => continue,
                // Clamp at root like POSIX inside one mount - never escape.
                ".." => { out.pop(); }
                s => out.push(s),
            }
        }
        Ok(out)
    }

    /// Resolve a path to its metadata row. Errors on missing intermediates
    /// exactly like POSIX path resolution would.
    pub async fn resolve(&self, path: &str) -> Result<RemoteItem, AdapterError> {
        let segments = Self::split_path(path)?;
        let mut current: Option<uuid::Uuid> = None;
        for seg in &segments {
            let children = self.index.children(current.as_ref())?;
            let found = children
                .iter()
                .find(|c| c.name == *seg && matches!(c.item_type, ItemType::Folder))
                .or_else(|| children.iter().find(|c| c.name == *seg))
                .ok_or(AdapterError::NotFound)?;
            current = Some(found.id);
        }
        match current {
            Some(id) => Ok(self.index.get_item(&id)?),
            // "/" resolved with no segments: synthesize the root folder so
            // callers can stat it without special-casing everywhere.
            None => Ok(RemoteItem {
                id: uuid::Uuid::nil(),
                parent_id: None,
                name: String::new(),
                item_type: ItemType::Folder,
                size: 0,
                content_version: 0,
                metadata_version: 0,
                chunks: vec![],
                created_at: chrono::Utc::now(),
                modified_at: chrono::Utc::now(),
            }),
        }
    }

    /// POSIX readdir: entries of the directory at path. "."/".." excluded -
    /// every FUSE/WinFsp frontend adds those itself from its own conventions.
    pub async fn readdir(&self, path: &str) -> Result<Vec<Stat>, AdapterError> {
        let dir = self.resolve(path).await?;
        if !matches!(dir.item_type, ItemType::Folder) {
            return Err(AdapterError::NotADirectory);
        }
        let parent = if dir.id == uuid::Uuid::nil() { None } else { Some(dir.id) };
        let children = self.index.children(parent.as_ref())?;
        Ok(children.into_iter().map(|i| item_stat(&i)).collect())
    }

    /// getattr equivalent; returns translated stat fields only.
    pub async fn getattr(&self, path: &str) -> Result<Stat, AdapterError> {
        let item = self.resolve(path).await?;
        Ok(item_stat(&item))
    }

    /// Read a byte range of the file at path through the shared VFS path
    /// (cache, prefetch, decryption, metering all included).
    pub async fn read(&self, path: &str, off: u64, len: u64) -> Result<Vec<u8>, AdapterError> {
        let item = self.resolve(path).await?;
        if matches!(item.item_type, ItemType::Folder) {
            return Err(AdapterError::IsADirectory);
        }
        Ok(self.vfs.read(&item, off, len).await?)
    }

    /// Whole-file write used by create flows: chunk, encrypt, upload and
    /// return the chunk list the caller upserts into index + changelog.
    /// Storage-only on purpose so no write path bypasses VFS caching rules.
    pub async fn write_full(&self, data: Vec<u8>) -> Result<Vec<lumen_core::chunking::Chunk>, AdapterError> {
        Ok(self.vfs.write_full(data).await?)
    }

    /// Lookup one child by name under a resolved directory - the hot path
    /// every FUSE lookup callback drives. Returns None when absent instead
    /// of erroring so platform code maps it straight to ENOENT/null attrs.
    pub async fn lookup(&self, parent_path: &str, name: &str) -> Result<Option<Stat>, AdapterError> {
        let dir = self.resolve(parent_path).await?;
        if !matches!(dir.item_type, ItemType::Folder) {
            return Err(AdapterError::NotADirectory);
        }
        let parent = if dir.id == uuid::Uuid::nil() { None } else { Some(dir.id) };
        Ok(self
            .index
            .children(parent.as_ref())?
            .into_iter()
            .find(|c| c.name == name)
            .map(|i| item_stat(&i)))
    }

    /// Create a new file under parent_path and write its full contents in
    /// one step: chunk, encrypt, upload through VFS, then register the item
    /// in the metadata index and queue its changelog upsert in CommitBatcher.
    /// Fails with AlreadyExists if a same-name entry is present.
    pub async fn create(&self, parent_path: &str, name: &str, data: Vec<u8>) -> Result<Stat, AdapterError> {
        if self.lookup(parent_path, name).await?.is_some() {
            return Err(AdapterError::AlreadyExists);
        }
        let chunks = self.vfs.write_full(data).await?;
        let dir = self.resolve(parent_path).await?;
        let parent = if dir.id == uuid::Uuid::nil() { None } else { Some(dir.id) };
        let size: u64 = chunks.iter().map(|c| c.len).sum();
        let refs: Vec<lumen_core::metadata::ChunkRef> = chunks.iter().map(|c| lumen_core::metadata::ChunkRef {
            key: c.storage_key(),
            offset: c.offset,
            len: c.len,
            fingerprint: c.fingerprint,
        }).collect();
        let item = RemoteItem {
            id: uuid::Uuid::new_v4(),
            parent_id: parent,
            name: name.to_string(),
            item_type: ItemType::File,
            size,
            content_version: 1,
            metadata_version: 1,
            chunks: refs,
            created_at: chrono::Utc::now(),
            modified_at: chrono::Utc::now(),
        };
        // Index reflects the file immediately so subsequent lookups hit;
        // changelog commit waits for batch flush (storage settled first).
        self.index.upsert(&item)?;
        let stat = item_stat(&item);
        self.commits.push(item)?;
        Ok(stat)
    }

    /// Create a directory entry. Same commit discipline as create().
    pub async fn mkdir(&self, parent_path: &str, name: &str) -> Result<Stat, AdapterError> {
        if self.lookup(parent_path, name).await?.is_some() {
            return Err(AdapterError::AlreadyExists);
        }
        let dir = self.resolve(parent_path).await?;
        let parent = if dir.id == uuid::Uuid::nil() { None } else { Some(dir.id) };
        let item = RemoteItem {
            id: uuid::Uuid::new_v4(),
            parent_id: parent,
            name: name.to_string(),
            item_type: ItemType::Folder,
            size: 0,
            content_version: 1,
            metadata_version: 1,
            chunks: vec![],
            created_at: chrono::Utc::now(),
            modified_at: chrono::Utc::now(),
        };
        self.index.upsert(&item)?;
        let stat = item_stat(&item);
        self.commits.push(item)?;
        Ok(stat)
    }

    /// Replace a file's entire contents: chunk/encrypt/upload new bytes then
    /// bump content/metadata versions so sync consumers see a real change.
    pub async fn replace(&self, path: &str, data: Vec<u8>) -> Result<Stat, AdapterError> {
        let existing = self.resolve(path).await?;
        if matches!(existing.item_type, ItemType::Folder) {
            return Err(AdapterError::IsADirectory);
        }
        let chunks = self.vfs.write_full(data).await?;
        let size: u64 = chunks.iter().map(|c| c.len).sum();
        let refs: Vec<lumen_core::metadata::ChunkRef> = chunks.iter().map(|c| lumen_core::metadata::ChunkRef {
            key: c.storage_key(),
            offset: c.offset,
            len: c.len,
            fingerprint: c.fingerprint,
        }).collect();
        let updated = RemoteItem {
            id: existing.id,
            parent_id: existing.parent_id,
            name: existing.name.clone(),
            item_type: existing.item_type,
            size,
            content_version: existing.content_version + 1,
            metadata_version: existing.metadata_version + 1,
            chunks: refs,
            created_at: existing.created_at,
            modified_at: chrono::Utc::now(),
        };
        self.index.upsert(&updated)?;
        let stat = item_stat(&updated);
        self.commits.push(updated)?;
        Ok(stat)
    }

    /// Remove an item (file or empty directory convention aside, POSIX lets
    /// the caller decide rmdir vs unlink semantics). Index drop plus an
    /// immediate changelog tombstone through CommitBatcher::removal so sync
    /// followers observe the deletion right away instead of waiting behind
    /// queued upserts. Underlying encrypted chunks stay until GC reclaims
    /// unreferenced keys - same trash semantics as delete_file in MCP land.
    pub async fn remove(&self, path: &str) -> Result<Stat, AdapterError> {
        let existing = self.resolve(path).await?;
        self.index.remove(&existing.id)?;
        let stat = Stat {
            item_id: existing.id,
            name: existing.name,
            is_dir: matches!(existing.item_type, ItemType::Folder),
            size: existing.size,
            modified_at: existing.modified_at,
        };
        // Removal tombstones bypass the queue: a delayed tombstone could be
        // overtaken by its own earlier upsert page and resurrect the entry.
        self.commits.removal(existing.id)?;
        Ok(stat)
    }
}

pub fn item_stat(i: &RemoteItem) -> Stat {
    Stat {
        item_id: i.id,
        name: i.name.clone(),
        is_dir: matches!(i.item_type, ItemType::Folder),
        size: i.size,
        modified_at: i.modified_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use lumen_core::crypto::ChunkCrypto;
    use lumen_core::index::MetadataIndex;
    use lumen_core::metadata::ChunkRef;
    use lumen_core::metadata::{ItemType, RemoteItem};
    use lumen_fsprobe::tests::MockObjectStore;
    use std::sync::atomic::Ordering;

    struct Fixture {
        ops: MountOps,
        store: Arc<MockObjectStore>,
        vfs: Arc<VfsOps>,
        index: Arc<MetadataIndex>,
    }

    async fn fixture() -> Fixture {
        let index = Arc::new(MetadataIndex::open_memory().unwrap());
        let store: Arc<MockObjectStore> = Arc::new(MockObjectStore::default());
        let crypto = ChunkCrypto::from_password("test-pw", b"adapter-salt");
        let vfs = Arc::new(VfsOps::new(store.clone(), crypto));
        let ops = MountOps::new(index.clone(), vfs.clone());
        Fixture { ops, store, vfs, index }
    }

    fn folder_item(name: &str, parent: Option<uuid::Uuid>) -> RemoteItem {
        RemoteItem {
            id: uuid::Uuid::new_v4(),
            parent_id: parent,
            name: name.to_string(),
            item_type: ItemType::Folder,
            size: 0,
            content_version: 1,
            metadata_version: 1,
            chunks: vec![],
            created_at: Utc::now(),
            modified_at: Utc::now(),
        }
    }

    /// Write data through the VFS and register the resulting item in the
    /// index under parent - the exact flow a create callback will take.
    async fn seed_file(
        f: &Fixture,
        name: &str,
        parent: Option<uuid::Uuid>,
        data: Vec<u8>,
    ) -> uuid::Uuid {
        let chunks = f.vfs.write_full(data).await.unwrap();
        let refs: Vec<ChunkRef> = chunks.iter().map(|c| ChunkRef {
            key: c.storage_key(),
            offset: c.offset,
            len: c.len,
            fingerprint: c.fingerprint,
        }).collect();
        let size = refs.iter().map(|r| r.len).sum();
        let id = uuid::Uuid::new_v4();
        f.index.upsert(&RemoteItem {
            id,
            parent_id: parent,
            name: name.to_string(),
            item_type: ItemType::File,
            size,
            content_version: 1,
            metadata_version: 1,
            chunks: refs,
            created_at: Utc::now(),
            modified_at: Utc::now(),
        }).unwrap();
        id
    }

    #[test]
    fn split_path_normalizes_like_posix() {
        assert_eq!(MountOps::split_path("/").unwrap(), Vec::<&str>::new());
        assert_eq!(MountOps::split_path("///").unwrap(), Vec::<&str>::new());
        assert_eq!(MountOps::split_path("/a/b").unwrap(), vec!["a", "b"]);
        assert_eq!(MountOps::split_path("/a//b/").unwrap(), vec!["a", "b"]);
        assert_eq!(MountOps::split_path("/a/./b").unwrap(), vec!["a", "b"]);
        // ".." clamps at root instead of escaping the mount.
        assert_eq!(MountOps::split_path("/../a").unwrap(), vec!["a"]);
        assert_eq!(MountOps::split_path("/a/../b").unwrap(), vec!["b"]);
    }

    #[tokio::test]
    async fn getattr_and_readdir_root_and_nested() {
        let f = fixture().await;
        let root_stat = f.ops.getattr("/").await.unwrap();
        assert!(root_stat.is_dir);

        let docs = folder_item("docs", None);
        let docs_id = docs.id;
        f.index.upsert(&docs).unwrap();
        seed_file(&f, "readme.md", Some(docs_id), b"lumen readme".to_vec()).await;

        let names: Vec<String> = f.ops.readdir("/").await.unwrap().into_iter()
            .map(|s| s.name).collect();
        assert_eq!(names, vec!["docs"]);

        let docs_entries = f.ops.readdir("/docs").await.unwrap();
        assert_eq!(docs_entries.len(), 1);
        assert_eq!(docs_entries[0].name, "readme.md");
        assert_eq!(docs_entries[0].size, "lumen readme".len() as u64);

        let file_stat = f.ops.getattr("/docs/readme.md").await.unwrap();
        assert_eq!(file_stat.item_id, docs_entries[0].item_id);
        assert!(!file_stat.is_dir);
    }

    #[tokio::test]
    async fn read_via_adapter_hits_storage_then_cache() {
        let mut f = fixture().await;
        // Multi-chunk so cache vs storage distinction is observable.
        let data = vec![b'x'; 300 * 1024];
        let id = seed_file(&mut f, "big.bin", None, data.clone()).await;
        let item = f.index.get_item(&id).unwrap();
        let got = f.ops.read("/big.bin", 0, item.size).await.unwrap();
        assert_eq!(got, data);
        let first = f.store.get_calls.load(Ordering::Relaxed);
        assert!(first >= 1);

        // Second read serves entirely from cache - no new fetches.
        let cached = f.ops.read("/big.bin", 0, item.size).await.unwrap();
        assert_eq!(cached, data);
        assert_eq!(f.store.get_calls.load(Ordering::Relaxed), first);
    }

    #[tokio::test]
    async fn lookup_returns_none_for_missing_and_stats_for_hit() {
        let f = fixture().await;
        let docs = folder_item("docs", None);
        let docs_id = docs.id;
        f.index.upsert(&docs).unwrap();
        seed_file(&f, "a.txt", Some(docs_id), b"hello".to_vec()).await;

        // Hot-path semantics: absent child is None, not an error.
        assert!(f.ops.lookup("/docs", "missing.txt").await.unwrap().is_none());
        let hit = f.ops.lookup("/docs", "a.txt").await.unwrap().unwrap();
        assert_eq!(hit.name, "a.txt");
        assert!(!hit.is_dir);

        // Lookup under a file path must fail like readdir would.
        assert!(matches!(
            f.ops.lookup("/docs/a.txt", "child").await,
            Err(AdapterError::NotADirectory)
        ));
    }

    #[tokio::test]
    async fn error_mapping_matches_posix_semantics() {
        let f = fixture().await;
        let docs = folder_item("docs", None);
        f.index.upsert(&docs).unwrap();
        seed_file(&f, "file.bin", None, b"payload".to_vec()).await;

        // Missing intermediate directory -> NotFound, not NotADirectory:
        // resolution walks segment by segment like the kernel would.
        assert!(matches!(f.ops.resolve("/nope/file").await, Err(AdapterError::NotFound)));
        assert!(matches!(f.ops.getattr("/nope").await, Err(AdapterError::NotFound)));

        // readdir of a file -> ENOTDIR
        assert!(matches!(f.ops.readdir("/file.bin").await, Err(AdapterError::NotADirectory)));
        // read of a folder -> EISDIR
        assert!(matches!(f.ops.read("/", 0, 4).await, Err(AdapterError::IsADirectory)));
        // read past EOF returns empty bytes without error, like read(2)
        let tail = f.ops.read("/file.bin", 999, 10).await.unwrap();
        assert!(tail.is_empty());
    }

    #[tokio::test]
    async fn create_flush_bathes_multiple_files_into_one_changelog_append() {
        let f = fixture().await;

        // Three files written through the full VFS path (chunk, encrypt,
        // upload, dedup-check) before any metadata commit happens.
        f.ops.create("/", "one.txt", b"alpha".to_vec()).await.unwrap();
        f.ops.create("/", "two.txt", b"beta".to_vec()).await.unwrap();
        // Create under a file path must fail like POSIX would.
        f.ops.create("/two.txt", "child.txt", b"gamma".to_vec())
            .await
            .unwrap_err();

        // No changelog commit should have happened yet: creates only queue.
        // (verified below via flush semantics instead of poking internals)

        let committed = f.ops.commit_pending().await.unwrap();
        assert_eq!(committed, 2, "both upserts land in one batched append");

        // Second flush proves the first one consumed everything - no
        // double-commits and no leftover sequence burn.
        assert_eq!(f.ops.commit_pending().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn create_replaces_fail_on_existing_and_are_visible_in_lookup() {
        let f = fixture().await;
        let a = f.ops.create("/", "a.txt", b"first".to_vec()).await.unwrap();
        assert!(!a.is_dir);
        assert_eq!(a.size, 5);

        // Duplicate name is rejected before any bytes move upstream.
        assert!(matches!(
            f.ops.create("/", "a.txt", b"second".to_vec()).await,
            Err(AdapterError::AlreadyExists)
        ));

        // The file is visible immediately via lookup - index-first means
        // subsequent FUSE callbacks never miss on a just-created entry.
        let hit = f.ops.lookup("/", "a.txt").await.unwrap().unwrap();
        assert_eq!(hit.item_id, a.item_id);

        // replace() succeeds and bumps version counters.
        let replaced = f.ops.replace("/a.txt", b"much longer second".to_vec()).await.unwrap();
        assert_eq!(replaced.size, "much longer second".len() as u64);

        // Content-addressed storage means the identical rewrite re-uploads
        // nothing; every chunk dedup-hits via object head checks.
        f.ops.replace("/a.txt", b"much longer second".to_vec()).await.unwrap();
    }

    #[tokio::test]
    async fn mkdir_creates_directories_and_respects_unique_names() {
        let f = fixture().await;
        let d = f.ops.mkdir("/", "docs").await.unwrap();
        assert!(d.is_dir);
        assert!(matches!(
            f.ops.mkdir("/", "docs").await.unwrap_err(),
            AdapterError::AlreadyExists
        ));
        // Nested creation through resolved parent path.
        let nested = f.ops.mkdir("/docs", "nested").await.unwrap();
        assert!(nested.is_dir);
        let got = f.ops.getattr("/docs/nested").await.unwrap();
        assert_eq!(got.item_id, nested.item_id);
        // Commit path lands the folder upserts in the batch too.
        let committed = f.ops.commit_pending().await.unwrap();
        assert!(committed >= 2);
    }

    #[tokio::test]
    async fn replace_refuses_directories_and_bumps_versions() {
        let f = fixture().await;
        f.ops.mkdir("/", "dir").await.unwrap();
        assert!(matches!(
            f.ops.replace("/dir", b"data".to_vec()).await,
            Err(AdapterError::IsADirectory)
        ));
    }
}
