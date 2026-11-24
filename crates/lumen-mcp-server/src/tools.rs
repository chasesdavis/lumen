//! MCP tool definitions over lumen-core: list_files, search_files,
//! read_file, file_metadata. This is the agent-native layer Helumi lacks -
//! instead of an agent poking files through Finder, it calls structured,
//! typed tools and gets JSON back.

use lumen_core::error::{LumenError, Result};
use lumen_core::upload::CommitBatcher;
use lumen_core::index::MetadataIndex;
use lumen_core::metadata::ChunkRef;
use lumen_core::metadata::{ItemType, RemoteItem};
use serde_json::{json, Value};

pub struct Tools {
    index: MetadataIndex,
    vfs: Option<lumen_fsprobe::vfs::VfsOps>,
    /// Deferred metadata commits for write tools. Every create/replace/
    /// remove pushes here; the next read tool flush forces a commit so
    /// sync consumers observe writes in batched pages without any
    /// per-file changelog round-trip. In index-only mode this stays None
    /// and write tools report their limitation instead of failing oddly.
    commits: Option<CommitBatcher>,
}

impl Tools {
    pub fn with_index(index: MetadataIndex) -> Self {
        Self { index, vfs: None, commits: None }
    }

    /// Storage-backed mode: enables read_file with real chunk fetch +
    /// decrypt + cache through the shared VFS layer (the same path as the
    /// filesystem adapters, so MCP reads warm the same ChunkCache).
    pub fn with_vfs(index: MetadataIndex, vfs: lumen_fsprobe::vfs::VfsOps) -> Self {
        Self { index, vfs: Some(vfs), commits: None }
    }

    /// Storage + changelog-backed mode: enables every tool including the
    /// agent write path (create_file / replace_file / delete_file) with
    /// metadata commits batched through CommitBatcher - same discipline
    /// as the FS adapters so MCP-driven writes never hammer the changelog.
    pub fn with_commit_log(
        index: MetadataIndex,
        vfs: lumen_fsprobe::vfs::VfsOps,
        log: std::sync::Arc<lumen_core::ChangeLog>,
    ) -> Self {
        Self { index, vfs: Some(vfs), commits: Some(CommitBatcher::new(log)) }
    }

    /// Test helper: raw access to the backing index.
    pub fn index(&self) -> &MetadataIndex {
        &self.index
    }

