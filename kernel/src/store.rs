//! The store ties the pool, the log and the tree together and owns the two
//! orderings that durability depends on.
//!
//! COMMIT    append -> fsync -> apply to pages -> publish
//! CHECKPOINT flush pages -> fsync file -> fsync dir -> rotate log
//!
//! Moving the rotation earlier changes nothing observable until the machine loses
//! power, which is why it has an explicit test rather than a review comment.

use crate::btree::{BTree, RangeIter};
use crate::budget::MemoryBudget;
use crate::io::{open_file_writer, Barrier, FileIo, IoMode};
#[cfg(test)]
use crate::io::open_file;
use crate::meta::Meta;
use crate::page::PAGE_SIZE;
use crate::pool::BufferPool;
use crate::wal::{RecKind, Wal};
use crate::{Error, Result};
use std::cell::Cell;
use std::path::Path;
use std::sync::Arc;

/// Deterministic failures at the SQL/native-to-storage boundary. This is
/// compiled only for tests: production stores contain neither the branch nor
/// the counter, while callers can prove error handling without depending on a
/// full disk, permissions, or a particular filesystem.
#[cfg(feature = "test-support")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestFaultKind { Read, Write, Commit }

#[cfg(feature = "test-support")]
#[derive(Debug, Default)]
pub struct TestFaultInjector {
    armed: std::sync::Mutex<Option<(TestFaultKind, usize)>>,
}

#[cfg(feature = "test-support")]
impl TestFaultInjector {
    /// Fail the matching operation after `successful_matches` matching calls.
    pub fn arm(&self, kind: TestFaultKind, successful_matches: usize) {
        *self.armed.lock().unwrap() = Some((kind, successful_matches));
    }

    fn check(&self, kind: TestFaultKind) -> Result<()> {
        let mut armed = self.armed.lock().unwrap();
        let Some((wanted, remaining)) = armed.as_mut() else { return Ok(()) };
        if *wanted != kind { return Ok(()) }
        if *remaining > 0 {
            *remaining -= 1;
            return Ok(());
        }
        *armed = None;
        Err(std::io::Error::other(format!("injected {kind:?} failure at storage boundary")).into())
    }
}

/// One bounded logical batch for empty-valued index rows: at most 64 keys and
/// one page-sized WAL payload. Ordinary GIN batches are ~2 KiB. Both bounds
/// are format-validated during recovery.
pub const PUT_EMPTY_BATCH_MAX_KEYS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode { Full, Normal, Off }

#[derive(Debug, Clone, Copy)]
pub struct Config { pub budget_bytes: usize, pub io: IoMode, pub sync: SyncMode }

/// Durable description of an unreachable, independently verified graft.
/// Persist this beside a resumable build before acknowledging its final
/// sorter watermark. On reopen, [`Store::publish_existing_candidate`] checks
/// both generations, re-verifies every reachable candidate page through a
/// fresh handle, reconstructs the standing parent path, and only then flips
/// the root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedGraft {
    pub base_generation: u64,
    pub write_generation: u64,
    pub root: u32,
    pub rows: u64,
    pub min: Vec<u8>,
    pub max: Vec<u8>,
    pub last_next: u32,
    pub inserted_min: Vec<u8>,
    pub inserted_max: Vec<u8>,
    pub inserted_rows: u64,
}

impl PreparedGraft {
    const MAGIC: &'static [u8; 8] = b"KGRFT01\0";

    pub fn encode(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        out.extend_from_slice(Self::MAGIC);
        out.extend_from_slice(&self.base_generation.to_le_bytes());
        out.extend_from_slice(&self.write_generation.to_le_bytes());
        out.extend_from_slice(&self.root.to_le_bytes());
        out.extend_from_slice(&self.rows.to_le_bytes());
        out.extend_from_slice(&self.last_next.to_le_bytes());
        out.extend_from_slice(&self.inserted_rows.to_le_bytes());
        for bytes in [&self.min, &self.max, &self.inserted_min, &self.inserted_max] {
            let len = u32::try_from(bytes.len()).map_err(|_| Error::TooLarge)?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(bytes);
        }
        let crc = crc32c::crc32c(&out);
        out.extend_from_slice(&crc.to_le_bytes());
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        const FIXED: usize = 8 + 8 + 8 + 4 + 8 + 4 + 8 + 4 * 4 + 4;
        let invalid = || Error::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData, "prepared graft manifest is invalid"));
        if bytes.len() < FIXED || bytes.get(..8) != Some(Self::MAGIC) { return Err(invalid()); }
        let crc_at = bytes.len() - 4;
        let want = u32::from_le_bytes(bytes[crc_at..].try_into().unwrap());
        if crc32c::crc32c(&bytes[..crc_at]) != want { return Err(invalid()); }
        let mut pos = 8usize;
        let mut u64_at = || {
            let end = pos.checked_add(8).ok_or_else(invalid)?;
            let value = u64::from_le_bytes(bytes.get(pos..end).ok_or_else(invalid)?.try_into().unwrap());
            pos = end; Ok::<_, Error>(value)
        };
        let base_generation = u64_at()?;
        let write_generation = u64_at()?;
        drop(u64_at);
        let root = u32::from_le_bytes(bytes.get(pos..pos + 4).ok_or_else(invalid)?.try_into().unwrap());
        pos += 4;
        let rows = u64::from_le_bytes(bytes.get(pos..pos + 8).ok_or_else(invalid)?.try_into().unwrap());
        pos += 8;
        let last_next = u32::from_le_bytes(bytes.get(pos..pos + 4).ok_or_else(invalid)?.try_into().unwrap());
        pos += 4;
        let inserted_rows = u64::from_le_bytes(bytes.get(pos..pos + 8).ok_or_else(invalid)?.try_into().unwrap());
        pos += 8;
        let mut fields = Vec::with_capacity(4);
        for _ in 0..4 {
            let raw = bytes.get(pos..pos + 4).ok_or_else(invalid)?;
            pos += 4;
            let len = u32::from_le_bytes(raw.try_into().unwrap()) as usize;
            let end = pos.checked_add(len).ok_or_else(invalid)?;
            if end > crc_at { return Err(invalid()); }
            fields.push(bytes[pos..end].to_vec());
            pos = end;
        }
        if pos != crc_at || fields.iter().any(Vec::is_empty) { return Err(invalid()); }
        Ok(Self { base_generation, write_generation, root, rows,
            min: fields.remove(0), max: fields.remove(0), inserted_min: fields.remove(0),
            inserted_max: fields.remove(0), inserted_rows, last_next })
    }

    /// Durably replace a candidate descriptor. A torn next descriptor remains
    /// at `.tmp`; reopen reads only the last fsynced-and-renamed manifest.
    pub fn write_manifest(&self, path: &Path) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let tmp = path.with_extension("tmp");
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&tmp)?;
            let bytes = self.encode()?;
            file.write_all(&bytes)?;
            crate::write_stats::add(
                crate::write_stats::Phase::Manifest,
                bytes.len() as u64,
            );
            file.sync_all()?;
        }
        std::fs::rename(&tmp, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    }

    pub fn read_manifest(path: &Path) -> Result<Self> {
        let metadata = std::fs::metadata(path)?;
        if metadata.len() > (1 << 20) {
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData, "prepared graft manifest is too large")));
        }
        Self::decode(&std::fs::read(path)?)
    }
}

/// The blessed default (D24): 64 MiB pool, buffered I/O, Normal durability.
/// 64 MiB is the WEIGHTED-TRAINING budget (D25): every gate and ablation is
/// measured at this budget or smaller, so optimization pressure lands on the
/// disk path -- layout, clustering, read counts -- never on cache flatter.
/// A server that can spend 4 GB sets it; the layout never changes.
impl Default for Config {
    fn default() -> Config {
        Config { budget_bytes: 64 << 20, io: IoMode::Buffered, sync: SyncMode::Normal }
    }
}

/// The `Barrier` a `SyncMode` maps to for the buffer pool's own flush. A free
/// function, not just an inherent method on `Store`, because `build`'s fresh
/// path needs it before a `Store` exists to call a method on.
fn sync_barrier(sync: SyncMode) -> Barrier {
    match sync {
        SyncMode::Full => Barrier::Full,
        SyncMode::Normal => Barrier::Data,
        SyncMode::Off => Barrier::None,
    }
}

pub struct Store {
    pool: BufferPool,
    /// None = snapshot reader (2f): opened read-only at a published
    /// generation; every mutating method refuses with Error::ReadOnly.
    wal: Option<Wal>,
    root: u32,
    /// Directory this store lives in -- lets `nearest_par` (2g.2) open
    /// sibling snapshot readers on the same files.
    dir: std::path::PathBuf,
    /// Publication counter (2f): the generation of the newest meta slot on
    /// disk. The next checkpoint publishes generation + 1.
    generation: u64,
    /// A snapshot reader's registration in the reader table (2n): present
    /// iff this store is a reader; dropping it releases the pin.
    reader_slot: Option<crate::readers::ReaderSlot>,
    tree_id: u16,
    sync: SyncMode,
    io_mode: IoMode,
    #[cfg(feature = "test-support")]
    test_faults: Arc<TestFaultInjector>,
    /// Set once, never cleared within this instance's life, when a logged
    /// write or `checkpoint` reaches a fallible durability step and fails.
    /// Checked at the top of EVERY writer -- `put`, `delete`, `commit`,
    /// `checkpoint`, `bulk_load` -- because the one that matters most is
    /// `checkpoint`: it ends in `Wal::rotate`, which deletes the log. See
    /// `Error::StorePoisoned`'s doc comment for why a failed barrier leaves
    /// the store unable to say what is durable, and why the remedy is to
    /// drop this instance and reopen (which clears the flag by rebuilding
    /// every belief from disk) rather than to retry in place.
    poisoned: bool,
    /// The append hint, hoisted here from `BTree` (Task 16). `BTree::open` is
    /// called fresh on every `put`/`delete`/`get`/`scan` and dropped
    /// immediately, so a hint owned by that transient `BTree` would reset to
    /// `None` before the next call ever saw it -- the fast path added in
    /// Task 15 exercised only a long-lived `BTree` in its own tests and
    /// bought nothing through `Store`, the path production and the
    /// benchmark actually use. `Store` outlives every one of those calls, so
    /// the hint lives here instead -- PostgreSQL's `rel->rd_targblock` on the
    /// `Relation`, not on a per-statement scan. `Cell`, not a plain field,
    /// because `BTree<'p>` needs `&'p Cell<Option<u32>>` and `put`/`delete`
    /// already borrow `self` mutably elsewhere; interior mutability avoids
    /// fighting the borrow checker over a single `u32` hint.
    last_leaf: Cell<Option<u32>>,
    /// Times `insert` used `last_leaf` instead of a full descent, borrowed
    /// into each transient `BTree` the same way `last_leaf` is so the count
    /// survives across `Store::put` calls instead of resetting with every
    /// fresh `BTree`.
    fast_path_hits: Cell<u64>,
    /// Times `insert` found the hint armed and tried `fast_path_leaf` at
    /// all, hit or miss (Task 18). Same borrowed-`Cell` reasoning as
    /// `fast_path_hits`: a counter living on the transient `BTree` would
    /// reset every call, and the disarm regression test needs a count of
    /// attempts, not hits, that survives across `Store::put` calls to prove
    /// a random-order workload pays for one wasted probe and then stops --
    /// not one wasted probe per row forever.
    fast_path_attempts: Cell<u64>,
    #[cfg(test)]
    trace: Vec<&'static str>,
    #[cfg(test)]
    barriers: Vec<&'static str>,
}

#[cfg(test)]
#[path = "store_reuse_probe.rs"]
mod reuse_probe;

