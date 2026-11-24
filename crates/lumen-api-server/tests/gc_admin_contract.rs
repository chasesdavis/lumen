//! GC admin surface contract tests: HTTP upserts/deletes feed the
//! server-side index, a GC pass reclaims orphaned chunks over the real
//! router, and reclaimed bytes land as a NEGATIVE stored-bytes delta on
//! the caller's ledger row. Pass-through billing must shrink when storage
//! is reclaimed - otherwise deleted data would keep costing users money.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use lumen_api_server::routes::{router, AppState, GcBackend};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

async fn call(
    state: Arc<AppState>,
    method: &str,
    path: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json");
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let body = match body {
        Some(v) => Body::from(serde_json::to_vec(&v).unwrap()),
        None => Body::empty(),
    };
    let req = req.body(body).unwrap();
    let response = router(state).oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, json)
}

#[tokio::test]
async fn gc_admin_reclaims_orphans_and_reduces_metered_usage() {
    // In-memory index + local dir object store: no S3 or network needed.
    let dir = tempfile::TempDir::new().unwrap();
    let store_dir = dir.path().join("store");
    std::fs::create_dir(&store_dir).unwrap();
    let store = lumen_fsprobe::local::LocalObjectStore::open(&store_dir).unwrap();

    let state = Arc::new(AppState::new_dev().with_gc_backend(GcBackend {
        index: Arc::new(lumen_core::index::MetadataIndex::open_memory().unwrap()),
        store: Arc::new(store),
    }));

    // Signup to get an access token; its sub scopes the ledger delta.
    let creds = json!({"email": "gc@example.com", "password": "correct horse battery staple"});
    let (status, body) = call(state.clone(), "POST", "/v1/auth/signup", Some(creds), None).await;
    assert_eq!(status, StatusCode::OK);
    let token = body["token"].as_str().unwrap().to_string();

    // Two chunks land in the store; file A stays referenced after we delete
    // file B through HTTP. Sizes are what billing observes.
    let key_a = "chunks/aa/bb/cc/dd/keya".to_string();
    let key_b = "chunks/aa/bb/cc/dd/keyb".to_string();
    let backend_store = state.gc_backend.as_ref().unwrap().store.clone();
    backend_store
        .put(&key_a, bytes::Bytes::from(vec![0u8; 400]))
        .await
        .unwrap();
    backend_store
        .put(&key_b, bytes::Bytes::from(vec![0u8; 300]))
        .await
        .unwrap();

    // Upsert both files through HTTP so the index carries their chunk maps.
    let id_a = uuid::Uuid::new_v4();
    let id_b = uuid::Uuid::new_v4();
    for (id, name, size, key) in [
        (id_a, "a.txt", 400u64, key_a.clone()),
        (id_b, "b.txt", 300u64, key_b.clone()),
    ] {
        let upsert = call(
            state.clone(),
            "POST",
            "/v1/files",
            Some(json!({
                "id": id.to_string(),
                "name": name,
                "size": size,
                "chunks": [{ "key": key, "offset": 0, "len": size,
                             "fingerprint": [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0] }],
            })),
            None,
        )
        .await;
        assert_eq!(upsert.0, StatusCode::OK);
    }

    // Simulate prior metered uploads for both files: A=400 + B=300 stored.
    let user_id = uuid::Uuid::new_v4();
    state
        .usage
        .record(&user_id.to_string(), 700, 0, chrono::Utc::now())
        .unwrap();

    // Delete file B through HTTP: index row drops, chunk becomes orphaned.
    let del = call(
        state.clone(),
        "POST",
        "/v1/files/delete",
        Some(json!({ "id": id_b.to_string() })),
        None,
    )
    .await;
    assert_eq!(del.0, StatusCode::OK);
    assert!(del.1["removed"].as_bool().unwrap());

    // Unauthenticated GC request is rejected before touching storage.
    let (unauth_status, _) = call(state.clone(), "POST", "/v1/admin/gc", None, None).await;
    assert_eq!(unauth_status, StatusCode::UNAUTHORIZED);

    // But wait: signup created its own user, while we metered under a random
    // id above. Grab the JWT holder's identity and align the ledger row.
    let me_body = serde_json::Value::Null;
    let _ = me_body;

    // GC authenticated with the JWT issued at signup: reclaimed bytes hit
    // the ledger row of the token subject, not our arbitrary row.
    let uid = state
        .users
        .login("gc@example.com", "correct horse battery staple")
        .map(|u| u.id)
        .unwrap();
    state
        .usage
        .record(&uid, 700, 0, chrono::Utc::now())
        .unwrap();

    let (gc_status, gc_body) =
        call(state.clone(), "POST", "/v1/admin/gc", None, Some(&token)).await;
    assert_eq!(gc_status, StatusCode::OK);
    assert_eq!(gc_body["scanned_keys"].as_u64().unwrap(), 2);
    assert_eq!(gc_body["reclaimed_keys"].as_u64().unwrap(), 1);
    assert_eq!(gc_body["reclaimed_bytes"].as_u64().unwrap(), 300);

    // Ledger proof: only B's 300 bytes net out after reclaim. The negative
    // delta applies to the JWT subject's row.
    let (stored, retrieved) = state.usage.current_period_usage(&uid).unwrap();
    assert_eq!(stored, 400, "700 metered minus 300 reclaimed");
    assert_eq!(retrieved, 0);

    // Orphan chunk is gone from the store; survivor remains readable.
    assert!(backend_store.get(&key_a).await.is_ok());
    assert!(backend_store.get(&key_b).await.is_err());

    // Second GC pass is a no-op: nothing else is orphaned.
    let (_, idle) = call(state.clone(), "POST", "/v1/admin/gc", None, Some(&token)).await;
    assert_eq!(idle["reclaimed_keys"].as_u64().unwrap(), 0);
}

#[tokio::test]
async fn gc_admin_without_backend_returns_501() {
    // No LUMEN_INDEX_DB equivalent here: pure changelog/metadata-only mode.
    let state = Arc::new(AppState::new_dev());
    let creds =
        json!({"email": "nobackend@example.com", "password": "correct horse battery staple"});
    let (status, body) = call(state.clone(), "POST", "/v1/auth/signup", Some(creds), None).await;
    assert_eq!(status, StatusCode::OK);
    let token = body["token"].as_str().unwrap().to_string();

    let (gc_status, _) = call(state.clone(), "POST", "/v1/admin/gc", None, Some(&token)).await;
    assert_eq!(gc_status, StatusCode::NOT_IMPLEMENTED);
}
