//! A fixed array of frames, allocated once and never grown.
//!
//! Eviction is CLOCK: one reference bit per frame, a hand that sweeps. LRU's
//! per-entry links are pure overhead at this budget.
//!
//! SACRIFICE (Law 4): CLOCK approximates LRU, so a pathological access order can
//! evict a page that LRU would have kept. Bought: one bit per frame instead of
//! two pointers, and no list surgery on every hit.
//!
//! Each frame carries a pin count plus a `writer` flag. The count keeps a
//! frame out of `victim`'s reach while any guard references it; the flag
//! additionally enforces reader/writer exclusivity between `get` and
//! `get_mut` on the same page, the same way `RefCell` enforces it between
//! `borrow` and `borrow_mut` — a conflicting request panics rather than
//! handing out a second `&mut [u8]` over memory another guard already
//! points into. Without that flag, `get` and `get_mut` on the same page_no
//! could alias a live `&[u8]`/`&mut [u8]` pair, which the `unsafe fn`s on
//! `AlignedRegion` exist specifically to rule out.

use crate::budget::{Class, MemoryBudget, Reservation};
use crate::io::{AlignedRegion, Barrier, FileIo};
use crate::page::PAGE_SIZE;
use crate::{Error, Result};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

const FREE_MAGIC: [u8; 8] = *b"SEKFREE\0";
const FREE_VERSION: u16 = 2;
const FREE_HEADER_LEN: usize = 24;

#[derive(Debug, Default, Clone, Copy)]
pub struct PoolStats {
    pub hits: u64, pub misses: u64, pub evictions: u64,
    /// Total frames the pool reserved at open -- its CAPACITY, not its
    /// residency. The region is allocated once and never grown, so this is
    /// the anonymous memory the pool holds from open onward whether or not
    /// the frames are populated.
    ///
    /// It was called `frames_used`, and that name cost a reviewer a false
    /// finding: 170 MB "used" against a 5 MB database reads as an accounting
    /// bug rather than as a preallocated arena working exactly as designed. A
    /// number that invites a wrong inference is a reporting defect even when
    /// the arithmetic is right. See also `a_full_scan_pins_one_leaf_at_a_time`
    /// in btree.rs, which independently found this field cannot move and so
    /// cannot be asserted on for any specific operation -- `peak_pins` is the
    /// field for that.
    pub frames_total: usize,
    pub peak_pins: u32,
    /// Barriers actually placed by `flush_all`, counted at the point they are
    /// issued rather than where they are chosen. Asserting on what a caller
    /// like `Store::checkpoint` *passed* would only prove it passed
    /// something -- these live here because this is where the syscall
    /// actually happens, so a caller that silently ignored its own argument
    /// could not still make the test pass.
    pub sync_data_calls: u64,
    pub sync_full_calls: u64,
}

struct Frame {
    page_no: u32,
    present: bool,
    dirty: bool,
    referenced: bool,
    /// Number of live PinnedRead/PinnedWrite guards over this frame.
    pins: u32,
    /// True while one of those live guards is a PinnedWrite. Invariant:
    /// `writer` implies `pins > 0`.
    writer: bool,
    /// True once the slot directory passed full validation during THIS
    /// residency (2f-A1). Cleared on every load from disk; our own PageMut
    /// writes maintain the slot invariants, so they do not clear it.
    validated: bool,
}

struct Inner {
    sweep_steps: u64,
    frames: Vec<Frame>,
    table: HashMap<u32, usize>,
    hand: usize,
    stats: PoolStats,
    next_page: u32,
    limits: Option<crate::limits::ResourceLimits>,
    free_count: usize,
    epoch_allocated_pages: u64,
    /// Generation stamped into every page sealed to disk (2n): the epoch
    /// being built, i.e. last published generation + 1.
    stamp_gen: u64,
    /// Pages superseded by CoW shadows, keyed by the epoch that freed them
    /// (2n). A page freed during epoch g still serves the g-1 tree until
    /// epoch g+1 publishes (the dual-slot fallback), so it may be reused
    /// only once `reuse_limit >= g` -- the store advances the limit after
    /// each checkpoint, already min'd against the oldest live reader.
    free: std::collections::BTreeMap<u64, Vec<u32>>,
    /// Highest freed-generation currently safe to recycle (0 = nothing).
    reuse_limit: u64,
    /// Verified birth evidence for retired tree and overflow pages.
    /// Zero/unknown birth evidence keeps the conservative generation horizon.
    free_birth: HashMap<u32, u64>,
    promotion_readers: Vec<u64>,
    promotion_checked_through: u64,
    /// Pages recycled during the CURRENT epoch: exempt from the frozen
    /// check (their number is below the boundary but their content is
    /// this epoch's). Cleared when the boundary advances.
    thawed: std::collections::HashSet<u32>,
    live_pins: u32,
    /// See `set_frozen_boundary`.
    frozen_boundary: u32,
}

pub struct BufferPool {
    file: Arc<dyn FileIo>,
    region: AlignedRegion,
    inner: RefCell<Inner>,
    /// Which leaf-cell encoding new cells are written in for THIS database.
    ///
    /// The encoding is a property of the stored file, declared in its own
    /// header, not of the build that opens it (Law 8: a release must read AND
    /// write every database an earlier release wrote, whatever cargo features
    /// that build had). Every build decodes both families unconditionally;
    /// this flag only decides what a WRITE produces, and the owner installs it
    /// from the database's declared features right after opening the pool.
    /// The `compact-cells` cargo feature survives only as the default for
    /// databases this build CREATES, which is what seeds the initial value.
    compact_cells: std::cell::Cell<bool>,
    _res: Reservation,
}

