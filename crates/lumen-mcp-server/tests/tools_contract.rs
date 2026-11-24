//! MCP tool invocation contract tests against seeded fixture data.
//!
//! Mirrors the test plan: validate list/search/metadata/read behaviors,
//! including the storage-backed read_file path (encrypt fixture chunks with
//! ChunkCrypto, fetch through VfsOps, verify plaintext reaches the agent).

use lumen_core::index::MetadataIndex;
use lumen_core::metadata::{ChunkRef, ItemType, RemoteItem};
use lumen_core::store::ObjectStore;
use lumen_fsprobe::tests::MockObjectStore;
use lumen_fsprobe::vfs::VfsOps;
use lumen_mcp_server::tools::Tools;
use serde_json::json;
use std::sync::Arc;

fn crypto() -> lumen_core::crypto::ChunkCrypto {
    lumen_core::crypto::ChunkCrypto::from_password("test-password", b"contract-salt")
}

/// Seed one folder and two files into index + encrypted store.
async fn seed(tools: &Tools, store: Arc<MockObjectStore>) -> Vec<RemoteItem> {
    let now = chrono::Utc::now();
    let folder = RemoteItem {
        id: uuid::Uuid::new_v4(),
        parent_id: None,
        name: "docs".into(),
        item_type: ItemType::Folder,
        size: 0,
        content_version: 1,
        metadata_version: 1,
        chunks: vec![],
        created_at: now,
        modified_at: now,
    };

    let file_body = b"alpha file body - read through MCP".to_vec();
    let fp = [7u8; 16];
    let blob = crypto().encrypt_chunk(&file_body, &fp).unwrap();
    store.put("chunks/a", blob.into()).await.unwrap();
    let alpha = RemoteItem {
        parent_id: Some(folder.id),
        name: "alpha.txt".into(),
        item_type: ItemType::File,
        size: file_body.len() as u64,
        content_version: 3,
        metadata_version: 2,
        chunks: vec![ChunkRef { key: "chunks/a".into(), offset: 0, len: file_body.len() as u64, fingerprint: fp }],
        created_at: now,
        modified_at: now,
        ..folder_with_id(folder.id)
    };

    let beta = RemoteItem {
        parent_id: Some(folder.id),
        name: "beta.md".into(),
        item_type: ItemType::File,
        size: 0,
        chunks: vec![],
        created_at: now,
        modified_at: now,
        ..alpha_like(&folder)
    };

    tools.index().upsert(&folder).unwrap();
    tools.index().upsert(&alpha).unwrap();
    tools.index().upsert(&beta).unwrap();
    vec![folder, alpha, beta]
}

fn folder_with_id(_: uuid::Uuid) -> RemoteItem {
    let now = chrono::Utc::now();
    RemoteItem {
        id: uuid::Uuid::new_v4(),
        parent_id: None,
        name: String::new(),
        item_type: ItemType::File,
        size: 0,
        content_version: 0,
        metadata_version: 0,
        chunks: vec![],
        created_at: now,
        modified_at: now,
    }
}

fn alpha_like(folder: &RemoteItem) -> RemoteItem {
    RemoteItem {
        id: uuid::Uuid::new_v4(),
        parent_id: Some(folder.id),
        name: String::new(),
        item_type: ItemType::File,
        size: 0,
        content_version: 0,
        metadata_version: 0,
        chunks: vec![],
        created_at: chrono::Utc::now(),
        modified_at: chrono::Utc::now(),
    }
}

