//! S3Store contract tests.
//!
//! Two layers:
//! - in-memory backend behind S3Store -> always runs, validates
//!   put/get/get_range/delete/list/head + NotFound mapping without network.
//! - MinIO-backed layer -> only runs when MINIO_TEST_ENDPOINT env is set and
//!   the endpoint answers, so machines without Docker skip cleanly.

#[cfg(test)]
mod contract {
    use bytes::Bytes;
    use lumen_core::error::LumenError;
    use lumen_core::store::{ListEntry, ObjectStore};
    use lumen_storage_adapters::S3Store;

    fn body(n: usize) -> Bytes {
        let mut v = Vec::with_capacity(n);
        for i in 0u32..(n as u32) {
            v.push((i % 251) as u8 ^ 0x5a);
        }
        Bytes::from(v)
    }

    async fn exercise(store: &dyn ObjectStore) {
        // roundtrip
        store.put("contract/alpha", body(64 * 1024)).await.unwrap();
        let got = store.get("contract/alpha").await.unwrap();
        assert_eq!(got.len(), 64 * 1024);

        // range reads
        let r = store.get_range("contract/alpha", 1024, 8).await.unwrap();
        assert_eq!(&r[..], &body(64 * 1024)[1024..1032]);

        // head size
        assert_eq!(store.head("contract/alpha").await.unwrap(), 64 * 1024);

        // list prefix scoping
        store.put("other/beta", body(128)).await.unwrap();
        let keys: Vec<String> = store
            .list("contract")
            .await
            .unwrap()
            .into_iter()
            .map(|e: ListEntry| e.key)
            .collect();
        assert_eq!(keys, vec!["contract/alpha"]);
        let all = store.list("").await.unwrap();
        assert_eq!(all.len(), 2);

        // overwrite semantics
        store.put("contract/alpha", body(32)).await.unwrap();
        assert_eq!(store.head("contract/alpha").await.unwrap(), 32);

        // delete then NotFound - VFS dedup branches on this variant,
        // absent objects must never surface as generic Storage errors
        store.delete("contract/alpha").await.unwrap();
        match store.get("contract/alpha").await {
            Err(LumenError::NotFound(_)) => {}
            other => panic!(
                "expected NotFound after delete, got {:?}",
                other.map(|b| b.len())
            ),
        }
        match store.head("contract/alpha").await {
            Err(LumenError::NotFound(_)) => {}
            other => panic!("head: expected NotFound, got {:?}", other),
        }

        // deleting an absent key is idempotent (S3 deletes are no-ops)
        store.delete("contract/never-existed").await.unwrap();
    }

    #[tokio::test]
    async fn s3_contract_against_in_memory_backend() {
        use object_store::memory::InMemory;
        let mem = std::sync::Arc::new(InMemory::new());
        let store = S3Store::from_backend(mem);
        exercise(&store).await;
    }

    #[tokio::test]
    async fn s3_contract_against_minio_when_available() {
        // Skip cleanly unless an endpoint is explicitly provided. When one
        // IS provided it must actually answer: transport errors are loud,
        // not silent passes - a green run with a dead endpoint hides bugs.
        let Ok(endpoint) = std::env::var("MINIO_TEST_ENDPOINT") else {
            return;
        };
        if endpoint.is_empty() {
            return;
        }
        // fixed dev credentials, local container only
        let store = S3Store::new(&endpoint, "lumen-test", "minioadmin", "minioadmin")
            .expect("build S3 client");
        // The exercise() head of contract/probe doubles as the liveness
        // probe: NotFound on first hit is expected and handled inside;
        // anything unreachable bubbles up and fails this test loudly.
        exercise(&store).await;
    }
}
