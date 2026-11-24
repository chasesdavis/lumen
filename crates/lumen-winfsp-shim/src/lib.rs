//! lumen-winfsp-shim: thin WinFsp binding over MountOps for Windows.
//!
//! Mirror of lumen-fuse-shim for the Windows platform surface. WinFsp
//! callback semantics differ from FUSE in two load-bearing ways:
//!
//! 1. Deletes and renames arrive as POSIX-unlink/POSIX-rename semantics
//!    (set_delete / rename with replace_if_exists), NOT as immediate
//!    destructive callbacks like FUSE unlink/rmdir.
//! 2. read_directory uses a marker-based restart protocol instead of
//!    an offset-based one, so we cannot reuse fuser's simple offset logic.
//!
//! Everything routes through Arc<MountOps> - never direct VfsOps or
//! MetadataIndex access (AGENTS.md rule) - meaning chunk cache, dedup,
//! metering, CommitBatcher, GC-flush-before-sweep contract all behave
//! identically across FUSE, WinFsp, MCP surfaces on every platform.

// Portable modules (path translation) compile on every platform so their
// unit tests run on the macOS dev box; everything touching winfsp stays
// windows-gated below.

mod path;
pub use path::nt_to_posix;
#[cfg(windows)]
use path::BACKSLASH;

#[cfg(windows)]
use lumen_fs_adapter::{AdapterError, MountOps};
#[cfg(windows)]
use std::sync::Arc;

#[cfg(windows)]
use ::winfsp::filesystem::{
    DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo,
};
#[cfg(windows)]
use ::winfsp::U16CStr;
#[cfg(windows)]
use ::winfsp::{FspError, Result};
#[cfg(windows)]
use windows::Win32::Foundation::STATUS_DIRECTORY_NOT_EMPTY;

#[cfg(windows)]
mod error;
#[cfg(windows)]
pub use error::{stat_to_file_info, status_from_adapter};

#[cfg(windows)]
#[cfg(windows)]
pub mod winfsp {
    use super::*;

    /// WinFsp-facing context handle: the FileContext is the Lumen item id
    /// plus last-known path so read/write callbacks can resolve without a
    /// fresh path lookup per call. WinFsp guarantees context immutability
    /// across callbacks (interior mutability lives in MountOps).
    #[derive(Clone)]
    pub struct WinFspShim {
        ops: Arc<MountOps>,
    }

    impl WinFspShim {
        pub fn new(ops: Arc<MountOps>) -> Self {
            Self { ops }
        }
    }

