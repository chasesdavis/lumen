//! Local-disk ObjectStore implementation.
//!
//! Serves two purposes:
//! 1. File-backed dogfooding and dev mounts (file:// scheme in lumen-mount)
//!    with the exact same trait surface production providers implement, so
//!    FS adapter behavior never forks between local and remote stores.
//! 2. A deterministic store for GC/integration tests that must survive
//!    process boundaries without MinIO.

use lumen_core::error::{LumenError, Result};
use lumen_core::store::{ListEntry, ObjectStore};
use std::io::{Read, Seek};
use std::path::PathBuf;

pub struct LocalObjectStore {
    root: PathBuf,
}

impl LocalObjectStore {
    pub fn open<P: Into<PathBuf>>(root: P) -> Result<Self> {
        let root = root.into();
        std::fs::create_dir_all(&root).map_err(|e| LumenError::Storage(e.to_string()))?;
        Ok(Self { root })
    }

    fn key_path(&self, key: &str) -> PathBuf {
        // Keys arrive from Chunk::storage_key ("chunks/aa/bb/00/...").
        // Lexical join only; keys containing ".." would need explicit
        // sanitizing but storage_key construction keeps them hex-only.
        self.root.join(key)
    }
}

#[async_trait::async_trait]
impl ObjectStore for LocalObjectStore {
    async fn put(&self, key: &str, body: bytes::Bytes) -> Result<()> {
        let path = self.key_path(key);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| LumenError::Storage(e.to_string()))?;
        }
        // Write-through-temp then rename: a crash mid-put never leaves a
        // truncated object that a later get_range read treats as valid.
        let tmp = path.with_extension("tmp-partial");
        std::fs::write(&tmp, &body).map_err(|e| LumenError::Storage(e.to_string()))?;
        std::fs::rename(&tmp, &path).map_err(|e| LumenError::Storage(e.to_string()))?;
        Ok(())
    }

    async fn get(&self, key: &str) -> Result<bytes::Bytes> {
        let bytes = std::fs::read(self.key_path(key))
            .map_err(|_| LumenError::NotFound(key.to_string()))?;
        Ok(bytes.into())
    }

    async fn get_range(&self, key: &str, offset: u64, len: u64) -> Result<bytes::Bytes> {
        use std::io::SeekFrom;
        let mut f = std::fs::File::open(self.key_path(key))
            .map_err(|_| LumenError::NotFound(key.to_string()))?;
        f.seek(SeekFrom::Start(offset)).map_err(|e| LumenError::Storage(e.to_string()))?;
        let take = len.min(4 * 1024 * 1024 * 1024) as usize;
        let mut buf = vec![0u8; take];
        let read = f.read(&mut buf).map_err(|e| LumenError::Storage(e.to_string()))?;
        buf.truncate(read);
        Ok(buf.into())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        match std::fs::remove_file(self.key_path(key)) {
            Ok(()) => Ok(()),
            Err(_) if !self.key_path(key).exists() => Ok(()),
            Err(e) => Err(LumenError::Storage(e.to_string())),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ListEntry>> {
        // Object-store listing is flat over full keys; on disk a prefix can
        // span nested directories ("chunks/" covers chunks/aa/bb/cc/dd/key),
        // so this walks depth-first collecting files under the prefix.
        let base = self.key_path(prefix.trim_end_matches('/'));
        let mut out: Vec<ListEntry> = Vec::new();
        let mut stack: Vec<std::path::PathBuf> = vec![base];
        while let Some(dir) = stack.pop() {
            let dir_iter = match std::fs::read_dir(&dir) {
                Ok(it) => it,
                Err(_) => continue,
            };
            for entry in dir_iter {
                let entry = entry.map_err(|e| LumenError::Storage(e.to_string()))?;
                let meta = entry.metadata().map_err(|e| LumenError::Storage(e.to_string()))?;
                if meta.is_dir() {
                    stack.push(entry.path());
                    continue;
                }
                if entry.path().extension().and_then(|e| e.to_str()) == Some("tmp-partial") {
                    continue;
                }
                let entry_path = entry.path();
                let rel = entry_path
                .strip_prefix(&self.root)
                .map_err(|e| LumenError::Storage(e.to_string()))?;
                let key = rel
                .components()
                .filter_map(|c| c.as_os_str().to_str())
                .collect::<Vec<_>>()
                .join("/");
                out.push(ListEntry { key, size: meta.len(), etag: None });
            }
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }

    async fn head(&self, key: &str) -> Result<u64> {
        let meta = std::fs::metadata(self.key_path(key))
            .map_err(|_| LumenError::NotFound(key.to_string()))?;
        Ok(meta.len())
    }
}