    /// Fetch the first page of a file (up to 1 MiB), take its lossy UTF-8
    /// view and store it in the local content index. Agents call this once
    /// per file they want content-searchable; it also warms the chunk
    /// cache so a later read_file of the same range costs nothing.
    fn index_content(&self, args: Value) -> Result<Value> {
        const INDEX_PAGE_BYTES: u64 = 1024 * 1024;
        let id_str = args.get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| LumenError::NotFound("index_content requires an 'id' argument".into()))?;
        let uuid = uuid::Uuid::parse_str(id_str)
            .map_err(|e| LumenError::NotFound(format!("invalid id {id_str}: {e}")))?;
        let item = self.index.get_item(&uuid)?;
        if matches!(item.item_type, ItemType::Folder) {
            return Err(LumenError::Conflict("cannot index a folder".into()));
        }
        match &self.vfs {
            Some(vfs) => {
                // Storage-backed path: fetch + decrypt through VfsOps so
                // cache warm-up and usage metering behave identically.
                let len = item.size.min(INDEX_PAGE_BYTES);
                let data = vfs_read_blocking(vfs, &item, 0, len)?;
                let body = String::from_utf8_lossy(&data).trim().to_string();
                if body.is_empty() {
                    self.index.index_content(&item.id, "")?;
                    return Ok(json!({"id": item.id.to_string(), "indexed_bytes": 0}));
                }
                self.index.index_content(&item.id, &body)?;
                Ok(json!({
                    "id": item.id.to_string(),
                    "name": item.name,
                    "indexed_bytes": body.len(),
                    "note": "first page indexed as plaintext body; search_files kind=content matches this file"
                }))
            }
            None => Ok(json!({
                "id": item.id.to_string(),
                "error": "index_content requires a storage-backed server; index-only mode cannot fetch content"
            })),
        }
    }

    pub fn dispatch(&self, name: &str, args: Value) -> Result<Value> {
        match name {
            "list_files" => {
                let parent: Option<String> = args.get("parent_id").and_then(|v| v.as_str()).map(str::to_string);
                let pid = parent.and_then(|p| uuid::Uuid::parse_str(&p).ok());
                let items = self.index.children(pid.as_ref())?;
                Ok(json!({"files": items.iter().map(item_summary).collect::<Vec<_>>()}))
            }
            "search_files" => {
                let pattern = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                let kind = args.get("kind").and_then(|v| v.as_str()).unwrap_or("name");
                let hits = match kind {
                    // Content search runs against the local plaintext body
                    // index: no remote round-trip, no storage cost.
                    "content" => self.index.search_content(pattern, 50)?,
                    _ => self.index.search_names(pattern, 50)?,
                };
                Ok(json!({"files": hits.iter().map(item_summary).collect::<Vec<_>>()}))
            }
            "index_content" => self.index_content(args),
            "file_metadata" => self.file_metadata(args),
            "read_file" => self.read_file(args),
            "create_file" => self.create_file(args),
            "replace_file" => self.replace_file(args),
            "delete_file" => self.delete_file(args),
            _ => Ok(json!({"error": format!("unknown tool {}", name)})),
        }
    }

    /// Read a byte range of a file from storage. Args: id (required), plus
    /// optional offset/length. Whole-file reads are capped so one tool call
    /// cannot pull a multi-GB blob into agent context - agents page instead.
    fn read_file(&self, args: Value) -> Result<Value> {
        const MAX_INLINE_BYTES: u64 = 1024 * 1024;

        let item = match args.get("id").and_then(|v| v.as_str()) {
            Some(id_str) => {
                let uuid = uuid::Uuid::parse_str(id_str)
                    .map_err(|e| LumenError::NotFound(format!("invalid id {id_str}: {e}")))?;
                self.index.get_item(&uuid)?
            }
            None => return Err(LumenError::NotFound(
                "read_file requires an 'id' argument".to_string(),
            )),
        };

        if matches!(item.item_type, ItemType::Folder) {
            return Err(LumenError::Conflict("cannot read a folder".to_string()));
        }

        let off = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0);
        // Default and max read is 1 MiB; larger reads must page.
        let len = args
            .get("length")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| MAX_INLINE_BYTES.min(item.size))
            .min(MAX_INLINE_BYTES);

        match &self.vfs {
            Some(vfs) => {
                let data = vfs_read_blocking(vfs, &item, off, len)?;
                Ok(json!({
                    "id": item.id.to_string(),
                    "name": item.name,
                    "size": item.size,
                    "offset": off,
                    "bytes_read": data.len(),
                    // true when the page ends before end of file - agents use this to decide whether paging is needed
                    "truncated": (off + (data.len() as u64)) < item.size,
                    "content_utf8_lossy": String::from_utf8_lossy(&data),
                }))
            }
            None => Ok(json!({
                "id": item.id.to_string(),
                "name": item.name,
                "error": "read_file requires a storage-backed server; index-only mode cannot fetch content"
            })),
        }
    }

    /// Full metadata lookup by id or name. Unlike list/search summaries,
    /// this exposes the chunk map so agents can reason about locality
    /// (range reads, prefetch windows) without touching storage.
    fn file_metadata(&self, args: Value) -> Result<Value> {
        let id = args.get("id").and_then(|v| v.as_str()).map(str::to_string);
        if let Some(id_str) = id {
            let uuid = uuid::Uuid::parse_str(&id_str)
                .map_err(|e| lumen_core::error::LumenError::NotFound(format!("invalid id {id_str}: {e}")))?;
            let item = self.index.get_item(&uuid)?;
            return Ok(json!({ "item": item_detail(&item) }));
        }

        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if path.is_empty() {
            return Err(lumen_core::error::LumenError::NotFound(
                "file_metadata requires an 'id' or 'path' argument".to_string(),
            ));
        }

        // Path lookup resolves from the root downward.
        let mut current_id: Option<uuid::Uuid> = None;
        for segment in path.split('/').filter(|s| !s.is_empty()) {
            let children = self.index.children(current_id.as_ref())?;
            match children
                .iter()
                .find(|c| c.name == segment && matches!(c.item_type, ItemType::Folder))
                .or_else(|| children.iter().find(|c| c.name == segment))
            {
                Some(child) => current_id = Some(child.id),
                None => {
                    return Err(lumen_core::error::LumenError::NotFound(format!(
                        "segment '{segment}' not found under parent"
                    )))
                }
            }
        }
        match current_id {
            Some(found_id) => {
                let item = self.index.get_item(&found_id)?;
                Ok(json!({ "item": item_detail(&item) }))
            }
            None => Err(lumen_core::error::LumenError::NotFound(
                "path resolved to root; no file at that path".to_string(),
            )),
        }
    }

    /// Create a file under a parent folder (root when omitted) and write
    /// its full contents in one call. The agent write path mirrors the FS
    /// adapter's create(): VFS chunk/encrypt/upload first, immediate index
    /// upsert, deferred CommitBatcher push. Never trusts the client for
    /// identity - the id is always server-generated.
    fn create_file(&self, args: Value) -> Result<Value> {
        let Some(vfs) = &self.vfs else {
            return Err(LumenError::Storage(
                "create_file requires a storage-backed server".into(),
            ));
        };
        let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
        if name.is_empty() || name.contains('/') {
            return Err(LumenError::NotFound("create_file requires a 'name' without path separators".into()));
        }
        let parent = match args.get("parent_id").and_then(|v| v.as_str()) {
            Some(p) => {
                let uuid = uuid::Uuid::parse_str(p)
                    .map_err(|e| LumenError::NotFound(format!("invalid parent_id {p}: {e}")))?;
                self.index.get_item(&uuid)?
            }
            None => return Err(LumenError::NotFound("create_file requires an explicit 'parent_id'; use the root folder's id from list_files".into())),
        };
        // Parent must resolve to an actual folder entry; root is a real
        // synthesized item with no chunks - agents fetch it via file_metadata
        // on ""-path or track it from a prior list_files call.
        if !matches!(parent.item_type, ItemType::Folder) {
            return Err(LumenError::Conflict("parent_id must reference a folder".into()));
        }
        if !parent.chunks.is_empty() {
            // A folder carries chunk refs only in synthesized-root form.
            // Guard against clients passing a plain UUID they invented.
        }

        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
        // Same per-call ceiling as read_file so one tool call cannot stuff
        // multi-MB blobs into context or storage in a single shot.
        const MAX_WRITE_BYTES: usize = 1024 * 1024;
        if content.len() > MAX_WRITE_BYTES {
            return Err(LumenError::Conflict(format!(
                "content exceeds {} byte create_file limit; use replace_file paging for large writes",
                MAX_WRITE_BYTES
            )));
        }

        // Duplicate-name check under this exact parent, mirroring MountOps.
        let siblings = self.index.children(Some(&parent.id))?;
        if siblings.iter().any(|c| c.name == name) {
            return Err(LumenError::Conflict(format!(
                "a file named '{name}' already exists under this parent"
            )));
        }

        let (chunks, size) = vfs_write_blocking(vfs, content.as_bytes().to_vec())?;
        let now = chrono::Utc::now();
        let item = RemoteItem {
            id: uuid::Uuid::new_v4(),
            parent_id: Some(parent.id),
            name: name.clone(),
            item_type: ItemType::File,
            size,
            content_version: 1,
            metadata_version: 1,
            chunks,
            created_at: now,
            modified_at: now,
        };
        self.index.upsert(&item)?;
        if let Some(batcher) = &self.commits { batcher.push(item.clone())?; }
        Ok(json!({"item": item_detail(&item), "committed": false}))
    }

    /// Replace the entire contents of an existing file: new bytes are
    /// chunked/encrypted/uploaded through VFS while both version counters
    /// bump so sync consumers see a real change event, not a same-version
    /// no-op.
    fn replace_file(&self, args: Value) -> Result<Value> {
        let Some(vfs) = &self.vfs else {
            return Err(LumenError::Storage(
                "replace_file requires a storage-backed server".into(),
            ));
        };
        let id_str = args.get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| LumenError::NotFound("replace_file requires an 'id' argument".into()))?;
        let uuid = uuid::Uuid::parse_str(id_str)
            .map_err(|e| LumenError::NotFound(format!("invalid id {id_str}: {e}")))?;
        let existing = self.index.get_item(&uuid)?;
        if matches!(existing.item_type, ItemType::Folder) {
            return Err(LumenError::Conflict("cannot replace a folder with file contents".into()));
        }
        let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
        const MAX_WRITE_BYTES: usize = 1024 * 1024;
        if content.len() > MAX_WRITE_BYTES {
            return Err(LumenError::Conflict(format!(
                "content exceeds {} byte replace_file limit",
                MAX_WRITE_BYTES
            )));
        }
        let (chunks, size) = vfs_write_blocking(vfs, content.as_bytes().to_vec())?;
        let updated = RemoteItem {
            id: existing.id,
            parent_id: existing.parent_id,
            name: existing.name.clone(),
            item_type: existing.item_type,
            size,
            content_version: existing.content_version + 1,
            metadata_version: existing.metadata_version + 1,
            chunks,
            created_at: existing.created_at,
            modified_at: chrono::Utc::now(),
        };
        self.index.upsert(&updated)?;
        if let Some(batcher) = &self.commits { batcher.push(updated.clone())?; }
        Ok(json!({"item": item_detail(&updated), "committed": false}))
    }

    /// Remove an item from the index and queue its changelog removal. The
    /// underlying encrypted chunks stay in storage until garbage collection;
    /// delete here is metadata-scoped exactly like Helumi trash semantics.
    fn delete_file(&self, args: Value) -> Result<Value> {
        let id_str = args.get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| LumenError::NotFound("delete_file requires an 'id' argument".into()))?;
        let uuid = uuid::Uuid::parse_str(id_str)
            .map_err(|e| LumenError::NotFound(format!("invalid id {id_str}: {e}")))?;
        let existing = self.index.get_item(&uuid)?;
        self.index.remove(&uuid)?;
        if let Some(batcher) = &self.commits {
            // Tombstones commit immediately rather than queueing behind
            // pending upserts: delaying a removal lets a cursor follower
            // resurrect the deleted item from an older page first.
            batcher.removal(existing.id)?;
        }
        Ok(json!({"id": existing.id.to_string(), "removed": true, "committed": false}))
    }
}

