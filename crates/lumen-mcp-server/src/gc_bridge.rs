//! Bridge between the sync MCP tool dispatch and the async core GC pass.

use lumen_core::error::{LumenError, Result};
use lumen_core::gc::{gc_and_report, GcReport};
use lumen_core::index::MetadataIndex;
use lumen_core::meter::UsageMeter;
use lumen_core::store::ObjectStore;
use std::sync::Arc;

/// Storage + meter handles the gc_run tool drives. Built from the same
/// VfsOps the read/write tools use so GC never sees a second backend.
pub struct GcContext {
    pub store: Arc<dyn ObjectStore>,
    pub meter: UsageMeter,
}

impl GcContext {
    pub fn from_vfs(vfs: &lumen_fsprobe::vfs::VfsOps) -> Self {
        Self {
            store: vfs.store_handle(),
            // Disabled by default; with_gc_meter swaps in a live one for
            // tests and hosts that want reclaim events in their sink.
            meter: UsageMeter::disabled(),
        }
    }
}

/// Sync wrapper over the async GC pass. Runtime handling mirrors
/// vfs_read_blocking: block_in_place on a multi-thread host runtime,
/// inline polling under current-thread, private runtime otherwise.
pub fn gc_run_blocking(
    index: &MetadataIndex,
    store: &Arc<dyn ObjectStore>,
    meter: &UsageMeter,
) -> Result<GcReport> {
    match tokio::runtime::Handle::try_current() {
        Ok(_) if tokio::runtime::Handle::current().runtime_flavor()
            == tokio::runtime::RuntimeFlavor::MultiThread =>
        {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(gc_and_report(index, store.clone(), meter))
            })
        }
        Ok(_) => futures::executor::block_on(gc_and_report(index, store.clone(), meter)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| LumenError::Storage(e.to_string()))?
            .block_on(gc_and_report(index, store.clone(), meter)),
    }
}