#[tokio::test]
async fn list_search_metadata_read_contract() {
    // Storage-backed Tools: dispatch drives VfsOps for read_file.
    let store = Arc::new(MockObjectStore::default());
    let vfs = VfsOps::new(store.clone() as Arc<dyn ObjectStore>, crypto());
    let tools = Tools::with_vfs(MetadataIndex::open_memory().unwrap(), vfs);
    let items = seed(&tools, store.clone()).await;
    let (folder, alpha, _beta) = (&items[0], &items[1], &items[2]);

    // list_files at root returns the folder
    let listed = tools.dispatch("list_files", json!({})).unwrap();
    assert_eq!(listed["files"].as_array().unwrap().len(), 1);
    assert_eq!(listed["files"][0]["name"], "docs");
    assert_eq!(listed["files"][0]["type"], "folder");

    // list_files by parent id
    let children = tools.dispatch("list_files", json!({ "parent_id": folder.id.to_string() })).unwrap();
    assert_eq!(children["files"].as_array().unwrap().len(), 2);

    // search
    let hits = tools.dispatch("search_files", json!({ "query": "beta" })).unwrap();
    assert_eq!(hits["files"].as_array().unwrap().len(), 1);
    assert_eq!(hits["files"][0]["name"], "beta.md");
    let none = tools.dispatch("search_files", json!({ "query": "nonexistent" })).unwrap();
    assert_eq!(none["files"].as_array().unwrap().len(), 0);

    // content search runs against the local plaintext index: nothing
    // indexed yet, so there are no hits but the query is not an error.
    let content = tools.dispatch("search_files", json!({ "query": "x", "kind": "content" })).unwrap();
    assert!(content["files"].as_array().unwrap().is_empty());

    // index_content fetches alpha's first page and makes it searchable
    tools.dispatch("index_content", json!({ "id": alpha.id.to_string() })).unwrap();
    let found = tools.dispatch("search_files", json!({ "query": "alpha file", "kind": "content" })).unwrap();
    assert_eq!(found["files"].as_array().unwrap().len(), 1);
    assert_eq!(found["files"][0]["name"], "alpha.txt");

    // file_metadata by id exposes chunk map + versions
    let meta = tools.dispatch("file_metadata", json!({ "id": alpha.id.to_string() })).unwrap();
    assert_eq!(meta["item"]["size"], file_size(alpha));
    assert_eq!(meta["item"]["chunks"].as_array().unwrap().len(), 1);
    assert_eq!(meta["item"]["chunks"][0]["key"], "chunks/a");

    // file_metadata resolves nested paths from root
    let by_path = tools.dispatch("file_metadata", json!({ "path": "/docs/alpha.txt" })).unwrap();
    assert_eq!(by_path["item"]["id"], alpha.id.to_string());

    // metadata NotFound is a typed error, not a panic
    let missing = tools.dispatch("file_metadata", json!({ "id": uuid::Uuid::new_v4().to_string() }));
    assert!(missing.is_err());

    // read_file returns decrypted plaintext paged under the cap
    let read = tools.dispatch("read_file", json!({ "id": alpha.id.to_string(), "offset": 6, "length": 10 })).unwrap();
    let expected = String::from_utf8_lossy(b"alpha file body");
    assert!(read["content_utf8_lossy"]
        .as_str()
        .unwrap()
        .starts_with(&expected[6..16.min(expected.len())]));
    assert_eq!(read["bytes_read"], 10);
    // page ends at 16 of 34 bytes, so the truncation flag must be set
    assert!(read["truncated"].as_bool().unwrap());

    // default length caps at 1 MiB; requesting a huge length still returns small page
    let big = tools.dispatch("read_file", json!({ "id": alpha.id.to_string(), "length": u64::MAX })).unwrap();
    assert_eq!(big["bytes_read"], file_size(alpha) as u64);

    // folders refuse reads with a Conflict, not a raw storage error
    let err = tools.dispatch("read_file", json!({ "id": folder.id.to_string() }));
    assert!(err.is_err());
}

#[tokio::test]
async fn read_file_index_only_mode_reports_limitation() {
    let store = Arc::new(MockObjectStore::default());
    let vfs = VfsOps::new(store.clone() as Arc<dyn ObjectStore>, crypto());
    let tools = Tools::with_index(MetadataIndex::open_memory().unwrap());
    let items = seed(&tools, store).await;
    let alpha = &items[1];

    let out = tools.dispatch("read_file", json!({ "id": alpha.id.to_string() })).unwrap();
    assert!(out["error"].as_str().unwrap().contains("storage-backed"));
}

fn file_size(i: &RemoteItem) -> usize {
    i.size as usize
}

