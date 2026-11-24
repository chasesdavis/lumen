//! lumen-core: shared engine for chunking, encryption, storage abstraction,
//! caching, prefetch prediction, metadata indexing, and cursor sync.
//!
//! Platform FS adapters (lumen-fsprobe) and service surfaces (lumen-mcp-server,
//! lumen-api-server) consume this crate exclusively.

pub mod cache;
pub mod changelog;
pub mod chunking;
pub mod crypto;
pub mod error;
pub mod gc;
pub mod index;
pub mod meter;
pub mod metadata;
pub mod prefetch;
pub mod store;
pub mod sync;
pub mod upload;

pub use cache::ChunkCache;
pub use changelog::ChangeLog;
pub use chunking::{FastCdc, Chunk};
pub use crypto::ChunkCrypto;
pub use error::{LumenError, Result};
pub use gc::{gc_and_report, gc_collect, GcReport};
pub use metadata::{DriveTree, ItemType, RemoteItem};
pub use meter::{UsageEvent, UsageMeter, UsageSink};
pub use prefetch::Prefetcher;
pub use sync::{ChangePage, SyncEngine};