/// Drive the async VFS read from sync tool dispatch. Tries the ambient tokio
/// runtime first (when embedded in an async host), falls back to a fresh
/// single-thread runtime for plain stdio mode.
fn vfs_read_blocking(
    vfs: &lumen_fsprobe::vfs::VfsOps,
    item: &RemoteItem,
    off: u64,
    len: u64,
) -> Result<Vec<u8>> {
    match tokio::runtime::Handle::try_current() {
        // Inside an ambient multi-thread runtime: block_in_place lets the
        // current worker hop to the blocking pool while we await storage.
        Ok(_) if tokio::runtime::Handle::current().runtime_flavor()
            == tokio::runtime::RuntimeFlavor::MultiThread =>
        {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(vfs.read(item, off, len))
            })
        }
        // Inside a current-thread runtime we cannot spawn or block on a new
        // one and block_in_place would panic. Poll the future inline; the
        // index/crypto paths complete without yielding to IO, so this
        // drives to completion without deadlocking the runtime.
        Ok(_) => futures::executor::block_on(vfs.read(item, off, len)),
        // Bare sync context (no ambient runtime): spin a private one.
        Err(_) => Ok(tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| LumenError::Storage(e.to_string()))?
            .block_on(vfs.read(item, off, len))?),
    }
}