/// Storage + commit-log backed Tools for the agent write path. Exercises
/// create_file / replace_file / delete_file end to end: VFS chunk upload,
/// index upsert, CommitBatcher deferral, and immediate tombstones.
#[tokio::test]
async fn write_tools_create_replace_delete_contract() {
    use lumen_core::ChangeLog;

    let store = Arc::new(MockObjectStore::default());
    let vfs = VfsOps::new(store.clone() as Arc<dyn ObjectStore>, crypto());
    let log = Arc::new(ChangeLog::default());
    let tools = Tools::with_commit_log(MetadataIndex::open_memory().unwrap(), vfs, log.clone());
    let items = seed(&tools, store).await;
    let folder = &items[0];

    // create_file under the seeded docs folder: new item lands in the
    // index with the correct parent, size reflects encrypted content bytes,
    // and the upsert is deferred in the batcher - not yet in the changelog.
    let created = tools.dispatch("create_file", json!({
        "name": "notes.md",
        "parent_id": folder.id.to_string(),
        "content": "agent written body",
    })).unwrap();
    let created_id = created["item"]["id"].as_str().unwrap();
    assert_eq!(created["item"]["name"], "notes.md");
    assert_eq!(created["item"]["size"], 18);
    assert_eq!(created["committed"], false);
    let child = tools.index().get_item(&uuid::Uuid::parse_str(created_id).unwrap()).unwrap();
    assert_eq!(child.parent_id, Some(folder.id));
    assert!(!child.chunks.is_empty());
    assert_eq!(log.len(), 0); // still queued in batcher

    // read_file returns exactly what was written through VFS storage
    let read = tools.dispatch("read_file", json!({ "id": created_id })).unwrap();
    assert_eq!(read["content_utf8_lossy"], "agent written body");

    // list_files now shows three children under docs
    let children = tools.dispatch("list_files", json!({ "parent_id": folder.id.to_string() })).unwrap();
    assert_eq!(children["files"].as_array().unwrap().len(), 3);

    // duplicate name under same parent is a typed conflict
    let dup = tools.dispatch("create_file", json!({
        "name": "notes.md",
        "parent_id": folder.id.to_string(),
        "content": "",
    }));
    assert!(dup.is_err());

    // path separators in name are rejected: MCP names are flat, pathing
    // belongs to parent_id resolution, never client-supplied strings
    let bad = tools.dispatch("create_file", json!({
        "name": "a/b.txt",
        "parent_id": folder.id.to_string(),
    }));
    assert!(bad.is_err());

    // missing parent_id is rejected rather than defaulting to root: agents
    // must resolve the explicit root id from list_files first, which keeps
    // every create auditable against a known parent entry.
    let no_parent = tools.dispatch("create_file", json!({ "name": "orphan.txt" }));
    assert!(no_parent.is_err());

    // replace_file rewrites content and bumps both version counters
    let replaced = tools.dispatch("replace_file", json!({
        "id": created_id,
        "content": "replacement body v2",
    })).unwrap();
    assert_eq!(replaced["item"]["content_version"], 2);
    assert_eq!(replaced["item"]["metadata_version"], 2);
    assert_eq!(replaced["item"]["size"], 19);
    let read_v2 = tools.dispatch("read_file", json!({ "id": created_id })).unwrap();
    assert_eq!(read_v2["content_utf8_lossy"], "replacement body v2");

    // replacing a folder is refused
    let is_dir = tools.dispatch("replace_file", json!({
        "id": folder.id.to_string(),
        "content": "x",
    }));
    assert!(is_dir.is_err());

    // delete_file removes from the index and commits the tombstone to the
    // shared changelog immediately (not deferred behind pending upserts)
    let deleted = tools.dispatch("delete_file", json!({ "id": created_id })).unwrap();
    assert_eq!(deleted["removed"], true);
    assert!(tools.index().get_item(&uuid::Uuid::parse_str(created_id).unwrap()).is_err());
    // changelog carries the tombstone even before any flush call:
    // removals bypass the queue so cursor followers cannot resurrect items
    let page = log.changes_since(0, 100).unwrap();
    assert_eq!(page.removals.len(), 1);
    assert_eq!(page.removals[0], uuid::Uuid::parse_str(created_id).unwrap());
}