impl BufferPool {
    pub fn new(file: Arc<dyn FileIo>, budget: Arc<MemoryBudget>, frames: usize) -> Result<Self> {
        let file_len = file.len()?;
        if file_len % PAGE_SIZE as u64 != 0 {
            return Err(Error::Corrupt { page_no: 0, why: "data file is not page aligned" });
        }
        let pages = file_len / PAGE_SIZE as u64;
        if pages > u32::MAX as u64 {
            return Err(Error::Corrupt { page_no: 0, why: "data file has too many pages" });
        }
        let bytes = frames.checked_mul(PAGE_SIZE).ok_or(Error::OutOfBudget)?;
        let res = budget.reserve(Class::Pool, bytes)?;
        let region = AlignedRegion::new(bytes)?;
        let next_page = pages as u32;
        Ok(BufferPool {
            file,
            region,
            inner: RefCell::new(Inner {
                sweep_steps: 0,
                frozen_boundary: 0,
                frames: (0..frames).map(|_| Frame {
                    page_no: 0, present: false, dirty: false, referenced: false,
                    pins: 0, writer: false, validated: false,
                }).collect(),
                table: HashMap::with_capacity(frames * 2),
                hand: 0,
                stats: PoolStats { frames_total: frames, ..Default::default() },
                next_page,
                limits: None,
                free_count: 0,
                epoch_allocated_pages: 0,
                stamp_gen: 1,
                free: std::collections::BTreeMap::new(),
                reuse_limit: 0,
                free_birth: HashMap::new(),
                promotion_readers: Vec::new(),
                promotion_checked_through: 0,
                thawed: std::collections::HashSet::new(),
                live_pins: 0,
            }),
            compact_cells: std::cell::Cell::new(cfg!(feature = "compact-cells")),
            _res: res,
        })
    }

    /// The cell encoding new cells on this pool are written in.
    pub fn compact_cells(&self) -> bool { self.compact_cells.get() }
    /// Install the encoding the DATABASE declares. Called once per open, by
    /// the owner that read the header; never derived from build flags after
    /// creation, and never changed while a tree is being written.
    pub fn set_compact_cells(&self, on: bool) { self.compact_cells.set(on); }

    pub fn resource_limits(&self) -> Option<crate::limits::ResourceLimits> { self.inner.borrow().limits }
    pub(crate) fn set_resource_limits(&self, limits: crate::limits::ResourceLimits) -> Result<()> {
        let limits = limits.validate()?;
        let mut inner = self.inner.borrow_mut();
        if inner.next_page as u64 * PAGE_SIZE as u64 > limits.data_bytes {
            return Err(Error::ResourceLimit("existing data exceeds data allowance"));
        }
        inner.limits = Some(limits);
        Ok(())
    }
    pub fn tracked_pages(&self) -> (usize, usize) {
        let i = self.inner.borrow(); (i.free_count, i.thawed.len())
    }

    pub fn stats(&self) -> PoolStats { self.inner.borrow().stats }

    /// Pages allocated in this file so far. The scan uses it as a ceiling on how
    /// many leaves a sibling chain can legitimately visit.
    pub fn page_count(&self) -> u32 { self.inner.borrow().next_page }
    /// Allocations (including reuse) since the last root publication. This
    /// counts touched shadow/overflow pages even after dirty-frame eviction.
    pub fn epoch_allocated_bytes(&self) -> u64 {
        self.inner.borrow().epoch_allocated_pages.saturating_mul(PAGE_SIZE as u64)
    }

    /// Set the generation stamped into pages sealed from now on (2n): the
    /// store calls this at open and after every checkpoint flip.
    pub fn set_stamp_gen(&self, gen: u64) { self.inner.borrow_mut().stamp_gen = gen; }
    /// Birth bound for a writable page retired before its first publication.
    pub(crate) fn write_generation(&self) -> u64 { self.inner.borrow().stamp_gen }

    /// Record `page_no` as superseded (2n): its replacement was just
    /// shadowed into a fresh page. Keyed by the CURRENT epoch; see the
    /// `free` field for when it becomes reusable. Never pages 0/1 (meta).
    pub fn free_page(&self, page_no: u32) -> Result<()> {
        if page_no < 2 { return Ok(()); }
        if self.file.manages_free_pages() {
            { let mut inner = self.inner.borrow_mut();
              if let Some(fi) = inner.table.remove(&page_no) {
                  assert_eq!(inner.frames[fi].pins, 0, "freeing a pinned page");
                  inner.frames[fi].present=false;inner.frames[fi].dirty=false;
              }
            }
            return self.file.push_free_page(page_no);
        }
        let mut inner = self.inner.borrow_mut();
        if inner.limits.is_some_and(|l| inner.free_count >= l.tracked_pages as usize) {
            return Err(Error::ResourceLimit("retired-page bookkeeping full"));
        }
        let g = inner.stamp_gen;
        inner.free.entry(g).or_default().push(page_no);
        inner.free_count += 1;
        Ok(())
    }

    /// `birth` came from an independently verified immutable source page.
    /// The original generation is read before making its shadow, so a cache
    /// eviction cannot erase this evidence. No extra disk read is required.
    pub(crate) fn free_shadow_page(&self, page_no: u32, birth: u64) -> Result<()> {
        if self.file.manages_free_pages() { return self.free_page(page_no); }
        self.free_page(page_no)?;
        let mut inner = self.inner.borrow_mut();
        if birth > 0 && birth <= inner.stamp_gen {
            inner.free_birth.insert(page_no, birth);
        }
        Ok(())
    }

