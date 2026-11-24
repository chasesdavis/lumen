//! Object storage abstraction, S3-compatible shaped.
//!
//! Every provider (Cloudflare R2, Backblaze B2, Storj, user BYOB) implements
//! this single trait. Business logic never talks to a concrete SDK.

use crate::error::Result;
use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListEntry {
    pub key: String,
    pub size: u64,
    pub etag: Option<String>,
}

#[async_trait]
pub trait ObjectStore: Send + Sync {
    async fn put(&self, key: &str, body: Bytes) -> Result<()>;

    async fn get(&self, key: &str) -> Result<Bytes>;

    /// Range read of an object: start offset, length. Enables streaming
    /// fetches of individual chunks without downloading whole blobs.
    async fn get_range(&self, key: &str, offset: u64, len: u64) -> Result<Bytes>;

    async fn delete(&self, key: &str) -> Result<()>;

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>>;

    async fn head(&self, key: &str) -> Result<u64>;
}
