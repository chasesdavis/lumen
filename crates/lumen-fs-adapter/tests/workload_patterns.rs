//! Workload-pattern regression suite for the platform-independent mount
//! core. These mirror the three production patterns that kill naive
//! virtual-drive implementations, per the original test plan:
//!
//! - Xcode-style builds: hundreds of small object files created in a
//!   burst, then every byte read back sequentially like a link phase.
//! - Docker bind-mount: append-heavy log files rewritten in place plus
//!   scattered config reads while container churn hits the workspace.
//! - Video editing: multi-chunk large files read in scattered seek order
//!   (scrubbing), exercising chunk-boundary stitching.
//!
//! All assertions are content-correctness only. No assumption is made
//! about chunk counts or cache internals; those belong elsewhere.

use lumen_core::crypto::ChunkCrypto;
use lumen_core::index::MetadataIndex;
use lumen_fs_adapter::MountOps;
use lumen_fsprobe::local::LocalObjectStore;
use lumen_fsprobe::vfs::VfsOps;
use std::collections::BTreeMap;
use std::sync::Arc;

fn ops() -> (MountOps, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalObjectStore::open(dir.path()).unwrap();
    let index = Arc::new(MetadataIndex::open_memory().unwrap());
    let crypto = ChunkCrypto::from_password("workload-suite", b"adapter-salt");
    let vfs = Arc::new(VfsOps::new(Arc::new(store), crypto));
    (MountOps::new(index, vfs), dir)
}

/// Deterministic pseudo-payload generator so failures reproduce exactly.
fn burst(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| seed.wrapping_add(i as u8)).collect()
}

#[tokio::test]
async fn xcode_pattern_object_burst_then_sequential_readback() {
    let (m, _dir) = ops();

    m.mkdir("/", "MyApp.xcodeproj").await.unwrap();
    m.mkdir("/", "build").await.unwrap();
    m.mkdir("/build", "MyApp.build").await.unwrap();
    m.mkdir("/build/MyApp.build", "Objects-normal")
        .await
        .unwrap();

    for name in [
        "project.pbxproj",
        "contents.xcworkspacedata",
        "settings.plist",
    ] {
        let data = burst(256, name.len() as u8);
        m.create("/MyApp.xcodeproj", name, data.clone())
            .await
            .unwrap();
    }

    const COUNT: usize = 200;
    let sizes = [12 * 1024usize, 48 * 1024, 96 * 1024, 3 * 1024];
    let mut expected: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for i in 0..COUNT {
        let path_name = format!("file_{i}.o");
        let data = burst(sizes[i % sizes.len()], i as u8);
        let created = m
            .create(
                "/build/MyApp.build/Objects-normal",
                &path_name,
                data.clone(),
            )
            .await
            .unwrap();
        assert!(!created.is_dir);
        expected.insert(
            format!("/build/MyApp.build/Objects-normal/{path_name}"),
            data,
        );
    }

    for (path, data) in &expected {
        let stat = m.getattr(path).await.unwrap();
        assert_eq!(stat.size, data.len() as u64, "bad size for {path}");
        let got = m.read(path, 0, stat.size).await.unwrap();
        assert_eq!(got.as_slice(), data, "linker read corrupted {path}");
    }

    let entries = m
        .readdir("/build/MyApp.build/Objects-normal")
        .await
        .unwrap();
    assert_eq!(entries.len(), expected.len(), "readdir lost objects");
    for e in entries {
        let path = format!("/build/MyApp.build/Objects-normal/{}", e.name);
        let data = expected.get(&path).expect("stray readdir entry");
        assert_eq!(e.size, data.len() as u64);
    }

    // Incremental relink: one object replaced, neighbors must stay intact.
    let relinked_path = "/build/MyApp.build/Objects-normal/file_42.o";
    m.replace(relinked_path, burst(64 * 1024, 200))
        .await
        .unwrap();
    for i in [41usize, 43] {
        let neighbor = format!("/build/MyApp.build/Objects-normal/file_{i}.o");
        let stat = m.getattr(&neighbor).await.unwrap();
        assert_eq!(stat.size, sizes[i % sizes.len()] as u64);
        let got = m.read(&neighbor, 0, stat.size).await.unwrap();
        assert_eq!(got, burst(sizes[i % sizes.len()], i as u8));
    }
    let got = m.read(relinked_path, 0, 64 * 1024).await.unwrap();
    assert_eq!(got, burst(64 * 1024, 200));
    m.commit_pending().await.unwrap();
}