    /// Promote only retired pages absent from BOTH metadata roots and every
    /// currently registered snapshot. A page exists in [birth, retirement).
    /// New readers can only select the current/fallback roots; generation-zero
    /// reservations and every ambiguous reader disable this optimization.
    /// Persist promotion in the ordinary freelist (generation 1 = certified
    /// free), while v2 birth fields preserve still-pending lifetimes across writer reopen.
    pub(crate) fn refresh_reuse(&self, published: u64, readers: Option<&[u64]>) {
        let mut inner = self.inner.borrow_mut();
        let Some(readers) = readers else { inner.reuse_limit = 0; return; };
        let oldest = readers.first().copied().unwrap_or(u64::MAX);
        let fallback_limit = published.saturating_sub(1);
        inner.reuse_limit = fallback_limit.min(oldest);
        let lower = if inner.promotion_readers == readers {
            oldest.max(inner.promotion_checked_through)
        } else {
            inner.promotion_readers = readers.to_vec();
            oldest
        };
        inner.promotion_checked_through = fallback_limit;
        if lower >= fallback_limit { return; }
        let mut promoted = Vec::new();
        let mut birth = std::mem::take(&mut inner.free_birth);
        for (&retired, pages) in inner.free.range_mut((std::ops::Bound::Excluded(lower), std::ops::Bound::Included(fallback_limit))) {
            pages.retain(|p| {
                let Some(&born) = birth.get(p) else { return true; };
                let first = readers.partition_point(|g| *g < born);
                let pinned = readers.get(first).is_some_and(|g| *g < retired);
                if !pinned { promoted.push(*p); birth.remove(p); }
                pinned
            });
        }
        inner.free.retain(|_, pages| !pages.is_empty());
        inner.free_birth = birth;
        if !promoted.is_empty() { inner.free.entry(1).or_default().extend(promoted); }
    }

    /// Advance the recycling horizon (2n): pages freed at generations
    /// <= `limit` may be handed out again. The store computes the limit
    /// (published - 1, min'd with the oldest live snapshot reader).
    pub fn set_reuse_limit(&self, limit: u64) { self.inner.borrow_mut().reuse_limit = limit; }

    /// Serialize the freelist for the checkpoint's sidecar file (2n step D):
    /// [magic 8][version u16][reserved 6][published generation u64], then
    /// [freed-at-gen u64][n u32][(page u32, birth u64) * n]... and a crc32c trailer.
    /// The publication generation is what makes an old, otherwise valid
    /// sidecar unusable beside a newer data file. Loss, mismatch or corruption
    /// can therefore only LEAK pages, never recycle live ones. Version 2
    /// adds eight bytes per retired page to avoid losing lifetime evidence on
    /// reopen. Version 1 is rejected as derived state, never guessed/migrated.
    pub fn export_free(&self, published_generation: u64) -> Vec<u8> {
        let inner = self.inner.borrow();
        let mut v = Vec::with_capacity(FREE_HEADER_LEN + 4 + 12 * inner.free.len() + 12 * inner.free_count);
        v.extend_from_slice(&Self::empty_free(published_generation)[..FREE_HEADER_LEN]);
        for (g, pages) in &inner.free {
            v.extend_from_slice(&g.to_le_bytes());
            let n = u32::try_from(pages.len()).expect("one file cannot contain more than u32 pages");
            v.extend_from_slice(&n.to_le_bytes());
            for p in pages {
                v.extend_from_slice(&p.to_le_bytes());
                v.extend_from_slice(&inner.free_birth.get(p).copied().unwrap_or(0).to_le_bytes());
            }
        }
        let c = crc32c::crc32c(&v);
        v.extend_from_slice(&c.to_le_bytes());
        v
    }

    /// The canonical empty sidecar used by recovery after it renumbers every
    /// page. Kept here so ordinary checkpoints and recovery cannot drift into
    /// two encoders for the same durability handshake.
    pub(crate) fn empty_free(published_generation: u64) -> Vec<u8> {
        let mut v = Vec::with_capacity(FREE_HEADER_LEN + 4);
        v.extend_from_slice(&FREE_MAGIC);
        v.extend_from_slice(&FREE_VERSION.to_le_bytes());
        v.extend_from_slice(&[0; 6]);
        v.extend_from_slice(&published_generation.to_le_bytes());
        let c = crc32c::crc32c(&v);
        v.extend_from_slice(&c.to_le_bytes());
        v
    }

