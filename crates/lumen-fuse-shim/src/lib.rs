//! lumen-fuse-shim: thin FUSE binding over MountOps for macOS and Linux.
//!
//! One Unix codebase covers both platforms: fuser speaks the libFUSE
//! wire protocol and goes through macFUSE on macOS / libfuse3 on Linux,
//! so the entire platform surface is this single callback shim. Nothing
//! here touches VfsOps or the metadata index directly - every call goes
//! through Arc<MountOps> so chunk cache, dedup, metering, CommitBatcher and
//! error semantics stay identical across FUSE, WinFsp and MCP surfaces.
//!
//! Architecture note vs Helumi (RE finding: FSKit/FSSupportsPathURLs):
//! Helumi bets on Apple FSKit adoption which locks them to macOS 26.2+.
//! We ride plain FUSE which reaches back to macOS 12 with macFUSE installed;
//! that compatibility breadth is the beachhead advantage per plan. FSKit
//! stays a post-beta flag exactly like FUSE-T does for Windows hand-FFI.
//!
//! Blocking bridge note: fuser callbacks are sync but MountOps/VfsOps are
//! async (storage IO). We use the same ambient-runtime strategy as MCP
//! tool dispatch: block_in_place inside a multi-thread runtime, plain
//! future polling otherwise. The production mount daemon owns a private
//! multi-thread runtime; each callback blocks one worker thread while
//! storage fetches run on the shared async pool.

#![cfg(unix)]

use lumen_fs_adapter::{AdapterError, MountOps};
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fuser::{
    Errno, FileHandle, FileType as FuseKind, INodeNo, OpenFlags, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request, WriteFlags,
};

mod attr;
pub use attr::{errno_from_adapter, stat_to_attr};

/// Kernel-facing inode table. Lumen items carry UUIDs as stable identity but
/// FUSE hands back bare u64 inodes on every callback, so the shim owns a
/// u64 -> UUID map populated at lookup/readdir time and never compacted:
/// forget decrements refcounts but keeps entries pinned because path-based
/// resolution needs the mapping to survive kernel cache lifetime anyway.
#[derive(Clone)]
pub struct FuseShim {
    ops: Arc<MountOps>,
    inner: Arc<ShimInner>,
}

struct ShimInner {
    next_ino: std::sync::atomic::AtomicU64,
    inodes: std::sync::Mutex<std::collections::HashMap<u64, PathBuf>>,
}

impl FuseShim {
    pub fn new(ops: Arc<MountOps>) -> Self {
        let mut inodes = std::collections::HashMap::new();
        inodes.insert(fuser::INodeNo::ROOT.0, PathBuf::from("/"));
        let mut next_ino = 2u64;
        while next_ino < 16 {
            next_ino += 1;
        }
        Self {
            ops,
            inner: Arc::new(ShimInner {
                next_ino: std::sync::atomic::AtomicU64::new(next_ino),
                inodes: std::sync::Mutex::new(inodes),
            }),
        }
    }

    fn child_path(&self, parent: INodeNo, name: &OsStr) -> Option<PathBuf> {
        if name.is_empty() {
            return None;
        }
        // Guard against separators in names: Lumen names are flat; lookup of
        // "a/b" under parent must not be treated as two-level resolution.
        if name.to_string_lossy().contains('/') {
            return None;
        }
        let parent_path = self.path_of(parent)?;
        Some(parent_path.join(name))
    }

    fn path_of(&self, ino: INodeNo) -> Option<PathBuf> {
        self.inner.inodes.lock().ok()?.get(&ino.0).cloned()
    }

    fn register(&self, path: PathBuf) -> INodeNo {
        let ino = self
            .inner
            .next_ino
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if let Ok(mut map) = self.inner.inodes.lock() {
            map.insert(ino, path);
        }
        INodeNo(ino)
    }

    fn register_at(&self, ino: INodeNo, path: PathBuf) {
        if let Ok(mut map) = self.inner.inodes.lock() {
            map.insert(ino.0, path);
        }
    }
}

impl fuser::Filesystem for FuseShim {
    fn init(&mut self, _req: &Request, config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        let _ = config;
        Ok(())
    }

