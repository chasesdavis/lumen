//! Windows mount daemon: opens index + storage, builds MountOps, runs the
//! WinFsp FileSystemHost under a private multi-thread runtime. The service
//! architecture is intentionally skipped for now: lumen-mount on Windows is
//! a console process mounting a drive letter directly, which keeps parity
//! with the Unix daemon surface until a proper WinFsp NT-service wrapper
//! lands with Phase 5 packaging.
#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

use clap::Parser;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "lumen-mount",
    version,
    about = "Mount a Lumen drive as a WinFsp volume"
)]
struct Args {
    /// Drive letter or directory to mount at (e.g. L: or C:\Lumen).
    mountpoint: String,

    /// SQLite file backing the metadata index.
    #[arg(long)]
    index_path: String,

    /// S3-compatible object store endpoint URL for chunk storage.
    #[arg(long)]
    store_endpoint: String,

    /// Bucket name on the object store endpoint (or local dir with file://).
    #[arg(long)]
    bucket: String,

    /// Master password deriving the per-chunk encryption keys.
    #[arg(long)]
    password: String,
}

#[cfg(windows)]
fn main() {
    let args = Args::parse();

    tracing_subscriber::fmt()
        .with_env_filter("lumen=info")
        .with_writer(std::io::stderr)
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("failed to build tokio runtime");

    rt.block_on(async {
        let index = match lumen_core::index::MetadataIndex::open(&args.index_path) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("index open failed: {e}");
                std::process::exit(1);
            }
        };
        let store = build_store(&args.store_endpoint, &args.bucket)
            .await
            .unwrap_or_else(|e| {
                eprintln!("store setup failed: {e}");
                std::process::exit(1);
            });
        let vfs = lumen_fsprobe::vfs::VfsOps::new(
            store,
            lumen_core::crypto::ChunkCrypto::from_password(&args.password, b"lumen-mount-salt"),
        );
        let ops = Arc::new(lumen_fs_adapter::MountOps::new(
            Arc::new(index),
            Arc::new(vfs),
        ));
        let shim = lumen_winfsp_shim::WinFspShim::new(ops);

        let _init = winfsp::winfsp_init();

        let mut volume_params = winfsp::host::VolumeParams::new();
        volume_params
            .prefix("")
            .filesystem_name("Lumen")
            .file_info_timeout(u32::MAX)
            .case_sensitive_search(false)
            .case_preserved_names(true)
            .unicode_on_disk(true)
            .flush_and_purge_on_cleanup(true)
            .post_cleanup_when_modified_only(true)
            .pass_query_directory_filename(false);
        let host = winfsp::host::FileSystemHost::<lumen_winfsp_shim::WinFspShim>::new_with_options(
            winfsp::host::FileSystemParams {
                use_dir_info_by_name: false,
                volume_params,
                debug_mode: winfsp::host::DebugMode::none(),
            },
            shim,
        );
        let mut host = host.expect("failed to create WinFsp host");
        if let Err(e) = host.mount(&args.mountpoint) {
            eprintln!("mount failed: {}", e.to_ntstatus());
            std::process::exit(1);
        }
        tracing::info!(target: "lumen", "Lumen volume mounted at {}", args.mountpoint);

        // Keep the process alive while mounted; Ctrl+C triggers unmount+drop.
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    });
}

async fn build_store(
    endpoint: &str,
    bucket: &str,
) -> Result<Arc<dyn lumen_core::store::ObjectStore>, String> {
    // file:// scheme keeps the mount fully local without any S3 provider -
    // useful for dogfooding and Windows development before credentials are
    // wired into a release config. Anything else routes through S3Store.
    if endpoint == "file://" {
        return Ok(Arc::new(
            lumen_fsprobe::local::LocalObjectStore::open(bucket).map_err(|e| e.to_string())?,
        ));
    }
    if endpoint.is_empty() {
        return Err("missing --store-endpoint: pass an S3 endpoint URL or file://".into());
    }
    lumen_storage_adapters::S3Store::from_env(endpoint, bucket)
        .map(|s| Arc::new(s) as Arc<dyn lumen_core::store::ObjectStore>)
}

#[cfg(not(windows))]
fn main() {
    eprintln!("lumen-mount-winfsp is Windows-only; Unix ships the FUSE binary instead");
    std::process::exit(1);
}