    /// Decode without mutating the pool. Checkpoint/recovery use this on a
    /// freshly reopened candidate before renaming it over the standing
    /// sidecar: build, independently verify, then publish.
    fn walk_free<F>(
        bytes: &[u8],
        expected_generation: u64,
        page_count: u32,
        mut visit: F,
    ) -> Option<()>
    where
        F: FnMut(u64, u32, u64),
    {
        if bytes.len() < FREE_HEADER_LEN + 4 { return None; }
        let (body, tail) = bytes.split_at(bytes.len() - 4);
        if crc32c::crc32c(body) != u32::from_le_bytes(tail.try_into().ok()?)
            || body.get(..8)? != FREE_MAGIC
            || u16::from_le_bytes(body.get(8..10)?.try_into().ok()?) != FREE_VERSION
            || body.get(10..16)? != [0; 6]
            || u64::from_le_bytes(body.get(16..24)?.try_into().ok()?) != expected_generation
        {
            return None;
        }
        let mut pos = FREE_HEADER_LEN;
        let mut previous_generation = None;
        let mut seen = std::collections::HashSet::new();
        while pos < body.len() {
            let header_end = pos.checked_add(12)?;
            if header_end > body.len() { return None; }
            let g = u64::from_le_bytes(body[pos..pos + 8].try_into().unwrap());
            let n = u32::from_le_bytes(body[pos + 8..pos + 12].try_into().unwrap()) as usize;
            if g == 0
                || g > expected_generation
                || n == 0
                || previous_generation.is_some_and(|previous| g <= previous)
            {
                return None;
            }
            previous_generation = Some(g);
            pos = header_end;
            let pages_bytes = n.checked_mul(12)?;
            let pages_end = pos.checked_add(pages_bytes)?;
            if pages_end > body.len() { return None; }
            for i in 0..n {
                let at = pos + i * 12;
                let p = u32::from_le_bytes(body[at..at + 4].try_into().unwrap());
                let birth = u64::from_le_bytes(body[at + 4..at + 12].try_into().unwrap());
                if p < 2 || p >= page_count || birth > g || !seen.insert(p) { return None; }
                visit(g, p, birth);
            }
            pos = pages_end;
        }
        Some(())
    }

    pub(crate) fn verify_free(
        bytes: &[u8],
        expected_generation: u64,
        page_count: u32,
    ) -> bool {
        Self::walk_free(bytes, expected_generation, page_count, |_, _, _| {}).is_some()
    }

    /// Load a persisted freelist (reopen). Anything malformed or stamped for
    /// another data generation becomes an empty list (the leak-only posture).
    /// Every page number is rejected unless it is inside this exact file.
    pub fn import_free(&self, bytes: &[u8], expected_generation: u64) -> bool {
        let page_count = self.inner.borrow().next_page;
        if self.resource_limits().is_some_and(|l| bytes.len() as u64 > l.freelist_bytes()) { return false; }
        let mut count = 0usize;
        let mut free = std::collections::BTreeMap::new();
        let mut births = HashMap::new();
        let valid = Self::walk_free(bytes, expected_generation, page_count, |g, p, birth| {
            count += 1;
            free.entry(g).or_insert_with(Vec::new).push(p);
            if birth != 0 { births.insert(p, birth); }
        });
        if valid.is_none() || self.resource_limits().is_some_and(|l| count > l.tracked_pages as usize) {
            return false;
        }
        let mut inner = self.inner.borrow_mut();
        inner.free = free;
        inner.free_count = count;
        inner.free_birth = births;
        inner.promotion_checked_through = 0;
        inner.promotion_readers.clear();
        true
    }

    pub(crate) fn sync_dir(&self) -> Result<()> { self.file.sync_dir() }
    pub(crate) fn file_ref(&self) -> &dyn FileIo { &*self.file }

    /// (eligible-now, waiting-on-horizon) freelist depths (tests/probes).
    pub fn free_pages_split(&self) -> (usize, usize) {
        let inner = self.inner.borrow();
        let lim = inner.reuse_limit;
        let el: usize = inner.free.range(..=lim).map(|(_, v)| v.len()).sum();
        let tot: usize = inner.free.values().map(|v| v.len()).sum();
        (el, tot - el)
    }
    /// Sum of pages currently waiting on the freelist (tests/probes).
    pub fn free_pages_pending(&self) -> usize {
        self.inner.borrow().free.values().map(|v| v.len()).sum()
    }

    /// Pop one recyclable page number, if any (2n).
    fn pop_free(inner: &mut Inner) -> Option<u32> {
        let limit = inner.reuse_limit;
        let g = *inner.free.range(..=limit).next()?.0;
        let v = inner.free.get_mut(&g)?;
        let p = v.pop()?;
        inner.free_birth.remove(&p);
        inner.free_count -= 1;
        if v.is_empty() { inner.free.remove(&g); }
        Some(p)
    }

    /// 2f: page numbers below this boundary belong to the last PUBLISHED
    /// root (the checkpoint a snapshot reader may be standing on) and are
    /// immutable -- writers shadow them to fresh numbers instead of editing
    /// in place. `get_mut` asserts it. Meta slots (pages 0 and 1) are the
    /// publication mechanism itself and are exempt. Boundary 0 = no epoch
    /// published yet, everything mutable (fresh store before first publish).
    pub fn set_frozen_boundary(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.frozen_boundary = inner.next_page;
        // recycled pages just published with this epoch: frozen again
        inner.thawed.clear();
        inner.epoch_allocated_pages = 0;
    }
    pub fn frozen_boundary(&self) -> u32 { self.inner.borrow().frozen_boundary }
    /// Experimental stable-page pager owns snapshot versions outside the pool.
    pub fn finish_stable_page_epoch(&self) {
        let mut inner = self.inner.borrow_mut();
        assert_eq!(inner.frozen_boundary, 0);
        inner.thawed.clear();
        inner.epoch_allocated_pages = 0;
    }
    pub fn is_frozen(&self, page_no: u32) -> bool {
        let inner = self.inner.borrow();
        page_no >= 2 && page_no < inner.frozen_boundary && !inner.thawed.contains(&page_no)
    }
    pub fn io_stats(&self) -> Option<&crate::io::IoStats> { self.file.stats() }

    /// Re-baseline the high-water mark to the pins currently held, so a caller
    /// can measure the peak over one specific operation.
    pub fn reset_peak_pins(&self) {
        let mut inner = self.inner.borrow_mut();
        inner.stats.peak_pins = inner.live_pins;
    }

