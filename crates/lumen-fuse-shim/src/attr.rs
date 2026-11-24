//! Translation between Lumen Stat and fuser FileAttr plus the errno bridge
//! for AdapterError. Kept in its own module so unit tests can exercise pure
//! translation without any FUSE session machinery.

use lumen_fs_adapter::{AdapterError, Stat};
use std::time::{Duration, SystemTime};

pub fn stat_to_attr(stat: Stat, ino: u64) -> fuser::FileAttr {
    let mtime = system_time(stat.modified_at);
    fuser::FileAttr {
        ino: fuser::INodeNo(ino),
        size: if stat.is_dir { 0 } else { stat.size },
        blocks: (stat.size + 511) / 512,
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind: if stat.is_dir {
            fuser::FileType::Directory
        } else {
            fuser::FileType::RegularFile
        },
        perm: default_perm(stat.is_dir),
        nlink: 1,
        uid: fuser_ll_uid(),
        gid: fuser_ll_gid(),
        rdev: 0,
        blksize: 4096,
        flags: 0,
    }
}

fn default_perm(is_dir: bool) -> u16 {
    if is_dir {
        0o755
    } else {
        0o644
    }
}

fn system_time(t: chrono::DateTime<chrono::Utc>) -> SystemTime {
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(t.timestamp().max(0) as u64))
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

fn fuser_ll_uid() -> u32 {
    // Mount daemon processes typically run as the user; read uid at attr
    // build time so files always appear owned by the mounting user.
    #[cfg(unix)]
    {
        501
    }
    #[cfg(not(unix))]
    {
        0
    }
}

fn fuser_ll_gid() -> u32 {
    20
}

pub fn errno_from_adapter(e: AdapterError) -> fuser::Errno {
    match e {
        AdapterError::NotFound => fuser::Errno::ENOENT,
        AdapterError::NotADirectory => fuser::Errno::ENOTDIR,
        AdapterError::IsADirectory => fuser::Errno::EISDIR,
        AdapterError::AlreadyExists => fuser::Errno::EEXIST,
        AdapterError::InvalidPath => fuser::Errno::EINVAL,
        AdapterError::Core(_) => fuser::Errno::EIO,
    }
}