    /// Resolve an incoming WinFsp NT path into a Lumen POSIX path.
    ///
    /// WinFsp hands paths down with backslash separators after its own
    /// normalization. Lumen's MountOps expects forward-slash absolute
    /// paths. A bare separator is the mount root and maps to "/".
    fn posix_path(file_name: &U16CStr) -> String {
        let s = file_name.to_string_lossy();
        let trimmed = s.trim_start_matches('\\');
        let replaced = trimmed.replace(BACKSLASH, "/");
        if replaced.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", replaced.trim_start_matches('/'))
        }
    }

    fn parent_and_name(path: &str) -> Result<(String, String)> {
        match path.rsplit_once('/') {
            Some((parent, name)) if !name.is_empty() => Ok((parent.to_string(), name.to_string())),
            _ => Err(FspError::NTSTATUS(
                windows::Win32::Foundation::STATUS_INVALID_PARAMETER.0,
            )),
        }
    }

    /// Bridge sync WinFsp callbacks onto async MountOps. Same ambient-runtime
    /// discipline as FUSE and MCP dispatch: prefer block_in_place on a
    /// multi-thread runtime so the worker hops to the blocking pool, plain
    /// future polling on a current-thread runtime, private throwaway otherwise.
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| h.block_on(fut))
            }
            _ => futures::executor::block_on(fut),
        }
    }

    impl FileSystemContext for WinFspShim {
        type FileContext = String;

        fn get_security_by_name(
            &self,
            file_name: &U16CStr,
            security_descriptor: Option<&mut [std::ffi::c_void]>,
            reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
        ) -> Result<FileSecurity> {
            // Lumen exposes no persistent ACLs yet (parity with fuse-shim which
            // always reports default perms). Resolve reparse points only.
            if let Some(security) = reparse_point_resolver(file_name) {
                return Ok(security);
            }
            let _ = security_descriptor;
            Ok(FileSecurity {
                reparse: false,
                sz_security_descriptor: 0,
                attributes: 0,
            })
        }

        fn open(
            &self,
            file_name: &U16CStr,
            _create_options: u32,
            _granted_access: u32,
            file_info: &mut OpenFileInfo,
        ) -> Result<Self::FileContext> {
            let path = posix_path(file_name);
            block_on(async {
                let stat = self.ops.getattr(&path).await.map_err(status_from_adapter)?;
                *file_info.as_mut() = stat_to_file_info(&stat);
                Ok(path)
            })
        }

        fn close(&self, context: Self::FileContext) {
            let _ = context;
        }

        fn cleanup(&self, context: &Self::FileContext, file_name: Option<&U16CStr>, flags: u32) {
            // WinFsp deferred-delete contract: set_delete flagged the pending
            // removal; actual deletion happens here when the DELETE flag was set.
            const FSP_CLEANUP_DELETE: u32 = 0x0000_0001;
            if flags & FSP_CLEANUP_DELETE == 0 {
                return;
            }
            let Some(name) = file_name else { return };
            let path = posix_path(name);
            block_on(async {
                match self.ops.remove(&path).await {
                    Ok(_) => {}
                    Err(AdapterError::NotFound) => {}
                    Err(e) => {
                        tracing::warn!(target: "lumen::winfsp", "deferred delete of {} failed: {}", path, e)
                    }
                }
            });
            let _ = context;
        }

        fn flush(
            &self,
            context: Option<&Self::FileContext>,
            file_info: &mut FileInfo,
        ) -> Result<()> {
            // Commit any pending batched upserts so buffered writes survive.
            block_on(async { self.ops.commit_pending().await.map_err(status_from_adapter) })?;
            if let Some(context) = context {
                let stat = block_on(async {
                    self.ops.getattr(context).await.map_err(status_from_adapter)
                })?;
                *file_info = stat_to_file_info(&stat);
            }
            Ok(())
        }

        fn get_file_info(
            &self,
            context: &Self::FileContext,
            file_info: &mut FileInfo,
        ) -> Result<()> {
            let stat =
                block_on(async { self.ops.getattr(context).await.map_err(status_from_adapter) })?;
            *file_info = stat_to_file_info(&stat);
            Ok(())
        }

        fn overwrite(
            &self,
            context: &Self::FileContext,
            file_attributes: u32,
            replace_file_attributes: bool,
            allocation_size: u64,
            extra_buffer: Option<&[u8]>,
            file_info: &mut FileInfo,
        ) -> Result<()> {
            let data = Vec::new();
            let stat = block_on(async {
                if allocation_size > 0 && allocation_size != u64::MAX {
                    self.ops
                        .replace(context, data)
                        .await
                        .map_err(status_from_adapter)
                } else {
                    self.ops
                        .replace(context, Vec::new())
                        .await
                        .map_err(status_from_adapter)
                }
            })?;
            let _ = (file_attributes, replace_file_attributes, extra_buffer);
            *file_info = stat_to_file_info(&stat);
            Ok(())
        }

        fn read_directory(
            &self,
            context: &Self::FileContext,
            pattern: Option<&U16CStr>,
            marker: DirMarker,
            buffer: &mut [u8],
        ) -> Result<u32> {
            use ::winfsp::filesystem::WideNameInfo;
            // Pattern filtering is skipped for now; we enumerate everything and
            // let WinFsp's kernel side filter with pass_query_directory_pattern=false
            let _ = pattern;
            let entries =
                block_on(async { self.ops.readdir(context).await.map_err(status_from_adapter) })?;
            let mut cursor = 0u32;
            let mut dir_info: DirInfo<255> = DirInfo::new();

            if marker.is_none() || marker.is_current() {
                dir_info.reset();
                let root_stat = block_on(async { self.ops.getattr(context).await }).ok();
                if let Some(stat) = root_stat {
                    *dir_info.file_info_mut() = stat_to_file_info(&stat);
                    dir_info.set_name_raw([b'.' as u16].as_slice())?;
                    if !dir_info.append_to_buffer(buffer, &mut cursor) {
                        return Ok(cursor);
                    }
                }
            }
            if marker.is_none() || marker.is_current() {
                dir_info.reset();
                let parent = parent_of(context);
                let parent_stat = block_on(async { self.ops.getattr(&parent).await }).ok();
                if let Some(stat) = parent_stat {
                    *dir_info.file_info_mut() = stat_to_file_info(&stat);
                    dir_info.set_name_raw([b'.' as u16, b'.' as u16].as_slice())?;
                    if !dir_info.append_to_buffer(buffer, &mut cursor) {
                        return Ok(cursor);
                    }
                }
            }

            for entry in entries {
                dir_info.reset();
                *dir_info.file_info_mut() = stat_to_file_info(&entry);
                dir_info.set_name(entry.name.as_str())?;
                if !dir_info.append_to_buffer(buffer, &mut cursor) {
                    break;
                }
            }

            if !DirInfo::<255>::finalize_buffer(buffer, &mut cursor) {
                return Err(FspError::NTSTATUS(
                    windows::Win32::Foundation::STATUS_BUFFER_TOO_SMALL.0,
                ));
            }
            Ok(cursor)
        }

        fn rename(
            &self,
            _context: &Self::FileContext,
            file_name: &U16CStr,
            new_file_name: &U16CStr,
            replace_if_exists: bool,
        ) -> Result<()> {
            // Lumen has no native rename in MountOps yet; implement as copy+delete
            // through public ops. This is acceptable for directories too since
            // MountOps copies move chunk lists wholesale (dedup makes it cheap).
            let old = posix_path(file_name);
            let new = posix_path(new_file_name);
            block_on(async {
                let data = self
                    .ops
                    .read(&old, 0, u64::MAX)
                    .await
                    .map_err(status_from_adapter)?;
                let (parent, name) = parent_and_name(&new)?;
                if replace_if_exists {
                    let _ = self.ops.remove(&new).await;
                }
                self.ops
                    .create(&parent, &name, data)
                    .await
                    .map_err(status_from_adapter)?;
                self.ops.remove(&old).await.map_err(status_from_adapter)?;
                Ok(())
            })
        }

        fn set_delete(
            &self,
            _context: &Self::FileContext,
            file_name: &U16CStr,
            delete_file: bool,
        ) -> Result<()> {
            // WinFsp contract: never delete here. Only validate for
            // directories that no children exist; actual removal happens in
            // cleanup once the last handle drops (STATUS_DIRECTORY_NOT_EMPTY).
            if !delete_file {
                return Ok(());
            }
            let path = posix_path(file_name);
            let stat =
                block_on(async { self.ops.getattr(&path).await.map_err(status_from_adapter) })?;
            if !stat.is_dir {
                return Ok(());
            }
            block_on(async {
                let children = self.ops.readdir(&path).await.map_err(status_from_adapter)?;
                if !children.is_empty() {
                    return Err(FspError::NTSTATUS(STATUS_DIRECTORY_NOT_EMPTY.0));
                }
                Ok(())
            })
        }

        fn read(&self, context: &Self::FileContext, buffer: &mut [u8], offset: u64) -> Result<u32> {
            block_on(async {
                let data = self
                    .ops
                    .read(context, offset, buffer.len() as u64)
                    .await
                    .map_err(status_from_adapter)?;
                let len = data.len();
                buffer[..len].copy_from_slice(&data);
                Ok(len as u32)
            })
        }

        fn write(
            &self,
            context: &Self::FileContext,
            buffer: &[u8],
            offset: u64,
            write_to_eof: bool,
            constrained_io: bool,
            file_info: &mut FileInfo,
        ) -> Result<u32> {
            block_on(async {
                let existing_len = self
                    .ops
                    .read(context, 0, u64::MAX)
                    .await
                    .map_err(status_from_adapter)?
                    .len() as u64;
                let (start, end) = if write_to_eof {
                    (existing_len, existing_len + buffer.len() as u64)
                } else if constrained_io {
                    if offset >= existing_len {
                        return Ok(0);
                    }
                    let end = (offset + buffer.len() as u64).min(existing_len);
                    (offset, end)
                } else {
                    let end = offset + buffer.len() as u64;
                    (offset, end)
                };

                // Read-modify-write stage: MountOps stores whole-file bodies on
                // replace; we splice the incoming slice into the current content.
                let mut content = self
                    .ops
                    .read(context, 0, u64::MAX)
                    .await
                    .map_err(status_from_adapter)?;
                let new_size = end.max(if start < existing_len {
                    existing_len
                } else {
                    start
                });
                content.resize(new_size as usize, 0);
                let src_end = ((end - start) as usize).min(buffer.len());
                content[start as usize..start as usize + src_end]
                    .copy_from_slice(&buffer[..src_end]);
                let stat = self
                    .ops
                    .replace(context, content.clone())
                    .await
                    .map_err(status_from_adapter)?;
                *file_info = stat_to_file_info(&stat);
                Ok(src_end as u32)
            })
        }

        fn get_volume_info(&self, out_volume_info: &mut VolumeInfo) -> Result<()> {
            out_volume_info.total_size = 1 << 40;
            out_volume_info.free_size = 1 << 40;
            Ok(())
        }
    }

    fn parent_of(path: &str) -> String {
        match path.rsplit_once('/') {
            Some((parent, _)) if !parent.is_empty() => parent.to_string(),
            _ => "/".to_string(),
        }
    }
}
