//! Mount daemon: opens index + storage, builds MountOps, runs the FUSE
//! session under a private multi-thread runtime.
//!
//! Blocking note: fuser callbacks are sync and arrive on session threads,
//! while VfsOps are async. Session::run consumes its threads, so we spawn
//! a multi-thread tokio runtime and keep it alive for the process lifetime;
//! callbacks pick it up through Handle::try_current in FuseShim::block_on
//! via block_in_place, hopping to the blocking pool during storage IO.

#[cfg(unix)]
fn main() {
    use clap::Parser;

    #[derive(Parser)]
    #[command(
        name = "lumen-mount",
        version,
        about = "Mount a Lumen drive as a FUSE volume"
    )]
    struct Args {
        /// Mountpoint directory (e.g. /Volumes/Lumen).
        mountpoint: std::path::PathBuf,

        /// SQLite file backing the metadata index. Required: a mounted volume
        /// without persistence would forget its tree on every unmount.
        #[arg(long)]
        index_path: String,

        /// S3-compatible object store endpoint URL for chunk storage.
        #[arg(long)]
        store_endpoint: String,

        /// Bucket name on the object store endpoint.
        #[arg(long)]
        bucket: String,

        /// Master password deriving the per-chunk encryption keys.
        #[arg(long)]
        password: String,

        /// Number of FUSE event loop worker threads.
        #[arg(long, default_value_t = 4)]
        workers: usize,
    }

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
        let index = lumen_core::index::MetadataIndex::open(&args.index_path)
            .expect("failed to open metadata index");
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
        let ops = std::sync::Arc::new(lumen_fs_adapter::MountOps::new(
            std::sync::Arc::new(index),
            std::sync::Arc::new(vfs),
        ));
        let shim = lumen_fuse_shim::FuseShim::new(ops);
        let mut config = fuser::Config::default();
        config.mount_options = vec![
            fuser::MountOption::FSName("lumen".into()),
            fuser::MountOption::Subtype("lumen".into()),
            fuser::MountOption::AutoUnmount,
            fuser::MountOption::DefaultPermissions,
            fuser::MountOption::NoAtime,
        ];
        config.acl = fuser::SessionACL::RootAndOwner;
        config.n_threads = Some(args.workers);
        let session = fuser::Session::new(shim, &args.mountpoint, &config)
            .expect("failed to establish FUSE session");
        tracing::info!(target: "lumen", "Lumen volume mounted at {}", args.mountpoint.display());
        if let Err(e) = session.run() {
            eprintln!("FUSE session error: {}", e);
            std::process::exit(1);
        }
    });
}

#[cfg(unix)]
async fn build_store(
    endpoint: &str,
    bucket: &str,
) -> std::result::Result<std::sync::Arc<dyn lumen_core::store::ObjectStore>, String> {
    // "file://" scheme keeps the mount fully local without any S3 provider -
    // useful for dogfooding and Mac development before provider credentials
    // are wired into a release config. Anything else routes through S3Store.
    if endpoint == "file://" {
        return Ok(std::sync::Arc::new(
            lumen_fsprobe::local::LocalObjectStore::open(bucket).map_err(|e| e.to_string())?,
        ));
    }
    if endpoint.is_empty() {
        return Err("missing --store-endpoint: pass an S3 endpoint URL or file://".into());
    }
    lumen_storage_adapters::S3Store::from_env(endpoint, bucket)
        .map(|s| std::sync::Arc::new(s) as std::sync::Arc<dyn lumen_core::store::ObjectStore>)
}

#[cfg(not(unix))]
fn main() {
    eprintln!("lumen-mount is Unix-only; Windows ships the WinFsp binary instead");
    std::process::exit(1);
}