    /// Diagnostic: total clock-sweep steps across all victim() calls. A CLOCK
    /// pathology shows here as steps >> misses -- every hit re-arms a ref bit,
    /// so a miss arriving after many hits must strip a large stretch of the
    /// table before it finds a victim.
    pub fn sweep_steps(&self) -> u64 { self.inner.borrow().sweep_steps }

    fn victim(&self, inner: &mut Inner) -> Result<usize> {
        let n = inner.frames.len();
        for _ in 0..(n * 4) {
            inner.sweep_steps += 1;
            let i = inner.hand;
            inner.hand = (inner.hand + 1) % n;
            if inner.frames[i].pins > 0 { continue; }
            if !inner.frames[i].present { return Ok(i); }
            if inner.frames[i].referenced { inner.frames[i].referenced = false; continue; }
            if inner.frames[i].dirty {
                let no = inner.frames[i].page_no;
                // SAFETY: frame i has pins == 0 here. Every guard that could
                // reference frame i (PinnedRead or PinnedWrite) decrements
                // pins on drop before releasing its borrow, so pins == 0
                // means no `&[u8]`/`&mut [u8]` into this frame is live.
                crate::page::seal(unsafe { self.region.page_mut(i) }, inner.stamp_gen);
                self.file.write_at(unsafe { self.region.page(i) }, no as u64 * PAGE_SIZE as u64)?;
                crate::write_stats::add(
                    crate::write_stats::Phase::FinalPages,
                    PAGE_SIZE as u64,
                );
                inner.frames[i].dirty = false;
            }
            let old = inner.frames[i].page_no;
            inner.table.remove(&old);
            inner.frames[i].present = false;
            inner.stats.evictions += 1;
            return Ok(i);
        }
        Err(Error::OutOfBudget) // every frame is pinned
    }

    /// Resolve `page_no` to a resident frame, pinning it. `write` selects
    /// which exclusivity rule applies: a write pin requires the frame to
    /// currently have no other live pin at all; a read pin merely requires
    /// no live writer.
    fn load(&self, page_no: u32, write: bool) -> Result<usize> {
        let mut inner = self.inner.borrow_mut();
        if let Some(&i) = inner.table.get(&page_no) {
            let f = &inner.frames[i];
            assert!(
                !(write && f.pins > 0),
                "page {page_no} is already pinned; get_mut requires exclusive access"
            );
            assert!(
                write || !f.writer,
                "page {page_no} is already pinned by a writer"
            );
            inner.stats.hits += 1;
            inner.frames[i].referenced = true;
            inner.frames[i].pins += 1;
            inner.live_pins += 1;
            if inner.live_pins > inner.stats.peak_pins { inner.stats.peak_pins = inner.live_pins; }
            if write { inner.frames[i].writer = true; }
            return Ok(i);
        }
        inner.stats.misses += 1;
        let i = self.victim(&mut inner)?;
        // SAFETY: `victim` returned frame i with pins == 0 and removed it
        // from the table, so no reference to it is live.
        self.file.read_at(unsafe { self.region.page_mut(i) }, page_no as u64 * PAGE_SIZE as u64)?;
        // The medium boundary: the one place a checksum needs verifying.
        // Failure leaves the pool untouched (frame not yet in the table).
        crate::page::PageRef::open(unsafe { self.region.page(i) }, page_no)?;
        inner.frames[i] = Frame {
            page_no, present: true, dirty: false, referenced: true, pins: 1, writer: write, validated: false,
        };
        inner.table.insert(page_no, i);
        inner.live_pins += 1;
        if inner.live_pins > inner.stats.peak_pins { inner.stats.peak_pins = inner.live_pins; }
        Ok(i)
    }

    /// Read `n` consecutive pages in ONE pread into `buf`, WITHOUT caching
    /// them (2e ablation A1, for overflow chains). Returns Ok(false) if any
    /// page in the run is resident -- a resident frame may be dirtier than
    /// disk, so the caller must take the per-page pool path instead. Each
    /// page's checksum is verified at this medium boundary exactly as `load`
    /// verifies it; a failure refuses the whole run and caches nothing.
    ///
    /// SACRIFICE (Law 4): pages read this way do not warm the cache -- an
    /// immediate re-read pays the pread again. Bought: one syscall per chain
    /// instead of one per page, and single-use payload pages never evict a
    /// hot tree page.
    pub fn read_run_uncached(&self, start: u32, n: u32, buf: &mut Vec<u8>) -> Result<bool> {
        {
            let inner = self.inner.borrow();
            if n == 0 || start.checked_add(n).map_or(true, |e| e > inner.next_page) {
                return Err(Error::Corrupt { page_no: start, why: "overflow run out of bounds" });
            }
            for p in start..start + n {
                if inner.table.contains_key(&p) { return Ok(false); }
            }
        }
        buf.clear();
        buf.resize(n as usize * PAGE_SIZE, 0);
        self.file.read_at(buf, start as u64 * PAGE_SIZE as u64)?;
        for i in 0..n as usize {
            crate::page::PageRef::open(&buf[i * PAGE_SIZE..(i + 1) * PAGE_SIZE], start + i as u32)?;
        }
        Ok(true)
    }