impl Store {
    fn build(dir: &Path, cfg: Config, fresh: bool) -> Result<Store> {
        Self::build_limited(dir, cfg, fresh, None)
    }
    fn build_limited(dir: &Path, cfg: Config, fresh: bool, limits: Option<crate::limits::ResourceLimits>) -> Result<Store> {
        if fresh && dir.join("data").exists() {
            return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "create refuses existing data; use open").into());
        }
        std::fs::create_dir_all(dir)?;
        // A refused same-process open normally reports the writer lock. If an
        // external fault has already made that process's WAL demonstrably
        // corrupt, preserve the more specific pre-existing corruption
        // refusal. Inspect only in this already-refused local case, never on
        // a successful-open path and never for another process whose WAL may
        // still be changing.
        if !fresh && crate::io::writer_owned_by_this_process(&dir.join("data")) {
            if let crate::wal::Stop::Damaged { offset, why } =
                crate::wal::Wal::inspect(&dir.join("wal"), cfg.io)?.stop
            {
                return Err(crate::Error::CorruptWal { offset, why });
            }
        }
        let (file, io_mode) = open_file_writer(&dir.join("data"), cfg.io)?;
        Self::build_on_limited(dir, cfg, fresh, file.into(), io_mode, limits)
    }

    /// `build` with the data file handed in rather than opened here. The
    /// split exists for the same reason `recover_impl`'s does: a test needs
    /// to drive a store whose disk fails in a specific way, and without a
    /// seam the poisoning TRIGGER is unreachable from any test -- which is
    /// exactly how it came to be shipped unpinned (Task 17 final review,
    /// F6). Nothing but the `open_file` call moves out of the path a test
    /// can reach.
    #[cfg(test)]
    fn build_on(dir: &Path, cfg: Config, fresh: bool, file: Arc<dyn FileIo>, io_mode: IoMode)
        -> Result<Store> {
        Self::build_on_limited(dir, cfg, fresh, file, io_mode, None)
    }
    fn build_on_limited(dir: &Path, cfg: Config, fresh: bool, file: Arc<dyn FileIo>, io_mode: IoMode,
        requested: Option<crate::limits::ResourceLimits>) -> Result<Store> {
        let dir_owned = dir.to_path_buf();
        // Two thirds of the budget to the pool; the rest is scratch and log.
        let frames = (cfg.budget_bytes / 3 * 2) / PAGE_SIZE;
        let budget = Arc::new(MemoryBudget::new(cfg.budget_bytes));
        let pool = BufferPool::new(file, budget, frames.max(16))?;
        let limits = if fresh { requested } else { Meta::read_limits(&pool)? };
        if let Some(l) = limits { pool.set_resource_limits(l)?; }
        let mut wal = Wal::open_limited(&dir.join("wal"), cfg.io, limits.map(|l| l.wal_bytes))?;

        // Declared before the `fresh`/reopen branch because `BTree::create`
        // below borrows them for its lifetime, and they must still be here,
        // outside that borrow, to move into the `Store` literal afterward.
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);

        let (root, generation) = if fresh {
            let _meta_page = pool.allocate()?;     // page 0 is the superblock
            drop(_meta_page);
            let _slot_b = pool.allocate()?;        // page 1 is meta slot B (2f)
            drop(_slot_b);
            Meta::init_slot_b(&pool)?;
            let t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts)?;
            let r = t.root();
            Meta { format_version: crate::meta::FORMAT_VERSION, roots: [r, 0, 0, 0, 0, 0, 0, 0], next_lsn: wal.next_lsn(),
                   generation: 0 }
                .write(&pool)?;
            pool.flush_all(sync_barrier(cfg.sync))?;
            // The data and WAL contents now have the requested barriers; make
            // their new directory entries durable before create returns and a
            // later commit can be acknowledged. This is unconditional because
            // even SyncMode::Off does not ask for files to vanish by name.
            pool.sync_dir()?;
            (r, 0)
        } else {
            // Re-supply the LSN high-water mark BEFORE anything else touches
            // the log: `Wal::open` above has already derived `next_lsn` from
            // whatever the log file itself contains, which after a rotation
            // is empty and would renumber from 1. `set_lsn_floor` is a no-op
            // if the log turned out to know a higher number already (i.e. no
            // rotation happened since the last checkpoint).
            let meta = Meta::read_latest(&pool)?;
            wal.set_lsn_floor(meta.next_lsn);
            // Opening can recreate a missing WAL. Publish that filename once
            // before any later acknowledgement, rather than relying on a
            // recurring ordinary-checkpoint directory barrier.
            pool.sync_dir()?;
            (meta.roots[0], meta.generation)
        };
        // 2f: everything on disk up to here is the published state a snapshot
        // reader may be standing on. Freeze it BEFORE recovery replays the
        // WAL tail -- the replay goes through the ordinary shadowed write
        // path, so it relocates instead of tearing the published pages.
        pool.set_frozen_boundary();
        let write_generation = generation.checked_add(1).ok_or(crate::Error::Corrupt {
            page_no: if generation % 2 == 0 { crate::meta::META_PAGE } else { crate::meta::META_PAGE_B },
            why: "published generation is exhausted",
        })?;
        pool.set_stamp_gen(write_generation);
        if !fresh {
            // Derived state may be lost, but never allocate from an unchecked
            // file length. At most one retirement per physical page.
            let cap = limits.map_or(28 + 24 * pool.page_count() as u64, |l| l.freelist_bytes());
            if let Ok(f) = std::fs::File::open(dir.join("free")) {
                use std::io::Read;
                if f.metadata()?.len() <= cap {
                    let mut b = Vec::new();
                    f.take(cap + 1).read_to_end(&mut b)?;
                    if b.len() as u64 <= cap { pool.import_free(&b, generation); }
                }
            }
        }
        pool.set_reuse_limit(
            generation.saturating_sub(1)
                .min(crate::readers::oldest_live_reader(dir)));

        let mut s = Store { pool, wal: Some(wal), root, generation, reader_slot: None, dir: dir_owned, tree_id: 1, sync: cfg.sync, io_mode,
                            #[cfg(feature = "test-support")] test_faults: Arc::new(TestFaultInjector::default()),
                            poisoned: false, last_leaf, fast_path_hits, fast_path_attempts,
                            #[cfg(test)] trace: Vec::new(),
                            #[cfg(test)] barriers: Vec::new() };
        if !fresh {
            if limits.is_some() {
                // Limited commits publish their root without a WAL Commit.
                // Reject a foreign committed tail before applying anything.
                if s.wal.as_ref().unwrap().committed_end()? != 0 {
                    return Err(Error::ResourceLimit("unexpected committed WAL in constrained store; preserve and inspect"));
                }
                s.wal_mut()?.rotate()?;
            } else { s.recover_from_log()?; }
        }
        Ok(s)
    }

    /// 2f: open a SNAPSHOT READER on `dir`. Serves the newest PUBLISHED
    /// generation (the last checkpoint's roots) and nothing later: the WAL
    /// tail is deliberately not replayed -- replay writes, and this store
    /// cannot write. Correct beside a live writer with zero coordination:
    /// published pages are immutable (the writer shadows instead of editing,
    /// and page numbers are never reused), so everything reachable from a
    /// published root stays byte-identical for as long as this reader lives.
    /// Sacrifice (Law 4): staleness up to one checkpoint cadence.
    pub fn open_snapshot(dir: &Path, cfg: Config) -> Result<Store> {
        Self::open_snapshot_with_after_meta(dir, cfg, |_| Ok(()))
    }

    /// Test seam inside snapshot open: metadata has been selected while a
    /// conservative generation-zero reader slot is already live. Production
    /// supplies an empty closure; the concurrency regression test holds this
    /// point open to prove the writer cannot recycle the selected generation.
    fn open_snapshot_with_after_meta<F>(dir: &Path, cfg: Config, after_meta: F) -> Result<Store>
    where
        F: FnOnce(u64) -> Result<()>,
    {
        // Register BEFORE selecting metadata. Zero is deliberately
        // conservative: until the exact published generation is known, the
        // writer must assume this reader can need every historical page.
        let mut slot = crate::readers::ReaderSlot::reserve(dir)?;
        let file = crate::io::open_file_readonly(&dir.join("data"))?;
        let frames = (cfg.budget_bytes / 3 * 2) / PAGE_SIZE;
        let budget = Arc::new(MemoryBudget::new(cfg.budget_bytes));
        let pool = BufferPool::new(file.into(), budget, frames.max(16))?;
        let meta = Meta::read_latest(&pool)?;
        if let Some(l) = Meta::read_limits(&pool)? { pool.set_resource_limits(l)?; }
        after_meta(meta.generation)?;
        slot.set_generation(meta.generation)?;
        let s = Store {
            pool, wal: None, root: meta.roots[0], generation: meta.generation,
            reader_slot: Some(slot),
            dir: dir.to_path_buf(),
            tree_id: 1, sync: cfg.sync, io_mode: IoMode::Buffered,
            #[cfg(feature = "test-support")]
            test_faults: Arc::new(TestFaultInjector::default()),
            poisoned: false,
            last_leaf: Cell::new(None),
            fast_path_hits: Cell::new(0), fast_path_attempts: Cell::new(0),
            #[cfg(test)] trace: Vec::new(),
            #[cfg(test)] barriers: Vec::new(),
        };
        Ok(s)
    }

    /// The pool, for probes and tests (2n).
    pub fn pool_ref(&self) -> &BufferPool { &self.pool }

    /// Create a new constrained entry store. Commit publishes durable metadata;
    /// external-sort/graft and in-place repair require a separate workspace.
    /// Existing directories are refused, so policy cannot be applied halfway.
    pub fn create_limited(dir: &Path, cfg: Config, limits: crate::limits::ResourceLimits) -> Result<Store> {
        let limits = limits.validate()?;
        std::fs::create_dir(dir)?;
        Self::build_limited(dir, cfg, true, Some(limits))
    }
    pub fn resource_limits(&self) -> Option<crate::limits::ResourceLimits> { self.pool.resource_limits() }
    fn refuse_external_workspace(&self) -> Result<()> {
        if self.resource_limits().is_some() {
            return Err(Error::ResourceLimit("external sort/graft requires a separately budgeted destination; use batched put"));
        }
        Ok(())
    }

    pub fn create(dir: &Path, cfg: Config) -> Result<Store> { Self::build(dir, cfg, true) }

    /// `create`, but on a caller-supplied data file. Test-only: the one
    /// caller is the poisoning-trigger test, which needs a `FileIo` whose
    /// barrier can be made to fail on demand.
    #[cfg(test)]
    fn create_on(dir: &Path, cfg: Config, file: Arc<dyn FileIo>) -> Result<Store> {
        std::fs::create_dir_all(dir)?;
        Self::build_on(dir, cfg, true, file, IoMode::Buffered)
    }
    pub fn open(dir: &Path, cfg: Config) -> Result<Store> {
        if dir.join("data").exists() { Self::build(dir, cfg, false) }
        else { Self::build(dir, cfg, true) }
    }

    pub fn io_mode(&self) -> IoMode { self.io_mode }

    #[cfg(feature = "test-support")]
    #[doc(hidden)]
    pub fn test_fault_injector(&self) -> Arc<TestFaultInjector> {
        self.test_faults.clone()
    }

    /// Replay committed transactions the pages have not yet absorbed.
    fn recover_from_log(&mut self) -> Result<()> {
        // Pass 1 names the final committed byte. Pass 2 applies one frame at a
        // time through that boundary. Nothing after it happened, and neither a
        // whole WAL nor a whole transaction is ever resident (Law 1).
        let committed_end = self.wal.as_ref().ok_or(crate::Error::ReadOnly)?.committed_end()?;
        let mut off = 0u64;
        while off < committed_end {
            let (_, kind, payload, next) = self.wal.as_ref()
                .ok_or(crate::Error::ReadOnly)?
                .record_at(off, committed_end)?
                .ok_or(crate::Error::CorruptWal {
                    offset: off, why: "committed recovery prefix ended early",
                })?;
            self.apply(kind, &payload, off)?;
            off = next;
        }
        Ok(())
    }

    fn apply(&mut self, kind: RecKind, payload: &[u8], wal_offset: u64) -> Result<()> {
        let corrupt = |why| crate::Error::CorruptWal { offset: wal_offset, why };
        match kind {
            RecKind::Put => {
                let klen = payload.get(..2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
                    .ok_or_else(|| corrupt("put payload has no key length"))?;
                let key_end = 2usize.checked_add(klen)
                    .ok_or_else(|| corrupt("put key boundary overflow"))?;
                let key = payload.get(2..key_end)
                    .ok_or_else(|| corrupt("put key crosses its WAL payload"))?;
                let val = payload.get(key_end..)
                    .ok_or_else(|| corrupt("put value boundary is invalid"))?;
                // Salvage publishes a marker at generation zero and retains
                // the logical WAL. Replay repairs source rows, but old derived
                // counts cannot be trusted beside a potentially damaged source.
                // Fresh never-checkpointed databases have no marker.
                if self.generation == 0 && crate::keys::is_field_aggregate_key(key) {
                    if Meta::is_salvaged(&self.pool)? { return Ok(()); }
                }
                let mut t = BTree::open(&self.pool, self.tree_id, self.root, &self.last_leaf, &self.fast_path_hits, &self.fast_path_attempts);
                t.insert(key, val)?;
                self.root = t.root();
            }
            RecKind::PutEmptyBatch => {
                if payload.len() > crate::page::MAX_RECORD_LEN {
                    return Err(corrupt("put-empty-batch exceeds the WAL frame bound"));
                }
                let count = payload.get(..2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
                    .ok_or_else(|| corrupt("put-empty-batch payload has no key count"))?;
                if count == 0 || count > PUT_EMPTY_BATCH_MAX_KEYS {
                    return Err(corrupt("put-empty-batch key count is outside its fixed bound"));
                }

                // Validate every boundary and key-size invariant before the
                // first tree mutation. A CRC-valid malformed frame must not
                // apply a valid prefix and fail halfway through.
                let mut at = 2usize;
                for _ in 0..count {
                    let len_end = at.checked_add(2)
                        .ok_or_else(|| corrupt("put-empty-batch length boundary overflow"))?;
                    let len_bytes = payload.get(at..len_end)
                        .ok_or_else(|| corrupt("put-empty-batch key has no length"))?;
                    let key_len = u16::from_le_bytes([len_bytes[0], len_bytes[1]]) as usize;
                    let key_end = len_end.checked_add(key_len)
                        .ok_or_else(|| corrupt("put-empty-batch key boundary overflow"))?;
                    payload.get(len_end..key_end)
                        .ok_or_else(|| corrupt("put-empty-batch key crosses its WAL payload"))?;
                    if 4 + key_len + 12 > crate::page::MAX_RECORD_LEN {
                        return Err(corrupt("put-empty-batch key cannot fit a leaf record"));
                    }
                    at = key_end;
                }
                if at != payload.len() {
                    return Err(corrupt("put-empty-batch payload has trailing bytes"));
                }

                let mut t = BTree::open(&self.pool, self.tree_id, self.root,
                    &self.last_leaf, &self.fast_path_hits, &self.fast_path_attempts);
                at = 2;
                for _ in 0..count {
                    let key_len = u16::from_le_bytes([payload[at], payload[at + 1]]) as usize;
                    at += 2;
                    t.insert(&payload[at..at + key_len], &[])?;
                    at += key_len;
                }
                self.root = t.root();
            }
            RecKind::Delete => {
                let klen = payload.get(..2)
                    .map(|b| u16::from_le_bytes([b[0], b[1]]) as usize)
                    .ok_or_else(|| corrupt("delete payload has no key length"))?;
                let key_end = 2usize.checked_add(klen)
                    .ok_or_else(|| corrupt("delete key boundary overflow"))?;
                if key_end != payload.len() {
                    return Err(corrupt("delete key length does not match its WAL payload"));
                }
                let mut t = BTree::open(&self.pool, self.tree_id, self.root, &self.last_leaf, &self.fast_path_hits, &self.fast_path_attempts);
                t.delete(&payload[2..key_end])?;
                self.root = t.root();
            }
            RecKind::DeletePrefix => {
                if payload.is_empty() {
                    return Err(corrupt("delete-prefix WAL payload is empty"));
                }
                let mut t = BTree::open(&self.pool, self.tree_id, self.root, &self.last_leaf, &self.fast_path_hits, &self.fast_path_attempts);
                t.delete_prefix(payload)?;
                self.root = t.root();
            }
            RecKind::Commit => {
                if !payload.is_empty() {
                    return Err(corrupt("commit WAL payload is not empty"));
                }
            }
            RecKind::PageImage => {
                return Err(corrupt("page-image WAL records have no recovery implementation"));
            }
        }
        Ok(())
    }

    /// [klen u16][key][value...to end]. The value's length is implicit -- the
    /// WAL frame already carries its own length. The old format stored vlen as
    /// u16, which silently TRUNCATED any value over 64KB on its way into the
    /// log (v.len() as u16); with overflow chains making big values legal,
    /// that was a data-loss bug waiting on the replay path.
    fn frame(k: &[u8], v: &[u8]) -> Vec<u8> {
        let mut b = Vec::with_capacity(2 + k.len() + v.len());
        b.extend_from_slice(&(k.len() as u16).to_le_bytes());
        b.extend_from_slice(k);
        b.extend_from_slice(v);
        b
    }

    /// The writer's log, or ReadOnly for a snapshot reader (2f).
    fn wal_mut(&mut self) -> Result<&mut Wal> {
        self.wal.as_mut().ok_or(crate::Error::ReadOnly)
    }

    pub fn put(&mut self, k: &[u8], v: &[u8]) -> Result<()> {
        #[cfg(feature = "test-support")]
        self.test_faults.check(TestFaultKind::Write)?;
        if self.poisoned { return Err(crate::Error::StorePoisoned); }
        if self.wal.is_none() { return Err(crate::Error::ReadOnly); }
        let wal_payload_len = 2usize.checked_add(k.len())
            .and_then(|n| n.checked_add(v.len()))
            .ok_or(crate::Error::TooLarge)?;
        if self.resource_limits().is_some_and(|l| v.len() > l.record_bytes as usize) {
            return Err(Error::ResourceLimit("record exceeds configured maximum"));
        }
        // Reject before `frame` copies the change. D7's large overflow values
        // retain the encoded format's nearly 4 GiB range, while no disk length
        // may exceed the exact maximum any writer can emit.
        if wal_payload_len as u64 > crate::wal::MAX_PAYLOAD_BYTES {
            return Err(crate::Error::TooLarge);
        }
        if k.len() > crate::page::MAX_RECORD_LEN - 16 || v.len() > u32::MAX as usize {
            return Err(crate::Error::TooLarge);
        }
        #[cfg(feature = "write-trace")]
        let frame_started = crate::write_trace::active().then(std::time::Instant::now);
        let payload = Self::frame(k, v);
        #[cfg(feature = "write-trace")]
        if let Some(started) = frame_started {
            crate::write_trace::add(crate::write_trace::Field::FrameEncode, started.elapsed());
            crate::write_trace::value_copy();
        }
        // Reject BEFORE the WAL ever sees it (Task 17 re-review, R1): a frame
        // the insert below would refuse must never enter the log, or replay
        // meets a record nothing can apply. With overflow chains the value can
        // be any size; what must still fit in a page is the KEY (a marker
        // record is 4 + klen + 12 bytes), and the value length must fit the
        // marker's u32.
        if 4 + k.len() + 12 > crate::page::MAX_RECORD_LEN || v.len() > u32::MAX as usize {
            return Err(crate::Error::TooLarge);
        }
        if let Err(e) = self.wal_mut()?.append(RecKind::Put, &payload) {
            // An append error can leave a partial frame in the log buffer or
            // file. The tree is still intact, but continuing could place a
            // later commit beyond an unreadable tail, so this is not benign.
            self.poisoned = true;
            return Err(e);
        }
        let mut t = BTree::open(&self.pool, self.tree_id, self.root, &self.last_leaf, &self.fast_path_hits, &self.fast_path_attempts);
        #[cfg(feature = "write-trace")]
        let btree_started = crate::write_trace::active().then(std::time::Instant::now);
        let inserted = t.insert(k, v);
        #[cfg(feature = "write-trace")]
        if let Some(started) = btree_started {
            crate::write_trace::add(crate::write_trace::Field::BtreeTotal, started.elapsed());
        }
        let root = t.root();
        drop(t);
        if let Err(e) = inserted {
            self.poisoned = true;
            return Err(e);
        }
        self.root = root;
        Ok(())
    }

    /// The exact ordinary put path, with feature-gated clocks enabled for the
    /// load diagnostic. This does not exist in production builds.
    #[cfg(feature = "write-trace")]
    pub fn put_profiled(&mut self, k: &[u8], v: &[u8]) -> (Result<()>, crate::write_trace::PutTrace) {
        let trace_started = crate::write_trace::begin();
        let result = self.put(k, v);
        let trace = crate::write_trace::finish(trace_started);
        (result, trace)
    }

    /// Write up to 64 empty-valued index rows under one WAL frame and one
    /// B-tree handle. Key order is preserved exactly. This is not bulk-load:
    /// it appends to the existing shared tree and participates in the caller's
    /// ordinary commit/recovery boundary.
    pub fn put_empty_batch(&mut self, keys: &[Vec<u8>]) -> Result<()> {
        if keys.is_empty() { return Ok(()); }
        #[cfg(feature = "test-support")]
        self.test_faults.check(TestFaultKind::Write)?;
        if self.poisoned { return Err(crate::Error::StorePoisoned); }
        if self.wal.is_none() { return Err(crate::Error::ReadOnly); }
        if keys.len() > PUT_EMPTY_BATCH_MAX_KEYS { return Err(crate::Error::TooLarge); }

        let payload_len = keys.iter().try_fold(2usize, |total, key| {
            if 4 + key.len() + 12 > crate::page::MAX_RECORD_LEN || key.len() > u16::MAX as usize {
                return None;
            }
            total.checked_add(2 + key.len())
        }).ok_or(crate::Error::TooLarge)?;
        if payload_len > crate::page::MAX_RECORD_LEN { return Err(crate::Error::TooLarge); }
        let mut payload = Vec::with_capacity(payload_len);
        payload.extend_from_slice(&(keys.len() as u16).to_le_bytes());
        for key in keys {
            payload.extend_from_slice(&(key.len() as u16).to_le_bytes());
            payload.extend_from_slice(key);
        }
        if let Err(e) = self.wal_mut()?.append(RecKind::PutEmptyBatch, &payload) {
            self.poisoned = true;
            return Err(e);
        }
        let mut t = BTree::open(&self.pool, self.tree_id, self.root,
            &self.last_leaf, &self.fast_path_hits, &self.fast_path_attempts);
        let inserted = keys.iter().try_for_each(|key| t.insert(key, &[]));
        let root = t.root();
        drop(t);
        if let Err(e) = inserted {
            self.poisoned = true;
            return Err(e);
        }
        self.root = root;
        Ok(())
    }

    pub fn delete(&mut self, k: &[u8]) -> Result<bool> {
        #[cfg(feature = "test-support")]
        self.test_faults.check(TestFaultKind::Write)?;
        if self.poisoned { return Err(crate::Error::StorePoisoned); }
        if self.wal.is_none() { return Err(crate::Error::ReadOnly); }
        if k.len() > crate::page::MAX_RECORD_LEN - 2 { return Ok(false); }
        let mut payload = (k.len() as u16).to_le_bytes().to_vec();
        payload.extend_from_slice(k);
        // The same pre-WAL bound as `put`, on the PAYLOAD -- `2 + k.len()`
        // -- not on the key (Task 17 final review, F1: a guard written
        // against the key while the append writes the payload is a guard
        // that is two bytes wrong, and a bound that is two bytes wrong is
        // the whole defect). No key this long can be present in the first
        // place: `put` above refuses any record whose key alone would reach
        // this size, so `Ok(false)` -- `delete`'s ordinary "not found"
        // answer -- is not merely convenient here, it is the true one.
        if payload.len() > crate::page::MAX_RECORD_LEN { return Ok(false); }
        if let Err(e) = self.wal_mut()?.append(RecKind::Delete, &payload) {
            self.poisoned = true;
            return Err(e);
        }
        let mut t = BTree::open(&self.pool, self.tree_id, self.root, &self.last_leaf, &self.fast_path_hits, &self.fast_path_attempts);
        let deleted = t.delete(k);
        let root = t.root();
        drop(t);
        let hit = match deleted {
            Ok(hit) => hit,
            Err(e) => {
                self.poisoned = true;
                return Err(e);
            }
        };
        self.root = root;
        Ok(hit)
    }

    /// Delete every key starting with `prefix` (2h A4). One WAL record,
    /// idempotent on replay; leaves wholly inside the range are cleared in
    /// ONE page write instead of per-slot removals -- the fold's head erase
    /// was 8M row deletes (~20 minutes at 1M docs) and is now ~one write
    /// per leaf. Returns the number of keys removed.
    pub fn delete_prefix(&mut self, prefix: &[u8]) -> Result<u64> {
        #[cfg(feature = "test-support")]
        self.test_faults.check(TestFaultKind::Write)?;
        if self.poisoned { return Err(crate::Error::StorePoisoned); }
        if self.wal.is_none() { return Err(crate::Error::ReadOnly); }
        if prefix.is_empty() || prefix.len() > crate::page::MAX_RECORD_LEN { return Err(crate::Error::TooLarge); }
        if let Err(e) = self.wal_mut()?.append(RecKind::DeletePrefix, prefix) {
            self.poisoned = true;
            return Err(e);
        }
        let mut t = BTree::open(&self.pool, self.tree_id, self.root, &self.last_leaf, &self.fast_path_hits, &self.fast_path_attempts);
        let deleted = t.delete_prefix(prefix);
        let root = t.root();
        drop(t);
        let n = match deleted {
            Ok(n) => n,
            Err(e) => {
                self.poisoned = true;
                return Err(e);
            }
        };
        self.root = root;
        Ok(n)
    }

    pub fn get(&self, k: &[u8]) -> Result<Option<Vec<u8>>> {
        if self.poisoned && self.resource_limits().is_some() {
            return Err(crate::Error::StorePoisoned);
        }
        #[cfg(feature = "test-support")]
        self.test_faults.check(TestFaultKind::Read)?;
        BTree::open(&self.pool, self.tree_id, self.root, &self.last_leaf, &self.fast_path_hits, &self.fast_path_attempts).get(k)
    }

    pub fn scan(&self, from: &[u8]) -> Result<RangeIter<'_>> {
        if self.poisoned && self.resource_limits().is_some() {
            return Err(crate::Error::StorePoisoned);
        }
        BTree::open(&self.pool, self.tree_id, self.root, &self.last_leaf, &self.fast_path_hits, &self.fast_path_attempts).range(from)
    }

    /// Descending scan of keys strictly below `to`.
    pub fn scan_reverse(&self, to: &[u8]) -> Result<crate::btree::ReverseRangeIter<'_>> {
        if self.poisoned && self.resource_limits().is_some() {
            return Err(crate::Error::StorePoisoned);
        }
        BTree::open(&self.pool, self.tree_id, self.root, &self.last_leaf,
                    &self.fast_path_hits, &self.fast_path_attempts).range_reverse(to)
    }

    /// `SyncMode` is a promise about what reached the medium, so the three modes
    /// must issue three different things. An earlier draft had `Full` and
    /// `Normal` both call one `sync()` -- which made the label decorative, and a
    /// decorative durability label is worse than none, because every benchmark
    /// carrying it becomes unattributable. It is worse than that on macOS
    /// specifically: `std::fs::File::sync_data` (what the single `sync()` used)
    /// issues `fcntl(F_FULLFSYNC)` there, so `Normal` would have silently been
    /// `Full`'s ~65x-costlier barrier on this machine while meaning something
    /// cheaper on Linux -- the same code looking wildly different speeds for
    /// reasons the label never states.
    ///
    /// Nothing in the committed suite failed if this collapsed back into one
    /// call for both arms -- `barriers` exists so something does.
    pub fn commit(&mut self) -> Result<()> {
        if self.resource_limits().is_some() { return self.checkpoint(); }
        #[cfg(feature = "test-support")]
        self.test_faults.check(TestFaultKind::Commit)?;
        if self.poisoned { return Err(crate::Error::StorePoisoned); }
        let wal = self.wal.as_mut().ok_or(crate::Error::ReadOnly)?;
        wal.append(RecKind::Commit, &[])?;
        // Buffering does not redefine commit: Off promises no BARRIER, not
        // that the record stays in process RAM. Free under Normal/Full (their
        // barriers flush anyway).
        wal.flush()?;
        match self.sync {
            SyncMode::Full => {
                #[cfg(test)] self.barriers.push("sync_full");
                wal.sync_full()?
            }
            SyncMode::Normal => {
                #[cfg(test)] self.barriers.push("sync_data");
                wal.sync_data()?
            }
            SyncMode::Off => {}
        }
        // A snapshot can close between checkpoints. Refresh at the existing
        // transaction boundary so the next batch can use newly eligible pages
        // instead of growing for the rest of this epoch. No publication or
        // page scan: only the live reader table, with the same conservative
        // fallback/ambiguous-reader protection as checkpoint and reopen.
        let readers = crate::readers::live_generations(&self.dir);
        self.pool.refresh_reuse(self.generation, readers.as_deref());
        Ok(())
    }

    /// Commit with an explicit byte-based publication policy. Limits are
    /// triggers checked at the transaction boundary, NOT disk quotas. Both
    /// WAL bytes and allocated/shadow pages matter: logical WAL records are
    /// much smaller than the pages scattered updates cause us to copy.
    ///
    /// On the checkpoint branch, the data + metadata barriers establish the
    /// commit's durability; a redundant WAL barrier is avoided. Checkpoint
    /// errors remain errors and preserve the WAL for reopening. This opt-in
    /// API does not change ordinary commit() or snapshot staleness defaults.
    pub fn commit_with_checkpoint(&mut self, wal_bytes: u64, page_bytes: u64) -> Result<bool> {
        if self.resource_limits().is_some() { self.commit()?; return Ok(true); }
        if self.poisoned { return Err(crate::Error::StorePoisoned); }
        let end = self.wal.as_ref().ok_or(crate::Error::ReadOnly)?.end_offset();
        let publish = (wal_bytes > 0 && end >= wal_bytes)
            || (page_bytes > 0 && self.pool.epoch_allocated_bytes() >= page_bytes);
        if !publish { self.commit()?; return Ok(false); }
        #[cfg(feature = "test-support")]
        self.test_faults.check(TestFaultKind::Commit)?;
        let wal = self.wal_mut()?;
        if let Err(e) = wal.append(RecKind::Commit, &[]).and_then(|_| wal.flush()) {
            self.poisoned = true;
            return Err(e);
        }
        if let Err(e) = self.checkpoint() {
            self.poisoned = true;
            return Err(e);
        }
        Ok(true)
    }

    #[cfg(test)]
    fn barriers(&self) -> Vec<&'static str> { self.barriers.clone() }

    /// The exact primitive `SyncMode::Full` issues on this platform, so a
    /// measurement can name it instead of implying it.
    pub fn dir(&self) -> &Path { &self.dir }
    /// The published root of the main tree. For structural verification of a
    /// live database — see `verify::verify_published_tree`.
    pub fn published_root(&self) -> u32 { self.root }
    /// The main tree's identity, so a verifier can prove every page belongs to it.
    pub fn main_tree_id(&self) -> u16 { self.tree_id }

    pub fn sync_full_primitive(&self) -> &'static str {
        self.wal.as_ref().map_or("none (snapshot reader)", |w| w.sync_full_primitive())
    }

    /// The pool's own counters, including how many `sync_data`/`sync_full`
    /// barriers it has actually issued. Real observability API, not test
    /// instrumentation -- `PoolStats` and `BufferPool::stats` are already
    /// public and ungated -- so this needs no `#[cfg(test)]` and the test
    /// that reads it can live in `kernel/tests/durability.rs` like any other
    /// public-API test.
    pub fn pool_stats(&self) -> crate::pool::PoolStats { self.pool.stats() }
    /// Data-file counters only; the WAL keeps its own.
    pub fn io_stats(&self) -> Option<&crate::io::IoStats> { self.pool.io_stats() }
    pub fn sweep_steps(&self) -> u64 { self.pool.sweep_steps() }

    /// The `Barrier` `checkpoint`'s page flush should use, derived from this
    /// store's `SyncMode`. Before this, the pool's checkpoint flush went
    /// through `FileIo::sync` -- `File::sync_data`, `F_FULLFSYNC` on macOS,
    /// `fdatasync` on Linux -- ungoverned by `SyncMode` at all: the exact
    /// ambiguity `commit` was fixed to no longer have, left standing in the
    /// quieter of the two places that used to share it.
    fn barrier(&self) -> Barrier { sync_barrier(self.sync) }

    /// Runtime durability change (SQL SET WAL_SYNC): applies to every
    /// subsequent commit/checkpoint barrier.
    pub fn set_sync(&mut self, s: SyncMode) { self.sync = s; }

    /// The level in force right now. A caller that raises durability for one
    /// operation has to be able to put back what it found -- without this it
    /// could only guess, and guessing wrong silently re-levels every later
    /// commit.
    pub fn sync_mode(&self) -> SyncMode { self.sync }
    /// The published generation this handle currently serves.
    pub fn generation(&self) -> u64 { self.generation }

    pub fn checkpoint(&mut self) -> Result<()> {
        let result = self.checkpoint_inner();
        if result.is_err() && self.wal.is_some() { self.poisoned = true; }
        result
    }
    fn checkpoint_inner(&mut self) -> Result<()> {
        if self.poisoned { return Err(crate::Error::StorePoisoned); }
        if self.wal.is_none() { return Err(crate::Error::ReadOnly); }
        // Preflight both numbers before flushing any page or touching either
        // meta slot. Publishing `u64::MAX` would leave the following epoch
        // with no distinct stamp; wrapping it to zero would make recycled
        // pages look older than every live snapshot.
        let gen = self.generation.checked_add(1).ok_or(crate::Error::Corrupt {
            page_no: if self.generation % 2 == 0 { crate::meta::META_PAGE } else { crate::meta::META_PAGE_B },
            why: "published generation is exhausted",
        })?;
        let next_write_generation = gen.checked_add(1).ok_or(crate::Error::Corrupt {
            page_no: if self.generation % 2 == 0 { crate::meta::META_PAGE } else { crate::meta::META_PAGE_B },
            why: "no generation remains for the next write epoch",
        })?;
        // The LSN high-water mark goes into the same page as the roots, so it
        // is flushed atomically with them below, and a reopen after this
        // checkpoint's rotation re-supplies it via `set_lsn_floor` instead of
        // renumbering from 1.
        #[cfg(test)] self.trace.clear();
        // Poison on `Err` rather than merely propagating it (Task 17
        // re-review, R5): `flush_all` clears each frame's dirty bit BEFORE
        // issuing its barrier, so a failure here leaves the store believing
        // pages are durable that may not be -- and the very next line
        // deletes the log those pages' contents are otherwise only recorded
        // in. See `Error::StorePoisoned`.
        if let Err(e) = self.pool.flush_all(self.barrier()) {
            self.poisoned = true;
            return Err(e);
        }
        #[cfg(test)] { self.trace.push("flush_pages"); self.trace.push("sync_file"); }
        // 2f dual-slot flip, strictly AFTER the data barrier above: the slot
        // write must not be able to reach the medium before the pages its
        // roots name. A crash between the two leaves the previous generation
        // standing and the WAL tail replayable -- exactly a missed
        // checkpoint, never a torn one.
        Meta {
            format_version: crate::meta::FORMAT_VERSION,
            roots: [self.root, 0, 0, 0, 0, 0, 0, 0],
            next_lsn: self.wal.as_ref().ok_or(crate::Error::ReadOnly)?.next_lsn(),
            generation: gen,
        }.write_slot(&self.pool)?;
        if let Err(e) = self.pool.flush_all(self.barrier()) {
            self.poisoned = true;
            return Err(e);
        }
        self.generation = gen;
        self.pool.set_stamp_gen(next_write_generation);
        // The new root is durable, making the preceding generation's hint
        // stale. Reuse its existing file: a torn overwrite loses only derived
        // reuse knowledge. Generation and whole-body CRC are checked on reopen.
        let readers = crate::readers::live_generations(&self.dir);
        self.pool.refresh_reuse(gen, readers.as_deref());
        let free_bytes = self.pool.export_free(gen);
        let _ = crate::verify::persist_checkpoint_freelist(
            &self.dir,
            &free_bytes,
            self.pool.file_ref(),
        );
        // 2n recycling horizon: pages freed at generations <= this are safe.
        // published - 1 keeps the dual-slot fallback tree intact; the oldest
        // live snapshot reader caps it further. An unreadable reader table
        // reports 0 -- recycling halts rather than guesses.

        #[cfg(test)] self.trace.push("flip_meta");
        // Everything just published is now immutable to writers (2f): the
        // next epoch's writes shadow instead of editing in place.
        self.pool.set_frozen_boundary();
        // Data/WAL names were published at create/open; no name is replaced at
        // ordinary checkpoint. Newly created free hints sync their own name.
        self.wal_mut()?.rotate_published()?;
        #[cfg(test)] self.trace.push("rotate_wal");
        Ok(())
    }

    #[cfg(test)]
    fn checkpoint_trace(&self) -> Vec<&'static str> { self.trace.clone() }

    /// The log's own high-water mark, test-only. Exists to pin the LSN
    /// persistence obligation: without `checkpoint` writing `next_lsn` into
    /// `Meta` and `open` re-supplying it via `set_lsn_floor`, a rotation
    /// followed by a reopen would renumber records from 1 -- silently
    /// harmless today because nothing compares a page's `lsn` against a
    /// record's yet, but wrong the moment that check is added.
    #[cfg(test)]
    fn next_lsn(&self) -> u64 { self.wal.as_ref().unwrap().next_lsn() }

    /// Build the tree from scratch by external sort and pack.
    ///
    /// The new tree replaces the old one, but the old one's PAGES are not
    /// reclaimed -- page reclamation is deferred in Phase 1, so calling this
    /// on a non-empty store leaves the previous tree's pages allocated and
    /// unreachable. Intended for loading into a fresh store; on a populated
    /// one the file grows by the size of both trees.
    ///
    /// Unlike `put`/`delete`, this writes pages straight into the pool and
    /// never through the WAL -- logging every bulk-loaded record before
    /// packing it would reintroduce the per-record write this task exists to
    /// remove. That means there is nothing in the log for recovery to replay,
    /// so this makes itself durable before returning rather than leaving that
    /// to a caller who may reasonably assume `bulk_load` behaves like any
    /// other write the engine accepted: pack, then `checkpoint()`. Without
    /// this, `bulk_load` followed only by `commit()` followed by a crash
    /// loses everything -- the superblock still names the OLD root, and the
    /// log holds a commit record describing nothing -- while the caller was
    /// told `Ok` twice. `checkpoint()` also rotates the log, so this discards
    /// prior log state; correct for a whole-tree replacement, not something
    /// an incremental write may do.
    ///
    /// SACRIFICE (Law 4): publication adds one sequential read traversal of
    /// the packed tree. It retains only 16 verification pages and tree-height
    /// state; ordinary queries and writes do not use this path.
    pub fn bulk_load<I>(&mut self, items: I) -> Result<()>
    where I: Iterator<Item = (Vec<u8>, Vec<u8>)> {
        self.bulk_load_with_before_publish(items, |_, _| Ok(()))
    }

    /// Test seam at the exact Law-3 boundary: packing has returned a fresh
    /// root, but that root is not yet authoritative.
    fn bulk_load_with_before_publish<I, F>(&mut self, items: I, before_publish: F) -> Result<()>
    where
        I: Iterator<Item = (Vec<u8>, Vec<u8>)>,
        F: FnOnce(&Path, u32) -> Result<()>,
    {
        // Guarded like every other writer (Task 17 final review, F5): this
        // is the write that replaces the whole tree AND, via `checkpoint`,
        // discards the log -- the last thing a store with an ambiguous
        // checkpoint behind it should be allowed to do.
        self.refuse_external_workspace()?;
        if self.poisoned { return Err(crate::Error::StorePoisoned); }
        if self.wal.is_none() { return Err(crate::Error::ReadOnly); }
        // A directory keyed only on the process id would be shared by every
        // `bulk_load` call live in this process at once -- and the default
        // test harness runs the tests in `kernel/tests/bulk.rs` concurrently
        // on separate threads of the SAME process, each calling this method.
        // Two concurrent `ExternalSort`s in one directory collide on their
        // `run-NNNNN.tmp` names, and `SortedRuns::drop`'s `remove_dir_all`
        // would delete run files a sibling call is still merging from. The
        // sequence number makes each call's scratch directory its own.
        //
        // `pack_tree` spills each tree level's separators into this same
        // directory rather than inventing a second temp-file scheme with its
        // own lifetime and its own collision rules, so the guarantee this
        // sequence number provides covers the pack's level files too.
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir()
            .join(format!("kernel-sort-{}-{}", std::process::id(), seq));
        let mut s = crate::bulk::ExternalSort::new(&tmp, 64 << 20)?;
        let mut expected_rows = 0u64;
        for (k, v) in items {
            expected_rows = expected_rows.checked_add(1).ok_or(crate::Error::TooLarge)?;
            // Oversized values spill to overflow chains HERE, before the sort,
            // so the sorted stream and the packed leaves only ever carry
            // page-sized records. Chain pages are allocated from the same pool
            // the pack writes through; the marker is what gets sorted.
            if 4 + k.len() + v.len() > crate::page::MAX_RECORD_LEN {
                let (head, crc) = crate::btree::write_overflow(&self.pool, &v)?;
                let m = crate::btree::enc_marker(v.len() as u32, head, crc);
                s.push_flagged(k, m.to_vec(), true)?;
            } else {
                s.push(k, v)?;
            }
        }
        let mut runs = s.finish()?;
        // `&tmp` is the sort's own scratch directory: `pack_tree` spills its
        // level separators there and removes them as it consumes them, and
        // `SortedRuns::drop` takes the directory itself when `runs` falls out
        // of scope below.
        let root = crate::bulk::pack_tree(&self.pool, self.tree_id, runs.iter()?, 0.9, &tmp)?;
        // Make the candidate a physical file, then reopen it through a fresh,
        // fixed-size pool. The old root remains authoritative throughout.
        if let Err(e) = self.pool.flush_all(Barrier::None) {
            self.poisoned = true;
            return Err(e);
        }
        let data = self.dir.join("data");
        before_publish(&data, root)?;
        let verified = crate::verify::verify_file(
            &data,
            self.io_mode,
            root,
            self.tree_id,
            expected_rows,
        )?;
        debug_assert_eq!(verified.rows, expected_rows);
        debug_assert!(verified.pages > 0);
        self.root = root;
        // The old hint names a leaf number that means nothing in the
        // repacked tree -- it may not even be allocated, or may now belong
        // to an unrelated page. `fast_path_leaf`'s checks (tree_id, kind,
        // rightmost, room, ordering) would probably catch a reused page
        // number too, but "probably" is not the standard: a stale hint here
        // has no reason to survive `bulk_load`, so it is cleared rather than
        // left to be caught.
        self.last_leaf.set(None);
        self.checkpoint()
    }

    /// Sort and pack an empty key interval, independently reopen and verify
    /// it, then splice it into the shared tree through a copy-on-write parent
    /// path. Packed pages bypass the per-record WAL because they are
    /// unreachable until the checkpoint publishes the new root.
    ///
    /// The sort arena is fixed at 64 MiB. Inputs larger than that spill
    /// checksummed runs, so RAM is independent of corpus size; the named cost
    /// is scratch space approximately twice the index size plus one readback
    /// verification pass. Packed leaves are 90% full, leaving room for the
    /// first later live write before ordinary split policy takes over.
    pub fn graft_range<I>(&mut self, items: I) -> Result<()>
    where
        I: Iterator<Item = (Vec<u8>, Vec<u8>)>,
    {
        self.graft_range_with_before_publish(items, |_, _| Ok(()))
    }

    /// Test seam after candidate pages have reached the data file but before
    /// the independent reopen and the one logical child replacement.
    fn graft_range_with_before_publish<I, F>(
        &mut self,
        items: I,
        before_publish: F,
    ) -> Result<()>
    where
        I: Iterator<Item = (Vec<u8>, Vec<u8>)>,
        F: FnOnce(&Path, &crate::bulk::PackedRange) -> Result<()>,
    {
        self.refuse_external_workspace()?;
        if self.poisoned { return Err(crate::Error::StorePoisoned); }
        if self.wal.is_none() { return Err(crate::Error::ReadOnly); }

        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = std::env::temp_dir()
            .join(format!("kernel-graft-{}-{}", std::process::id(), seq));
        let mut sort = crate::bulk::ExternalSort::new(&tmp, 64 << 20)?;
        let mut expected_rows = 0u64;
        let mut min: Option<Vec<u8>> = None;
        let mut max: Option<Vec<u8>> = None;

        for (key, value) in items {
            expected_rows = expected_rows.checked_add(1).ok_or(crate::Error::TooLarge)?;
            if min.as_ref().is_none_or(|current| key.as_slice() < current.as_slice()) {
                min = Some(key.clone());
            }
            if max.as_ref().is_none_or(|current| key.as_slice() > current.as_slice()) {
                max = Some(key.clone());
            }
            if 4 + key.len() + value.len() > crate::page::MAX_RECORD_LEN {
                if value.len() > u32::MAX as usize || 4 + key.len() + 12 > crate::page::MAX_RECORD_LEN {
                    return Err(crate::Error::TooLarge);
                }
                let (head, crc) = crate::btree::write_overflow(&self.pool, &value)?;
                let marker = crate::btree::enc_marker(value.len() as u32, head, crc);
                sort.push_flagged(key, marker.to_vec(), true)?;
            } else {
                sort.push(key, value)?;
            }
        }
        let (Some(min), Some(max)) = (min, max) else {
            // An empty graft changes nothing and must not manufacture an empty
            // child that the verifier correctly refuses below a parent.
            return Ok(());
        };

        let mut runs = sort.finish()?;
        self.graft_sorted_range_with_before_publish(
            runs.iter()?, expected_rows, min, max, &tmp, true, before_publish)
    }

    /// Publish a caller's already-sorted, checksummed run stream. This is the
    /// lower half of [`graft_range`]: late-index builders that already paid for
    /// an external sort use it directly instead of materialising or sorting the
    /// same keys a second time.
    ///
    /// `expected_rows`, `min`, and `max` are trusted only for the preflight
    /// overlap probe. `pack_range` recomputes all three and this method refuses
    /// a mismatch before publication.
    pub fn graft_sorted_range<I>(
        &mut self,
        sorted: I,
        expected_rows: u64,
        min: Vec<u8>,
        max: Vec<u8>,
        scratch_dir: &Path,
    ) -> Result<()>
    where
        I: Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>>,
    {
        self.graft_sorted_range_with_before_publish(
            sorted, expected_rows, min, max, scratch_dir, true, |_, _| Ok(()))
    }

    /// Install an independently verified packed range in this writer's
    /// unpublished root. The caller must finish its other namespace changes
    /// and issue one checkpoint; until then snapshot readers continue to use
    /// the previous generation. This is the transaction form of
    /// [`graft_sorted_range`](Self::graft_sorted_range).
    pub fn graft_sorted_range_deferred<I>(
        &mut self,
        sorted: I,
        expected_rows: u64,
        min: Vec<u8>,
        max: Vec<u8>,
        scratch_dir: &Path,
    ) -> Result<()>
    where
        I: Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>>,
    {
        self.graft_sorted_range_with_before_publish(
            sorted, expected_rows, min, max, scratch_dir, false, |_, _| Ok(()))
    }

    /// Pack and independently verify a candidate without publishing it. The
    /// returned descriptor is self-checksummable and sufficient for a later
    /// process to publish the already-written pages without sorting or packing
    /// again. The standing root is unchanged on every return path.
    pub fn prepare_graft_candidate<I>(
        &mut self,
        sorted: I,
        expected_rows: u64,
        min: Vec<u8>,
        max: Vec<u8>,
        scratch_dir: &Path,
    ) -> Result<PreparedGraft>
    where
        I: Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>>,
    {
        self.refuse_external_workspace()?;
        if self.poisoned { return Err(Error::StorePoisoned); }
        if self.wal.is_none() { return Err(Error::ReadOnly); }
        if expected_rows == 0 || min > max { return Err(Error::TooLarge); }
        if let Some(row) = self.scan(&min)?.next() {
            let (key, _) = row?;
            if key <= max { return Err(Error::RangeNotEmpty); }
        }
        let tree = BTree::open(&self.pool, self.tree_id, self.root, &self.last_leaf,
            &self.fast_path_hits, &self.fast_path_attempts);
        let boundary = tree.plan_graft(&min)?;
        drop(tree);
        let last_next = boundary.right_page.unwrap_or(boundary.old_next);
        let packed = crate::bulk::pack_range(
            &self.pool, self.tree_id, sorted, 0.9, scratch_dir, last_next)?;
        if packed.rows != expected_rows || packed.min.as_deref() != Some(min.as_slice())
            || packed.max.as_deref() != Some(max.as_slice()) {
            return Err(Error::Corrupt { page_no: packed.root,
                why: "prepared range disagrees with its sorted-stream manifest" });
        }
        let tree = BTree::open(&self.pool, self.tree_id, self.root, &self.last_leaf,
            &self.fast_path_hits, &self.fast_path_attempts);
        let candidate = tree.build_graft_candidate(&boundary, &packed)?;
        drop(tree);
        if let Err(error) = self.pool.flush_all(Barrier::None) {
            self.poisoned = true;
            return Err(error);
        }
        let write_generation = self.generation.checked_add(1).ok_or(Error::Corrupt {
            page_no: 0, why: "prepared graft generation is exhausted" })?;
        crate::verify::verify_range_file_generation(
            &self.dir.join("data"), self.io_mode, candidate.root, self.tree_id,
            candidate.rows, &candidate.min, &candidate.max, candidate.last_next,
            Some(write_generation))?;
        Ok(PreparedGraft {
            base_generation: self.generation,
            write_generation,
            root: candidate.root,
            rows: candidate.rows,
            min: candidate.min,
            max: candidate.max,
            last_next: candidate.last_next,
            inserted_min: min,
            inserted_max: max,
            inserted_rows: expected_rows,
        })
    }

    /// Verify and publish an already-packed candidate discovered on reopen.
    /// Calling it after the candidate's checkpoint but before scratch cleanup
    /// is idempotent: the published generation and exact inserted interval are
    /// checked, then no second root flip occurs.
    pub fn publish_existing_candidate(&mut self, prepared: &PreparedGraft) -> Result<()> {
        self.refuse_external_workspace()?;
        if self.poisoned { return Err(Error::StorePoisoned); }
        if self.wal.is_none() { return Err(Error::ReadOnly); }
        if prepared.write_generation != prepared.base_generation.checked_add(1)
            .ok_or(Error::TooLarge)? || prepared.inserted_min > prepared.inserted_max
            || prepared.inserted_rows == 0 {
            return Err(Error::Corrupt { page_no: prepared.root,
                why: "prepared graft has an invalid generation or interval" });
        }
        let data = self.dir.join("data");
        crate::verify::verify_range_file_generation(
            &data, self.io_mode, prepared.root, self.tree_id, prepared.rows,
            &prepared.min, &prepared.max, prepared.last_next,
            Some(prepared.write_generation))?;

        if self.generation == prepared.write_generation {
            let mut rows = 0u64;
            self.scan(&prepared.inserted_min)?.for_each_ref(|key, _| {
                if key > prepared.inserted_max.as_slice() { return false; }
                rows = rows.saturating_add(1);
                true
            })?;
            if rows != prepared.inserted_rows {
                return Err(Error::Corrupt { page_no: prepared.root,
                    why: "published graft interval disagrees with its manifest" });
            }
            return Ok(());
        }
        if self.generation != prepared.base_generation {
            return Err(Error::Corrupt { page_no: prepared.root,
                why: "prepared graft belongs to a stale base generation" });
        }
        if let Some(row) = self.scan(&prepared.inserted_min)?.next() {
            let (key, _) = row?;
            if key <= prepared.inserted_max { return Err(Error::RangeNotEmpty); }
        }
        let mut tree = BTree::open(&self.pool, self.tree_id, self.root, &self.last_leaf,
            &self.fast_path_hits, &self.fast_path_attempts);
        let boundary = tree.plan_existing_graft(&prepared.inserted_min)?;
        let retired = tree.install_graft(&boundary, prepared.root, &prepared.min)?;
        self.root = tree.root();
        drop(tree);
        for page in retired { self.pool.free_page(page)?; }
        self.last_leaf.set(None);
        self.checkpoint()
    }

    fn graft_sorted_range_with_before_publish<I, F>(
        &mut self,
        sorted: I,
        expected_rows: u64,
        min: Vec<u8>,
        max: Vec<u8>,
        scratch_dir: &Path,
        publish: bool,
        before_publish: F,
    ) -> Result<()>
    where
        I: Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>>,
        F: FnOnce(&Path, &crate::bulk::PackedRange) -> Result<()>,
    {
        let trace = std::env::var_os("SEKEJAP_LOAD_BREAKDOWN").is_some();
        let total_started = trace.then(std::time::Instant::now);
        self.refuse_external_workspace()?;
        if self.poisoned {
            return Err(crate::Error::StorePoisoned);
        }
        if self.wal.is_none() {
            return Err(crate::Error::ReadOnly);
        }
        if expected_rows == 0 {
            return Ok(());
        }
        if min > max {
            return Err(crate::Error::TooLarge);
        }

        let preflight_started = trace.then(std::time::Instant::now);
        if let Some(row) = self.scan(&min)?.next() {
            let (key, _) = row?;
            if key <= max { return Err(crate::Error::RangeNotEmpty); }
        }

        let tree = BTree::open(
            &self.pool,
            self.tree_id,
            self.root,
            &self.last_leaf,
            &self.fast_path_hits,
            &self.fast_path_attempts,
        );
        let boundary = tree.plan_graft(&min)?;
        drop(tree);
        let last_next = boundary.right_page.unwrap_or(boundary.old_next);
        let preflight =
            preflight_started.map_or(std::time::Duration::ZERO, |started| started.elapsed());

        let pack_started = trace.then(std::time::Instant::now);
        let packed = crate::bulk::pack_range(
            &self.pool,
            self.tree_id,
            sorted,
            0.9,
            scratch_dir,
            last_next,
        )?;
        let pack = pack_started.map_or(std::time::Duration::ZERO, |started| started.elapsed());
        if packed.rows != expected_rows
            || packed.min.as_deref() != Some(min.as_slice())
            || packed.max.as_deref() != Some(max.as_slice())
        {
            return Err(crate::Error::Corrupt {
                page_no: packed.root,
                why: "packed range disagrees with its sorted-stream manifest",
            });
        }
        if packed.last_leaf >= self.pool.page_count() {
            return Err(crate::Error::Corrupt {
                page_no: packed.last_leaf,
                why: "packed range returned a last leaf outside the data file",
            });
        }

        let tree = BTree::open(
            &self.pool,
            self.tree_id,
            self.root,
            &self.last_leaf,
            &self.fast_path_hits,
            &self.fast_path_attempts,
        );
        let candidate = tree.build_graft_candidate(&boundary, &packed)?;
        drop(tree);

        // Materialise candidate bytes, then verify through a different file
        // handle and a 16-page pool. The authoritative root is still unchanged.
        let flush_started = trace.then(std::time::Instant::now);
        if let Err(error) = self.pool.flush_all(Barrier::None) {
            self.poisoned = true;
            return Err(error);
        }
        let flush = flush_started.map_or(std::time::Duration::ZERO, |started| started.elapsed());
        let data = self.dir.join("data");
        before_publish(&data, &packed)?;
        let verify_started = trace.then(std::time::Instant::now);
        let verified = crate::verify::verify_range_file(
            &data,
            self.io_mode,
            candidate.root,
            self.tree_id,
            candidate.rows,
            &candidate.min,
            &candidate.max,
            candidate.last_next,
        )?;
        let verify = verify_started.map_or(std::time::Duration::ZERO, |started| started.elapsed());
        debug_assert_eq!(verified.rows, candidate.rows);
        debug_assert!(verified.pages > 0);

        // Only after verification: build fresh copies of the O(height) parent
        // path and publish their root. Old pages are merely retired for a
        // future safe generation; snapshot readers retain their byte-stable
        // versions throughout.
        let install_started = trace.then(std::time::Instant::now);
        let mut tree = BTree::open(
            &self.pool,
            self.tree_id,
            self.root,
            &self.last_leaf,
            &self.fast_path_hits,
            &self.fast_path_attempts,
        );
        let retired = tree.install_graft(&boundary, candidate.root, &candidate.min)?;
        self.root = tree.root();
        drop(tree);
        for page in retired { self.pool.free_page(page)?; }
        self.last_leaf.set(None);
        let install =
            install_started.map_or(std::time::Duration::ZERO, |started| started.elapsed());
        let publish_started = trace.then(std::time::Instant::now);
        let result = if publish { self.checkpoint() } else { Ok(()) };
        let publication =
            publish_started.map_or(std::time::Duration::ZERO, |started| started.elapsed());
        if trace {
            eprintln!("graft detail: rows={} pages={} bytes={} preflight={:.6}s pack={:.6}s flush={:.6}s verify={:.6}s graft={:.6}s publish={:.6}s total={:.6}s deferred={}",
                expected_rows, verified.pages, verified.pages as u64 * crate::page::PAGE_SIZE as u64,
                preflight.as_secs_f64(), pack.as_secs_f64(), flush.as_secs_f64(),
                verify.as_secs_f64(), install.as_secs_f64(), publication.as_secs_f64(),
                total_started.unwrap().elapsed().as_secs_f64(), !publish);
        }
        result
    }
}

