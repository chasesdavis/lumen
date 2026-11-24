//! LocalObjectStore contract coverage: put/get/range/delete/list/head over a
//! real temp directory. GC and lumen-mount's file:// mode rely on this store
//! behaving identically to S3-backed ones.

use bytes::Bytes;
use lumen_core::store::ObjectStore;
use lumen_fsprobe::local::LocalObjectStore;

#[tokio::test]
async fn put_get_range_delete_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalObjectStore::open(dir.path()).unwrap();

    let body = Bytes::from(vec![42u8; 10_000]);
    store
        .put("chunks/aa/bb/cc/dd/key1", body.clone())
        .await
        .unwrap();

    assert_eq!(store.head("chunks/aa/bb/cc/dd/key1").await.unwrap(), 10_000);
    assert_eq!(store.get("chunks/aa/bb/cc/dd/key1").await.unwrap(), body);

    // Range read mid-object returns exactly the requested slice.
    let slice = store
        .get_range("chunks/aa/bb/cc/dd/key1", 100, 50)
        .await
        .unwrap();
    assert_eq!(slice.len(), 50);
    assert_eq!(&slice[..], &body[100..150]);

    // Missing key surfaces as typed NotFound, matching S3Store contract.
    assert!(store.head("chunks/nope").await.is_err());
    assert!(store.get("chunks/nope").await.is_err());

    store.delete("chunks/aa/bb/cc/dd/key1").await.unwrap();
    assert!(store.head("chunks/aa/bb/cc/dd/key1").await.is_err());

    // Deleting an absent key is idempotent - object stores behave that way.
    store.delete("chunks/aa/bb/cc/dd/key1").await.unwrap();
}

#[tokio::test]
async fn list_scopes_to_prefix_and_skips_partials() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalObjectStore::open(dir.path()).unwrap();

    store
        .put("chunks/aa/bb/00/x", Bytes::from_static(b"a"))
        .await
        .unwrap();
    store
        .put("chunks/aa/bb/11/y", Bytes::from_static(b"bb"))
        .await
        .unwrap();
    store
        .put("chunks/zz/99/w", Bytes::from_static(b"ccc"))
        .await
        .unwrap();
    // tmp-partial artifacts must never appear in GC scans.
    std::fs::write(dir.path().join("chunks/partial.tmp-partial"), b"junk").unwrap();

    let chunks = store.list("chunks/").await.unwrap();
    assert_eq!(chunks.len(), 3, "partial writes excluded from listing");
    assert!(chunks.iter().any(|e| e.key == "chunks/aa/bb/00/x"));
    assert!(chunks.iter().any(|e| e.key == "chunks/aa/bb/11/y"));
    assert_eq!(chunks[0].key, "chunks/aa/bb/00/x", "list is sorted by key");

    let scoped = store.list("chunks/aa/").await.unwrap();
    assert_eq!(scoped.len(), 2, "prefix filter narrows the scan");
    assert!(scoped.iter().all(|e| e.key.starts_with("chunks/aa/")));

    // Empty prefix region lists as empty rather than erroring.
    assert!(store.list("nothing-here/").await.unwrap().is_empty());
}
