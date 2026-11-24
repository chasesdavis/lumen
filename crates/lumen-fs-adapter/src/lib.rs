//! lumen-fs-adapter: the platform-independent mount core.
//!
//! Everything a FUSE/WinFsp kernel driver needs except the syscall
//! binding itself: path resolution, directory enumeration, read/write
//! dispatch into VfsOps, and attribute filling. The platform crates
//! (macFUSE/FUSE-T on macOS, libfuse3 on Linux, WinFsp on Windows) stay
//! thin callback shims over these operations - per-plan max ~500 LOC each
//! - so platform edge cases never leak back into the shared layer.

pub mod ops;

pub use ops::{AdapterError, MountOps, Stat};
