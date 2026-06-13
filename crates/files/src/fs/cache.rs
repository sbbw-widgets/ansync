//! TTL-bounded cache for remote `stat` and `readdir` results plus a
//! short negative cache for `NotFound` paths.
//!
//! Rationale (see PLAN.md "Plan FUSE mount"):
//! - `ls dir; ls dir; ls dir` is the dominant pattern. Without a
//!   cache, every `ls` round-trips three times (stat + opendir +
//!   readdir). With a 5 s TTL the second `ls` hits zero round-trips.
//! - Negative entries (cached `NotFound`) absorb tool path-probing.
//!   1 s TTL is short enough to not surface stale "missing" results.
//!
//! Cache is `Mutex<HashMap>` keyed by absolute path. Eviction is
//! lazy: stale entries are dropped on the next access. A `purge_now`
//! helper invalidates an entry plus its parent directory after any
//! mutating op (`write`, `create`, `unlink`, `rename`, `truncate`,
//! `chmod`) so the next read sees the post-mutation state.
//!
//! No content cache. Linux kernel + FUSE userspace already cache
//! read pages; layering our own buffer here doubles RAM cost without
//! changing hit rate.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ansync_proto::{FsEntry, FsMeta};

#[derive(Debug, Clone)]
pub enum CachedEntry {
    Stat(FsMeta),
    ReadDir(Vec<FsEntry>),
    NotFound,
}

#[derive(Debug)]
struct Slot {
    entry: CachedEntry,
    deadline: Instant,
}

pub struct MetadataCache {
    inner: Mutex<HashMap<PathBuf, Slot>>,
    stat_ttl: Duration,
    readdir_ttl: Duration,
    negative_ttl: Duration,
}

impl MetadataCache {
    pub fn with_default_ttl() -> Self {
        Self::new(Duration::from_secs(5), Duration::from_secs(5), Duration::from_secs(1))
    }

    pub fn new(stat_ttl: Duration, readdir_ttl: Duration, negative_ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            stat_ttl,
            readdir_ttl,
            negative_ttl,
        }
    }

    fn get(&self, path: &Path) -> Option<CachedEntry> {
        let mut guard = self.inner.lock().ok()?;
        let slot = guard.get(path)?;
        if slot.deadline <= Instant::now() {
            guard.remove(path);
            return None;
        }
        Some(slot.entry.clone())
    }

    pub fn get_stat(&self, path: &Path) -> Option<CachedEntry> {
        match self.get(path)? {
            v @ (CachedEntry::Stat(_) | CachedEntry::NotFound) => Some(v),
            CachedEntry::ReadDir(_) => None,
        }
    }

    pub fn get_readdir(&self, path: &Path) -> Option<Vec<FsEntry>> {
        match self.get(path)? {
            CachedEntry::ReadDir(v) => Some(v),
            _ => None,
        }
    }

    pub fn put_stat(&self, path: PathBuf, meta: FsMeta) {
        self.put(path, CachedEntry::Stat(meta), self.stat_ttl);
    }

    pub fn put_readdir(&self, path: PathBuf, entries: Vec<FsEntry>) {
        self.put(path, CachedEntry::ReadDir(entries), self.readdir_ttl);
    }

    pub fn put_not_found(&self, path: PathBuf) {
        self.put(path, CachedEntry::NotFound, self.negative_ttl);
    }

    fn put(&self, path: PathBuf, entry: CachedEntry, ttl: Duration) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.insert(
                path,
                Slot {
                    entry,
                    deadline: Instant::now() + ttl,
                },
            );
        }
    }

    /// Invalidate `path` and its parent directory. Called by the FUSE
    /// layer after every mutating op.
    pub fn invalidate(&self, path: &Path) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.remove(path);
            if let Some(parent) = path.parent() {
                guard.remove(parent);
            }
        }
    }

    pub fn clear(&self) {
        if let Ok(mut guard) = self.inner.lock() {
            guard.clear();
        }
    }
}

impl Default for MetadataCache {
    fn default() -> Self {
        Self::with_default_ttl()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> FsMeta {
        FsMeta {
            size: 0,
            mode: 0o644,
            mtime: 0,
            is_dir: false,
        }
    }

    #[test]
    fn put_get_roundtrip() {
        let c = MetadataCache::with_default_ttl();
        c.put_stat(PathBuf::from("/x"), meta());
        assert!(matches!(c.get_stat(Path::new("/x")), Some(CachedEntry::Stat(_))));
    }

    #[test]
    fn negative_cache_distinct() {
        let c = MetadataCache::with_default_ttl();
        c.put_not_found(PathBuf::from("/missing"));
        assert!(matches!(c.get_stat(Path::new("/missing")), Some(CachedEntry::NotFound)));
    }

    #[test]
    fn invalidate_evicts_self_and_parent() {
        let c = MetadataCache::with_default_ttl();
        c.put_readdir(PathBuf::from("/dir"), vec![]);
        c.put_stat(PathBuf::from("/dir/file"), meta());
        c.invalidate(Path::new("/dir/file"));
        assert!(c.get_stat(Path::new("/dir/file")).is_none());
        assert!(c.get_readdir(Path::new("/dir")).is_none());
    }

    #[test]
    fn expiry_drops_stale_entries() {
        let c = MetadataCache::new(Duration::from_millis(1), Duration::from_millis(1), Duration::from_millis(1));
        c.put_stat(PathBuf::from("/x"), meta());
        std::thread::sleep(Duration::from_millis(5));
        assert!(c.get_stat(Path::new("/x")).is_none());
    }
}
