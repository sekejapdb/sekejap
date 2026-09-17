//! The selected V2 release storage path for typed collections: `PageWalStore`
//! (`E4PWAL02` frames, two checkpoint metadata copies, committed-WAL overlay,
//! publication hint, reader slot locks). The inherited kernel `Store` is not
//! a collection backend and nothing routes to it. This module maps the public
//! `Database` configuration and `ResourceLimits` onto what the page-WAL
//! actually enforces and refuses, explicitly, everything it cannot honour.
use crate::pagewal::{PageWalStore, HINT_FILE_BYTES, READER_SLOTS};
use kernel::{
    btree::{RangeIter, ReverseRangeIter},
    io::IoMode,
    limits::ResourceLimits,
    store::{Config, SyncMode},
    Error, Result,
};
use std::path::Path;

/// The page-WAL bounds every transaction and its lookup index to 16 MiB.
pub const WAL_BOUND: u64 = 16 << 20;
/// Smallest pool the B-tree can descend and split with.
pub const MIN_CACHE: usize = 64 << 10;

/// Map a public `Config` onto the page-WAL path, or say why it cannot be.
/// Only buffered I/O and a FULL barrier at publication exist on this path;
/// a weaker request is refused rather than silently upgraded, so every run
/// is benchmarked as what it is.
pub fn check_config(cfg: &Config) -> std::result::Result<usize, String> {
    if !matches!(cfg.io, IoMode::Buffered) {
        return Err("page-WAL collections support IoMode::Buffered only".into());
    }
    if !matches!(cfg.sync, SyncMode::Full) {
        return Err(
            "page-WAL collections publish every commit with a FULL barrier; request SyncMode::Full"
                .into(),
        );
    }
    if cfg.budget_bytes < MIN_CACHE {
        return Err(format!("cache budget below {MIN_CACHE} bytes"));
    }
    Ok(cfg.budget_bytes)
}

/// Accept a policy only when each field is enforced on this path:
/// `data_bytes`/`wal_bytes` as pre-write allowances (their sum is also the
/// persisted page-WAL cap), `tracked_pages` at every distinct addition to the
/// WAL index between checkpoints, `record_bytes` before mutation, `readers`
/// by slot index, `recovery_bytes` by never being written into. Refused
/// explicitly, before any directory exists: a WAL above the page-WAL's
/// 16 MiB bound and more readers than the eight slots this release ships.
///
/// Quota accounting (`ResourceLimits::total_bytes`): the page-WAL keeps no
/// freelist file, so the policy's `2 * freelist_bytes` (>= 104 bytes)
/// allowance is spent on the 96 logical bytes of `readers.lock` (two
/// publication-hint copies); the zero-byte slot files cost 0 logical bytes
/// against `48 * readers`. Logical managed bytes are therefore
/// `data + wal + 96 <= total_bytes - recovery_bytes` for every valid policy.
/// Allocated/inode cost, outside the logical quota: one filesystem block for
/// `readers.lock` and one inode each for `writer.lock`, `readers.lock` and
/// eight slot files (no blocks) - at most the old fixed-slot footprint.
pub fn check_limits(l: ResourceLimits) -> Result<ResourceLimits> {
    let l = l.validate()?;
    if l.wal_bytes > WAL_BOUND {
        return Err(Error::ResourceLimit("page-WAL bounds wal_bytes to 16 MiB"));
    }
    if l.readers as usize > READER_SLOTS {
        return Err(Error::ResourceLimit("page-WAL bounds readers to eight slots"));
    }
    debug_assert!(2 * l.freelist_bytes() >= HINT_FILE_BYTES);
    Ok(l)
}

pub struct Backend {
    store: PageWalStore,
    #[cfg(test)]
    write_fault: std::cell::Cell<Option<usize>>,
}