#[tokio::test]
async fn docker_pattern_append_heavy_log_plus_config_scatter() {
    let (m, _dir) = ops();

    m.mkdir("/", "workspace").await.unwrap();
    m.mkdir("/workspace", "etc").await.unwrap();
    let cfg_line: &[u8] = b"key: value\nother: true\n";
    m.create("/workspace/etc", "config.yaml", cfg_line.to_vec())
        .await
        .unwrap();

    // Container writes logs through bind-mounted paths.
    m.mkdir("/workspace", "logs").await.unwrap();
    m.create("/workspace/logs", "app.jsonl", Vec::new())
        .await
        .unwrap();
    let mut log: Vec<u8> = Vec::new();
    for round in 0..40 {
        for i in 0..10 {
            let n = round * 10 + i;
            log.extend_from_slice(format!("ts={n} msg=req_{round}_{i} status=200\n").as_bytes());
        }
        m.replace("/workspace/logs/app.jsonl", log.clone())
            .await
            .unwrap();
    }
    let stat = m.getattr("/workspace/logs/app.jsonl").await.unwrap();
    assert_eq!(stat.size, log.len() as u64);
    let tail_len = 128u64.min(stat.size);
    let tail = m
        .read("/workspace/logs/app.jsonl", stat.size - tail_len, tail_len)
        .await
        .unwrap();
    assert_eq!(
        tail.as_slice(),
        &log[log.len() - tail_len as usize..],
        "docker logs --tail mismatch"
    );

    // Config scatter re-read must be byte-exact despite log churn.
    let cfg = m.read("/workspace/etc/config.yaml", 0, 1024).await.unwrap();
    assert_eq!(cfg.as_slice(), cfg_line);
    m.commit_pending().await.unwrap();
}

#[tokio::test]
async fn video_pattern_large_file_scattered_seek_reads() {
    let (m, _dir) = ops();

    // Multi-chunk render target: deterministic pieces spanning well past
    // any plausible CDC minimum so real boundary stitching gets hit.
    const PIECE: usize = 4 * 1024 * 1024;
    const COUNT: usize = 12;
    let mut whole: Vec<u8> = Vec::with_capacity(PIECE * COUNT);
    for i in 0..COUNT {
        whole.extend_from_slice(&burst(PIECE, i as u8));
    }
    m.create("/", "render_v1.mov", whole.clone()).await.unwrap();
    let stat = m.getattr("/render_v1.mov").await.unwrap();
    assert_eq!(stat.size, whole.len() as u64);

    // Scrubbing: seek reads in REVERSE order, frame-sized hops only.
    for i in (0..COUNT).rev() {
        let got = m
            .read("/render_v1.mov", (i * PIECE) as u64, PIECE as u64)
            .await
            .unwrap();
        assert_eq!(
            got.as_slice(),
            &whole[i * PIECE..(i + 1) * PIECE],
            "seek read corrupted piece {i}"
        );
    }

    // One big spanning read crossing multiple chunk boundaries.
    let span_start = 5 * PIECE - 512 * 1024;
    let span_len = PIECE + 1024 * 1024;
    let got = m
        .read("/render_v1.mov", span_start as u64, span_len as u64)
        .await
        .unwrap();
    assert_eq!(
        got.as_slice(),
        &whole[span_start..span_start + got.len()],
        "spanning read stitched chunks wrongly"
    );

    // Reads past EOF return empty without error, matching read(2) shape.
    let past_eof = m
        .read("/render_v1.mov", whole.len() as u64, 128)
        .await
        .unwrap();
    assert!(past_eof.is_empty());
    m.commit_pending().await.unwrap();
}
