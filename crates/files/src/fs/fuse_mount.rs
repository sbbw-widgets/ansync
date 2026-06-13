//! `fuser::Filesystem` impl that pumps FUSE callbacks through the
//! metadata cache and the [`FsClient`] RPC layer.
//!
//! Mount layout per device: `$XDG_RUNTIME_DIR/ansync/mounts/{device-name}/`.
//! The kernel hands us inode numbers; we translate to/from string paths
//! via [`InodeTable`]. Inode 1 is always root (the FUSE contract).
//!
//! Callbacks are sync; we bridge to async by holding a `tokio::runtime::Handle`
//! and `block_on`ing each RPC. fuser runs each callback on its own
//! kernel thread, so blocking is safe.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ansync_proto::FsMeta;
use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use tokio::runtime::Handle;
use tracing::{debug, warn};

use crate::fs::cache::{CachedEntry, MetadataCache};
use crate::fs::client::{FsClient, FsClientError};

/// FUSE attribute TTL handed to the kernel on every reply. Matches
/// the cache's stat TTL — there's no point telling the kernel a
/// longer lifetime than we'd serve internally.
const ATTR_TTL: Duration = Duration::from_secs(5);

/// Block size we advertise in `getattr`. FUSE rounds up readahead to
/// this; 256 KiB matches our chunk size in [`crate::transfer`] and
/// the "small readahead" recommendation from PLAN.md.
const BLOCK_SIZE: u32 = 256 * 1024;

pub struct FuseMount<S: ansync_transport::Stream + 'static> {
    client: FsClient<S>,
    cache: Arc<MetadataCache>,
    runtime: Handle,
    inodes: Arc<Mutex<InodeTable>>,
}

impl<S: ansync_transport::Stream + 'static> FuseMount<S> {
    pub fn new(client: FsClient<S>, runtime: Handle) -> Self {
        Self {
            client,
            cache: Arc::new(MetadataCache::with_default_ttl()),
            runtime,
            inodes: Arc::new(Mutex::new(InodeTable::new())),
        }
    }

    /// Mount on `mountpoint` and run until the kernel unmounts (e.g.
    /// `fusermount -u`) or the returned background session handle is
    /// dropped. The handle is `Send` so callers can `spawn_blocking`
    /// the mount thread without losing teardown control.
    pub fn spawn(self, mountpoint: &Path) -> std::io::Result<fuser::BackgroundSession> {
        let options = vec![
            MountOption::FSName("ansync".to_string()),
            MountOption::AutoUnmount,
            MountOption::AllowOther,
            MountOption::DefaultPermissions,
        ];
        fuser::spawn_mount2(self, mountpoint, &options)
    }

    fn lookup_path(&self, parent: u64, name: &OsStr) -> Option<PathBuf> {
        let table = self.inodes.lock().ok()?;
        let parent_path = table.path_of(parent)?;
        let child = parent_path.join(name);
        Some(child)
    }

    fn intern(&self, path: PathBuf, is_dir: bool) -> u64 {
        let mut table = self.inodes.lock().expect("inode table poisoned");
        table.intern(path, is_dir)
    }

    fn rpc_stat(&self, path: &Path) -> Result<FsMeta, FsClientError> {
        if let Some(CachedEntry::Stat(meta)) = self.cache.get_stat(path) {
            return Ok(meta);
        }
        if let Some(CachedEntry::NotFound) = self.cache.get_stat(path) {
            return Err(FsClientError::Remote {
                code: libc::ENOENT,
                message: "negative-cache".into(),
            });
        }
        let path_str = path.to_string_lossy().to_string();
        let meta_result = self
            .runtime
            .block_on(self.client.stat(&path_str));
        match &meta_result {
            Ok(m) => self.cache.put_stat(path.to_path_buf(), m.clone()),
            Err(FsClientError::Remote { code, .. }) if *code == libc::ENOENT => {
                self.cache.put_not_found(path.to_path_buf());
            }
            _ => {}
        }
        meta_result
    }
}