// Tests that need `checkpoint_trace`/`barriers`/`next_lsn` live here, inside
// the crate, where `#[cfg(test)]` actually applies. An integration test in
// kernel/tests/ links the library built WITHOUT `--cfg test` -- that cfg only
// governs the crate's own unit-test binary -- so a test-only method gated
// this way would compile fine and simply not exist from there. The
// public-API durability tests (kernel/tests/durability.rs) need nothing
// test-only and stay where a real consumer of this crate would exercise them.
#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> Config { Config { budget_bytes: 32 << 20, io: IoMode::Buffered, sync: SyncMode::Full } }

    #[test]
    fn byte_policy_publishes_exact_rows_without_a_redundant_wal_barrier() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        s.put(b"old", b"durable").unwrap(); s.commit().unwrap(); s.checkpoint().unwrap();
        let reader = Store::open_snapshot(d.path(), cfg()).unwrap();
        s.barriers.clear();
        s.put(b"new", &[7; 9000]).unwrap();
        let before = s.pool_stats().sync_full_calls;
        assert!(s.commit_with_checkpoint(u64::MAX, 4096).unwrap());
        assert_eq!(s.pool_stats().sync_full_calls - before, 2);
        assert!(s.barriers.is_empty(), "publication already supplies the durability barriers");
        assert_eq!(s.wal.as_ref().unwrap().end_offset(), 0);
        assert_eq!(reader.get(b"new").unwrap(), None);
        drop(s);
        let s = Store::open(d.path(), cfg()).unwrap();
        assert_eq!(s.get(b"old").unwrap().as_deref(), Some(&b"durable"[..]));
        assert_eq!(s.get(b"new").unwrap(), Some(vec![7; 9000]));
    }

    #[test]
    fn byte_policy_keeps_wal_and_old_snapshot_after_data_barrier_failure() {
        let d = tempfile::tempdir().unwrap();
        let (real, _) = open_file(&d.path().join("data"), IoMode::Buffered).unwrap();
        let fio = Arc::new(FailingBarrier { inner: real, fail: false.into() });
        let mut s = Store::create_on(d.path(), cfg(), fio.clone()).unwrap();
        s.put(b"old", b"committed").unwrap(); s.commit().unwrap(); s.checkpoint().unwrap();
        let reader = Store::open_snapshot(d.path(), cfg()).unwrap();
        s.put(b"new", &[8; 9000]).unwrap();
        fio.fail.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(s.commit_with_checkpoint(1,1).is_err());
        assert!(s.wal.as_ref().unwrap().end_offset() > 0);
        assert!(matches!(s.commit_with_checkpoint(1,1), Err(crate::Error::StorePoisoned)));
        assert_eq!(reader.get(b"old").unwrap().as_deref(), Some(&b"committed"[..]));
        assert_eq!(reader.get(b"new").unwrap(), None);
        fio.fail.store(false, std::sync::atomic::Ordering::Relaxed);
        drop(s); drop(fio);
        let s = Store::open(d.path(), cfg()).unwrap();
        assert_eq!(s.get(b"old").unwrap().as_deref(), Some(&b"committed"[..]));
        assert_eq!(s.get(b"new").unwrap(), Some(vec![8; 9000]));
    }

    #[test]
    fn constrained_commit_failure_preserves_published_state_and_snapshot() {
        for barrier in [false, true] {
            let d = tempfile::tempdir().unwrap();
            let (real, _) = open_file(&d.path().join("data"), IoMode::Buffered).unwrap();
            let fio = Arc::new(FailingBarrier { inner: real, fail: false.into() });
            let limits = crate::limits::ResourceLimits { data_bytes: 1 << 20, wal_bytes: 64 << 10,
                tracked_pages: 256, readers: 2, record_bytes: 16000, recovery_bytes: 65536 };
            let mut s = Store::build_on_limited(d.path(), cfg(), true, fio.clone(), IoMode::Buffered, Some(limits)).unwrap();
            s.put(b"old", b"durable").unwrap(); s.commit().unwrap();
            let reader = Store::open_snapshot(d.path(), cfg()).unwrap();
            s.put(b"new", &[8;9000]).unwrap();
            if barrier { fio.fail.store(true, std::sync::atomic::Ordering::Relaxed); }
            if barrier {
                assert!(s.commit().is_err());
                assert!(matches!(s.checkpoint(), Err(Error::StorePoisoned)));
            } // otherwise simulate process exit before commit
            assert_eq!(reader.get(b"old").unwrap(), Some(b"durable".to_vec()));
            assert_eq!(reader.get(b"new").unwrap(), None);
            fio.fail.store(false, std::sync::atomic::Ordering::Relaxed);
            drop(s); drop(fio);
            let s = Store::open(d.path(), cfg()).unwrap();
            assert_eq!(s.get(b"old").unwrap(), Some(b"durable".to_vec()));
            assert_eq!(s.get(b"new").unwrap(), None);
        }
    }

    #[test]
    fn constrained_commit_data_write_failures_do_not_publish_partial_rows() {
        for at in 0..3 {
            let d = tempfile::tempdir().unwrap();
            let (real, _) = open_file(&d.path().join("data"), IoMode::Buffered).unwrap();
            let fio = Arc::new(FailingWrite { inner: real, armed: std::sync::Mutex::new(None) });
            let limits = crate::limits::ResourceLimits { data_bytes: 1 << 20, wal_bytes: 64 << 10,
                tracked_pages: 256, readers: 2, record_bytes: 16000, recovery_bytes: 65536 };
            let mut s = Store::build_on_limited(d.path(), cfg(), true, fio.clone(), IoMode::Buffered, Some(limits)).unwrap();
            s.put(b"old", b"durable").unwrap(); s.commit().unwrap();
            let reader = Store::open_snapshot(d.path(), cfg()).unwrap();
            s.put(b"new", &[8;12000]).unwrap();
            *fio.armed.lock().unwrap() = Some(at);
            assert!(s.commit().is_err());
            assert!(matches!(s.put(b"later", b"no"), Err(Error::StorePoisoned)));
            assert_eq!(reader.get(b"old").unwrap(), Some(b"durable".to_vec()));
            *fio.armed.lock().unwrap() = None;
            drop(s); drop(fio);
            let s = Store::open(d.path(), cfg()).unwrap();
            assert_eq!(s.get(b"old").unwrap(), Some(b"durable".to_vec()));
            assert_eq!(s.get(b"new").unwrap(), None);
        }
    }

    /// F5c: this is a real reader/writer race with a handshake, not a reader
    /// registered before single-threaded churn. The reader has selected
    /// generation 1 and is stopped inside snapshot open. While it is stopped,
    /// the writer publishes enough epochs to make generation-1 pages eligible
    /// for recycling, then overwrites them. A correct open registers a
    /// conservative pin before selecting metadata, so the writer can never
    /// advance past the reader while the hook is held.
    #[test]
    fn a_snapshot_is_registered_before_its_generation_can_be_recycled() {
        let d = tempfile::tempdir().unwrap();
        let tiny = Config { budget_bytes: 1 << 16, io: IoMode::Buffered, sync: SyncMode::Off };
        let mut writer = Store::create(d.path(), tiny).unwrap();
        for i in 0..2_000u64 {
            writer.put(&i.to_be_bytes(), format!("generation one row {i}").as_bytes()).unwrap();
        }
        writer.commit().unwrap();
        writer.checkpoint().unwrap();

        let (selected_tx, selected_rx) = std::sync::mpsc::channel();
        let (churned_tx, churned_rx) = std::sync::mpsc::channel();
        let result = std::thread::scope(|scope| {
            let dir = d.path();
            let reader = scope.spawn(move || {
                let snapshot = Store::open_snapshot_with_after_meta(dir, tiny, |generation| {
                    selected_tx.send(generation).unwrap();
                    churned_rx.recv().unwrap();
                    Ok(())
                })?;
                snapshot.scan(&[])?.collect::<Result<Vec<_>>>()
            });
            let writer_thread = scope.spawn(move || {
                assert_eq!(selected_rx.recv().unwrap(), 1, "fixture must stop after selecting generation 1");
                for round in 0..4u64 {
                    for i in 0..2_000u64 {
                        writer.put(&i.to_be_bytes(),
                                   format!("writer round {round} row {i} is different").as_bytes()).unwrap();
                    }
                    writer.commit().unwrap();
                    writer.checkpoint().unwrap();
                }
                churned_tx.send(()).unwrap();
            });
            writer_thread.join().unwrap();
            reader.join().unwrap()
        });

        let rows = result.expect("a snapshot must not reach recycled pages while it is opening");
        assert_eq!(rows.len(), 2_000);
        for (i, (key, value)) in rows.iter().enumerate() {
            assert_eq!(key.as_slice(), &(i as u64).to_be_bytes());
            assert_eq!(value.as_slice(), format!("generation one row {i}").as_bytes(),
                       "the opening snapshot observed a recycled page at row {i}");
        }
    }

    struct CrashDirectory {
        inner: Box<dyn FileIo>,
        directory_durable: std::sync::atomic::AtomicBool,
    }

    impl FileIo for CrashDirectory {
        fn requires_alignment(&self) -> bool { self.inner.requires_alignment() }
        fn read_at(&self, buf: &mut [u8], off: u64) -> Result<()> { self.inner.read_at(buf, off) }
        fn write_at(&self, buf: &[u8], off: u64) -> Result<()> { self.inner.write_at(buf, off) }
        fn sync_data(&self) -> Result<()> { self.inner.sync_data() }
        fn sync_full(&self) -> Result<()> { self.inner.sync_full() }
        fn sync_full_primitive(&self) -> &'static str { self.inner.sync_full_primitive() }
        fn sync_dir(&self) -> Result<()> {
            self.inner.sync_dir()?;
            self.directory_durable.store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        }
        fn len(&self) -> Result<u64> { self.inner.len() }
        fn set_len(&self, n: u64) -> Result<()> { self.inner.set_len(n) }
    }

    /// F5d: model power loss after an acknowledged commit but before the first
    /// checkpoint. File barriers preserve contents; only a directory barrier
    /// preserves the new data/WAL names. If creation did not issue one, the
    /// crash model drops those volatile names and the acknowledged row vanishes.
    #[test]
    fn a_commit_before_the_first_checkpoint_survives_a_directory_crash() {
        let d = tempfile::tempdir().unwrap();
        let (real, _) = open_file(&d.path().join("data"), IoMode::Buffered).unwrap();
        let fio = Arc::new(CrashDirectory {
            inner: real,
            directory_durable: std::sync::atomic::AtomicBool::new(false),
        });
        {
            let mut s = Store::create_on(d.path(), cfg(), fio.clone()).unwrap();
            s.put(b"acknowledged", b"must survive power loss").unwrap();
            s.commit().unwrap();
        }

        if !fio.directory_durable.load(std::sync::atomic::Ordering::Acquire) {
            std::fs::remove_file(d.path().join("data")).unwrap();
            std::fs::remove_file(d.path().join("wal")).unwrap();
        }
        let reopened = Store::open(d.path(), cfg()).unwrap();
        assert_eq!(reopened.get(b"acknowledged").unwrap().as_deref(),
                   Some(&b"must survive power loss"[..]),
                   "creation must make file names durable before any commit can be acknowledged");
    }

    #[test]
    fn recreated_wal_name_survives_a_commit_before_checkpoint() {
        let d = tempfile::tempdir().unwrap();
        {
            let mut s = Store::create(d.path(), cfg()).unwrap();
            s.put(b"old", b"published").unwrap();
            s.checkpoint().unwrap();
        }
        std::fs::remove_file(d.path().join("wal")).unwrap();
        let (real, mode) = open_file(&d.path().join("data"), IoMode::Buffered).unwrap();
        let fio = Arc::new(CrashDirectory {
            inner: real,
            directory_durable: std::sync::atomic::AtomicBool::new(false),
        });
        {
            let mut s = Store::build_on(d.path(), cfg(), false, fio.clone(), mode).unwrap();
            s.put(b"new", b"acknowledged").unwrap();
            s.commit().unwrap();
        }
        if !fio.directory_durable.load(std::sync::atomic::Ordering::Acquire) {
            std::fs::remove_file(d.path().join("wal")).unwrap();
        }
        drop(fio);
        let s = Store::open(d.path(), cfg()).unwrap();
        assert_eq!(s.get(b"old").unwrap().as_deref(), Some(&b"published"[..]));
        assert_eq!(s.get(b"new").unwrap().as_deref(), Some(&b"acknowledged"[..]));
    }

    #[test]
    fn a_maximum_generation_read_from_disk_is_refused_not_wrapped() {
        let d = tempfile::tempdir().unwrap();
        {
            let s = Store::create(d.path(), cfg()).unwrap();
            Meta {
                format_version: crate::meta::FORMAT_VERSION,
                roots: [s.root, 0, 0, 0, 0, 0, 0, 0],
                next_lsn: s.wal.as_ref().unwrap().next_lsn(),
                generation: u64::MAX,
            }.write_slot(&s.pool).unwrap();
            s.pool.flush_all(Barrier::None).unwrap();
        }

        assert!(matches!(Store::open(d.path(), cfg()), Err(crate::Error::Corrupt { .. })),
                "the generation after u64::MAX does not exist and must not become zero");
    }

    #[test]
    fn a_checkpoint_refuses_when_no_later_page_generation_exists() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        s.generation = u64::MAX - 1;
        s.put(b"pending", b"kept in the log").unwrap();
        s.commit().unwrap();

        assert!(matches!(s.checkpoint(), Err(crate::Error::Corrupt { .. })),
                "publishing the final generation would leave the next epoch wrapping to zero");
        assert!(std::fs::metadata(d.path().join("wal")).unwrap().len() > 0,
                "refusing exhaustion must preserve the committed log");
    }

    #[test]
    fn a_checkpoint_rotates_the_log_only_after_the_pages_are_durable() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        for i in 0..2000u64 { s.put(&i.to_be_bytes(), b"v").unwrap(); }
        s.commit().unwrap();
        s.checkpoint().unwrap();
        let order = s.checkpoint_trace();
        // 2f: the dual-slot flip sits strictly between the data barrier and
        // the WAL rotation -- a flip before the pages are durable can
        // publish roots whose pages never landed; a rotation before the
        // flip is durable drops the only other copy of this epoch.
        assert_eq!(order, vec!["flush_pages", "sync_file", "flip_meta", "rotate_wal"],
                   "checkpoint order must be: data durable, then flip, then drop the log");
        assert!(s.get(&1999u64.to_be_bytes()).unwrap().is_some());
    }

    /// `Wal::scan` derives `next_lsn` from the file, so once `rotate()` empties
    /// it a restart would otherwise renumber from 1 and the LSN space would
    /// repeat. Nothing compares a page's `lsn` against a record's today, so a
    /// repeat is currently harmless -- but it becomes silently wrong the
    /// moment anyone adds the obvious "skip this record, the page already has
    /// a higher lsn" check. `checkpoint` persists `wal.next_lsn()` into
    /// `Meta`, and `open` re-supplies it via `wal.set_lsn_floor`; this test is
    /// the only thing in the suite that would notice if that wiring were
    /// dropped -- `wal.rs`'s own unit tests never call `set_lsn_floor`.
    #[test]
    fn a_rotation_followed_by_a_reopen_does_not_reissue_lsn_1() {
        let d = tempfile::tempdir().unwrap();
        let lsn_at_checkpoint = {
            let mut s = Store::create(d.path(), cfg()).unwrap();
            for i in 0..500u64 { s.put(&i.to_be_bytes(), b"v").unwrap(); }
            s.commit().unwrap();
            s.checkpoint().unwrap();   // rotates the log -- would reset next_lsn to 1 without the floor
            s.next_lsn()
        };
        assert!(lsn_at_checkpoint > 1, "sanity: many records were appended before the rotation");

        let s2 = Store::open(d.path(), cfg()).unwrap();
        assert_eq!(
            s2.next_lsn(), lsn_at_checkpoint,
            "a reopen after rotation must not renumber LSNs from 1"
        );
    }

    /// The three modes must issue three DIFFERENT things. Without this test,
    /// collapsing `Full` and `Normal` back into one call would break nothing --
    /// and a durability label no test defends is decorative, which makes every
    /// benchmark carrying it unattributable. This is the gap the round-1 fix
    /// shipped with: the reviewer found it independently of the coordinator.
    #[test]
    fn the_three_durability_modes_issue_different_barriers() {
        for (mode, want) in [
            (SyncMode::Full,   vec!["sync_full"]),
            (SyncMode::Normal, vec!["sync_data"]),
            (SyncMode::Off,    vec![]),
        ] {
            let d = tempfile::tempdir().unwrap();
            let cfg = Config { budget_bytes: 16 << 20, io: IoMode::Buffered, sync: mode };
            let mut s = Store::create(d.path(), cfg).unwrap();
            s.put(b"k", b"v").unwrap();
            s.commit().unwrap();
            assert_eq!(s.barriers(), want, "{mode:?} issued the wrong barrier");
        }
    }

    // -- Task 16: the append hint lives on `Store`, not on a throwaway `BTree` --

    /// The test Task 15 needed but did not get. Task 15's own fast-path test
    /// (`btree::tests::insert_ascending_uses_fast_path`) holds one long-lived
    /// `BTree` for its whole run and passes -- but that is not what
    /// production, or the benchmark, actually does: `Store::put` built a
    /// fresh `BTree` on every call and dropped it immediately, so
    /// `last_leaf` was `None` on entry to every single insert and the fast
    /// path never fired through `Store` at all. Before Task 16's fix this
    /// assertion reads 0, not `n - 1`; asserting the exact count, not just
    /// `> 0`, is what would have caught a path that only fires sometimes.
    ///
    /// `n` is small enough (as in `btree::tests::insert_ascending_uses_fast_path`)
    /// that these tiny records never fill a single leaf, so no split ever
    /// falls back to a full descent -- a split is a legitimate miss (the
    /// leaf genuinely has no room), and mixing that into this count would
    /// make the assertion about page capacity instead of about the hint.
    #[test]
    fn store_put_ascending_uses_fast_path() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        let n = 100u64;
        for i in 0..n { s.put(&i.to_be_bytes(), b"v").unwrap(); }
        assert_eq!(
            s.fast_path_hits.get(), n - 1,
            "every Store::put but the first must hit the append fast path"
        );
    }

    /// `bulk_load` repacks the whole tree by external sort, so any leaf
    /// number the hint named before the call means nothing afterward -- it
    /// may not even be allocated, or may now hold an unrelated page. A
    /// `Store::put` right after `bulk_load` must still land correctly (via
    /// a real descent, since the hint was cleared) and a full scan must stay
    /// in order.
    #[test]
    fn store_put_survives_bulk_load() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        let n = 1_000u64;
        let items = (0..n).map(|i| (i.to_be_bytes().to_vec(), b"bulk".to_vec()));
        s.bulk_load(items).unwrap();
        assert_eq!(s.last_leaf.get(), None, "bulk_load must clear a hint it just made meaningless");

        // Sorts after every bulk-loaded key.
        let tail_key = n.to_be_bytes();
        s.put(&tail_key, b"tail").unwrap();

        assert_eq!(
            s.get(&tail_key).unwrap().as_deref(), Some(&b"tail"[..]),
            "a put right after bulk_load must be found"
        );

        let scanned: Vec<Vec<u8>> = s.scan(&[]).unwrap().map(|r| r.unwrap().0).collect();
        let mut sorted = scanned.clone();
        sorted.sort();
        assert_eq!(scanned.len(), n as usize + 1);
        assert_eq!(scanned, sorted, "a full scan after bulk_load + put must stay in sorted order");
    }

    /// The correctness net for the hint's new, longer-lived home: the same
    /// 20k keys through `Store::put`, once in ascending order (exercising
    /// the fast path constantly) and once in scattered order (exercising
    /// `descend_for_write` and splits constantly, since a scattered key is
    /// essentially never greater than the rightmost leaf's last key), must
    /// produce byte-identical full scans out of two independent stores.
    #[test]
    fn store_random_order_matches_sequential() {
        let n = 20_000u64;
        let scatter = |i: u64| i.wrapping_mul(0x9E37_79B9_7F4A_7C15);

        let d1 = tempfile::tempdir().unwrap();
        let mut s1 = Store::create(d1.path(), cfg()).unwrap();
        for i in 0..n { s1.put(&i.to_be_bytes(), &i.to_le_bytes()).unwrap(); }

        let d2 = tempfile::tempdir().unwrap();
        let mut s2 = Store::create(d2.path(), cfg()).unwrap();
        let mut order: Vec<u64> = (0..n).collect();
        order.sort_by_key(|&i| scatter(i));
        for &i in &order { s2.put(&i.to_be_bytes(), &i.to_le_bytes()).unwrap(); }

        let seq1: Vec<(Vec<u8>, Vec<u8>)> = s1.scan(&[]).unwrap().map(|r| r.unwrap()).collect();
        let seq2: Vec<(Vec<u8>, Vec<u8>)> = s2.scan(&[]).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(seq1.len(), n as usize);
        assert_eq!(
            seq1, seq2,
            "ascending vs scattered insertion order through Store::put must produce identical scans"
        );
    }

    // -- Task 18: the append hint disarms itself when it stops paying --

    /// Before this task, `insert_into_leaf` re-armed `last_leaf` after EVERY
    /// descent insert whether or not the leaf it wrote was the rightmost
    /// one, and nothing ever cleared a hint that had just failed. On the
    /// probe's own scattered-key workload (MODE=incr: key =
    /// `i.wrapping_mul(0x9E37_79B9_7F4A_7C15)`, a 200-byte payload -- both
    /// matched exactly here, not approximated, because this is the workload
    /// the project is measured on) that meant a
    /// `pool.get_mut` plus a full CRC-verifying `PageRef::open` thrown away
    /// on very nearly every single row, forever: attempts grew with `n`.
    ///
    /// After the fix, an attempt is followed by the hint disarming unless it
    /// actually paid off, so most inserts see `last_leaf.get() == None` and
    /// never call `fast_path_leaf` at all. What is left is (a) the leaf's
    /// worth of inserts before the very first split -- every write lands on
    /// the sole, trivially-rightmost leaf, so the hint stays armed and is
    /// tried again next time regardless of key order -- plus (b) one more
    /// attempt each time a later scattered key happens to be a new running
    /// maximum, which is what actually re-arms the hint after (a) ends. Both
    /// are bounded by leaf capacity and by how many new maxima a random
    /// sequence produces (~log n), not by `n` -- measured at 85/97/107/117/126
    /// attempts for n=2000/5000/10000/20000/40000, a ~1.5x change over a 20x
    /// change in `n`. 20000 is asserted here exactly (117): a bound written
    /// as `< n` would still pass with the pre-fix O(n) behavior and catch
    /// nothing.
    #[test]
    fn random_order_disarms_fast_path() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        let n = 20_000u64;
        let scatter = |i: u64| i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let payload = vec![b'x'; 200]; // same shape as probe's MODE=incr payload
        for i in 0..n { s.put(&scatter(i).to_be_bytes(), &payload).unwrap(); }
        // Neighbor redistribution explicitly disarms the append hint, reducing
        // attempts (101 for this workload). Preserve the original upper bound;
        // the non-redistributing path still has its exact historical count.
        #[cfg(feature = "sqlite-balance")]
        assert!(s.fast_path_attempts.get() <= 117);
        #[cfg(not(feature = "sqlite-balance"))]
        assert_eq!(
            s.fast_path_attempts.get(), 117,
            "a disarming hint must attempt the fast path a number of times bounded by \
             leaf capacity and log(n), not by n -- a bound proportional to n would not \
             have caught the pre-Task-18 defect this test exists for"
        );
    }

    /// The regression guard test 1 needs: a too-aggressive disarm -- say, one
    /// that clears the hint on any descent instead of only on a failed
    /// fast-path attempt -- would silently destroy the append optimisation
    /// for the common ascending-key case (autoincrementing IDs, time-ordered
    /// writes) without any test noticing, since test 1 only bounds an upper
    /// limit. `n` is small enough that these tiny records never fill one
    /// leaf, so no split's legitimate room-check miss dilutes the count: the
    /// only insert that does not hit is the very first, before the hint is
    /// ever armed.
    #[test]
    fn ascending_keeps_fast_path_armed() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        let n = 100u64;
        for i in 0..n { s.put(&i.to_be_bytes(), b"v").unwrap(); }
        assert_eq!(
            s.fast_path_hits.get(), n - 1,
            "an unbroken ascending run must still hit the fast path on every insert but the first"
        );
    }

    /// Ascending, then one out-of-order key, then ascending again above the
    /// prior high-water mark: the fast path must disarm on the out-of-order
    /// key and come back on its own once a genuine append re-establishes it.
    ///
    /// `m = 300` ascending keys is deliberately past this leaf's capacity
    /// (~238 of these 13-byte records), so the run crosses exactly one
    /// split: the insert that fills the leaf is a legitimate room-check miss
    /// (attempted, since the hint was armed, but correctly rejected -- there
    /// is no space), immediately followed by the split path re-arming the
    /// hint onto the new right leaf. That costs one hit out of `m - 1`
    /// attempts, not `m - 1` hits, and is why the first assertion is `m - 2`
    /// rather than `m - 1`. Splitting first also means the tree has more
    /// than one leaf before the out-of-order key lands, so its failure is
    /// the real disarm this test exists to check -- with only one leaf in
    /// the tree that leaf is always "rightmost" and the hint would re-arm to
    /// it immediately regardless of this task's fix, proving nothing.
    ///
    /// Key `0` sorts before everything already inserted, so it lands on the
    /// leftmost leaf -- not the rightmost one the hint pointed at -- and
    /// that leaf's own last key is 300-ish, not `0`, so the ordering check
    /// (check 5) rejects it too if the wrong-leaf check somehow didn't.
    /// Either way it is one attempt, one miss, and (since the leftmost leaf
    /// is not rightmost) the hint stays cleared afterward: `last_leaf` reads
    /// `None`, not just "hits did not increase".
    ///
    /// The next ascending key, `m + 1`, finds the hint disarmed and pays for
    /// one descent -- no attempt is counted for it -- but that descent lands
    /// on the genuine rightmost leaf (it is the new global maximum) and
    /// re-arms the hint there. Every ascending key after that hits again:
    /// `k - 1` more hits and attempts, for a workload small enough (`k =
    /// 30`, well under the ~119 records of headroom left in that leaf) that
    /// no second split dilutes the count.
    #[test]
    fn mixed_workload_rearms() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        let m = 300u64;
        let k = 30u64;

        for i in 1..=m { s.put(&i.to_be_bytes(), b"v").unwrap(); }
        assert_eq!(s.fast_path_attempts.get(), m - 1, "one attempt per insert but the first");
        assert_eq!(
            s.fast_path_hits.get(), m - 2,
            "one miss expected: the insert that fills the leaf and forces the one split \
             this run crosses"
        );

        s.put(&0u64.to_be_bytes(), b"v").unwrap();
        assert_eq!(s.fast_path_attempts.get(), m, "the out-of-order key is one more attempt");
        assert_eq!(s.fast_path_hits.get(), m - 2, "the out-of-order key must not hit");
        assert_eq!(
            s.last_leaf.get(), None,
            "landing on a non-rightmost leaf must leave the hint disarmed, not re-armed \
             to the wrong leaf"
        );

        for i in (m + 1)..(m + 1 + k) { s.put(&i.to_be_bytes(), b"v").unwrap(); }
        assert_eq!(
            s.fast_path_attempts.get(), m + k - 1,
            "k - 1 more attempts: the first post-disarm insert pays for a descent instead"
        );
        assert_eq!(
            s.fast_path_hits.get(), m + k - 3,
            "the fast path must come back: every ascending insert after the one that \
             re-arms it must hit again (m - 2 from the first run, plus k - 1 more here)"
        );
    }

    // -- Task 19: poisoning, both halves. The CHECK (every writer refuses)
    // and the TRIGGER (a failed checkpoint barrier is what sets it), plus
    // the way out, which Law 5 requires any refusal to have.

    /// A `FileIo` whose barrier can be made to fail on demand. `Store` had
    /// no fault-injection seam at all, which is why the poisoning trigger
    /// shipped with zero executable evidence (Task 17 final review, F6):
    /// deleting the `self.poisoned = true` line left the whole suite green.
    struct FailingBarrier { inner: Box<dyn FileIo>, fail: std::sync::atomic::AtomicBool }
    impl FailingBarrier {
        fn failing(&self) -> bool { self.fail.load(std::sync::atomic::Ordering::Relaxed) }
        fn eio() -> crate::Error {
            std::io::Error::other("injected barrier failure").into()
        }
    }
    impl FileIo for FailingBarrier {
        fn requires_alignment(&self) -> bool { self.inner.requires_alignment() }
        fn read_at(&self, buf: &mut [u8], off: u64) -> Result<()> { self.inner.read_at(buf, off) }
        fn write_at(&self, buf: &[u8], off: u64) -> Result<()> { self.inner.write_at(buf, off) }
        fn sync_data(&self) -> Result<()> {
            if self.failing() { return Err(Self::eio()); }
            self.inner.sync_data()
        }
        fn sync_full(&self) -> Result<()> {
            if self.failing() { return Err(Self::eio()); }
            self.inner.sync_full()
        }
        fn sync_full_primitive(&self) -> &'static str { self.inner.sync_full_primitive() }
        fn sync_dir(&self) -> Result<()> { self.inner.sync_dir() }
        fn len(&self) -> Result<u64> { self.inner.len() }
        fn set_len(&self, n: u64) -> Result<()> { self.inner.set_len(n) }
    }

    /// Fail one selected data-page write, after a chosen number of matching
    /// writes have succeeded. The WAL is a separate file, so this reaches the
    /// exact pool eviction boundary without making log append fail first.
    struct FailingWrite {
        inner: Box<dyn FileIo>,
        armed: std::sync::Mutex<Option<usize>>,
    }
    impl FailingWrite {
        fn arm(&self, successful_writes: usize) {
            *self.armed.lock().unwrap() = Some(successful_writes);
        }
    }
    impl FileIo for FailingWrite {
        fn requires_alignment(&self) -> bool { self.inner.requires_alignment() }
        fn read_at(&self, buf: &mut [u8], off: u64) -> Result<()> { self.inner.read_at(buf, off) }
        fn write_at(&self, buf: &[u8], off: u64) -> Result<()> {
            let mut armed = self.armed.lock().unwrap();
            if let Some(remaining) = armed.as_mut() {
                if *remaining == 0 {
                    *armed = None;
                    return Err(std::io::Error::other(
                        "injected page-write failure during a tree mutation",
                    ).into());
                }
                *remaining -= 1;
            }
            self.inner.write_at(buf, off)
        }
        fn sync_data(&self) -> Result<()> { self.inner.sync_data() }
        fn sync_full(&self) -> Result<()> { self.inner.sync_full() }
        fn sync_full_primitive(&self) -> &'static str { self.inner.sync_full_primitive() }
        fn sync_dir(&self) -> Result<()> { self.inner.sync_dir() }
        fn len(&self) -> Result<u64> { self.inner.len() }
        fn set_len(&self, n: u64) -> Result<()> { self.inner.set_len(n) }
    }

    #[test]
    fn deletion_merge_eviction_failures_cannot_publish_partial_changes() {
        for fail_after in 0..12 {
            let d=tempfile::tempdir().unwrap();
            let cfg=Config{budget_bytes:64<<10,io:IoMode::Buffered,sync:SyncMode::Full};
            let (real,_)=open_file(&d.path().join("data"),IoMode::Buffered).unwrap();
            let fio=Arc::new(FailingWrite{inner:real,armed:std::sync::Mutex::new(None)});
            let mut s=Store::create_on(d.path(),cfg,fio.clone()).unwrap();
            for i in 0u64..1024 {s.put(&i.to_be_bytes(),&vec![7;240]).unwrap();}
            // Fifteen 256-byte cells fit each full leaf. Keep six, just
            // above the maintenance threshold; subsequent hits force merges.
            for i in 0u64..1024 {if i%15>=6 {s.delete(&i.to_be_bytes()).unwrap();}}
            s.checkpoint().unwrap();
            let old=Store::open_snapshot(d.path(),cfg).unwrap();
            fio.arm(fail_after);let mut failed=false;
            for i in 0u64..1024 {
                if let Err(e)=s.delete(&((i*71)%1024).to_be_bytes()) {
                    assert!(matches!(e,crate::Error::Io(_)));failed=true;break;
                }
            }
            assert!(failed,"fault {fail_after} was not reached");
            assert!(s.commit().is_err());assert!(s.checkpoint().is_err());
            for i in 0u64..1024 {assert_eq!(old.get(&i.to_be_bytes()).unwrap(),(i%15<6).then(||vec![7;240]));}
            drop(old);drop(s);
            let reopened=Store::open(d.path(),cfg).unwrap();
            for i in 0u64..1024 {assert_eq!(reopened.get(&i.to_be_bytes()).unwrap(),(i%15<6).then(||vec![7;240]));}
            assert_eq!(crate::verify::verify_published_tree(&d.path().join("data"),IoMode::Buffered,reopened.root,1).unwrap().0,412);
        }
    }

    /// F7's reachable tear, not merely an error before a mutation starts.
    /// Earlier writes are committed in the same unpublished epoch. The next
    /// scattered insert splits the root leaf, rewrites its left half, then
    /// fails while allocating the new parent; committed keys moved right are
    /// no longer reachable from the still-leaf root.
    #[test]
    fn a_partial_tree_mutation_poisons_and_reopen_replays_the_retained_log() {
        let d = tempfile::tempdir().unwrap();
        let tiny = Config {
            budget_bytes: 16 * crate::page::PAGE_SIZE,
            io: IoMode::Buffered,
            sync: SyncMode::Off,
        };
        let (real, _) = open_file(&d.path().join("data"), IoMode::Buffered).unwrap();
        let fio = Arc::new(FailingWrite {
            inner: real,
            armed: std::sync::Mutex::new(None),
        });
        let mut s = Store::create_on(d.path(), tiny, fio.clone()).unwrap();

        let value = vec![b'v'; 256];
        let record_bytes = 4 + 8 + value.len() + 4;
        let mut rows = 0u64;
        loop {
            let room = {
                let r = s.pool.get(s.root).unwrap();
                crate::page::PageRef::open_resident(&r, s.root).unwrap().free_space()
            };
            if room < record_bytes { break; }
            s.put(&(rows * 2).to_be_bytes(), &value).unwrap();
            s.commit().unwrap();
            rows += 1;
        }
        assert!(rows > 4, "fixture must have committed rows to move across the split");

        // Fill every initially-unused frame, then pin the root while replacing
        // all other residents with dirty throwaway pages. The split's first
        // allocation can therefore evict once; its second allocation (the new
        // root, after the old leaf was rewritten) reaches the injected failure.
        while s.pool.page_count() < 16 {
            drop(s.pool.allocate().unwrap());
        }
        {
            let root_pin = s.pool.get(s.root).unwrap();
            for _ in 0..15 { drop(s.pool.allocate().unwrap()); }
            drop(root_pin);
        }

        let wal_before = std::fs::metadata(d.path().join("wal")).unwrap().len();
        fio.arm(1);
        let failed = s.put(&1u64.to_be_bytes(), &value);
        assert!(matches!(failed, Err(crate::Error::Io(_))),
                "the injected eviction failure must escape the mutating insert, got {failed:?}");
        assert_eq!(s.get(&((rows - 1) * 2).to_be_bytes()).unwrap(), None,
                   "fixture must prove the insert failed after committed keys moved off the root");

        // Exercise both dangerous retries before asserting: on the old code
        // commit succeeds and checkpoint then publishes the tear and rotates
        // away the only log that could rebuild it.
        let commit = s.commit();
        let checkpoint = s.checkpoint();
        let wal_after = std::fs::metadata(d.path().join("wal")).unwrap().len();
        assert!(matches!(commit, Err(crate::Error::StorePoisoned)),
                "a partial tree mutation must make commit refuse, got {commit:?}");
        assert!(matches!(checkpoint, Err(crate::Error::StorePoisoned)),
                "a partial tree mutation must make checkpoint refuse, got {checkpoint:?}");
        assert_eq!(wal_after, wal_before,
                   "a poisoned store must retain the committed recovery log byte-for-byte");

        drop(s);
        drop(fio);
        let reopened = Store::open(d.path(), tiny)
            .expect("reopen must rebuild a poisoned handle from the retained log");
        for i in 0..rows {
            assert_eq!(reopened.get(&(i * 2).to_be_bytes()).unwrap().as_deref(), Some(value.as_slice()));
        }
        assert_eq!(reopened.get(&1u64.to_be_bytes()).unwrap(), None,
                   "the failed, uncommitted insert must not be replayed");
    }

    /// A validation refusal happens before the WAL and before any tree page is
    /// touched. It is a caller-correctable input error, not evidence of a torn
    /// tree, and must not turn the handle into a denial of service.
    #[test]
    fn a_pre_mutation_validation_error_does_not_poison_the_store() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        let oversized_key = vec![b'k'; crate::page::MAX_RECORD_LEN];
        assert!(matches!(s.put(&oversized_key, b"v"), Err(crate::Error::TooLarge)));
        s.put(b"usable", b"still").unwrap();
        s.commit().unwrap();
        s.checkpoint().unwrap();
        assert_eq!(s.get(b"usable").unwrap().as_deref(), Some(&b"still"[..]));
    }

    /// THE TRIGGER, executed rather than asserted by inspection. A
    /// checkpoint whose barrier fails must (a) surface the error and (b)
    /// leave the store refusing every subsequent write -- above all
    /// `checkpoint` itself, whose last step deletes the log (see
    /// `Error::StorePoisoned`). Deleting `checkpoint`'s `self.poisoned =
    /// true` line makes every assertion after the first one fail: the store
    /// happily accepts a `put`, a `commit`, and another `checkpoint` -- and
    /// that second checkpoint rotates the log away.
    #[test]
    fn a_failed_checkpoint_barrier_poisons_every_writer() {
        let d = tempfile::tempdir().unwrap();
        let (real, _) = open_file(&d.path().join("data"), IoMode::Buffered).unwrap();
        let fio = Arc::new(FailingBarrier { inner: real, fail: false.into() });
        let mut s = Store::create_on(d.path(), cfg(), fio.clone()).unwrap();
        s.put(b"a", b"1").unwrap();
        s.commit().unwrap();
        s.checkpoint().expect("sanity: the checkpoint works while the disk does");

        s.put(b"b", b"2").unwrap();
        s.commit().unwrap();
        fio.fail.store(true, std::sync::atomic::Ordering::Relaxed);

        match s.checkpoint() {
            Err(crate::Error::Io(_)) => {}
            other => panic!("a failing barrier must surface, got {other:?}"),
        }
        fio.fail.store(false, std::sync::atomic::Ordering::Relaxed);   // the disk "recovers"

        assert!(matches!(s.put(b"c", b"3"), Err(crate::Error::StorePoisoned)));
        assert!(matches!(s.delete(b"a"), Err(crate::Error::StorePoisoned)));
        assert!(matches!(s.commit(), Err(crate::Error::StorePoisoned)));
        assert!(matches!(s.checkpoint(), Err(crate::Error::StorePoisoned)),
                "a retried checkpoint would flush nothing (those frames are marked clean \
                 already) and then rotate the log away");
        let items = vec![(b"z".to_vec(), b"9".to_vec())].into_iter();
        assert!(matches!(s.bulk_load(items), Err(crate::Error::StorePoisoned)),
                "bulk_load replaces the whole tree AND discards the log");
        // And it must refuse BEFORE doing any of that, not fail on its way
        // out. Without its own guard, `bulk_load` still returns
        // `StorePoisoned` -- from the `checkpoint()` it ends with -- having
        // already repacked the tree and republished the root, which is the
        // assertion below (and only the assertion below) that notices.
        assert_eq!(s.get(b"a").unwrap().as_deref(), Some(&b"1"[..]),
                   "a refused bulk_load must not have replaced the tree");
        assert_eq!(s.get(b"z").unwrap(), None);
    }

    /// The other half of the same guarantee, and the one Law 5 demands: a
    /// refusal with no way out is itself an unrecoverable state. Poisoning
    /// is per-instance and never persisted, so dropping the store and
    /// reopening the directory clears it -- and that reopen is not a way of
    /// ignoring the problem, it is what re-derives every belief from disk
    /// and replays the log that was never rotated away.
    #[test]
    fn reopening_clears_poisoning_and_the_committed_data_is_still_there() {
        let d = tempfile::tempdir().unwrap();
        let (real, _) = open_file(&d.path().join("data"), IoMode::Buffered).unwrap();
        let fio = Arc::new(FailingBarrier { inner: real, fail: false.into() });
        {
            let mut s = Store::create_on(d.path(), cfg(), fio.clone()).unwrap();
            s.put(b"a", b"1").unwrap();
            s.commit().unwrap();
            fio.fail.store(true, std::sync::atomic::Ordering::Relaxed);
            assert!(s.checkpoint().is_err());
            assert!(matches!(s.put(b"b", b"2"), Err(crate::Error::StorePoisoned)));
        }
        fio.fail.store(false, std::sync::atomic::Ordering::Relaxed);

        let mut s2 = Store::open(d.path(), cfg()).expect("a poisoned store must not poison the DIRECTORY");
        assert_eq!(s2.get(b"a").unwrap().as_deref(), Some(&b"1"[..]),
                   "the committed row is in the log, which the failed checkpoint never rotated");
        s2.put(b"b", b"2").unwrap();
        s2.commit().unwrap();
    }

    /// CRC-valid does not mean structurally valid. Recovery must reject a
    /// malformed frame without indexing through unchecked bytes and without
    /// letting any part of that frame reach the tree.
    #[test]
    fn malformed_wal_payloads_are_bounded_and_do_not_mutate_the_tree() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        s.put(b"kept", b"value").unwrap();
        s.commit().unwrap();
        let root = s.root;

        let malformed: &[(RecKind, &[u8])] = &[
            (RecKind::Put, &[]),
            (RecKind::Put, &[4, 0, b'a']),
            (RecKind::Delete, &[]),
            (RecKind::Delete, &[2, 0, b'a']),
            (RecKind::Delete, &[0, 0, b'x']),
            (RecKind::DeletePrefix, &[]),
            (RecKind::PutEmptyBatch, &[]),
            (RecKind::PutEmptyBatch, &[1, 0, 4, 0, b'a']),
            (RecKind::PutEmptyBatch, &[65, 0]),
            (RecKind::PutEmptyBatch, &[1, 0, 1, 0, b'a', b'x']),
            (RecKind::Commit, b"not empty"),
            (RecKind::PageImage, b"unsupported"),
        ];
        for (n, &(kind, payload)) in malformed.iter().enumerate() {
            assert!(matches!(
                s.apply(kind, payload, n as u64),
                Err(crate::Error::CorruptWal { offset, .. }) if offset == n as u64
            ));
            assert_eq!(s.root, root, "malformed frame {n} changed the root");
            assert_eq!(s.get(b"kept").unwrap().as_deref(), Some(&b"value"[..]),
                "malformed frame {n} changed existing data");
        }
    }

    #[test]
    fn bounded_empty_key_batch_replays_as_one_committed_wal_unit() {
        let d = tempfile::tempdir().unwrap();
        {
            let mut s = Store::create(d.path(), cfg()).unwrap();
            s.put_empty_batch(&[b"alpha".to_vec(), b"beta".to_vec()]).unwrap();
            s.commit().unwrap();
        }
        let s = Store::open(d.path(), cfg()).unwrap();
        assert_eq!(s.get(b"alpha").unwrap().as_deref(), Some(&b""[..]));
        assert_eq!(s.get(b"beta").unwrap().as_deref(), Some(&b""[..]));
    }

    #[test]
    fn a_corrupted_packed_tree_never_becomes_authoritative() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        s.put(b"old", b"authoritative").unwrap();
        s.commit().unwrap();
        s.checkpoint().unwrap();
        let old_root = s.root;

        let result = s.bulk_load_with_before_publish(
            (0..2_000u64).map(|i| (i.to_be_bytes().to_vec(), b"new".to_vec())),
            |data, root| {
                use std::io::{Seek, SeekFrom, Write};
                let mut file = std::fs::OpenOptions::new().write(true).open(data)?;
                file.seek(SeekFrom::Start(root as u64 * PAGE_SIZE as u64))?;
                file.write_all(&[0u8])?;
                file.sync_all()?;
                Ok(())
            },
        );

        assert!(result.is_err(), "a packed tree corrupted before publication must be refused");
        assert_eq!(s.root, old_root, "the old root must stay authoritative after refusal");
        assert_eq!(s.get(b"old").unwrap().as_deref(), Some(&b"authoritative"[..]));
    }

    fn graft_key(space: u8, i: u32) -> Vec<u8> {
        let mut key = vec![space];
        key.extend_from_slice(&i.to_be_bytes());
        key
    }

    fn seed_graft_base(store: &mut Store) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut rows = Vec::new();
        for space in [0x10, 0x30] {
            for i in 0..1_500u32 {
                let row = (graft_key(space, i), format!("base-{space:02x}-{i}").into_bytes());
                store.put(&row.0, &row.1).unwrap();
                rows.push(row);
            }
        }
        store.commit().unwrap();
        store.checkpoint().unwrap();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    fn graft_rows() -> Vec<(Vec<u8>, Vec<u8>)> {
        (0..2_000u32)
            .rev()
            .map(|i| (graft_key(0x20, i), format!("graft-{i}").into_bytes()))
            .collect()
    }

    fn collect_from(store: &Store, from: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        store.scan(from).unwrap().map(Result::unwrap).collect()
    }

    fn collect_below(store: &Store, to: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut rows = Vec::new();
        store.scan_reverse(to).unwrap().for_each_ref(|key, value| {
            rows.push((key.to_vec(), value.to_vec()));
            true
        }).unwrap();
        rows
    }

    /// Stage 1's first obligation: installing an empty key range may not
    /// replace the shared tree. Keys on both sides exercise a true mid-tree
    /// graft rather than the easier leftmost/rightmost append cases.
    #[test]
    fn graft_preserves_every_preexisting_key() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        let base = seed_graft_base(&mut s);
        let pinned = Store::open_snapshot(d.path(), cfg()).unwrap();

        s.graft_range(graft_rows().into_iter()).unwrap();

        for (key, value) in &base {
            assert_eq!(s.get(key).unwrap().as_deref(), Some(value.as_slice()),
                "graft lost pre-existing key {key:?}");
        }
        assert_eq!(s.scan(&[]).unwrap().count(), base.len() + 2_000);
        assert!(pinned.get(&graft_key(0x20, 17)).unwrap().is_none(),
            "a reader pinned before publication must remain on the old generation");
        assert_eq!(pinned.scan(&[]).unwrap().count(), base.len());
        let fresh = Store::open_snapshot(d.path(), cfg()).unwrap();
        assert_eq!(fresh.get(&graft_key(0x20, 17)).unwrap().as_deref(), Some(&b"graft-17"[..]));
    }

    /// THE EQUIVALENCE ORACLE: construction is allowed to change, answers are
    /// not. Point, forward-range, and reverse-range queries must be byte-for-
    /// byte identical to the ordinary insertion path over the same rows.
    #[test]
    fn graft_and_individual_inserts_answer_every_kernel_query_identically() {
        let dg = tempfile::tempdir().unwrap();
        let di = tempfile::tempdir().unwrap();
        let mut grafted = Store::create(dg.path(), cfg()).unwrap();
        let mut inserted = Store::create(di.path(), cfg()).unwrap();
        seed_graft_base(&mut grafted);
        seed_graft_base(&mut inserted);
        let rows = graft_rows();

        grafted.graft_range(rows.clone().into_iter()).unwrap();
        for (key, value) in &rows { inserted.put(key, value).unwrap(); }
        inserted.commit().unwrap();
        inserted.checkpoint().unwrap();

        for (key, _) in seed_query_keys(&rows) {
            assert_eq!(grafted.get(&key).unwrap(), inserted.get(&key).unwrap(),
                "point query disagreed at {key:?}");
        }
        for from in [vec![], graft_key(0x10, 777), graft_key(0x20, 0),
                     graft_key(0x20, 999), graft_key(0x30, 0), vec![0xff]] {
            assert_eq!(collect_from(&grafted, &from), collect_from(&inserted, &from),
                "forward range disagreed from {from:?}");
        }
        for to in [graft_key(0x10, 0), graft_key(0x20, 0), graft_key(0x20, 999),
                   graft_key(0x30, 0), vec![0xff]] {
            assert_eq!(collect_below(&grafted, &to), collect_below(&inserted, &to),
                "reverse range disagreed below {to:?}");
        }

        // LIVE WRITES stay on the existing path after publication. Exercise a
        // later key in the grafted namespace, including its first packed-page
        // split, and compare again.
        let live_key = graft_key(0x20, 2_500);
        grafted.put(&live_key, b"later-live-write").unwrap();
        inserted.put(&live_key, b"later-live-write").unwrap();
        grafted.commit().unwrap();
        inserted.commit().unwrap();
        grafted.checkpoint().unwrap();
        inserted.checkpoint().unwrap();
        assert_eq!(collect_from(&grafted, &graft_key(0x20, 1_900)),
                   collect_from(&inserted, &graft_key(0x20, 1_900)));
    }

    #[test]
    fn graft_refuses_a_nonempty_range_without_changing_the_tree() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        let base = seed_graft_base(&mut s);
        let old_root = s.root;
        let rows = vec![
            (graft_key(0x0f, 0), b"before".to_vec()),
            (graft_key(0x10, 10), b"overlap".to_vec()),
        ];
        assert!(matches!(s.graft_range(rows.into_iter()), Err(crate::Error::RangeNotEmpty)));
        assert_eq!(s.root, old_root);
        assert_eq!(s.scan(&[]).unwrap().count(), base.len());
        for (key, value) in base {
            assert_eq!(s.get(&key).unwrap().as_deref(), Some(value.as_slice()));
        }
    }

    #[test]
    fn graft_into_an_empty_tree_becomes_the_tree() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        let rows = graft_rows();
        s.graft_range(rows.clone().into_iter()).unwrap();
        assert_eq!(s.scan(&[]).unwrap().count(), rows.len());
        for (key, value) in rows.iter().step_by(97) {
            assert_eq!(s.get(key).unwrap().as_deref(), Some(value.as_slice()));
        }
    }

    fn seed_query_keys(rows: &[(Vec<u8>, Vec<u8>)]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut keys = rows.to_vec();
        keys.push((graft_key(0x10, 0), Vec::new()));
        keys.push((graft_key(0x10, 1_499), Vec::new()));
        keys.push((graft_key(0x30, 0), Vec::new()));
        keys.push((graft_key(0x30, 1_499), Vec::new()));
        keys.push((graft_key(0x20, 2_001), Vec::new()));
        keys
    }

    /// Packed bytes have no WAL. Before the verified subtree is linked, a
    /// crash must reopen the old generation; after the link returns, the new
    /// generation must already be durable without a caller checkpoint.
    #[test]
    fn graft_crash_boundary_is_old_before_and_durable_after() {
        let before_dir = tempfile::tempdir().unwrap();
        {
            let mut s = Store::create(before_dir.path(), cfg()).unwrap();
            seed_graft_base(&mut s);
            let old_root = s.root;
            let result = s.graft_range_with_before_publish(
                graft_rows().into_iter(),
                |_, _| Err(std::io::Error::other("crash before graft").into()),
            );
            assert!(result.is_err());
            assert_eq!(s.root, old_root, "a pre-publication failure changed the live root");
        }
        let before = Store::open(before_dir.path(), cfg()).unwrap();
        assert!(before.get(&graft_key(0x20, 17)).unwrap().is_none());
        assert_eq!(before.scan(&[]).unwrap().count(), 3_000);

        let after_dir = tempfile::tempdir().unwrap();
        {
            let mut s = Store::create(after_dir.path(), cfg()).unwrap();
            seed_graft_base(&mut s);
            s.graft_range(graft_rows().into_iter()).unwrap();
            // No commit and no extra checkpoint.
        }
        let after = Store::open(after_dir.path(), cfg()).unwrap();
        assert_eq!(after.get(&graft_key(0x20, 17)).unwrap().as_deref(), Some(&b"graft-17"[..]));
        assert_eq!(after.scan(&[]).unwrap().count(), 5_000);
    }

    /// Kill after packing, resume and publish, kill after publication, then
    /// resume again before cleanup. The second resume must recognize that the
    /// exact generation is already authoritative rather than grafting twice.
    #[test]
    fn prepared_graft_publishes_after_reopen_and_resume_of_resume_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let scratch = d.path().join("prepared-graft-scratch");
        let manifest = d.path().join("prepared-graft");
        {
            let mut s = Store::create(d.path(), cfg()).unwrap();
            seed_graft_base(&mut s);
            let mut rows = graft_rows();
            rows.sort_by(|a, b| a.0.cmp(&b.0));
            let min = rows.first().unwrap().0.clone();
            let max = rows.last().unwrap().0.clone();
            let prepared = s.prepare_graft_candidate(
                rows.into_iter().map(|(k, v)| Ok((k, v, false))),
                2_000, min, max, &scratch).unwrap();
            prepared.write_manifest(&manifest).unwrap();
            assert!(s.get(&graft_key(0x20, 17)).unwrap().is_none(),
                "preparing a candidate must not publish it in the live handle");
            // Simulated kill before root publication.
        }
        std::fs::write(manifest.with_extension("tmp"), b"torn next candidate").unwrap();
        let prepared = PreparedGraft::read_manifest(&manifest).unwrap();
        {
            let mut resumed = Store::open(d.path(), cfg()).unwrap();
            assert!(resumed.get(&graft_key(0x20, 17)).unwrap().is_none());
            resumed.publish_existing_candidate(&prepared).unwrap();
            assert_eq!(resumed.get(&graft_key(0x20, 17)).unwrap().as_deref(),
                Some(&b"graft-17"[..]));
            // Simulated kill after checkpoint, before manifest cleanup.
        }
        {
            let mut resumed_again = Store::open(d.path(), cfg()).unwrap();
            let generation = resumed_again.generation;
            resumed_again.publish_existing_candidate(&prepared).unwrap();
            assert_eq!(resumed_again.generation, generation,
                "resume-of-resume must not publish another generation");
            assert_eq!(resumed_again.scan(&[]).unwrap().count(), 5_000);
        }
    }

    #[test]
    fn prepared_graft_manifest_and_generation_are_both_enforced() {
        let d = tempfile::tempdir().unwrap();
        let scratch = d.path().join("prepared-graft-scratch");
        let mut s = Store::create(d.path(), cfg()).unwrap();
        seed_graft_base(&mut s);
        let mut rows = graft_rows();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        let prepared = s.prepare_graft_candidate(
            rows.clone().into_iter().map(|(k, v)| Ok((k, v, false))), 2_000,
            rows.first().unwrap().0.clone(), rows.last().unwrap().0.clone(), &scratch).unwrap();
        let mut damaged = prepared.encode().unwrap();
        damaged[20] ^= 0x80;
        assert!(PreparedGraft::decode(&damaged).is_err());

        let mut wrong_generation = prepared.clone();
        wrong_generation.base_generation += 1;
        wrong_generation.write_generation += 1;
        assert!(s.publish_existing_candidate(&wrong_generation).is_err(),
            "candidate pages stamped for one generation must not publish in another");
        assert!(s.get(&graft_key(0x20, 17)).unwrap().is_none());
    }

    /// Verification is through an independent reopen. Damage a packed page
    /// after the pool flush but before publication: the old root must remain
    /// authoritative and the damaged candidate must never become reachable.
    #[test]
    fn corrupt_packed_range_is_caught_and_never_published() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        seed_graft_base(&mut s);
        let old_root = s.root;

        let result = s.graft_range_with_before_publish(
            graft_rows().into_iter(),
            |data, packed| {
                use std::io::{Seek, SeekFrom, Write};
                let mut file = std::fs::OpenOptions::new().write(true).open(data)?;
                file.seek(SeekFrom::Start(packed.root as u64 * PAGE_SIZE as u64 + 80))?;
                file.write_all(&[0xa5])?;
                file.sync_all()?;
                Ok(())
            },
        );

        assert!(matches!(result, Err(crate::Error::Corrupt { .. })),
            "a corrupt packed page must be refused, got {result:?}");
        assert_eq!(s.root, old_root);
        assert_eq!(s.get(&graft_key(0x10, 17)).unwrap().as_deref(), Some(&b"base-10-17"[..]));
        assert!(s.get(&graft_key(0x20, 17)).unwrap().is_none());
    }

    /// An oversized record must be refused BEFORE it reaches the log, and
    /// the refusal must cost the caller nothing else -- neither `put` nor
    /// `delete` may leave a frame behind that nothing will ever apply.
    /// `delete`'s bound is on its PAYLOAD (`2 + k.len()`), not its key: the
    /// two differ by exactly the two bytes that made a 4,052-byte key
    /// produce a 4,054-byte frame (Task 17 final review, F1).
    #[test]
    fn an_oversized_record_never_reaches_the_log() {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(d.path(), cfg()).unwrap();
        s.put(b"a", b"1").unwrap();
        s.commit().unwrap();
        let before = s.wal.as_ref().unwrap().end_offset();

        let max = crate::page::MAX_RECORD_LEN;
        // A huge VALUE is legal now (overflow chains) -- what must never reach
        // the log is a record nothing could apply: a KEY too big for a leaf
        // even as a marker record.
        let k = vec![b'k'; max];
        assert!(matches!(s.put(&k, b"v"), Err(crate::Error::TooLarge)));
        // A delete whose PAYLOAD is over the bound while its KEY is not:
        // the exact two-byte band that bricked a store in round 3.
        let k = vec![b'k'; max - 1];
        assert!(!s.delete(&k).unwrap(),
                "a key that long can never have been inserted, so `not found` is the truth");
        // Each key fits alone, but their encoded batch would exceed the
        // classifier's one-frame bound. Refuse the whole unit before WAL.
        let half = vec![b'b'; max / 2];
        assert!(matches!(
            s.put_empty_batch(&[half.clone(), half]),
            Err(crate::Error::TooLarge)
        ));
        assert_eq!(
            s.wal.as_ref().unwrap().end_offset(), before,
            "neither refusal may append a single byte to the log"
        );
    }
}
