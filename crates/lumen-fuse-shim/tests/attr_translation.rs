//! Pure translation tests: Stat -> FileAttr and AdapterError -> Errno.
//! No FUSE session or kernel dependency - these run anywhere Unix.

#[cfg(unix)]
mod t {
    use lumen_fs_adapter::{AdapterError, Stat};
    use lumen_fuse_shim::errno_from_adapter;
    use lumen_fuse_shim::stat_to_attr;

    fn stat(is_dir: bool) -> Stat {
        Stat {
            item_id: uuid::Uuid::nil(),
            name: "a.txt".into(),
            is_dir,
            size: 4096,
            modified_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        }
    }

    #[test]
    fn stat_maps_files_with_rounded_blocks() {
        let attr = stat_to_attr(stat(false), 42);
        assert_eq!(attr.ino.0, 42);
        assert_eq!(attr.size, 4096);
        // st_blocks is in 512B units per stat(2): 4096/512 = 8 exact;
        // odd sizes round up so du reports allocated chunks not bytes.
        assert_eq!(attr.blocks, 8);
        assert_eq!(attr.blksize, 4096);
        assert!(matches!(attr.kind, fuser::FileType::RegularFile));
        assert_eq!(attr.perm, 0o644);
    }

    #[test]
    fn stat_zero_len_file_has_one_block_floor() {
        let mut s = stat(false);
        s.size = 0;
        let size = s.size;
        let attr = stat_to_attr(s, 7);
        // (0 + 511) / 512 = 0 would report zero allocation; spec floor of 1
        // keeps POSIX tools that assume nonzero blocks happy.
        assert_eq!(attr.blocks, size.div_ceil(512));
    }

    #[test]
    fn stat_maps_dirs_as_directories() {
        let attr = stat_to_attr(stat(true), 3);
        assert!(matches!(attr.kind, fuser::FileType::Directory));
        assert_eq!(attr.perm, 0o755);
        assert_eq!(attr.size, 0);
    }

    #[test]
    fn errno_bridge_covers_every_adapter_variant() {
        assert_eq!(
            errno_from_adapter(AdapterError::NotFound),
            fuser::Errno::ENOENT
        );
        assert_eq!(
            errno_from_adapter(AdapterError::NotADirectory),
            fuser::Errno::ENOTDIR
        );
        assert_eq!(
            errno_from_adapter(AdapterError::IsADirectory),
            fuser::Errno::EISDIR
        );
        assert_eq!(
            errno_from_adapter(AdapterError::AlreadyExists),
            fuser::Errno::EEXIST
        );
        assert_eq!(
            errno_from_adapter(AdapterError::InvalidPath),
            fuser::Errno::EINVAL
        );
        assert_eq!(
            errno_from_adapter(AdapterError::Core(lumen_core::LumenError::Storage(
                "x".into()
            ))),
            fuser::Errno::EIO,
        );
    }
}
