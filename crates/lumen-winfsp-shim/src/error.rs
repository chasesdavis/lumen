//! Translation between Lumen Stat and WinFsp FileInfo plus the NTSTATUS
//! bridge for AdapterError. Kept in its own module so unit tests can
//! exercise pure translation without any WinFsp session machinery,
//! mirroring the attr.rs split in lumen-fuse-shim.

#![cfg(windows)]

use ::winfsp::filesystem::FileInfo;
use ::winfsp::FspError;
use lumen_fs_adapter::{AdapterError, Stat};
use windows::Win32::Foundation::{
    STATUS_ACCESS_DENIED, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_OBJECT_PATH_NOT_FOUND,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY};

/// Lumen timestamps are chrono UTC DateTime; FileInfo carries Windows
/// filetime (100ns intervals since 1601-01-01). The epoch delta is
/// 11644473600 seconds.
const WINDOWS_EPOCH_DELTA_SECS: i64 = 11_644_473_600;

/// Convert a Lumen Stat into a WinFsp FileInfo struct. Attributes use
/// FILE_ATTRIBUTE_DIRECTORY for directories and FILE_ATTRIBUTE_ARCHIVE
/// for regular files; index_number is derived from a stable hash of the
/// item UUID so handles stay stable across path renames.
pub fn stat_to_file_info(stat: &Stat) -> FileInfo {
    let mtime_ft = to_filetime(stat.modified_at);
    let mut size = if stat.is_dir { 0 } else { stat.size };
    if stat.is_dir {
        size = 0;
    }
    FileInfo {
        file_attributes: if stat.is_dir {
            FILE_ATTRIBUTE_DIRECTORY.0
        } else {
            FILE_ATTRIBUTE_ARCHIVE.0
        },
        reparse_tag: 0,
        allocation_size: 4096 * ((size + 4095) / 4096),
        file_size: size,
        creation_time: mtime_ft,
        last_access_time: mtime_ft,
        last_write_time: mtime_ft,
        change_time: mtime_ft,
        index_number: fnv_index(stat.item_id.as_bytes()),
        hard_links: 0,
        ea_size: 0,
    }
}

fn to_filetime(t: chrono::DateTime<chrono::Utc>) -> u64 {
    let secs = t.timestamp() + WINDOWS_EPOCH_DELTA_SECS;
    (secs as i64 * 10_000_000).max(0) as u64
}

/// FNV-1a over the item UUID bytes for a stable per-item index number.
/// Collisions are harmless here - index_number is informational only.
fn fnv_index(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 14695981039346656037;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(1099511628211);
    }
    hash
}

/// Map an AdapterError onto a WinFsp NTSTATUS error.
pub fn status_from_adapter(e: AdapterError) -> FspError {
    match e {
        AdapterError::NotFound => FspError::NTSTATUS(STATUS_OBJECT_NAME_NOT_FOUND.0),
        AdapterError::NotADirectory => FspError::NTSTATUS(STATUS_OBJECT_PATH_NOT_FOUND.0),
        AdapterError::IsADirectory => FspError::NTSTATUS(STATUS_OBJECT_NAME_COLLISION.0),
        AdapterError::AlreadyExists => FspError::NTSTATUS(STATUS_OBJECT_NAME_COLLISION.0),
        AdapterError::InvalidPath => FspError::NTSTATUS(STATUS_ACCESS_DENIED.0),
        AdapterError::Core(_) => FspError::NTSTATUS(STATUS_ACCESS_DENIED.0),
    }
}