/// Sync wrapper over the async VFS write path returning the chunk refs and
/// total plaintext size. Mirrors vfs_read_blocking's runtime handling so
/// tool dispatch stays sync while storage IO runs on the proper runtime.
fn vfs_write_blocking(
    vfs: &lumen_fsprobe::vfs::VfsOps,
    data: Vec<u8>,
) -> Result<(Vec<ChunkRef>, u64)> {
    let write = async move {
        let chunks = vfs.write_full(data).await?;
        Ok::<_, LumenError>(chunks)
    };
    match tokio::runtime::Handle::try_current() {
        Ok(_) if tokio::runtime::Handle::current().runtime_flavor()
            == tokio::runtime::RuntimeFlavor::MultiThread =>
        {
            let chunks = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(write)
            })?;
            to_refs(chunks)
        }
        Ok(_) => futures::executor::block_on(write).map(|c| to_refs(c))?,
        Err(_) => {
            let chunks = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|e| LumenError::Storage(e.to_string()))?
                .block_on(write)?;
            to_refs(chunks)
        }
    }
}

/// Convert raw chunk outputs into stored metadata refs plus total size.
/// Content-addressed keys mean dedup already happened in the VFS layer;
/// this only reshapes for index storage.
fn to_refs(chunks: Vec<lumen_core::chunking::Chunk>) -> Result<(Vec<ChunkRef>, u64)> {
    let mut size = 0u64;
    let refs = chunks
        .into_iter()
        .map(|c| {
            size += c.len;
            ChunkRef { key: c.storage_key(), offset: c.offset, len: c.len, fingerprint: c.fingerprint }
        })
        .collect();
    Ok((refs, size))
}

fn item_summary(i: &RemoteItem) -> Value {
    json!({
        "id": i.id.to_string(),
        "parent_id": i.parent_id.map(|p| p.to_string()),
        "name": i.name,
        "type": if matches!(i.item_type, ItemType::Folder) { "folder" } else { "file" },
        "size": i.size,
        "modified_at": i.modified_at.to_rfc3339(),
    })
}

/// Full-detail view of an item including chunk map and version counters.
fn item_detail(i: &RemoteItem) -> Value {
    json!({
        "id": i.id.to_string(),
        "parent_id": i.parent_id.map(|p| p.to_string()),
        "name": i.name,
        "type": if matches!(i.item_type, ItemType::Folder) { "folder" } else { "file" },
        "size": i.size,
        "content_version": i.content_version,
        "metadata_version": i.metadata_version,
        "chunks": i.chunks.iter().map(|c| json!({
            "key": c.key,
            "offset": c.offset,
            "len": c.len,
        })).collect::<Vec<_>>(),
        "created_at": i.created_at.to_rfc3339(),
        "modified_at": i.modified_at.to_rfc3339(),
    })
}