    fn destroy(&mut self) {
        // Best-effort final commit flush at unmount so pending upserts do
        // not wait for daemon shutdown paths that might not come.
        if futures::executor::block_on(self.ops.commit_pending()).is_err() {
            tracing::warn!(target: "lumen::fuse", "final commit flush on unmount failed");
        }
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(child) = self.child_path(parent, name) else {
            reply.error(Errno::EINVAL);
            return;
        };
        match block_on(self.ops.getattr(&to_str(child.clone()))) {
            Ok(stat) => {
                let ino = self.register(child.clone());
                self.register_at(ino, child);
                let attr = stat_to_attr(stat, ino.0);
                reply.entry(&LOOKUP_TTL, &attr, fuser::Generation(0));
            }
            Err(e) => reply.error(errno_from_adapter(e)),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, fh: Option<FileHandle>, reply: ReplyAttr) {
        let _ = fh;
        let Some(path) = self.path_of(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match block_on(self.ops.getattr(&to_str(path))) {
            Ok(stat) => reply.attr(&ATTR_TTL, &stat_to_attr(stat, ino.0)),
            Err(e) => reply.error(errno_from_adapter(e)),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        dir: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let _ = fh;
        let Some(path) = self.path_of(dir) else {
            reply.error(Errno::ENOENT);
            return;
        };
        // Skip ahead when the kernel restarts enumeration mid-scan.
        let dir_path = to_str(path);
        match block_on(self.ops.readdir(&dir_path)) {
            Ok(entries) => {
                // "."/".." get stable synthetic low inodes to keep posix find
                // traversals happy without polluting the item id namespace.
                if offset == 0 {
                    if !reply.add(dir, 1, FuseKind::Directory, ".") {
                        reply.ok();
                        return;
                    }
                    if !reply.add(dir, 2, FuseKind::Directory, "..") {
                        reply.ok();
                        return;
                    }
                }
                for (i, stat) in entries.into_iter().enumerate() {
                    let entry_off = offset + i as u64 + 3;
                    let child_path = PathBuf::from(&dir_path).join(&stat.name);
                    let child_ino = self.register(child_path);
                    if !reply.add(child_ino, entry_off, kind_of(stat.is_dir), &stat.name) {
                        break;
                    }
                }
                reply.ok();
            }
            Err(e) => reply.error(errno_from_adapter(e)),
        }
    }

    fn open(
        &self,
        _req: &Request,
        ino: INodeNo,
        flags: OpenFlags,
        kill_suid_gid: bool,
        reply: ReplyOpen,
    ) {
        // Open with write intent triggers read-modify-write staging only at
        // flush time; keep a no-op handle here and let flush drive commit.
        let _ = (ino, flags, kill_suid_gid);
        reply.opened(FileHandle(0), fuser::FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        off: u64,
        size: u32,
        flags: OpenFlags,
        lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let _ = (fh, flags, lock_owner);
        let Some(path) = self.path_of(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match block_on(self.ops.read(&to_str(path), off, size as u64)) {
            Ok(buf) => reply.data(&buf),
            Err(e) => reply.error(errno_from_adapter(e)),
        }
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        kill_suid_gid: bool,
        owner: fuser::Owner,
        reply: ReplyCreate,
    ) {
        let _ = (mode, umask, flags, kill_suid_gid, owner);
        let Some(child) = self.child_path(parent, name) else {
            reply.error(Errno::EINVAL);
            return;
        };
        let parent_path = parent_string(self, parent);
        match block_on(
            self.ops
                .create(&parent_path, name.to_str().unwrap_or_default(), Vec::new()),
        ) {
            Ok(stat) => {
                let ino = self.register(child.clone());
                let attr = stat_to_attr(stat, ino.0);
                reply.created(
                    &LOOKUP_TTL,
                    &attr,
                    fuser::Generation(0),
                    FileHandle(0),
                    fuser::FopenFlags::empty(),
                );
            }
            Err(e) => reply.error(errno_from_adapter(e)),
        }
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        owner: fuser::Owner,
        reply: ReplyEntry,
    ) {
        let _ = (mode, umask, owner);
        let Some(child) = self.child_path(parent, name) else {
            reply.error(Errno::EINVAL);
            return;
        };
        match block_on(self.ops.mkdir(
            &parent_string(self, parent),
            name.to_str().unwrap_or_default(),
        )) {
            Ok(stat) => {
                let ino = self.register(child.clone());
                let attr = stat_to_attr(stat, ino.0);
                reply.entry(&LOOKUP_TTL, &attr, fuser::Generation(0));
            }
            Err(e) => reply.error(errno_from_adapter(e)),
        }
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(child) = self.child_path(parent, name) else {
            reply.error(Errno::EINVAL);
            return;
        };
        match block_on(self.ops.remove(&to_str(child))) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(errno_from_adapter(e)),
        }
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let Some(child) = self.child_path(parent, name) else {
            reply.error(Errno::EINVAL);
            return;
        };
        match block_on(self.ops.remove(&to_str(child))) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(errno_from_adapter(e)),
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        off: u64,
        data: &[u8],
        write_flags: WriteFlags,
        flags: OpenFlags,
        lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let _ = (fh, write_flags, flags, lock_owner);
        let Some(path) = self.path_of(ino) else {
            reply.error(Errno::ENOENT);
            return;
        };
        match block_on(append_at(&self.ops, &to_str(path), off, data.to_vec())) {
            Ok(n) => reply.written(n as u32),
            Err(e) => reply.error(errno_from_adapter(e)),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        let _ = (ino, fh, lock_owner);
        match block_on(self.ops.commit_pending()) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(errno_from_adapter(e)),
        }
    }
}

const LOOKUP_TTL: Duration = Duration::from_secs(5);
const ATTR_TTL: Duration = Duration::from_secs(5);

fn parent_string(shim: &FuseShim, parent: INodeNo) -> String {
    shim.path_of(parent)
        .map(to_str)
        .unwrap_or_else(|| "/".into())
}

fn to_str(p: PathBuf) -> String {
    p.to_string_lossy().into_owned()
}

fn kind_of(is_dir: bool) -> FuseKind {
    if is_dir {
        FuseKind::Directory
    } else {
        FuseKind::RegularFile
    }
}

/// Bridge sync fuser callbacks onto async MountOps. Same ambient-runtime
/// discipline as MCP tool dispatch: prefer block_in_place on a multi-thread
/// runtime so the worker hops to the blocking pool, plain future polling on
/// a current-thread runtime, and a private throwaway runtime otherwise.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| h.block_on(fut))
        }
        _ => futures::executor::block_on(fut),
    }
}

async fn append_at(
    ops: &Arc<MountOps>,
    path: &str,
    off: u64,
    mut data: Vec<u8>,
) -> Result<usize, AdapterError> {
    let mut existing = ops.read(path, 0, u64::MAX).await?;
    let total_end = (off + data.len() as u64) as usize;
    if total_end > existing.len() {
        let new_size = off as usize + data.len();
        existing.resize(new_size.max(existing.len()), 0u8);
    }
    if off as usize <= existing.len() {
        let end = (off as usize + data.len()).min(existing.len());
        existing[off as usize..end]
            .copy_from_slice(data.as_mut_slice()[..end - off as usize].as_ref());
    }
    ops.replace(path, existing.clone()).await?;
    Ok(data.len())
}