    pub fn get(&self, page_no: u32) -> Result<PinnedRead<'_>> {
        let i = self.load(page_no, false)?;
        Ok(PinnedRead { pool: self, frame: i })
    }

    pub fn get_mut(&self, page_no: u32) -> Result<PinnedWrite<'_>> {
        // The 2f no-overwrite invariant, enforced at the single chokepoint
        // every in-place write passes through: a frozen page is part of a
        // published root a snapshot reader may hold; editing it would tear
        // that reader's view. Loud, not a Result -- reaching here means a
        // write path forgot to shadow, which is our bug, never the caller's.
        assert!(
            !self.is_frozen(page_no),
            "page {page_no} is frozen (published at the last checkpoint); write paths must shadow it"
        );
        let i = self.load(page_no, true)?;
        self.inner.borrow_mut().frames[i].dirty = true;
        Ok(PinnedWrite { pool: self, frame: i, page_no })
    }

    /// Reserve a brand-new page at the end of the file.
    ///
    /// `next_page` only ever increases and a page number handed out here is
    /// never reclaimed or reused -- `recover.rs`'s duplicate-key resolution
    /// depends on that: among two surviving leaves claiming the same key, it
    /// trusts the higher page number as the one written later. A change that
    /// reclaims/reuses page numbers would silently break that reasoning and
    /// must revisit `recover.rs`'s `tag`/`untag`/dedup logic.
    fn admit_allocation(i: &Inner) -> Result<()> {
        if let Some(l) = i.limits {
            let reuse = i.free.range(..=i.reuse_limit).next().is_some();
            if reuse && i.thawed.len() >= l.tracked_pages as usize {
                return Err(Error::ResourceLimit("recycled-page bookkeeping full"));
            }
            if !reuse && i.next_page as u64 >= l.data_bytes / PAGE_SIZE as u64 {
                return Err(Error::ResourceLimit("data extent full; snapshots or fallback roots may pin pages"));
            }
        }
        Ok(())
    }

    pub fn allocate(&self) -> Result<PinnedWrite<'_>> {
        let managed = self.file.manages_free_pages();
        let external_free = if managed { self.file.pop_free_page()? } else { None };
        let page_no = { let mut inner = self.inner.borrow_mut();
            Self::admit_allocation(&inner)?;
            inner.epoch_allocated_pages += 1;
            match if managed { external_free } else { Self::pop_free(&mut inner) } {
                Some(p) => {
                    // recycled: its number sits below the frozen boundary but
                    // its content belongs to THIS epoch -- thaw it, and drop
                    // any stale cached frame for the old content.
                    inner.thawed.insert(p);
                    if let Some(&fi) = inner.table.get(&p) {
                        debug_assert_eq!(inner.frames[fi].pins, 0,
                            "recycling page {p} while a guard holds its stale frame");
                        inner.table.remove(&p);
                        inner.frames[fi].present = false;
                        inner.frames[fi].dirty = false;
                    }
                    p
                }
                None => { let p = inner.next_page; inner.next_page = p.checked_add(1).ok_or(Error::TooLarge)?; p }
            } };
        let mut inner = self.inner.borrow_mut();
        let i = self.victim(&mut inner)?;
        // SAFETY: freshly evicted frame, pins == 0, not yet in the table, so
        // no reference to it is live.
        //
        // Left as a VALID empty Free page, not zeroes: a caller that only wants
        // the page number (bulk.rs reserves one by allocate-and-drop, three
        // times) leaves the frame dirty, and an eviction before anything writes
        // it would publish ZEROES to disk -- bad magic, undetectable until read.
        // No path through the pool may publish a page a reader must refuse.
        {
            let b = unsafe { self.region.page_mut(i) };
            b.fill(0);
            crate::page::PageMut::init(b, crate::page::PageKind::Free, 0, page_no).finalise(0);
        }
        inner.frames[i] = Frame {
            page_no, present: true, dirty: true, referenced: true, pins: 1, writer: true, validated: false,
        };
        inner.table.insert(page_no, i);
        inner.live_pins += 1;
        if inner.live_pins > inner.stats.peak_pins { inner.stats.peak_pins = inner.live_pins; }
        drop(inner);
        Ok(PinnedWrite { pool: self, frame: i, page_no })
    }

    /// Reserve a page number for a bulk packer that will write the complete,
    /// checksummed page directly. Packed pages are unreachable until graft
    /// publication, so caching every one only forces the fixed pool to evict
    /// and issue a 4 KiB write per page. The direct writer batches them while
    /// preserving the same allocator/freelist invariants.
    pub(crate) fn allocate_unpooled(&self) -> Result<u32> {
        let mut inner = self.inner.borrow_mut();
        Self::admit_allocation(&inner)?;
        inner.epoch_allocated_pages += 1;
        Ok(match Self::pop_free(&mut inner) {
            Some(page_no) => {
                inner.thawed.insert(page_no);
                if let Some(&frame) = inner.table.get(&page_no) {
                    assert_eq!(inner.frames[frame].pins, 0,
                        "recycling page {page_no} while a guard holds its stale frame");
                    inner.table.remove(&page_no);
                    inner.frames[frame].present = false;
                    inner.frames[frame].dirty = false;
                }
                page_no
            }
            None => {
                let page_no = inner.next_page;
                inner.next_page = page_no.checked_add(1).ok_or(Error::TooLarge)?;
                page_no
            }
        })
    }

    pub(crate) fn stamp_generation(&self) -> u64 { self.inner.borrow().stamp_gen }

    pub(crate) fn write_unpooled_run(&self, first_page: u32, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() || bytes.len() % PAGE_SIZE != 0 {
            return Err(Error::Corrupt { page_no: first_page, why: "bulk page run is not aligned" });
        }
        let pages = u32::try_from(bytes.len() / PAGE_SIZE).map_err(|_| Error::TooLarge)?;
        if first_page.checked_add(pages).is_none_or(|end| end > self.page_count()) {
            return Err(Error::Corrupt { page_no: first_page, why: "bulk page run exceeds allocation" });
        }
        self.file.write_at(bytes, first_page as u64 * PAGE_SIZE as u64)?;
        crate::write_stats::add(
            crate::write_stats::Phase::CandidatePages,
            bytes.len() as u64,
        );
        Ok(())
    }

    pub fn flush_all(&self, barrier: Barrier) -> Result<()> {
        let mut inner = self.inner.borrow_mut();
        for i in 0..inner.frames.len() {
            if inner.frames[i].present && inner.frames[i].dirty {
                // A dirty frame that is still pinned means a guard is live
                // across a checkpoint. Skipping it silently would let
                // flush_all return Ok(()) having left dirty data unwritten —
                // a checkpoint that quietly declines to write is exactly how
                // a row goes missing later, and Law 3 exists because a
                // process that reports success while dropping state is the
                // shape that loses data. `get`/`get_mut` already treat a
                // conflicting access as a contract violation; this is the
                // same violation and gets the same answer: loud, not a
                // Result, because the single writer never holds a guard
                // across a checkpoint, so this firing means a bug in our own
                // code, not a condition a caller could act on.
                assert_eq!(
                    inner.frames[i].pins, 0,
                    "flush_all with frame {i} (page {}) pinned and dirty",
                    inner.frames[i].page_no
                );
                let no = inner.frames[i].page_no;
                // SAFETY: pins == 0 was just asserted, so no guard to this
                // frame is live.
                crate::page::seal(unsafe { self.region.page_mut(i) }, inner.stamp_gen);
                self.file.write_at(unsafe { self.region.page(i) }, no as u64 * PAGE_SIZE as u64)?;
                crate::write_stats::add(
                    crate::write_stats::Phase::FinalPages,
                    PAGE_SIZE as u64,
                );
                inner.frames[i].dirty = false;
            }
        }
        // Count, then drop the borrow, then issue the syscall: a RefCell
        // guard held across a blocking call (F_FULLFSYNC is ~65x an ordinary
        // fsync on macOS) is a hazard worth not creating even though nothing
        // else can reach `inner` mid-syscall in this single-writer engine.
        match barrier {
            Barrier::Full => { inner.stats.sync_full_calls += 1; drop(inner); self.file.sync_full() }
            Barrier::Data => { inner.stats.sync_data_calls += 1; drop(inner); self.file.sync_data() }
            Barrier::None => { drop(inner); Ok(()) }
        }
    }

    fn unpin(&self, frame: usize, writer: bool) {
        let mut inner = self.inner.borrow_mut();
        inner.frames[frame].pins -= 1;
        inner.live_pins -= 1;
        if writer { inner.frames[frame].writer = false; }
    }
}