/// Index-only mode reports the write-tool limitation cleanly instead of
/// panicking or silently writing nothing: an agent connecting without
/// storage backing gets a typed error naming the constraint.
#[test]
fn write_tools_index_only_mode_reports_limitation() {
    let tools = Tools::with_index(MetadataIndex::open_memory().unwrap());
    assert!(tools.dispatch("create_file", json!({ "name": "x" })).is_err());
    assert!(tools.dispatch("replace_file", json!({ "id": uuid::Uuid::new_v4().to_string() })).is_err());
}

#[test]
fn gc_run_index_only_mode_reports_limitation() {
    let tools = Tools::with_index(MetadataIndex::open_memory().unwrap());
    let err = tools.dispatch("gc_run", json!({})).unwrap_err();
    assert!(err.to_string().contains("storage-backed mode"));
}

#[tokio::test]
async fn gc_run_sweeps_orphans_and_preserves_live_chunks() {
    let store = Arc::new(MockObjectStore::default());
    let vfs = VfsOps::new(store.clone() as Arc<dyn ObjectStore>, crypto());
    let log = Arc::new(lumen_core::ChangeLog::default());
    let tools = Tools::with_commit_log(
        MetadataIndex::open_memory().unwrap(),
        vfs,
        log.clone(),
    );

    // Seed one live file whose chunk must survive the sweep, plus a second
    // file we then delete - its chunk becomes reclaimable.
    let items = seed(&tools, store.clone()).await;
    // Route one item through the real MCP write path so its upsert sits in
    // the CommitBatcher queue when GC runs - a pre-flush scan would see its
    // freshly uploaded chunk as an orphan.
    tools.dispatch("create_file", json!({
        "name": "queued-by-mcp.txt",
        "parent_id": items[0].id.to_string(),
        "content": "content whose chunk is uploaded but not yet committed"
    })).unwrap();

    let dead_body = b"beta payload that will be deleted".to_vec();
    let fp = [9u8; 16];
    let blob = crypto().encrypt_chunk(&dead_body, &fp).unwrap();
    store.put("chunks/deadbeef", blob.into()).await.unwrap();

    // beta references nothing; create an item that points at the dead key so
    // the sweep can prove it separates referenced vs orphan keys. Attach it
    // to the seeded folder via direct index seeding like seed() does.
    let now = chrono::Utc::now();
    let victim = RemoteItem {
        id: uuid::Uuid::new_v4(),
        parent_id: Some(items[0].id),
        name: "victim.txt".into(),
        item_type: ItemType::File,
        size: dead_body.len() as u64,
        content_version: 1,
        metadata_version: 1,
        chunks: vec![ChunkRef { key: "chunks/deadbeef".into(), offset: 0, len: dead_body.len() as u64, fingerprint: fp }],
        created_at: now,
        modified_at: now,
    };
    tools.index().upsert(&victim).unwrap();

    // Delete the victim through the MCP write path: immediate tombstone,
    // chunk intentionally left behind for GC.
    tools.dispatch("delete_file", json!({ "id": victim.id.to_string() })).unwrap();
    assert!(store.map.lock().unwrap().contains_key("chunks/deadbeef"));

    // GC run must flush pending commits first (alpha is still queued in
    // the batcher from seed-time; a pre-flush scan would see its chunk as
    // an orphan) and then report exactly one reclaimed key/byte count.
    let out = tools.dispatch("gc_run", json!({})).unwrap();
    assert_eq!(out["reclaimed_keys"], 1);
    assert_eq!(out["scanned_keys"], 3, "chunks/a + deadbeef + the queued MCP write chunk");
    assert_eq!(
        out["pending_commits_flushed"].as_u64().map(|v| v > 0),
        Some(true),
        "MCP-created upsert must be flushed before scanning"
    );
    assert!(!store.map.lock().unwrap().contains_key("chunks/deadbeef"));
    assert!(store.map.lock().unwrap().contains_key("chunks/a"));

    // The tombstone survived in front of the queue and the changelog still
    // shows exactly one removal recorded by delete_file.
    let page = log.changes_since(0, 100).unwrap();
    assert_eq!(page.removals.len(), 1);
    assert_eq!(page.removals[0], victim.id);

    // A second pass over clean storage reclaims nothing.
    let idle = tools.dispatch("gc_run", json!({})).unwrap();
    assert_eq!(idle["reclaimed_keys"], 0);
}
