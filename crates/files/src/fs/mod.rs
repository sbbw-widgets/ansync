//! Remote filesystem RPC + FUSE mount glue.
//!
//! Three layers:
//!
//! - [`client::FsClient`] — sequential async RPC over a single QUIC
//!   `StreamKind::Fs` stream. Owns the wire send/recv loop.
//! - [`cache::MetadataCache`] — TTL caches for `stat` / `readdir`
//!   results + a 1 s negative cache for `NotFound`. Eviction is
//!   triggered on writes by the FUSE layer.
//! - [`fuse_mount::FuseMount`] — `fuser::Filesystem` impl that pumps
//!   FUSE callbacks through the cache and into the client. Mount
//!   point lives at `$XDG_RUNTIME_DIR/ansync/mounts/{device-name}/`.
//!
//! Content of files is NOT cached at this layer: the kernel page
//! cache + a small per-handle readahead window (256 KiB) absorbs the
//! common sequential-read patterns. Duplicating bytes here would
//! waste RAM without measurable hit-rate.

pub mod cache;
pub mod client;

#[cfg(feature = "fuse")]
pub mod fuse_mount;

pub use cache::{CachedEntry, MetadataCache};
pub use client::{FsClient, FsClientError};

/// Re-export so downstream crates can hold the FUSE session handle
/// without taking a direct `fuser` dep.
#[cfg(feature = "fuse")]
pub use fuser::BackgroundSession;