pub struct PinnedRead<'a> { pool: &'a BufferPool, frame: usize }
impl std::ops::Deref for PinnedRead<'_> {
    type Target = [u8];
    // SAFETY: this guard holds a read pin on the frame. `load` refuses to
    // hand out a write pin (PinnedWrite) for the same frame while any pin is
    // live, so no `&mut [u8]` can coexist with this borrow; `victim` refuses
    // to touch a pinned frame, so it cannot be evicted or overwritten either.
    fn deref(&self) -> &[u8] { unsafe { self.pool.region.page(self.frame) } }
}
impl PinnedRead<'_> {
    /// 2f-A1: has this frame's slot directory been fully validated during
    /// its current residency? See `Frame::validated`.
    pub fn validated(&self) -> bool {
        self.pool.inner.borrow().frames[self.frame].validated
    }
    pub fn set_validated(&self) {
        self.pool.inner.borrow_mut().frames[self.frame].validated = true;
    }
}
impl Drop for PinnedRead<'_> { fn drop(&mut self) { self.pool.unpin(self.frame, false) } }

pub struct PinnedWrite<'a> { pool: &'a BufferPool, frame: usize, page_no: u32 }
impl PinnedWrite<'_> {
    pub fn page_no(&self) -> u32 { self.page_no }
    // SAFETY: `&mut self` plus a write pin means this guard is the only live
    // reference to the frame: `load` refuses to create a write pin unless
    // pins was 0, and refuses any further pin (read or write) while this
    // frame's writer flag is set, so nothing else can alias it.
    pub fn bytes_mut(&mut self) -> &mut [u8] { unsafe { self.pool.region.page_mut(self.frame) } }
    // SAFETY: `&self` plus the write pin; no other `&mut` can coexist with
    // this borrow for the same reason as `bytes_mut`, and Rust's own borrow
    // checker prevents this call from overlapping a `bytes_mut` call on the
    // same guard.
    pub fn bytes(&self) -> &[u8] { unsafe { self.pool.region.page(self.frame) } }
}
impl Drop for PinnedWrite<'_> { fn drop(&mut self) { self.pool.unpin(self.frame, true) } }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::budget::MemoryBudget;
    use crate::io::{open_file, IoMode};
    use crate::page::{PageKind, PageMut, PageRef, PAGE_SIZE};
    use std::sync::Arc;

    fn pool_with(frames: usize) -> (BufferPool, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let (f, _) = open_file(&dir.path().join("t.db"), IoMode::Buffered).unwrap();
        let budget = Arc::new(MemoryBudget::new(64 * 1024 * 1024));
        (BufferPool::new(f.into(), budget, frames).unwrap(), dir)
    }

    /// The whole claim in one test: a pool far smaller than the working set
    /// still answers every page correctly.
    #[test]
    fn a_pool_smaller_than_the_working_set_still_serves_every_page() {
        let (pool, _d) = pool_with(8);
        let n = 400u32;
        for _ in 0..n {
            let mut w = pool.allocate().unwrap();
            let no = w.page_no();
            let mut p = PageMut::init(w.bytes_mut(), PageKind::Leaf, 1, no);
            p.insert_slot(0, &no.to_le_bytes()).unwrap();
            p.finalise(0);
            drop(w);
        }
        pool.flush_all(Barrier::Data).unwrap();

        for i in 0..n {
            let r = pool.get(i).unwrap();
            let pr = PageRef::open(&r, i).unwrap();
            assert_eq!(pr.slot(0), &i.to_le_bytes());
        }
        assert!(pool.stats().evictions > 0, "8 frames over 400 pages must evict");
        assert_eq!(pool.stats().frames_total, 8, "the pool must never grow");
    }

    #[test]
    fn a_pinned_page_is_never_evicted() {
        let (pool, _d) = pool_with(4);
        for _ in 0..4 { let mut w = pool.allocate().unwrap(); let no = w.page_no();
            PageMut::init(w.bytes_mut(), PageKind::Leaf, 1, no).finalise(0); }
        pool.flush_all(Barrier::Data).unwrap();

        let held = pool.get(0).unwrap();          // pin page 0 for the whole test
        for i in 1..4 { let _ = pool.get(i).unwrap(); }
        // Touching more pages than there are frames must not have stolen page 0.
        let pr = PageRef::open(&held, 0).unwrap();
        assert_eq!(pr.page_no(), 0);
    }

    #[test]
    fn a_dirty_page_survives_eviction_because_it_was_written_out() {
        let (pool, _d) = pool_with(2);
        let mut w = pool.allocate().unwrap();
        let no = w.page_no();
        let mut p = PageMut::init(w.bytes_mut(), PageKind::Leaf, 9, no);
        p.insert_slot(0, b"survives").unwrap();
        p.finalise(7);
        drop(w);
        // Force eviction by touching more pages than frames.
        for _ in 0..4 { let mut x = pool.allocate().unwrap(); let n2 = x.page_no();
            PageMut::init(x.bytes_mut(), PageKind::Leaf, 9, n2).finalise(0); }
        let r = pool.get(no).unwrap();
        assert_eq!(PageRef::open(&r, no).unwrap().slot(0), b"survives");
    }

    /// The exclusivity rules are the whole reason `AlignedRegion::page_mut` is
    /// an `unsafe fn`. They were added because a pin COUNT cannot distinguish a
    /// reader from a writer, so `get` followed by `get_mut` on the same page
    /// handed out a `&mut [u8]` aliasing a live `&[u8]` — reachable with two
    /// ordinary safe calls. These three tests are what stop that returning.
    #[test]
    #[should_panic(expected = "already pinned")]
    fn taking_a_writer_on_a_page_a_reader_holds_is_refused() {
        let (pool, _d) = pool_with(4);
        { let mut w = pool.allocate().unwrap(); let no = w.page_no();
          PageMut::init(w.bytes_mut(), PageKind::Leaf, 1, no).finalise(0); }
        pool.flush_all(Barrier::Data).unwrap();
        let _reader = pool.get(0).unwrap();
        let _writer = pool.get_mut(0).unwrap();   // must panic, not alias
    }

    #[test]
    #[should_panic(expected = "writer")]
    fn taking_a_reader_on_a_page_a_writer_holds_is_refused() {
        let (pool, _d) = pool_with(4);
        { let mut w = pool.allocate().unwrap(); let no = w.page_no();
          PageMut::init(w.bytes_mut(), PageKind::Leaf, 1, no).finalise(0); }
        pool.flush_all(Barrier::Data).unwrap();
        let _writer = pool.get_mut(0).unwrap();
        let _reader = pool.get(0).unwrap();       // must panic, not alias
    }

    /// A checkpoint that returns Ok() having left a dirty page unwritten is a
    /// silent data-loss shape. It must be loud.
    #[test]
    #[should_panic(expected = "pinned and dirty")]
    fn flushing_while_a_dirty_page_is_pinned_is_refused() {
        let (pool, _d) = pool_with(4);
        let mut w = pool.allocate().unwrap();
        let no = w.page_no();
        PageMut::init(w.bytes_mut(), PageKind::Leaf, 1, no).finalise(0);
        pool.flush_all(Barrier::Data).unwrap();                // w is still alive: must panic
    }

    #[test]
    fn the_budget_refuses_a_pool_it_cannot_fund() {
        let dir = tempfile::tempdir().unwrap();
        let (f, _) = open_file(&dir.path().join("t.db"), IoMode::Buffered).unwrap();
        let budget = Arc::new(MemoryBudget::new(PAGE_SIZE * 4));
        assert!(BufferPool::new(f.into(), budget, 1000).is_err());
    }

    #[test]
    fn a_file_with_a_partial_page_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial.db");
        std::fs::write(&path, b"one bad trailing byte").unwrap();
        let (f, _) = open_file(&path, IoMode::Buffered).unwrap();
        let budget = Arc::new(MemoryBudget::new(4 * PAGE_SIZE));

        assert!(matches!(
            BufferPool::new(f.into(), budget, 4),
            Err(Error::Corrupt { page_no: 0, .. })
        ));
    }
}
