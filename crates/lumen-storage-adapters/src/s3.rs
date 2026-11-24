//! S3-compatible adapter over the `object_store` crate. Works with any
//! provider that speaks the S3 API: Cloudflare R2, Backblaze B2, Storj,
//! MinIO, and BYOB endpoints.

use async_trait::async_trait;
use bytes::Bytes;
use lumen_core::error::{LumenError, Result};
use lumen_core::store::{ListEntry, ObjectStore};
use object_store::aws::AmazonS3Builder;
use object_store::path::Path;
use object_store::Error as OSError;
use object_store::ObjectStore as OS;

pub struct S3Store {
    inner: std::sync::Arc<dyn OS>,
}

impl S3Store {
    pub fn new(endpoint: &str, bucket: &str, access_key: &str, secret_key: &str) -> Result<Self> {
        let store = AmazonS3Builder::new()
            .with_endpoint(endpoint)
            .with_bucket_name(bucket)
            .with_access_key_id(access_key)
            .with_secret_access_key(secret_key)
            .build()
            .map_err(|e| LumenError::Storage(e.to_string()))?;
        Ok(Self {
            inner: std::sync::Arc::new(store),
        })
    }

    /// Test seam: wrap a pre-built object_store backend (in-memory, fake)
    /// without talking to S3 at all.
    pub fn from_backend(inner: std::sync::Arc<dyn OS>) -> Self {
        Self { inner }
    }

    /// Build from LUMEN_S3_* environment variables. Keeps credentials out of
    /// argv and config files while letting lumen-mount talk to a real R2/B2/
    /// Storj/MinIO deployment without code changes:
    /// - LUMEN_S3_ACCESS_KEY_ID / LUMEN_S3_SECRET_ACCESS_KEY (required)
    /// - optional LUMEN_S3_REGION (object_store defaults work for most S3-compat providers)
    pub fn from_env(endpoint: &str, bucket: &str) -> std::result::Result<Self, String> {
        let access = std::env::var("LUMEN_S3_ACCESS_KEY_ID")
            .map_err(|_| "LUMEN_S3_ACCESS_KEY_ID not set".to_string())?;
        let secret = std::env::var("LUMEN_S3_SECRET_ACCESS_KEY")
            .map_err(|_| "LUMEN_S3_SECRET_ACCESS_KEY not set".to_string())?;
        let mut builder = AmazonS3Builder::new()
            .with_endpoint(endpoint)
            .with_bucket_name(bucket)
            .with_access_key_id(access)
            .with_secret_access_key(secret);
        if let Ok(region) = std::env::var("LUMEN_S3_REGION") {
            builder = builder.with_region(region);
        }
        let store = builder
            .build()
            .map_err(|e| format!("S3 backend construction failed: {e}"))?;
        Ok(Self {
            inner: std::sync::Arc::new(store),
        })
    }

    /// Map object_store errors to the LumenError variants callers branch on.
    /// VFS dedup logic treats NotFound as a cache-miss signal, so absent
    /// objects must never surface as a generic Storage error here.
    fn map_err(e: OSError) -> LumenError {
        match &e {
            OSError::NotFound { path, .. } => LumenError::NotFound(path.to_string()),
            // object_store 0.10 backends (InMemory, some S3 shims) do not use
            // the typed NotFound variant everywhere - also match the generic
            // error text so contract behavior stays uniform per backend.
            _ if e.to_string().contains("not found") => LumenError::NotFound(e.to_string()),
            _ => LumenError::Storage(e.to_string()),
        }
    }
}

#[async_trait]
impl ObjectStore for S3Store {
    async fn put(&self, key: &str, body: Bytes) -> Result<()> {
        let loc = Path::from(key);
        self.inner
            .put(&loc, body.into())
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<Bytes> {
        let loc = Path::from(key);
        let out = self.inner.get(&loc).await.map_err(Self::map_err)?;
        out.bytes()
            .await
            .map(|b| b.to_vec().into())
            .map_err(Self::map_err)
    }

    async fn get_range(&self, key: &str, offset: u64, len: u64) -> Result<Bytes> {
        let loc = Path::from(key);
        let range = (offset as usize)..((offset + len) as usize);
        self.inner
            .get_range(&loc, range)
            .await
            .map(|b| b.to_vec().into())
            .map_err(Self::map_err)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let loc = Path::from(key);
        self.inner.delete(&loc).await.map_err(Self::map_err)
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>> {
        use futures::StreamExt;
        let prefix_path = if prefix.is_empty() {
            None
        } else {
            Some(Path::from(prefix))
        };
        let mut stream = self.inner.list(prefix_path.as_ref());
        let mut out = Vec::new();
        while let Some(meta) = stream.next().await {
            match meta {
                Ok(m) => out.push(ListEntry {
                    key: m.location.to_string(),
                    size: m.size as u64,
                    etag: m.e_tag.clone(),
                }),
                Err(_) => return Err(LumenError::Storage("list stream failed".to_string())),
            }
        }
        Ok(out)
    }

    async fn head(&self, key: &str) -> Result<u64> {
        let loc = Path::from(key);
        self.inner
            .head(&loc)
            .await
            .map(|m| m.size as u64)
            .map_err(Self::map_err)
    }
}