fn meta_to_attr(ino: u64, meta: &FsMeta) -> FileAttr {
    let mtime = SystemTime::UNIX_EPOCH + Duration::from_secs(meta.mtime);
    FileAttr {
        ino,
        size: meta.size,
        blocks: (meta.size + 511) / 512,
        atime: mtime,
        mtime,
        ctime: mtime,
        crtime: mtime,
        kind: if meta.is_dir { FileType::Directory } else { FileType::RegularFile },
        perm: (meta.mode & 0o7777) as u16,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: BLOCK_SIZE,
        flags: 0,
    }
}

fn errno_of(e: &FsClientError) -> i32 {
    match e {
        FsClientError::Remote { code, .. } => *code,
        _ => libc::EIO,
    }
}

impl<S: ansync_transport::Stream + 'static> Filesystem for FuseMount<S> {
    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let Some(path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };
        match self.rpc_stat(&path) {
            Ok(meta) => {
                let ino = self.intern(path, meta.is_dir);
                let attr = meta_to_attr(ino, &meta);
                reply.entry(&ATTR_TTL, &attr, 0);
            }
            Err(e) => reply.error(errno_of(&e)),
        }
    }

    fn getattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: Option<u64>,
        reply: ReplyAttr,
    ) {
        let path = match self.inodes.lock() {
            Ok(t) => t.path_of(ino).cloned(),
            Err(_) => None,
        };
        let path = match path {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        if ino == 1 {
            // Root: synthesize a stable attr without round-tripping
            // every time the kernel asks for it.
            let meta = FsMeta {
                size: 0,
                mode: 0o755,
                mtime: now_secs(),
                is_dir: true,
            };
            reply.attr(&ATTR_TTL, &meta_to_attr(1, &meta));
            return;
        }
        match self.rpc_stat(&path) {
            Ok(meta) => reply.attr(&ATTR_TTL, &meta_to_attr(ino, &meta)),
            Err(e) => reply.error(errno_of(&e)),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let path = match self.inodes.lock().ok().and_then(|t| t.path_of(ino).cloned()) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        let path_str = path.to_string_lossy().to_string();
        let entries = if let Some(cached) = self.cache.get_readdir(&path) {
            cached
        } else {
            match self.runtime.block_on(self.client.readdir(&path_str)) {
                Ok(v) => {
                    self.cache.put_readdir(path.clone(), v.clone());
                    v
                }
                Err(e) => {
                    reply.error(errno_of(&e));
                    return;
                }
            }
        };
        let idx = offset as usize;
        // FUSE-required "." and ".." come first.
        let synthetic = [
            (ino, FileType::Directory, ".".to_string()),
            (1, FileType::Directory, "..".to_string()),
        ];
        for (i, (child_ino, kind, name)) in synthetic.iter().enumerate() {
            if idx > i {
                continue;
            }
            if reply.add(*child_ino, (i + 1) as i64, *kind, name) {
                reply.ok();
                return;
            }
        }
        // Real entries start at idx = 2.
        let start = idx.saturating_sub(2);
        for (i, entry) in entries.iter().enumerate().skip(start) {
            let child_path = path.join(&entry.name);
            let child_ino = self.intern(child_path, entry.meta.is_dir);
            let kind = if entry.meta.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            if reply.add(child_ino, (i + 3) as i64, kind, &entry.name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let path = match self.inodes.lock().ok().and_then(|t| t.path_of(ino).cloned()) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        let path_str = path.to_string_lossy().to_string();
        match self.runtime.block_on(self.client.open(&path_str, flags as u32)) {
            Ok(handle) => reply.opened(handle, 0),
            Err(e) => reply.error(errno_of(&e)),
        }
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        match self.runtime.block_on(self.client.read(fh, offset as u64, size)) {
            Ok(bytes) => reply.data(&bytes),
            Err(e) => reply.error(errno_of(&e)),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let owned = data.to_vec();
        match self
            .runtime
            .block_on(self.client.write(fh, offset as u64, owned))
        {
            Ok(written) => {
                if let Some(path) = self.inodes.lock().ok().and_then(|t| t.path_of(ino).cloned()) {
                    self.cache.invalidate(&path);
                }
                reply.written(written);
            }
            Err(e) => reply.error(errno_of(&e)),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.runtime.block_on(self.client.close(fh)) {
            Ok(()) => reply.ok(),
            Err(e) => {
                debug!(handle = fh, error = %e, "close failed");
                reply.error(errno_of(&e));
            }
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let Some(path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };
        let path_str = path.to_string_lossy().to_string();
        match self.runtime.block_on(self.client.create(&path_str, mode)) {
            Ok(handle) => {
                self.cache.invalidate(&path);
                let meta = FsMeta {
                    size: 0,
                    mode,
                    mtime: now_secs(),
                    is_dir: false,
                };
                let ino = self.intern(path, false);
                let attr = meta_to_attr(ino, &meta);
                reply.created(&ATTR_TTL, &attr, 0, handle, 0);
            }
            Err(e) => reply.error(errno_of(&e)),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let Some(path) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };
        let path_str = path.to_string_lossy().to_string();
        match self.runtime.block_on(self.client.unlink(&path_str)) {
            Ok(()) => {
                self.cache.invalidate(&path);
                reply.ok();
            }
            Err(e) => reply.error(errno_of(&e)),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        let Some(from) = self.lookup_path(parent, name) else {
            reply.error(libc::ENOENT);
            return;
        };
        let Some(to) = self.lookup_path(new_parent, new_name) else {
            reply.error(libc::ENOENT);
            return;
        };
        let from_s = from.to_string_lossy().to_string();
        let to_s = to.to_string_lossy().to_string();
        match self.runtime.block_on(self.client.rename(&from_s, &to_s)) {
            Ok(()) => {
                self.cache.invalidate(&from);
                self.cache.invalidate(&to);
                reply.ok();
            }
            Err(e) => reply.error(errno_of(&e)),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let path = match self.inodes.lock().ok().and_then(|t| t.path_of(ino).cloned()) {
            Some(p) => p,
            None => {
                reply.error(libc::ENOENT);
                return;
            }
        };
        let path_str = path.to_string_lossy().to_string();
        if let Some(new_size) = size {
            if let Err(e) = self.runtime.block_on(self.client.truncate(&path_str, new_size)) {
                reply.error(errno_of(&e));
                return;
            }
            self.cache.invalidate(&path);
        }
        if let Some(new_mode) = mode {
            if let Err(e) = self.runtime.block_on(self.client.chmod(&path_str, new_mode)) {
                reply.error(errno_of(&e));
                return;
            }
            self.cache.invalidate(&path);
        }
        // Re-stat to return the post-mutation attrs.
        match self.rpc_stat(&path) {
            Ok(meta) => reply.attr(&ATTR_TTL, &meta_to_attr(ino, &meta)),
            Err(e) => {
                warn!(error = %e, "setattr follow-up stat failed");
                reply.error(errno_of(&e));
            }
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Two-way inode↔path table. Root path is `""` (empty PathBuf) and
/// is implicit at inode 1 — never insert it.
struct InodeTable {
    next: u64,
    by_path: HashMap<PathBuf, u64>,
    by_ino: HashMap<u64, PathBuf>,
}

impl InodeTable {
    fn new() -> Self {
        let mut by_ino = HashMap::new();
        by_ino.insert(1, PathBuf::from("/"));
        let mut by_path = HashMap::new();
        by_path.insert(PathBuf::from("/"), 1);
        Self {
            next: 2,
            by_path,
            by_ino,
        }
    }

    fn intern(&mut self, path: PathBuf, _is_dir: bool) -> u64 {
        if let Some(ino) = self.by_path.get(&path) {
            return *ino;
        }
        let ino = self.next;
        self.next += 1;
        self.by_path.insert(path.clone(), ino);
        self.by_ino.insert(ino, path);
        ino
    }

    fn path_of(&self, ino: u64) -> Option<&PathBuf> {
        self.by_ino.get(&ino)
    }
}