impl Backend {
    fn wrap(store: PageWalStore) -> Self {
        Self {
            store,
            #[cfg(test)]
            write_fault: std::cell::Cell::new(None),
        }
    }
    /// Create a database. With limits, the persisted cap is `data + wal` and
    /// the runtime allowances are installed before any collection byte lands.
    pub fn create(dir: &Path, cache: usize, limits: Option<ResourceLimits>) -> Result<Self> {
        let mut store = PageWalStore::open(dir, true, cache)?;
        if let Some(l) = limits {
            store.set_cap(l.data_bytes + l.wal_bytes)?;
            store.set_runtime_limits(l.data_bytes, l.wal_bytes, l.tracked_pages as usize)?;
        }
        Ok(Self::wrap(store))
    }
    /// Open a writer; `check` sees the committed state before the WAL tail is
    /// normalized or any coordination file is created, and can refuse with
    /// nothing modified.
    pub fn open(
        dir: &Path,
        cache: usize,
        check: impl FnOnce(&PageWalStore) -> Result<()>,
    ) -> Result<Self> {
        Ok(Self::wrap(PageWalStore::open_validated(dir, false, cache, check)?))
    }
    /// Admit a reader; `check` runs on the admitted view (on the quiescent
    /// path before any coordination file is created) and returns the
    /// persisted reader bound, enforced by the page-WAL on the held slot.
    pub fn open_snapshot(
        dir: &Path,
        cache: usize,
        check: impl FnOnce(&PageWalStore) -> Result<Option<usize>>,
    ) -> Result<Self> {
        Ok(Self::wrap(PageWalStore::open_snapshot_validated(dir, cache, check)?))
    }
    /// Reinstall a persisted policy's pre-write allowances on a writer.
    pub fn install_limits(&mut self, l: ResourceLimits) -> Result<()> {
        self.store
            .set_runtime_limits(l.data_bytes, l.wal_bytes, l.tracked_pages as usize)
    }
    pub fn store(&self) -> &PageWalStore {
        &self.store
    }
    pub fn dir(&self) -> &Path {
        self.store.dir()
    }
    pub fn is_snapshot(&self) -> bool {
        self.store.is_snapshot()
    }
    pub fn reader_slot(&self) -> Option<usize> {
        self.store.reader_slot()
    }
    pub fn get(&self, k: &[u8]) -> Result<Option<Vec<u8>>> {
        self.store.get(k)
    }
    pub fn range(&self, from: &[u8]) -> Result<RangeIter<'_>> {
        self.store.range(from)
    }
    /// Descending records strictly below `to`.
    pub fn range_reverse(&self, to: &[u8]) -> Result<ReverseRangeIter<'_>> {
        self.store.range_reverse(to)
    }
    pub fn put(&mut self, k: &[u8], v: &[u8]) -> Result<()> {
        self.write_fault()?;
        self.store.put(k, v)
    }
    pub fn delete(&mut self, k: &[u8]) -> Result<bool> {
        self.write_fault()?;
        self.store.delete(k)
    }
    /// Per-index tree access. The store keeps no catalog of trees: the caller
    /// (the index descriptor) names `(tree_id, root)` on every call and owns
    /// the durable copy of the root, which is why a root change must be saved
    /// into the same transaction that produced it.
    pub fn tree_create(&mut self, tree_id: u16) -> Result<u32> {
        self.write_fault()?;
        self.store.tree_create(tree_id)
    }
    pub fn tree_get(&self, tree_id: u16, root: u32, k: &[u8]) -> Result<Option<Vec<u8>>> {
        self.store.tree_get(tree_id, root, k)
    }
    pub fn tree_range(&self, tree_id: u16, root: u32, from: &[u8]) -> Result<Option<RangeIter<'_>>> {
        self.store.tree_range(tree_id, root, from)
    }
    pub fn tree_range_reverse(
        &self,
        tree_id: u16,
        root: u32,
        to: &[u8],
    ) -> Result<Option<ReverseRangeIter<'_>>> {
        self.store.tree_range_reverse(tree_id, root, to)
    }
    pub fn tree_put(&mut self, tree_id: u16, root: u32, k: &[u8], v: &[u8]) -> Result<u32> {
        self.write_fault()?;
        self.store.tree_put(tree_id, root, k, v)
    }
    pub fn tree_delete(&mut self, tree_id: u16, root: u32, k: &[u8]) -> Result<(bool, u32)> {
        self.write_fault()?;
        self.store.tree_delete(tree_id, root, k)
    }
    pub fn tree_free_root(&mut self, tree_id: u16, root: u32) -> Result<()> {
        self.write_fault()?;
        self.store.tree_free_root(tree_id, root)
    }
    pub fn tree_pack<I>(
        &mut self,
        tree_id: u16,
        sorted: I,
        fill: f32,
        scratch: &Path,
    ) -> Result<(u32, u64)>
    where
        I: Iterator<Item = kernel::Result<(Vec<u8>, Vec<u8>, bool)>>,
    {
        self.write_fault()?;
        self.store.tree_pack(tree_id, sorted, fill, scratch)
    }
    /// Durable (FULL barrier) and published through the hint: visible to
    /// every snapshot admitted afterwards.
    pub fn commit(&mut self) -> Result<()> {
        self.store.commit()
    }
    /// Fold the committed WAL into the data file. `false` means a reader in
    /// some process holds a slot and the fold is deferred, not skipped.
    pub fn checkpoint(&mut self) -> Result<bool> {
        self.store.checkpoint()
    }
    pub fn rollback(&mut self) -> Result<()> {
        self.store.rollback()
    }
    /// Uncommitted page writes exist in this handle.
    pub fn is_dirty(&self) -> bool {
        self.store.is_dirty()
    }
    pub fn wal_bytes(&self) -> u64 {
        self.store.wal_bytes()
    }
    pub fn data_bytes(&self) -> u64 {
        self.store.data_bytes()
    }
    pub fn tracked_pages(&self) -> Option<usize> {
        self.store.tracked_pages()
    }
    /// Fail the write after `successful` further writes reach the store,
    /// before the store sees it (the boundary the inherited fault tests used).
    #[cfg(test)]
    pub fn arm_write_fault(&self, successful: usize) {
        self.write_fault.set(Some(successful));
    }
    #[cfg(test)]
    fn write_fault(&self) -> Result<()> {
        match self.write_fault.get() {
            None => Ok(()),
            Some(0) => {
                self.write_fault.set(None);
                Err(std::io::Error::other("injected write failure at storage boundary").into())
            }
            Some(n) => {
                self.write_fault.set(Some(n - 1));
                Ok(())
            }
        }
    }
    #[cfg(not(test))]
    fn write_fault(&self) -> Result<()> {
        Ok(())
    }
}
