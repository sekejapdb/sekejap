//! A B+tree over the buffer pool.
//!
//! Leaves hold every key and value; interior pages hold separator keys and child
//! page numbers and are wholly derivable from the leaves. That derivability is
//! what makes Law 5 cheap: recovery sweeps for intact leaves and repacks.
//!
//! SACRIFICE (Law 4): a split leaves two leaves about half full, so the file is
//! roughly 1.3-1.5x the size of a packed layout. Bought: an insert lands in a page
//! that already exists, so adding one row never rewrites the store.

use crate::page::{PageKind, PageMut, PageRef, PAGE_SIZE};
use crate::pool::{BufferPool, PinnedRead, PinnedWrite};
use crate::verify::decode_record;
use crate::{Error, Result};
use std::cell::Cell;

/// Open a resident page through the frame's validated bit (2f-A1): the first
/// open of a residency pays the full slot validation and sets the bit; every
/// later open skips the O(entries) loop. See `PageRef::open_resident_validated`.
fn open_cached<'a>(r: &'a crate::pool::PinnedRead<'_>, no: u32) -> Result<PageRef<'a>> {
    if r.validated() {
        PageRef::open_resident_validated(r, no)
    } else {
        let p = PageRef::open_resident(r, no)?;
        validate_records(&p)?;
        r.set_validated();
        Ok(p)
    }
}

/// Validate every record once per buffer residency. The pool's `validated`
/// bit makes this page-local pass disappear on hot reopens, preserving the
/// scan path's existing cost while still ensuring no record field is used
/// before the shared fallible decoder has accepted it.
fn validate_records(page: &PageRef<'_>) -> Result<()> {
    if !matches!(page.kind(), PageKind::Leaf | PageKind::Interior) {
        return Ok(());
    }
    for i in 0..page.nentries() {
        decode_record(page.slot(i), page.page_no(), page.kind())?;
    }
    Ok(())
}

/// 2f copy-on-write: duplicate a frozen page under a fresh page number.
/// Byte-identical except its own number; the old version stays on disk,
/// still serving any snapshot reader pinned at a published root. The old
/// FRAME may remain cached -- it is clean (frozen pages cannot be dirtied)
/// and reads of the old number still deserve it; the new page starts dirty
/// and reaches disk by eviction or checkpoint like any other write.
fn shadow_page(pool: &BufferPool, page: u32) -> Result<u32> {
    let content = { pool.get(page)?.to_vec() };
    let mut w = pool.allocate()?;
    let no = w.page_no();
    let b = w.bytes_mut();
    b.copy_from_slice(&content);
    b[12..16].copy_from_slice(&no.to_le_bytes());
    PageMut::reopen(b).finalise(0);
    // 2n: the original is superseded the moment this epoch publishes --
    // record it for recycling (safe once the dual-slot fallback and every
    // live reader have moved past it; see pool's `free` field).
    let birth = u64::from_le_bytes(content[24..32].try_into().unwrap());
    pool.free_shadow_page(page, birth)?;
    Ok(no)
}


/// 2n: detach `child_no` from `parent_no` (both this epoch's writable
/// copies). Returns the removed child POSITION (always >= 1) so a walk
/// holding saved indices into this parent can shift them; None -- nothing
/// removed -- when the child is child0 or the parent's last child: those
/// leaves are kept cleared-in-place instead. (Child0 removal would need a
/// saved-index of -1 to express "revisit position 0"; a skipped child left
/// rows undeleted -- the model test measured 70 of 10000 -- so child0
/// simply stays, one kept-empty leaf per parent per pass.)
fn unlink_child(pool: &BufferPool, tree_id: u16, parent_no: u32, child_no: u32) -> Result<Option<usize>> {
    let pos = {
        let r = pool.get(parent_no)?;
        let p = open_cached(&r, parent_no)?;
        if p.kind() != PageKind::Interior || p.tree_id() != tree_id { return Ok(None); }
        let n = p.nentries();
        if p.child0() == child_no {
            None // child0 stays (see the doc comment)
        } else {
            let mut found = None;
            for i in 0..n {
                if validated_child(p.slot(i)) == child_no {
                    found = Some(i + 1);
                    break;
                }
            }
            found
        }
    };
    let Some(pos) = pos else { return Ok(None) };
    let mut w = pool.get_mut(parent_no)?;
    let mut p = PageMut::reopen(w.bytes_mut());
    p.remove_slot(pos - 1);
    p.finalise(0);
    Ok(Some(pos))
}

/// Point entry `i` of interior `page` at `new_child` (i == 0 is child0,
/// otherwise slot i-1). `page` must already be unfrozen -- the shadowed
/// descent guarantees it, and `get_mut` asserts it.
fn patch_child(pool: &BufferPool, page: u32, i: usize, new_child: u32) -> Result<()> {
    let mut w = pool.get_mut(page)?;
    if i == 0 {
        let p = PageRef::open_resident(w.bytes(), page)?;
        validate_records(&p)?;
        if p.kind() != PageKind::Interior {
            return Err(Error::Corrupt { page_no: page, why: "child patch reached a non-interior page" });
        }
        let mut p = PageMut::reopen(w.bytes_mut());
        p.set_child0(new_child);
        p.finalise(0);
        return Ok(());
    }
    // The child is the trailing 4 bytes of the slot's record (enc_interior).
    // Patch it in place: same length, same key, only the pointer changes.
    let (off, len) = {
        let p = PageRef::open_resident(w.bytes(), page)?;
        validate_records(&p)?;
        if p.kind() != PageKind::Interior || i > p.nentries() {
            return Err(Error::Corrupt { page_no: page, why: "child patch slot is out of bounds" });
        }
        validated_child(p.slot(i - 1));
        let b = w.bytes();
        let base = crate::page::HEADER_LEN + (i - 1) * 4;
        let off = u16::from_le_bytes([b[base], b[base + 1]]) as usize;
        let len = u16::from_le_bytes([b[base + 2], b[base + 3]]) as usize;
        (off, len)
    };
    let b = w.bytes_mut();
    b[off + len - 4..off + len].copy_from_slice(&new_child.to_le_bytes());
    PageMut::reopen(b).finalise(0);
    Ok(())
}

/// How many leaves the per-keyspace append hint remembers at once.
///
/// Bounded and constant (Law 1): the cache never grows with the store, and a
/// slot it cannot spare costs one ordinary descent, never an answer. Sixteen
/// is chosen from the shape of the write, not from taste: the multimodel load
/// writes at most a handful of tags per document (mapping, row, vector,
/// forward edge, reverse edge, field index) and the reverse-edge tag alone
/// needs two -- a near-ascending run over people and a second, far lower run
/// over the hundred organizations -- so the working set is single digits with
/// room left for the indexes a collection adds.
pub const TAG_HINTS: usize = 16;

/// The longest fence a hint will hold, inline.
///
/// INLINE, not a `Vec`, and that is a measured requirement rather than a
/// preference: `tests/edge_write_budget.rs` counts the allocations one
/// relationship is allowed, and two owned fences per armed leaf pushed a
/// two-put edge from 8 allocations to 10. Fences are cached on the descending
/// path, which is the path that is supposed to be getting cheaper.
///
/// 48 bytes holds every key this cache is for: an edge row is at most 41 bytes
/// (tag, two entities, context, type), a data row about 12, a mapping its
/// external key. A separator longer than this simply does not arm a hint --
/// the run keeps descending, which is what it did before.
const FENCE_MAX: usize = 48;

/// One fence, inline. `len` is meaningful only when `present`.
#[derive(Clone, Copy)]
struct Fence { present: bool, len: u8, bytes: [u8; FENCE_MAX] }

impl Fence {
    const ABSENT: Fence = Fence { present: false, len: 0, bytes: [0; FENCE_MAX] };
    /// `None` when the key does not fit, so the caller can decline to arm
    /// rather than remember a truncated fence -- a truncated upper fence is
    /// LOWER than the real one, which would only ever refuse keys, but a
    /// truncated LOWER fence is lower too, which would accept them.
    fn of(key: &[u8]) -> Option<Fence> {
        if key.len() > FENCE_MAX { return None; }
        let mut f = Fence { present: true, len: key.len() as u8, bytes: [0; FENCE_MAX] };
        f.bytes[..key.len()].copy_from_slice(key);
        Some(f)
    }
    fn get(&self) -> Option<&[u8]> {
        if self.present { Some(&self.bytes[..self.len as usize]) } else { None }
    }
}

/// The fences of one leaf, as a descent found them. `armable` is false when a
/// separator was too long to hold inline.
#[derive(Clone, Copy)]
pub(crate) struct LeafFences { lower: Fence, upper: Fence, armable: bool }

impl LeafFences {
    const NONE: LeafFences = LeafFences { lower: Fence::ABSENT, upper: Fence::ABSENT, armable: true };
    fn narrow_lower(&mut self, key: &[u8]) {
        match Fence::of(key) { Some(f) => self.lower = f, None => self.armable = false }
    }
    fn narrow_upper(&mut self, key: &[u8]) {
        match Fence::of(key) { Some(f) => self.upper = f, None => self.armable = false }
    }
    /// The half of this interval below `sep`, which is what the LEFT page of a
    /// split at `sep` inherits.
    fn below(mut self, sep: &[u8]) -> Self { self.narrow_upper(sep); self }
    /// The half at or above `sep` -- the RIGHT page's interval. Both halves
    /// are SUBintervals of this one, which is why arming either is sound: a
    /// narrower interval can only refuse keys, never misplace one.
    fn above(mut self, sep: &[u8]) -> Self { self.narrow_lower(sep); self }
}

/// One remembered leaf, and the exact interval of keys that may be appended
/// to it without descending.
///
/// The fences are the SEPARATORS the arming descent walked through, not the
/// leaf's own first and last keys: they are the fences the tree itself would
/// use to route a key to this leaf.
#[derive(Clone, Copy)]
struct TagHint {
    /// 0 marks a free slot; tree ids start at 1.
    tree: u16,
    /// The leading byte of the keys this slot serves -- the keyspace tag (D4).
    tag: u8,
    leaf: u32,
    /// The leaf's `next_leaf` when the hint was armed. A split of THIS leaf
    /// relinks it to the new right page, so comparing one `u32` tells the
    /// subdivision of this interval -- the one case a cached fence cannot
    /// survive -- from the ordinary appends that leave the chain alone. It
    /// doubles as identity: a recycled page number would have to be relinked
    /// identically to pass.
    next: u32,
    /// Lower fence: the separator below this leaf, absent for the leftmost.
    /// SELECTION ONLY. Nothing is placed on the strength of it; it exists so
    /// two ascending runs under the SAME tag (the reverse-edge rows of people
    /// and of organizations) can hold two slots and each find its own.
    lower: Fence,
    /// THE FENCE. Absent means this leaf is the tree's rightmost and has no
    /// separator above it; otherwise a key may be appended here only if it is
    /// STRICTLY below this. A key equal to the separator belongs to the next
    /// leaf: `upper_bound` routes it there, so appending it here would make it
    /// reachable by a scan and invisible to `get`.
    upper: Fence,
    /// When this slot was last used or armed, on the cache's own counter.
    /// The victim of an eviction is the smallest of these.
    used: u64,
}

impl TagHint {
    const FREE: TagHint = TagHint {
        tree: 0, tag: 0, leaf: 0, next: 0, lower: Fence::ABSENT, upper: Fence::ABSENT, used: 0,
    };
    /// Does this slot claim `key`? Lower fence inclusive, upper exclusive.
    ///
    /// `relaxed` makes the upper fence inclusive, which is WRONG -- see
    /// `fast_path_tag_leaf`. It exists only so the byte-equivalence oracle has
    /// a negative control: a test that proves two files identical proves
    /// nothing unless a deliberately different placement makes them differ.
    /// It is settable under `cfg(test)` alone and is a constant `false`
    /// everywhere else.
    fn claims(&self, tree: u16, tag: u8, key: &[u8], relaxed: bool) -> bool {
        self.tree == tree
            && self.tag == tag
            && self.lower.get().is_none_or(|l| key >= l)
            && self.upper.get().is_none_or(|u| if relaxed { key <= u } else { key < u })
    }
}

/// The per-keyspace append hints for one handle.
///
/// D9 widened the append SPLIT from "rightmost leaf of the tree" to "rightmost
/// leaf of the key's own tag". This widens the append HINT the same way, which
/// is the other half: the split policy was already right for a tag that is not
/// the tree's last, but every one of those inserts still paid a full
/// root-to-leaf descent to reach the leaf the policy then acted on.
///
/// WHY A CACHED FENCE IS SAFE. `fast_path_leaf`'s `next_leaf() == 0` test is
/// self-validating -- it reads the truth off the page every time -- and a
/// cached separator is not. It is exact only while the tree's shape is the one
/// the arming descent walked, so every way that shape can move is answered:
///   * a split of the hinted leaf subdivides its interval -- the cached
///     `next_leaf` changes, so the page-side check catches it, and the split
///     path forgets the slot as well;
///   * a neighbour redistribution moves separators BETWEEN siblings without
///     touching the chain, so it forgets the slots of every page in its
///     window itself;
///   * a delete, a graft, a bulk pack, a rollback and a checkpoint can merge
///     leaves, collapse the root or hand a page back to the allocator, and
///     each of those clears the whole cache.
///
/// SACRIFICE (Law 4): about 1.5 KB of fixed state per handle, and a linear
/// scan of sixteen slots per insert. Bought: an ascending run that is not the
/// tree's rightmost -- which is every run but one, in a store where D4 makes
/// every feature a tag in the same tree -- inserts with one page access
/// instead of the tree's full height.
pub struct TagHints {
    slots: [Cell<TagHint>; TAG_HINTS],
    /// Monotonic use counter. Evicting the LEAST RECENTLY USED slot, rather
    /// than the next one round-robin, is what lets a few hot runs share the
    /// cache with many cold ones: the relationship load writes two ascending
    /// runs (forward rows, reverse rows over people) beside a HUNDRED cold
    /// ones (reverse rows, one run per organization), and round-robin handed
    /// a cold organization the hot forward-edge slot every sixteenth write.
    clock: Cell<u64>,
    hits: Cell<u64>,
    attempts: Cell<u64>,
    /// Inserts for which no slot claimed the key at all. Separate from a
    /// refused probe because the two have different costs and different cures:
    /// a miss here costs nothing but the descent it was going to pay anyway,
    /// while a refused probe costs one page read on top of it.
    misses: Cell<u64>,
    /// Off means the handle descends for every insert, exactly as it did
    /// before this cache existed. Not a cargo feature: one build has to be
    /// able to run a workload BOTH ways and compare the files it produced,
    /// which is the only evidence that a hint changes where a write descends
    /// from and not where it lands.
    enabled: Cell<bool>,
    #[cfg(test)]
    relaxed_upper: Cell<bool>,
}

impl Default for TagHints {
    fn default() -> Self {
        TagHints {
            slots: std::array::from_fn(|_| Cell::new(TagHint::FREE)),
            clock: Cell::new(0),
            hits: Cell::new(0),
            attempts: Cell::new(0),
            misses: Cell::new(0),
            enabled: Cell::new(true),
            #[cfg(test)]
            relaxed_upper: Cell::new(false),
        }
    }
}

impl TagHints {
    /// The leaf that may hold `key` without a descent, and the `next_leaf` it
    /// had when armed. Pure memory: the fences are compared here so that a
    /// miss never costs a page read, and only the winner is opened.
    fn find(&self, tree: u16, key: &[u8]) -> Option<(u32, u32)> {
        if !self.enabled.get() { return None; }
        let &tag = key.first()?;
        let relaxed = self.relaxed();
        for slot in &self.slots {
            let mut h = slot.get();
            if h.claims(tree, tag, key, relaxed) {
                h.used = self.tick();
                slot.set(h);
                return Some((h.leaf, h.next));
            }
        }
        None
    }

    fn tick(&self) -> u64 {
        let now = self.clock.get() + 1;
        self.clock.set(now);
        now
    }

    /// Remember `leaf` for `key`'s tag between the fences a descent found.
    /// Any slot naming the same leaf, or covering an overlapping interval of
    /// the same tag, is replaced rather than duplicated: two slots that both
    /// claim one key would make which leaf gets tried depend on scan order.
    fn arm(&self, tree: u16, key: &[u8], leaf: u32, next: u32, fences: LeafFences) {
        if !self.enabled.get() { return; }
        let Some(&tag) = key.first() else { return };
        if !fences.armable { return; }
        let overlaps = |h: &TagHint| {
            h.tree == tree
                && (h.leaf == leaf
                    || (h.tag == tag
                        && fences.upper.get().is_none_or(|u| h.lower.get().is_none_or(|l| l < u))
                        && h.upper.get().is_none_or(|u| fences.lower.get().is_none_or(|l| l < u))))
        };
        let mut at = None;
        for (i, slot) in self.slots.iter().enumerate() {
            if overlaps(&slot.get()) {
                slot.set(TagHint::FREE);
                if at.is_none() { at = Some(i); }
            }
        }
        let at = at
            .or_else(|| self.slots.iter().position(|s| s.get().tree == 0))
            .unwrap_or_else(|| {
                let mut victim = 0;
                let mut oldest = u64::MAX;
                for (i, slot) in self.slots.iter().enumerate() {
                    let used = slot.get().used;
                    if used < oldest { oldest = used; victim = i; }
                }
                victim
            });
        self.slots[at].set(TagHint {
            tree, tag, leaf, next, lower: fences.lower, upper: fences.upper, used: self.tick(),
        });
    }

    /// Forget one slot after its leaf refused the record. PostgreSQL's
    /// `_bt_search_insert` disarms on any rejection; this disarms only the
    /// slot that was wrong, because the other fifteen describe other runs.
    fn forget_leaf(&self, tree: u16, leaf: u32) {
        for slot in &self.slots {
            let h = slot.get();
            if h.tree == tree && h.leaf == leaf { slot.set(TagHint::FREE); }
        }
    }

    /// Forget everything. The answer to every structural change this cache
    /// does not track page by page.
    pub fn clear(&self) {
        for slot in &self.slots { slot.set(TagHint::FREE); }
    }

    /// Forget one tree's slots, for a tree whose root is being freed or whose
    /// handle slot is being recycled.
    pub fn clear_tree(&self, tree: u16) {
        for slot in &self.slots {
            if slot.get().tree == tree { slot.set(TagHint::FREE); }
        }
    }

    /// Diagnostics: inserts that found a slot, and inserts that then landed on
    /// its leaf. Counted, not reasoned about -- the budget tests read these.
    pub fn attempts(&self) -> u64 { self.attempts.get() }
    pub fn hits(&self) -> u64 { self.hits.get() }
    /// Inserts no slot claimed.
    pub fn misses(&self) -> u64 { self.misses.get() }

    /// A cache that is off. Every insert descends, as it did before K1.
    pub fn disabled() -> Self {
        let t = TagHints::default();
        t.set_enabled(false);
        t
    }

    /// Turn the cache on or off for this handle. Turning it off forgets
    /// everything it held, so a handle cannot come back to a stale fence.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.set(on);
        if !on { self.clear(); }
    }

    pub fn enabled(&self) -> bool { self.enabled.get() }

    /// Negative control for the byte-equivalence oracle; see `TagHint::claims`.
    #[cfg(test)]
    pub(crate) fn set_relaxed_upper_fence(&self, on: bool) { self.relaxed_upper.set(on); }
    #[cfg(test)]
    fn relaxed(&self) -> bool { self.relaxed_upper.get() }
    #[cfg(not(test))]
    fn relaxed(&self) -> bool { false }
}

pub struct BTree<'p> {
    pool: &'p BufferPool,
    tree_id: u16,
    root: u32,
    /// The last leaf inserted into, cached the way PostgreSQL caches
    /// `RelationGetTargetBlock`. Tried before descending on the next insert;
    /// see `fast_path_leaf`. One `u32` — Law 1 (no allocation proportional to
    /// the store).
    ///
    /// Borrowed, not owned: `Store` builds a fresh `BTree` on every `put`,
    /// `delete`, `get` and `scan` and drops it immediately (Task 16), so a
    /// hint owned by the `BTree` itself would reset to `None` on every call
    /// and the fast path would never fire through `Store`. `Store` owns the
    /// `Cell` and lends it here for the `BTree`'s lifetime — the equivalent
    /// of PostgreSQL keeping the hint on the `Relation`, not on a per-insert
    /// scan state.
    last_leaf: &'p Cell<Option<u32>>,
    /// Times `insert` used `last_leaf` instead of a full descent. Borrowed
    /// the same way as `last_leaf` and for the same reason: a per-`BTree`
    /// counter would reset every time `Store` opened a fresh one, and the
    /// new `Store`-level tests need a count that survives across calls.
    fast_path_hits: &'p Cell<u64>,
    /// Times `insert` found `last_leaf` armed and called `fast_path_leaf` at
    /// all, hit or miss. Borrowed the same way as `fast_path_hits`, for the
    /// same reason (Task 18): a counter owned by the transient `BTree` would
    /// reset on every `Store::put`, and the disarm test needs a count of
    /// attempts -- not just hits -- that survives across calls to prove the
    /// hint stops trying after the first rejection instead of retrying
    /// forever on a workload it can never satisfy.
    fast_path_attempts: &'p Cell<u64>,
    /// The per-keyspace append hints, borrowed from the owning handle for the
    /// same reason `last_leaf` is: `Store` and `PageWalStore` build a fresh
    /// `BTree` for every single operation, so a cache owned here would be
    /// empty on arrival every time. `None` is a tree opened without them --
    /// every path that can invalidate a hint clears through this same borrow,
    /// so a handle either passes it everywhere or nowhere.
    tags: Option<&'p TagHints>,
}

/// Immutable plan for replacing exactly one live boundary leaf with a
/// verified candidate subtree. The old leaf's records are copied into the
/// candidate, so keys outside the empty graft interval are preserved without
/// being reinserted.
pub(crate) struct GraftBoundary {
    leaf: u32,
    path: Vec<(u32, usize)>,
    records: Vec<Vec<u8>>,
    split: usize,
    pub(crate) old_next: u32,
    pub(crate) right_page: Option<u32>,
}

pub(crate) struct GraftCandidate {
    pub(crate) root: u32,
    pub(crate) rows: u64,
    pub(crate) min: Vec<u8>,
    pub(crate) max: Vec<u8>,
    pub(crate) last_next: u32,
}

/// vlen sentinel marking a spilled value. Unambiguous: a real inline value
/// never exceeds MAX_RECORD_LEN (~4KB), far below 0xFFFF.
pub(crate) const OVERFLOW_VLEN: u16 = 0xFFFF;

/// The 12-byte body a marker record carries in place of its value:
/// [total_len u32][head_page u32][crc32 of the FULL value].
/// The crc is Law 5 for the chain as a WHOLE: each overflow page already
/// carries its own page checksum, but a chain crossed with another record's
/// chain (or truncated by a lost page) is made of individually-valid pages --
/// only a checksum over the assembled value catches that. Blast radius: this
/// one record.
pub(crate) fn enc_marker(total: u32, head: u32, crc: u32) -> [u8; 12] {
    let mut m = [0u8; 12];
    m[0..4].copy_from_slice(&total.to_le_bytes());
    m[4..8].copy_from_slice(&head.to_le_bytes());
    m[8..12].copy_from_slice(&crc.to_le_bytes());
    m
}

/// Overflow page layout, after the standard 40-byte page header:
///   [next_page u32][used u16][content ...]
/// Ordinary pages: PageKind::Overflow, sealed+checksummed at the pool write
/// like every other page.
pub(crate) const OV_NEXT: usize = crate::page::HEADER_LEN;
pub(crate) const OV_USED: usize = OV_NEXT + 4;
pub(crate) const OV_DATA: usize = OV_USED + 2;
pub(crate) const OV_CAP: usize = crate::page::PAGE_SIZE - OV_DATA;

/// Write `val` as a chain, return (head_page, crc). Pages are allocated
/// forward and linked as built; the LAST page's next is 0.
pub(crate) fn write_overflow(pool: &BufferPool, val: &[u8]) -> Result<(u32, u32)> {
    if pool.resource_limits().is_some_and(|l| val.len() > l.record_bytes as usize) {
        return Err(Error::ResourceLimit("overflow value exceeds record allowance"));
    }
    let crc = crc32c::crc32c(val);
    let mut chunks: Vec<&[u8]> = val.chunks(OV_CAP).collect();
    if chunks.is_empty() { chunks.push(&[]); }
    let mut pages = Vec::with_capacity(chunks.len());
    for _ in &chunks {
        let w = pool.allocate()?;
        pages.push(w.page_no());
        drop(w);
    }
    for (i, chunk) in chunks.iter().enumerate() {
        let mut w = pool.get_mut(pages[i])?;
        let b = w.bytes_mut();
        crate::page::PageMut::init(b, crate::page::PageKind::Overflow, 0, pages[i]).finalise(0);
        let next = if i + 1 < pages.len() { pages[i + 1] } else { 0 };
        b[OV_NEXT..OV_NEXT + 4].copy_from_slice(&next.to_le_bytes());
        b[OV_USED..OV_USED + 2].copy_from_slice(&(chunk.len() as u16).to_le_bytes());
        b[OV_DATA..OV_DATA + chunk.len()].copy_from_slice(chunk);
    }
    Ok((pages[0], crc))
}

/// Assemble a spilled value from its marker. Verifies the whole-value crc and
/// every structural field before believing anything (Law 5): a wrong page
/// kind, a length past the page, a chain longer than the file, or a crc
/// mismatch all refuse the RECORD -- never return partial bytes.
pub(crate) fn read_overflow(pool: &BufferPool, marker: &[u8]) -> Result<Vec<u8>> {
    if marker.len() != 12 {
        return Err(Error::Corrupt { page_no: 0, why: "overflow marker wrong size" });
    }
    let total = u32::from_le_bytes(marker[0..4].try_into().unwrap()) as usize;
    if pool.resource_limits().is_some_and(|l| total > l.record_bytes as usize) {
        return Err(Error::ResourceLimit("stored overflow value exceeds record allowance"));
    }
    let head = u32::from_le_bytes(marker[4..8].try_into().unwrap());
    let want_crc = u32::from_le_bytes(marker[8..12].try_into().unwrap());

    // Fast path (2e ablation A1): `write_overflow` allocates its pages in one
    // tight loop off a monotone counter, so a chain's page numbers are always
    // consecutive -- the whole chain is one speculative pread, verified page
    // by page, never cached. Falls back to the per-page pool path when any
    // page is resident (the frame may be dirtier than disk) or when anything
    // read fails to look like exactly the chain the marker promised. The
    // fallback re-reads from page 0 of the chain: this path trusts NOTHING it
    // saw here.
    let n_pages = total.div_ceil(OV_CAP).max(1);
    if n_pages <= u32::MAX as usize {
        let mut buf = Vec::new();
        if let Ok(true) = pool.read_run_uncached(head, n_pages as u32, &mut buf) {
            let mut out = Vec::with_capacity(total);
            let mut contiguous = true;
            for i in 0..n_pages {
                let b = &buf[i * crate::page::PAGE_SIZE..(i + 1) * crate::page::PAGE_SIZE];
                let page_no = head + i as u32;
                let Ok(p) = PageRef::open_resident(b, page_no) else { contiguous = false; break };
                if p.kind() != crate::page::PageKind::Overflow { contiguous = false; break; }
                let next = u32::from_le_bytes(b[OV_NEXT..OV_NEXT + 4].try_into().unwrap());
                let want_next = if i + 1 < n_pages { page_no + 1 } else { 0 };
                if next != want_next { contiguous = false; break; }
                let used = u16::from_le_bytes(b[OV_USED..OV_USED + 2].try_into().unwrap()) as usize;
                if used > OV_CAP || out.len() + used > total { contiguous = false; break; }
                out.extend_from_slice(&b[OV_DATA..OV_DATA + used]);
            }
            if contiguous && out.len() == total && crc32c::crc32c(&out) == want_crc {
                return Ok(out);
            }
            // else: fall through to the authoritative per-page walk, which
            // will refuse the record with a precise reason if it is corrupt.
        }
    }

    let mut page = head;
    let mut out = Vec::with_capacity(total);
    let mut hops = 0u32;
    let cap = pool.page_count();
    while page != 0 {
        hops += 1;
        if hops > cap {
            return Err(Error::Corrupt { page_no: page, why: "overflow chain cycles" });
        }
        let r = pool.get(page)?;
        let p = PageRef::open_resident(&r, page)?;
        if p.kind() != crate::page::PageKind::Overflow {
            return Err(Error::Corrupt { page_no: page, why: "chain points at a non-overflow page" });
        }
        let b = &r[..];
        let next = u32::from_le_bytes(b[OV_NEXT..OV_NEXT + 4].try_into().unwrap());
        let used = u16::from_le_bytes(b[OV_USED..OV_USED + 2].try_into().unwrap()) as usize;
        if used > OV_CAP || out.len() + used > total {
            return Err(Error::Corrupt { page_no: page, why: "overflow length out of bounds" });
        }
        out.extend_from_slice(&b[OV_DATA..OV_DATA + used]);
        drop(r);
        page = next;
    }
    if out.len() != total || crc32c::crc32c(&out) != want_crc {
        return Err(Error::Corrupt { page_no: 0, why: "overflow value fails its checksum" });
    }
    Ok(out)
}

/// Validate an old chain completely before allowing its pages onto the
/// retirement list. No value buffer: one (page, birth-generation) pair per page of
/// the changed value. The caller removes the marker first, then retires these page IDs
/// through the existing generation/reader horizon; nothing is freed on an
/// incomplete or corrupt chain. Cost is proportional to the old large value.
fn replaced_overflow_pages(pool: &BufferPool, rec: &[u8]) -> Result<Vec<(u32, u64)>> {
    let (_, marker, overflow) = validated_leaf(rec);
    if !overflow { return Ok(Vec::new()); }
    let total = u32::from_le_bytes(marker[..4].try_into().unwrap()) as usize;
    if pool.resource_limits().is_some_and(|l| total > l.record_bytes as usize) {
        return Err(Error::ResourceLimit("stored overflow value exceeds record allowance"));
    }
    let mut no = u32::from_le_bytes(marker[4..8].try_into().unwrap());
    let want = u32::from_le_bytes(marker[8..12].try_into().unwrap());
    let bound = total.div_ceil(OV_CAP).max(1);
    let mut pages = Vec::new();
    let (mut bytes, mut crc) = (0usize, 0u32);
    while no != 0 {
        if no < 2 || no >= pool.page_count() || pages.len() >= bound {
            return Err(Error::Corrupt { page_no: no, why: "retired overflow chain bounds" });
        }
        let r = pool.get(no)?;
        let p = PageRef::open_resident(&r, no)?;
        if p.kind() != PageKind::Overflow || p.tree_id() != 0 {
            return Err(Error::Corrupt { page_no: no, why: "retired overflow page kind" });
        }
        let used = u16::from_le_bytes(r[OV_USED..OV_USED+2].try_into().unwrap()) as usize;
        // Writer chains fill every non-final page. This also bounds traversal
        // without a database-sized visited set, even for forged cycles.
        if used != (total-bytes).min(OV_CAP) {
            return Err(Error::Corrupt { page_no: no, why: "retired overflow length" });
        }
        crc = crc32c::crc32c_append(crc, &r[OV_DATA..OV_DATA+used]);
        bytes += used;
        let birth = if pool.is_frozen(no) { p.lsn() } else { pool.stamp_generation() };
        pages.push((no, birth));
        no = u32::from_le_bytes(r[OV_NEXT..OV_NEXT+4].try_into().unwrap());
    }
    if pages.len() != bound || bytes != total || crc != want {
        return Err(Error::Corrupt { page_no: 0, why: "retired overflow whole-value checksum or length" });
    }
    Ok(pages)
}

/// A leaf record whose value is an overflow marker: vlen = OVERFLOW_VLEN,
/// body = enc_marker(). dec_val() on such a record returns the 12 marker
/// bytes; is_marker() is how readers know to resolve them.
pub(crate) fn enc_leaf_marker(key: &[u8], marker: &[u8; 12]) -> Vec<u8> {
    let mut r = Vec::with_capacity(4 + key.len() + 12);
    r.extend_from_slice(&(key.len() as u16).to_le_bytes());
    r.extend_from_slice(key);
    r.extend_from_slice(&OVERFLOW_VLEN.to_le_bytes());
    r.extend_from_slice(marker);
    r
}

/// `compact` is the encoding THIS DATABASE declares (see
/// `BufferPool::compact_cells`), not what this build was compiled with. A
/// database that declares the plain encoding gets plain cells even from a
/// `compact-cells` build, and the other way round; both families decode
/// everywhere, so a page may legitimately hold a mixture after an encoding
/// was declared differently at some point in its history.
pub(crate) fn enc_leaf(key: &[u8], val: &[u8], compact: bool) -> Vec<u8> {
    if compact && key.first().is_some_and(|b|(0x81..=0x88).contains(b))
        && key.len()==1+(key[0]-0x80)as usize {
        // The width-tagged integer key gives its own length; the validated
        // page slot gives the cell's end. FF + 81..88 cannot be a legal v1
        // u16 key length in a 4 KiB page. Large values keep overflow markers.
        let mut r=Vec::with_capacity(1+key.len()+val.len());
        r.push(0xff);r.extend_from_slice(key);r.extend_from_slice(val);return r;
    }
    if compact && key.len() <= 0x0fff {
        // 0x4xxx cannot be a legal legacy key length in a 4KiB page. The
        // CRC/bounds-checked slot already provides the value's end, so ordinary
        // keys need no duplicate u16 value length. Overflow markers stay distinct.
        let mut r=Vec::with_capacity(2+key.len()+val.len());
        r.extend_from_slice(&(0x4000|key.len() as u16).to_le_bytes());
        r.extend_from_slice(key);r.extend_from_slice(val);return r;
    }
    let mut r = Vec::with_capacity(4 + key.len() + val.len());
    r.extend_from_slice(&(key.len() as u16).to_le_bytes());
    r.extend_from_slice(key);
    r.extend_from_slice(&(val.len() as u16).to_le_bytes());
    r.extend_from_slice(val);
    r
}
/// Fast access after `validate_records` accepted the complete page through the
/// shared fallible decoder. These helpers never receive unvalidated disk bytes:
/// `open_cached` owns that boundary, and writable-page callers validate before
/// their first access. Keeping the proof on the buffer residency avoids paying
/// the same bounds checks again for every row of every hot scan.
#[inline(always)]
fn validated_key(rec: &[u8]) -> &[u8] {
    if rec[0]==0xff && (0x81..=0x88).contains(&rec[1]) {
        return &rec[1..2+(rec[1]-0x80)as usize];
    }
    // Unconditional: the cell family is stored in the bytes, so a build
    // without the `compact-cells` feature must still read one.
    if rec[1]&0xf0==0x40 {
        let k=(u16::from_le_bytes([rec[0],rec[1]])&0x0fff) as usize;
        return &rec[2..2+k];
    }
    let k = u16::from_le_bytes([rec[0], rec[1]]) as usize;
    &rec[2..2 + k]
}

#[inline(always)]
fn validated_leaf(rec: &[u8]) -> (&[u8], &[u8], bool) {
    if rec[0]==0xff && (0x81..=0x88).contains(&rec[1]) {
        let end=2+(rec[1]-0x80)as usize;return (&rec[1..end],&rec[end..],false);
    }
    if rec[1]&0xf0==0x40 {
        let end=2+(u16::from_le_bytes([rec[0],rec[1]])&0x0fff) as usize;
        return (&rec[2..end],&rec[end..],false);
    }
    let k = u16::from_le_bytes([rec[0], rec[1]]) as usize;
    let v = u16::from_le_bytes([rec[2 + k], rec[3 + k]]);
    let value_len = if v == OVERFLOW_VLEN { 12 } else { v as usize };
    (&rec[2..2 + k], &rec[4 + k..4 + k + value_len], v == OVERFLOW_VLEN)
}
fn enc_interior(key: &[u8], child: u32) -> Vec<u8> {
    let mut r = Vec::with_capacity(6 + key.len());
    r.extend_from_slice(&(key.len() as u16).to_le_bytes());
    r.extend_from_slice(key);
    r.extend_from_slice(&child.to_le_bytes());
    r
}
#[inline(always)]
fn validated_child(rec: &[u8]) -> u32 {
    let k = u16::from_le_bytes([rec[0], rec[1]]) as usize;
    u32::from_le_bytes(rec[2 + k..6 + k].try_into().unwrap())
}

fn child_at(pool: &BufferPool, page: &PageRef<'_>, index: usize) -> Result<u32> {
    let child = if index == 0 {
        page.child0()
    } else {
        validated_child(page.slot(index - 1))
    };
    if child == 0 || child >= pool.page_count() {
        return Err(Error::Corrupt {
            page_no: page.page_no(),
            why: "interior child pointer is outside the data file",
        });
    }
    Ok(child)
}

/// Build a page's contents in scratch, then commit them to the frame in one copy.
///
/// `PageMut::init` zeroes the page. Writing straight into a live frame and then
/// hitting a fallible `insert_slot` therefore leaves that frame wiped, dirty and
/// never finalised -- a stale checksum over a blank page, which the next read
/// refuses, losing every key on it. That is exactly the replace-path defect this
/// task already fixed once, relocated into the split.
///
/// Today `split_point`'s `+4` per-record accounting is the same arithmetic
/// `insert_slot` checks, so these inserts provably cannot fail. But that is an
/// invariant spanning two functions with nothing enforcing it, and a change to
/// `SLOT_LEN` or `HEADER_LEN` would reintroduce total leaf loss with no test
/// able to catch it. Building in scratch removes the class rather than relying
/// on the coincidence.
///
/// SACRIFICE (Law 4): one page of scratch per split plus one extra copy.
/// Bought: no error path can destroy a live page.
pub(crate) fn build_page(
    kind: PageKind,
    tree_id: u16,
    page_no: u32,
    recs: &[Vec<u8>],
    child0_or_next: u32,
) -> Result<Vec<u8>> {
    let mut scratch = vec![0u8; PAGE_SIZE];
    build_page_into(&mut scratch, kind, tree_id, page_no, recs.iter().map(|r| r.as_slice()), child0_or_next)?;
    Ok(scratch)
}

/// The same page build, into a buffer the caller already owns and over
/// records the caller does not have to own. This is the ONE inner insert
/// loop: `build_page` allocates a fresh page for it, the `slotref-split`
/// path hands it a reusable scratch page and an iterator over records that
/// are still sitting in their original page images. Keeping one loop is the
/// point -- two copies of "append every record, then stamp next_leaf" is two
/// places for the split's byte-for-byte contract to drift.
pub(crate) fn build_page_into<'r>(
    dst: &mut [u8],
    kind: PageKind,
    tree_id: u16,
    page_no: u32,
    recs: impl Iterator<Item = &'r [u8]>,
    child0_or_next: u32,
) -> Result<()> {
    let mut p = PageMut::init(dst, kind, tree_id, page_no);
    for r in recs {
        let at = p.nentries_pub();
        p.insert_slot(at, r)?;
    }
    p.set_next_leaf(child0_or_next);
    p.finalise(0);
    Ok(())
}

/// Index of the first entry whose key is >= `key`.
fn lower_bound(p: &PageRef, key: &[u8]) -> Result<usize> {
    let (mut lo, mut hi) = (0usize, p.nentries());
    while lo < hi {
        let mid = (lo + hi) / 2;
        if validated_key(p.slot(mid)) < key { lo = mid + 1 } else { hi = mid }
    }
    Ok(lo)
}

/// Index of the first entry whose key is > `key`.
fn upper_bound(p: &PageRef, key: &[u8]) -> Result<usize> {
    let (mut lo, mut hi) = (0usize, p.nentries());
    while lo < hi {
        let mid = (lo + hi) / 2;
        if validated_key(p.slot(mid)) <= key { lo = mid + 1 } else { hi = mid }
    }
    Ok(lo)
}

/// Where to cut a page's entries so that **both** halves fit in a page.
///
/// Splitting at `len / 2` by COUNT is wrong for variable-length records. So is
/// cutting at the first prefix to cross half the bytes: that prefix can exceed
/// a page on its own. Concretely, with a 4056-byte usable page, forty 100-byte
/// entries and one 4000-byte entry inserted in the middle, the first prefix to
/// reach half the bytes is 6000 bytes — a page and a half.
///
/// So this checks the constraint directly rather than approximating it: among
/// the cuts where the left side AND the right side each fit, take the most
/// balanced. Returns `None` when no two-way cut exists, which happens only when
/// one record is so large that no arrangement of the rest leaves room. The
/// caller refuses that insert rather than corrupting a page; the real answer is
/// overflow pages for large values, which the spec defers.
#[cfg(all(feature = "sqlite-balance", any(test, not(feature = "slotref-split"))))]
fn neighbor_cell_cuts(sizes: &[usize], usable: usize, existing: usize) -> Option<Vec<usize>> {
    if sizes.is_empty() || sizes.iter().any(|&v| v==0 || v>usable) { return None; }
    let n=sizes.len();
    let mut prefix=vec![0usize];
    for &size in sizes { prefix.push(prefix.last()?.checked_add(size)?); }
    // Positive, indivisible cells: the longest fitting prefix minimizes the
    // number of contiguous pages. Compute every suffix's minimum in O(cells).
    let mut ends=vec![0;n];let mut end=0;
    for i in 0..n {
        while end<n && prefix[end+1]-prefix[i]<=usable { end+=1; }
        ends[i]=end;
    }
    let mut needed=vec![0;n+1];
    for i in (0..n).rev() { needed[i]=1+needed[ends[i]]; }
    let groups=existing.max(needed[0]);
    if groups>existing+1 || n<groups { return None; }
    let mut cuts=vec![0];
    for group in 0..groups-1 {
        let begin=*cuts.last()?;let left=groups-group-1;
        let target=(prefix[n]-prefix[begin])/(left+1);
        let mut best=None;
        for end in begin+1..=ends[begin].min(n-left) {
            if needed[end]>left { continue; }
            let delta=(prefix[end]-prefix[begin]).abs_diff(target);
            if best.is_none_or(|(_,old)|delta<old) { best=Some((end,delta)); }
        }
        cuts.push(best?.0);
    }
    cuts.push(n);Some(cuts)
}

#[cfg(all(test,feature="sqlite-balance"))]
mod neighbor_capacity_tests {
    use super::neighbor_cell_cuts;
    #[test]
    fn full_leaf_and_one_sibling_use_at_most_three_pages() {
        let sizes=vec![272;29];
        let cuts=neighbor_cell_cuts(&sizes,4056,2).unwrap();
        assert_eq!(cuts.len(),4);
        for w in cuts.windows(2) { assert!((w[1]-w[0])*272<=4056); }
    }
    #[test]
    fn forty_three_indivisible_cells_need_four_pages() {
        let sizes=vec![272;43];
        assert!(sizes.iter().sum::<usize>()<3*4056);
        let cuts=neighbor_cell_cuts(&sizes,4056,3).unwrap();
        assert_eq!(cuts.len(),5);
        for w in cuts.windows(2) { assert!((w[1]-w[0])*272<=4056); }
    }
    #[test]
    fn bounded_planner_matches_exhaustive_partition_oracle() {
        fn possible(s:&[usize],pages:usize)->bool {
            if pages==0 { return s.is_empty(); }
            let mut sum=0;
            for end in 1..=s.len() {
                sum+=s[end-1];if sum>7 { break; }
                if possible(&s[end..],pages-1) { return true; }
            }
            false
        }
        for n in 2..=7 {
            for mut code in 0..3usize.pow(n as u32) {
                let s:Vec<_>=(0..n).map(|_| {let v=[1,3,5][code%3];code/=3;v}).collect();
                let expected=(2..=3).find(|&p|possible(&s,p));
                let result=neighbor_cell_cuts(&s,7,2);
                assert_eq!(result.as_ref().map(|v|v.len()-1),expected,"{s:?}");
                if let Some(cuts)=result {
                    assert_eq!(*cuts.last().unwrap(),n);
                    for w in cuts.windows(2) {assert!(w[1]>w[0]);assert!(s[w[0]..w[1]].iter().sum::<usize>()<=7);}
                }
            }
        }
    }
}

fn split_point(recs: &[Vec<u8>], usable: usize) -> Option<usize> {
    let sizes: Vec<usize> = recs.iter().map(|r| r.len() + 4).collect();
    let total: usize = sizes.iter().sum();
    let mut acc = 0usize;
    let mut best: Option<(usize, usize)> = None; // (imbalance, mid)
    for (j, size) in sizes.iter().enumerate().take(recs.len().saturating_sub(1)) {
        acc += size;
        let (left, right) = (acc, total - acc);
        if left <= usable && right <= usable {
            let imbalance = left.abs_diff(right);
            if best.is_none_or(|(b, _)| imbalance < b) {
                best = Some((imbalance, j + 1));
            }
        }
    }
    best.map(|(_, mid)| mid)
}

// ===================================================================== L2.3-2
// Allocation-free leaf split and redistribution.
//
// THE FINDING. On a Pi, a typed put spends about 46% of its CPU in the split
// path, and malloc+free together are a quarter of all cycles. The reason is
// visible in one line of the old split: `(0..p.nentries()).map(|j|
// p.slot(j).to_vec())`. Every record on the page becomes its own heap
// allocation -- about 86 of them for an ordinary leaf -- and a three-sibling
// redistribution does that for three leaves plus the parent's separators,
// about 320. None of those copies is needed: the records are already sitting,
// contiguous and bounds-checked, in pinned buffer-pool frames.
//
// THE SHAPE. SQLite's `balance_nonroot` does not copy cells either; it builds
// an array of cell POINTERS into the existing page images plus one scratch
// allocation, and assembles the new pages from that. `SlotRef` is that
// pointer array, written in the terms this file already uses: which page image
// (an index into a small `frames` array) and the slot directory's own
// (offset, length) pair, taken verbatim, with no decoding at all.
//
// WHAT DOES NOT CHANGE. Every decision: `split_point`, `neighbor_cell_cuts`,
// the `at_point` append test, the window order, the fit probe, the shadowing.
// The page images and separator bytes are identical, which is a claim about
// output and is therefore tested as one -- see `split_byte_equivalence`.
//
// SACRIFICE (Law 4): about 44 KiB of scratch, per thread that ever splits,
// held for the process's life instead of being allocated and freed per split
// (10 reusable pages + two SlotRef arrays that grow to the largest window
// seen, bounded by three pages of records). It is RAM proportional to a page,
// not to the store, so Law 1 is untouched -- but it is no longer transient,
// and that is the cost. Bought: a steady-state split allocates a small
// bounded constant instead of one allocation per record on the page.

/// A record left where it is. `page_idx` selects one of the `frames` the
/// caller passes alongside; `off`/`len` are the slot directory's own numbers.
#[cfg(any(test, feature = "slotref-split"))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct SlotRef { page_idx: u8, off: u16, len: u16 }

/// Frame indices. Fixed, because a `SlotRef` built while gathering the
/// splitting leaf is later resolved against the redistribution's larger frame
/// array, and the two must agree about what index 0 and 1 mean.
#[cfg(any(test, feature = "slotref-split"))]
mod frame {
    pub const LEAF: u8 = 0;   // the splitting leaf's image, copied to scratch
    pub const REC: u8 = 1;    // the incoming record
    pub const SIB0: u8 = 2;   // first redistribution sibling
    pub const SIB1: u8 = 3;   // second redistribution sibling
    pub const PARENT: u8 = 4; // the parent's image, copied to scratch
    pub const SEPS: u8 = 5;   // separators this redistribution had to rebuild
}

#[cfg(any(test, feature = "slotref-split"))]
#[inline(always)]
fn rec_of<'f>(frames: &[&'f [u8]], s: SlotRef) -> &'f [u8] {
    let b = frames[s.page_idx as usize];
    &b[s.off as usize..s.off as usize + s.len as usize]
}

/// The reusable buffers a split borrows instead of allocating.
///
/// Three thread-locals rather than one struct with three fields, because the
/// leaf image, the leaf's slot list and everything else are borrowed at the
/// same time by different parameters of the same call; splitting them is how
/// the borrow checker is told they never alias. A `BTree` is rebuilt per
/// operation (`Store::put` opens one and drops it), so the scratch cannot live
/// on it; the buffer pool is single-threaded by construction (`RefCell`
/// inside), so a thread-local is exactly one scratch per live writer.
#[cfg(any(test, feature = "slotref-split"))]
pub(crate) struct SplitScratch {
    /// Sibling page images, two pages: a redistribution window is at most
    /// three leaves and one of them is the splitting leaf itself.
    sib: Vec<u8>,
    /// The parent's page image.
    parent: Vec<u8>,
    /// Built page images, four pages: up to three leaves plus the parent.
    out: Vec<u8>,
    /// Interior records a redistribution had to rebuild (new key, or same key
    /// and a new child). Three pages: at most three such records, each at most
    /// one page's worth.
    seps: Vec<u8>,
    /// The window's records, in order, across the whole window.
    win: Vec<SlotRef>,
    /// The parent's separators as they stand, and as they will stand.
    pr0: Vec<SlotRef>,
    pr: Vec<SlotRef>,
    sizes: Vec<usize>,
    prefix: Vec<usize>,
    ends: Vec<usize>,
    needed: Vec<usize>,
    cuts: Vec<usize>,
    /// The parent's children as they stand, and after any shadowing.
    ids: Vec<u32>,
    newids: Vec<u32>,
}

#[cfg(any(test, feature = "slotref-split"))]
impl SplitScratch {
    fn new() -> Self {
        SplitScratch {
            sib: vec![0u8; 2 * PAGE_SIZE],
            parent: vec![0u8; PAGE_SIZE],
            out: vec![0u8; 4 * PAGE_SIZE],
            seps: vec![0u8; 3 * PAGE_SIZE],
            win: Vec::with_capacity(2048),
            pr0: Vec::with_capacity(1024),
            pr: Vec::with_capacity(1024),
            sizes: Vec::with_capacity(2048),
            prefix: Vec::with_capacity(2049),
            ends: Vec::with_capacity(2048),
            needed: Vec::with_capacity(2049),
            cuts: Vec::with_capacity(8),
            ids: Vec::with_capacity(1024),
            newids: Vec::with_capacity(1024),
        }
    }
}

#[cfg(any(test, feature = "slotref-split"))]
thread_local! {
    /// The splitting leaf's page image. Copied out of the frame once so the
    /// write guard can be handed to `redistribute_neighbors_ref` as `&mut`
    /// while its records are still readable -- 4 KiB of memcpy in place of one
    /// allocation per record.
    static SPLIT_LEAF: std::cell::RefCell<Vec<u8>> =
        std::cell::RefCell::new(vec![0u8; PAGE_SIZE]);
    /// The splitting leaf's records with the incoming one already in place.
    static SPLIT_SLOTS: std::cell::RefCell<Vec<SlotRef>> =
        std::cell::RefCell::new(Vec::with_capacity(1024));
    static SPLIT_SCRATCH: std::cell::RefCell<SplitScratch> =
        std::cell::RefCell::new(SplitScratch::new());
}

/// Write one interior record -- `[klen u16][key][child u32]`, byte for byte
/// what `enc_interior` builds -- into the separator arena, and return the
/// `SlotRef` that names it.
#[cfg(all(feature = "sqlite-balance", any(test, feature = "slotref-split")))]
fn push_sep(seps: &mut [u8], at: &mut usize, key: &[u8], child: u32) -> SlotRef {
    let off = *at;
    let len = 6 + key.len();
    seps[off..off + 2].copy_from_slice(&(key.len() as u16).to_le_bytes());
    seps[off + 2..off + 2 + key.len()].copy_from_slice(key);
    seps[off + 2 + key.len()..off + len].copy_from_slice(&child.to_le_bytes());
    *at = off + len;
    SlotRef { page_idx: frame::SEPS, off: off as u16, len: len as u16 }
}

/// Re-point an arena separator at a different child. `enc_interior` puts the
/// child in the record's trailing four bytes, so this is that patch -- the
/// same one `patch_child` performs on a live page.
#[cfg(all(feature = "sqlite-balance", any(test, feature = "slotref-split")))]
fn patch_sep_child(seps: &mut [u8], s: SlotRef, child: u32) {
    let end = s.off as usize + s.len as usize;
    seps[end - 4..end].copy_from_slice(&child.to_le_bytes());
}

/// `split_point` over borrowed records. Identical arithmetic: a record costs
/// its length plus the four bytes of its slot-directory entry, and among the
/// cuts where both sides fit a page the most balanced one wins.
#[cfg(any(test, feature = "slotref-split"))]
fn split_point_ref(slots: &[SlotRef], usable: usize) -> Option<usize> {
    let total: usize = slots.iter().map(|s| s.len as usize + 4).sum();
    let mut acc = 0usize;
    let mut best: Option<(usize, usize)> = None; // (imbalance, mid)
    for (j, s) in slots.iter().enumerate().take(slots.len().saturating_sub(1)) {
        acc += s.len as usize + 4;
        let (left, right) = (acc, total - acc);
        if left <= usable && right <= usable {
            let imbalance = left.abs_diff(right);
            if best.is_none_or(|(b, _)| imbalance < b) {
                best = Some((imbalance, j + 1));
            }
        }
    }
    best.map(|(_, mid)| mid)
}

/// `neighbor_cell_cuts` writing into buffers the caller reuses. Line for line
/// the same planner; only the four `vec![]`s became `clear()`s. `cuts` holds
/// the answer; `false` is the original's `None`.
#[cfg(all(feature = "sqlite-balance", any(test, feature = "slotref-split")))]
#[allow(clippy::too_many_arguments)]
fn neighbor_cell_cuts_into(
    sizes: &[usize], usable: usize, existing: usize,
    prefix: &mut Vec<usize>, ends: &mut Vec<usize>, needed: &mut Vec<usize>, cuts: &mut Vec<usize>,
) -> bool {
    cuts.clear();
    if sizes.is_empty() || sizes.iter().any(|&v| v == 0 || v > usable) { return false; }
    let n = sizes.len();
    prefix.clear();
    prefix.push(0);
    for &size in sizes {
        match prefix.last().and_then(|p| p.checked_add(size)) {
            Some(v) => prefix.push(v),
            None => return false,
        }
    }
    ends.clear();
    ends.resize(n, 0);
    let mut end = 0;
    for i in 0..n {
        while end < n && prefix[end + 1] - prefix[i] <= usable { end += 1; }
        ends[i] = end;
    }
    needed.clear();
    needed.resize(n + 1, 0);
    for i in (0..n).rev() { needed[i] = 1 + needed[ends[i]]; }
    let groups = existing.max(needed[0]);
    if groups > existing + 1 || n < groups { return false; }
    cuts.push(0);
    for group in 0..groups - 1 {
        let begin = *cuts.last().expect("cuts always holds its first entry");
        let left = groups - group - 1;
        let target = (prefix[n] - prefix[begin]) / (left + 1);
        let mut best = None;
        for end in begin + 1..=ends[begin].min(n - left) {
            if needed[end] > left { continue; }
            let delta = (prefix[end] - prefix[begin]).abs_diff(target);
            if best.is_none_or(|(_, old)| delta < old) { best = Some((end, delta)); }
        }
        match best {
            Some((e, _)) => cuts.push(e),
            None => { cuts.clear(); return false; }
        }
    }
    cuts.push(n);
    true
}

pub struct RangeIter<'p> {
    pool: &'p BufferPool,
    tree_id: u16,
    page: u32,
    idx: usize,
    done: bool,
    /// The descent path to the current leaf: (interior page, chosen child
    /// index), root first. Child index 0 means child0, k means slot k-1.
    ///
    /// 2f: scans advance THROUGH THE PARENT instead of following the
    /// on-page sibling pointer. Under copy-on-write a modified leaf moves
    /// to a fresh page number and its left neighbour's stored `next_leaf`
    /// silently names the STALE version -- the neighbour is not on any
    /// descent path, so nothing can fix it. The parent path has no such
    /// problem, and unlike re-descending from the root per leaf (measured:
    /// range_filter 0.53 -> 1.13 ms, rejected under D25) it costs one page
    /// open per crossing, the same as the chain did. Path staleness cannot
    /// occur: mutating the tree during a scan is already forbidden for the
    /// writer, and snapshot readers hold an immutable root. `next_leaf`
    /// stays on disk only as the rightmost flag (zero vs nonzero survives
    /// shadowing) for the append fast path.
    path: Vec<(u32, usize)>,
    /// One leaf's records, drained per `next()`, but only once a scan has
    /// PROVED long. Per-entry re-pinning cost ~132ns/key (`open_resident`
    /// bounds-checks every slot per open -- O(entries^2) per leaf) and made
    /// index ranges 7x slower than SQLite; whole-leaf batching fixed that and
    /// then cost graph hops 2-3x, because a 3-edge hop copied a ~130-entry
    /// leaf. So: the first `SHORT_SCAN` entries are served per-entry (a hop
    /// never notices), and batching starts at the threshold (a range scan
    /// amortises everything after). Bounded by leaf capacity; Law 1 intact.
    buf: std::collections::VecDeque<(Vec<u8>, Vec<u8>, bool)>,
    served: u32,
    /// Leaves visited, and the ceiling. A sibling chain cannot legitimately visit
    /// more leaves than the file has pages, so exceeding it means it cycles.
    leaves: u32,
    max_leaves: u32,
    /// Live pin of `page` for [`RangeIter::peek_at_or_after`]. Held across
    /// successive peeks into the same leaf so a lockstep cursor pays one
    /// `pool.get` per leaf, not per row. Dropped before `advance()` and
    /// before overflow-chain walks. `next()` / `for_each_ref` drop it on
    /// entry so their existing pin accounting is unchanged.
    pin: Option<PinnedRead<'p>>,
}

/// A descending range cursor. Unlike reversing a forward [`RangeIter`], this
/// pins one leaf at a time and never materialises the range it is walking.
pub struct ReverseRangeIter<'p> {
    pool: &'p BufferPool,
    tree_id: u16,
    page: u32,
    /// Exclusive slot bound in the current leaf; the next slot is `idx - 1`.
    idx: usize,
    done: bool,
    path: Vec<(u32, usize)>,
    leaves: u32,
    max_leaves: u32,
    /// Live pin of `page` for [`ReverseRangeIter::peek_ref`], held across
    /// successive peeks into the same leaf exactly as the forward cursor holds
    /// its own. `for_each_ref` never sets it and is unaffected.
    pin: Option<PinnedRead<'p>>,
    /// An overflow record resolved out of its chain. It cannot be borrowed
    /// from the pinned leaf, so it is parked here and served from here; the
    /// slot index has already stepped past it when it lands.
    pending: Option<(Vec<u8>, Vec<u8>)>,
}

impl<'p> BTree<'p> {
    pub fn create(
        pool: &'p BufferPool,
        tree_id: u16,
        last_leaf: &'p Cell<Option<u32>>,
        fast_path_hits: &'p Cell<u64>,
        fast_path_attempts: &'p Cell<u64>,
    ) -> Result<Self> {
        let mut w = pool.allocate()?;
        let no = w.page_no();
        // Page 0 is the superblock, and `next_leaf == 0` is how a leaf says it
        // has no sibling. A tree page numbered 0 would make that sentinel
        // ambiguous, so the caller must have reserved page 0 before creating any
        // tree. This is the bootstrap order `Store::build` follows.
        assert_ne!(no, crate::meta::META_PAGE, "page 0 is reserved for the superblock");
        let mut p = PageMut::init(w.bytes_mut(), PageKind::Leaf, tree_id, no);
        p.finalise(0);
        drop(w);
        Ok(BTree { pool, tree_id, root: no, last_leaf, fast_path_hits, fast_path_attempts, tags: None })
    }

    pub fn open(
        pool: &'p BufferPool,
        tree_id: u16,
        root: u32,
        last_leaf: &'p Cell<Option<u32>>,
        fast_path_hits: &'p Cell<u64>,
        fast_path_attempts: &'p Cell<u64>,
    ) -> Self {
        BTree { pool, tree_id, root, last_leaf, fast_path_hits, fast_path_attempts, tags: None }
    }

    /// Attach the owning handle's per-keyspace append hints. A handle must do
    /// this on EVERY tree it opens, not only the ones it means to speed up:
    /// the clearing a split or a delete performs happens through this borrow,
    /// so a mutation that arrives on an unattached `BTree` would leave a hint
    /// standing over a tree whose shape it no longer describes.
    pub fn with_tags(mut self, tags: &'p TagHints) -> Self { self.tags = Some(tags); self }

    pub fn root(&self) -> u32 { self.root }

    /// Drop every per-keyspace hint. Called wherever a separator can move or a
    /// page can be recycled.
    fn forget_tag_hints(&self) { if let Some(t) = self.tags { t.clear(); } }

    /// Re-arm the per-keyspace hint on the page a SPLIT left the record in.
    ///
    /// Without this a leaf fill costs the run two descents, not one: the probe
    /// is refused for want of room and disarms the slot, the descent splits,
    /// and then the NEXT key of that run finds no slot and descends again.
    /// Measured on the 200K relationship load, forward-edge rows: 6,383 of
    /// 99,840 puts found no slot and each paid about 7.4 page accesses --
    /// 1.774 accesses per put against 1.36 with this.
    ///
    /// Sound because a split's halves are SUBintervals of the interval the
    /// descent walked: the left page keeps everything below the new separator
    /// and the right page everything at or above it, so naming one of them is
    /// a narrowing of what was already exact.
    fn arm_after_split(&self, key: &[u8], fences: Option<LeafFences>, page: u32, next: u32,
        side: impl FnOnce(LeafFences) -> LeafFences) {
        if let (Some(tags), Some(f)) = (self.tags, fences) {
            tags.arm(self.tree_id, key, page, next, side(f));
        }
    }

    /// Choose the leaf immediately to the left of `min` (or the leftmost leaf
    /// when no predecessor exists), copy its encoded records, and allocate the
    /// optional right fragment.  Everything allocated here is unreachable;
    /// the standing tree is read only until `install_graft` runs after an
    /// independent verification.
    pub(crate) fn plan_graft(&self, min: &[u8]) -> Result<GraftBoundary> {
        self.plan_graft_inner(min, true)
    }

    /// Reconstruct only the standing-tree path needed to publish a candidate
    /// that was already packed before a crash. Its right fragment is already
    /// part of that verified candidate, so allocating it again would leak a
    /// fresh page on every resume-of-resume.
    pub(crate) fn plan_existing_graft(&self, min: &[u8]) -> Result<GraftBoundary> {
        self.plan_graft_inner(min, false)
    }

    fn plan_graft_inner(&self, min: &[u8], allocate_right: bool) -> Result<GraftBoundary> {
        // The boundary must be the leaf whose INTERVAL owns `min` — which is
        // exactly where `descend(min)` lands. An earlier draft descended to the
        // PREDECESSOR key's leaf instead; the two agree everywhere except when
        // an interior separator equals `min` — then the predecessor sits one
        // child to the LEFT, and installing there pushes the packed range past
        // that child's separator. A rebuild makes this real: deleting a grafted
        // range empties its leaf but deliberately keeps the leaf and its
        // separator, so the next graft of the same range anchored left of a
        // stale separator that still claimed the interval — the tree then held
        // keys its own root said could not be there, and every mid-range seek
        // (a spatial cover, a btree range) silently skipped them while full
        // scans still saw them. An index must change speed, never the answer.
        let (leaf, path) = self.descend_with_path(min)?;
        let (records, split, old_next) = {
            let r = self.pool.get(leaf)?;
            let page = open_cached(&r, leaf)?;
            if page.kind() != PageKind::Leaf || page.tree_id() != self.tree_id {
                return Err(Error::Corrupt { page_no: leaf, why: "graft boundary is not a tree leaf" });
            }
            let split = lower_bound(&page, min)?;
            let records = (0..page.nentries()).map(|i| page.slot(i).to_vec()).collect::<Vec<_>>();
            (records, split, page.next_leaf())
        };

        let right_page = if allocate_right && split < records.len() {
            let mut w = self.pool.allocate()?;
            let no = w.page_no();
            let page = build_page(
                PageKind::Leaf,
                self.tree_id,
                no,
                &records[split..],
                old_next,
            )?;
            w.bytes_mut().copy_from_slice(&page);
            Some(no)
        } else {
            None
        };

        Ok(GraftBoundary { leaf, path, records, split, old_next, right_page })
    }

    /// Assemble the verified unit that will replace the boundary leaf.  The
    /// packed pages themselves are not modified: `pack_range` received the
    /// right continuation up front, so every final page is written once.
    pub(crate) fn build_graft_candidate(
        &self,
        boundary: &GraftBoundary,
        packed: &crate::bulk::PackedRange,
    ) -> Result<GraftCandidate> {
        let packed_min = packed.min.as_deref().ok_or(Error::TooLarge)?;
        let packed_max = packed.max.as_deref().ok_or(Error::TooLarge)?;
        let left = &boundary.records[..boundary.split];
        let right = &boundary.records[boundary.split..];

        let left_page = if left.is_empty() {
            None
        } else {
            let mut w = self.pool.allocate()?;
            let no = w.page_no();
            let page = build_page(
                PageKind::Leaf,
                self.tree_id,
                no,
                left,
                packed.first_leaf,
            )?;
            w.bytes_mut().copy_from_slice(&page);
            Some(no)
        };

        let root = if left_page.is_none() && boundary.right_page.is_none() {
            packed.root
        } else {
            let child0 = left_page.unwrap_or(packed.root);
            let mut entries = Vec::with_capacity(2);
            if left_page.is_some() {
                entries.push(enc_interior(packed_min, packed.root));
            }
            if let Some(right_page) = boundary.right_page {
                let right_min = validated_key(&right[0]);
                entries.push(enc_interior(right_min, right_page));
            }
            let mut w = self.pool.allocate()?;
            let no = w.page_no();
            let page = build_page(PageKind::Interior, self.tree_id, no, &entries, child0)?;
            w.bytes_mut().copy_from_slice(&page);
            no
        };

        let min = left.first()
            .map(|record| validated_key(record).to_vec())
            .unwrap_or_else(|| packed_min.to_vec());
        let max = right.last()
            .map(|record| validated_key(record).to_vec())
            .unwrap_or_else(|| packed_max.to_vec());
        let rows = packed.rows
            .checked_add(boundary.records.len() as u64)
            .ok_or(Error::TooLarge)?;

        Ok(GraftCandidate {
            root,
            rows,
            min,
            max,
            last_next: boundary.old_next,
        })
    }

    /// Copy the boundary's parent path bottom-up and patch one child pointer
    /// in each fresh page. The only logical mutation is replacing the boundary
    /// child with the already-verified candidate; no published page is edited.
    pub(crate) fn install_graft(
        &mut self,
        boundary: &GraftBoundary,
        candidate_root: u32,
        candidate_min: &[u8],
    ) -> Result<Vec<u32>> {
        let mut replacement = candidate_root;
        let mut expected_child = boundary.leaf;
        let mut retired = vec![boundary.leaf];
        // With no left record retained, this subtree's minimum becomes the
        // packed range's minimum. Carry that change through child0 edges; the
        // first non-child0 edge owns the separator that names it.
        let mut propagate_min = boundary.split == 0;

        for &(parent, child_index) in boundary.path.iter().rev() {
            let (child0, mut records) = {
                let r = self.pool.get(parent)?;
                let page = open_cached(&r, parent)?;
                if page.kind() != PageKind::Interior || page.tree_id() != self.tree_id {
                    return Err(Error::Corrupt { page_no: parent, why: "graft path reaches a non-interior page" });
                }
                if child_at(self.pool, &page, child_index)? != expected_child {
                    return Err(Error::Corrupt { page_no: parent, why: "graft path child changed before publication" });
                }
                (page.child0(), (0..page.nentries())
                    .map(|i| page.slot(i).to_vec()).collect::<Vec<_>>())
            };

            let mut w = self.pool.allocate()?;
            let no = w.page_no();
            let next_child0 = if child_index == 0 {
                replacement
            } else {
                let slot = child_index - 1;
                let key = if propagate_min {
                    candidate_min
                } else {
                    validated_key(&records[slot])
                };
                records[slot] = enc_interior(key, replacement);
                child0
            };
            let page = build_page(PageKind::Interior, self.tree_id, no, &records, next_child0)?;
            w.bytes_mut().copy_from_slice(&page);
            if child_index != 0 { propagate_min = false; }
            replacement = no;
            expected_child = parent;
            retired.push(parent);
        }
        self.root = replacement;
        Ok(retired)
    }

    /// Pack a sorted run into full pages and splice it into an EMPTY key
    /// interval of this tree, instead of inserting its keys one at a time.
    ///
    /// This is the shape a `CREATE INDEX` has: every key of a new index shares
    /// one contiguous keyspace (`[tag][index id]...`) that holds nothing yet,
    /// so the run has no existing neighbours to interleave with. SQLite builds
    /// the same run into a separate, empty index B-tree with a sorter and
    /// `OP_IdxInsert`; E4 has one tree per database (D1), so the equivalent is
    /// to pack the pages and graft them in.
    ///
    /// The four steps:
    ///
    /// 1. **Refuse a non-empty interval.** One seek to `min` and one key
    ///    comparison. A key in `[min, max]` means the caller's assumption is
    ///    wrong, and the graft is refused with `RangeNotEmpty` before anything
    ///    is allocated or written.
    /// 2. **Plan the boundary.** `plan_graft` descends to the leaf whose
    ///    interval owns `min` and splits it at the insertion point: records
    ///    below `min` become the copied left fragment, records above it the
    ///    copied right fragment. Both are fresh pages; the standing leaf is
    ///    read only.
    /// 3. **Pack.** `pack_range_pooled` fills leaves to 90% and builds the
    ///    run's OWN interior levels bottom-up, allocating every page through
    ///    the ordinary buffer pool. The right continuation is known before the
    ///    last leaf is written, so the leaf chain is correct on the first and
    ///    only write of each page.
    /// 4. **Verify, then splice.** The packed subtree is walked
    ///    (`verify_range_pool`) before any standing page is touched. Then one
    ///    two-or-three-child wrapper joins {left fragment, packed root, right
    ///    fragment} and `install_graft` copies the O(height) parent path,
    ///    replacing exactly one child pointer. The retired page numbers are
    ///    returned for the caller to free.
    ///
    /// **Interior strategy.** The run brings its own interior levels and
    /// enters the standing tree as ONE separator. Inserting one separator per
    /// packed leaf was the alternative, and it is O(leaves) interior inserts
    /// with their own splits -- the cost this call exists to remove -- so the
    /// subtree is grafted whole. The cost is that the grafted subtree's height
    /// is independent of the standing tree's, so the tree is no longer
    /// uniformly deep; `descend_with_path` already reserves for that.
    ///
    /// **Page LAYOUT is not preserved, the ENTRY SET is.** Which key sits on
    /// which page differs from the one-at-a-time insert path (packed leaves
    /// are 90% full; split-built leaves are not), and so do page numbers and
    /// tree height. Every persisted key and value is identical, which is what
    /// `tests/index_build_equivalence.rs` digests.
    ///
    /// Nothing here bypasses the log or swaps a root: every page is an
    /// ordinary pooled page, so a page-WAL store logs each one as a normal
    /// frame and publishes the whole graft with the caller's commit. A crash
    /// before that commit leaves the standing tree exactly as it was.
    pub fn graft_sorted_range<I>(
        &mut self,
        sorted: I,
        expected_rows: u64,
        min: &[u8],
        max: &[u8],
        scratch_dir: &std::path::Path,
    ) -> Result<Vec<u32>>
    where I: Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>> {
        if expected_rows == 0 { return Ok(Vec::new()); }
        if min > max { return Err(Error::TooLarge); }
        // The bounded probe. `range` seeks once; the first key it returns is
        // the smallest at or above `min`, so one comparison settles the whole
        // interval.
        if let Some(row) = self.range(min)?.next() {
            let (key, _) = row?;
            if key.as_slice() <= max { return Err(Error::RangeNotEmpty); }
        }
        let boundary = self.plan_graft(min)?;
        let last_next = boundary.right_page.unwrap_or(boundary.old_next);
        // Fill factor 1.0, not the whole-tree load's 0.9.
        //
        // The path this replaces is the per-keyspace APPEND split (D9), which
        // leaves each completed leaf of an ascending run essentially full. A
        // 90% pack would therefore cost about one leaf in ten MORE than
        // inserting the same run key by key, and in a page-WAL store every
        // extra page is another logged frame and another page folded at the
        // next checkpoint -- measured as a 26% frame increase over the insert
        // path at 0.9, which moved a whole checkpoint into the next build
        // stage. SQLite's `CREATE INDEX` fills its index pages the same way.
        //
        // SACRIFICE (Law 4): a packed leaf has no slack, so the first live
        // write that lands inside one splits it. That is the ordinary split
        // path, once per leaf, and the run was just built from data that was
        // already there; the alternative was paying the extra page for every
        // leaf up front, whether or not anything ever writes to it.
        let packed = crate::bulk::pack_range_pooled(
            self.pool, self.tree_id, sorted, 1.0, scratch_dir, last_next)?;
        // The stream is the caller's; `pack_range_pooled` recomputes all three
        // of these from the bytes it actually packed, and a disagreement means
        // the interval that was proved empty is not the interval that was
        // packed.
        if packed.rows != expected_rows
            || packed.min.as_deref() != Some(min)
            || packed.max.as_deref() != Some(max) {
            return Err(Error::Corrupt { page_no: packed.root,
                why: "packed range disagrees with its sorted-stream manifest" });
        }
        crate::verify::verify_range_pool(self.pool, packed.root, self.tree_id,
            packed.rows, min, max, last_next)?;
        let candidate = self.build_graft_candidate(&boundary, &packed)?;
        let candidate_min = candidate.min.clone();
        let retired = self.install_graft(&boundary, candidate.root, &candidate_min)?;
        // The append hint names a leaf this graft may have just retired, and a
        // retired page number can be handed straight back out by the allocator.
        // `fast_path_leaf`'s checks are about shape, not identity, so a
        // recycled leaf of the SAME tree could pass all five. The hint has no
        // reason to survive a graft; drop it rather than rely on those checks.
        self.last_leaf.set(None);
        self.forget_tag_hints();
        Ok(retired)
    }

    /// Descend to the leaf that would hold `key`, recording the path.
    fn descend(&self, key: &[u8]) -> Result<(u32, Vec<u32>)> {
        let mut path = Vec::new();
        let mut cur = self.root;
        loop {
            let r = self.pool.get(cur)?;
            let p = open_cached(&r, cur)?;
            // PageRef::open proves the page is intact and is the page we asked
            // for. It cannot know which TREE we meant, and five trees share this
            // file, so a stale root would descend into another tree's pages and
            // answer confidently from them.
            if p.tree_id() != self.tree_id {
                return Err(Error::Corrupt { page_no: cur, why: "page belongs to another tree" });
            }
            if p.kind() == PageKind::Leaf { return Ok((cur, path)); }
            // INTERIOR CONVENTION. Entry i is (min_key_i, child_i), and child_i
            // holds keys in [min_key_i, min_key_{i+1}). The LEFTMOST child has
            // no minimum and lives in the header as child0, so the slot array
            // stays strictly sorted and binary search over it is valid.
            //
            // A sentinel entry with an empty key placed LAST would NOT be sorted
            // — the empty string compares smallest — so binary search would
            // silently return the wrong child for any key below the first
            // separator. That is why child0 is a header field, not a sentinel.
            let i = upper_bound(&p, key)?;
            let child = child_at(self.pool, &p, i)?;
            path.push(cur);
            cur = child;
        }
    }

    /// Descend to the leaf for `key` and, while that leaf is still pinned,
    /// answer where in it the scan starts.
    ///
    /// `range` used to take the path from `descend_with_path` and then pin the
    /// SAME leaf a second time purely to run `lower_bound` on it. Every scan in
    /// the engine paid that second `pool.get` -- a borrow of the pool's table
    /// and a hash lookup -- for a page the descent had open one line earlier.
    fn descend_positioned(&self, key: &[u8]) -> Result<(u32, usize, Vec<(u32, usize)>)> {
        let mut cur = self.root;
        let mut path = Vec::with_capacity(8);
        loop {
            let r = self.pool.get(cur)?;
            let p = open_cached(&r, cur)?;
            if p.tree_id() != self.tree_id {
                return Err(Error::Corrupt { page_no: cur, why: "page belongs to another tree" });
            }
            if p.kind() == PageKind::Leaf { return Ok((cur, lower_bound(&p, key)?, path)); }
            let i = upper_bound(&p, key)?;
            let child = child_at(self.pool, &p, i)?;
            path.push((cur, i));
            cur = child;
        }
    }

    /// Descend to the leaf for `key`, recording the path as (interior page,
    /// chosen child index) pairs for the scan cursor's parent-walk advance.
    fn descend_with_path(&self, key: &[u8]) -> Result<(u32, Vec<(u32, usize)>)> {
        let mut cur = self.root;
        // Grafted subtrees can add a shallow wrapper level. Reserve the
        // ordinary maximum up front so a bounded query does not acquire one
        // extra heap allocation merely because an index was bulk-built.
        let mut path = Vec::with_capacity(8);
        loop {
            let r = self.pool.get(cur)?;
            let p = open_cached(&r, cur)?;
            if p.tree_id() != self.tree_id {
                return Err(Error::Corrupt { page_no: cur, why: "page belongs to another tree" });
            }
            if p.kind() == PageKind::Leaf { return Ok((cur, path)); }
            let i = upper_bound(&p, key)?;
            let child = child_at(self.pool, &p, i)?;
            path.push((cur, i));
            cur = child;
        }
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let (leaf, _) = self.descend(key)?;
        let r = self.pool.get(leaf)?;
        let p = open_cached(&r, leaf)?;
        let i = lower_bound(&p, key)?;
        if i < p.nentries() {
            let rec = p.slot(i);
            let (stored_key, value, overflow) = validated_leaf(rec);
            if stored_key != key { return Ok(None); }
            if overflow {
                let marker = value.to_vec();
                drop(r); // release the leaf before walking the chain
                return Ok(Some(read_overflow(self.pool, &marker)?));
            }
            return Ok(Some(value.to_vec()));
        }
        Ok(None)
    }

    /// Walk root -> leaf taking READ guards on interior pages, dropping each
    /// as we step down (interiors are not modified by an insert), and end
    /// holding a single WRITE guard on the leaf. That one guard is carried
    /// through `insert_into_leaf` and, if needed, `split_leaf_and_insert`,
    /// so a logical insert acquires the leaf exactly once instead of the
    /// three separate acquisitions the old `descend` + re-`get`/`get_mut`
    /// shape required — each of which made the page evictable in between.
    fn descend_for_write(&mut self, key: &[u8]) -> Result<(PinnedWrite<'p>, Vec<u32>)> {
        let (w, path, _) = self.descend_for_write_fenced(key)?;
        Ok((w, path))
    }

    /// The same descent, also reporting the leaf's FENCES: the separators
    /// immediately below and above the leaf it lands on.
    ///
    /// They are free here. `upper_bound` already chose a child index at every
    /// interior page; the separator at that index is the first key of the
    /// subtree to the right, and the one before it is the first key of this
    /// subtree. The innermost level where the chosen child was not the last
    /// gives the tightest upper fence; where it WAS the last, the fence is
    /// inherited from further up, and a path that was last-child the whole way
    /// down ends at the tree's rightmost leaf, which has no upper fence at all.
    ///
    /// This is exactly the interval `upper_bound` will route to this leaf, so a
    /// key inside it needs no descent to find its page -- which is what
    /// `TagHints` remembers.
    fn descend_for_write_fenced(&mut self, key: &[u8])
        -> Result<(PinnedWrite<'p>, Vec<u32>, LeafFences)> {
        // 2f: this is THE shadowed descent. Every page on the write path is
        // relocated to a fresh number if it belongs to the published epoch,
        // top-down, so by the time any mutation below runs -- the leaf edit,
        // and every `get_mut(parent)` a split performs on the `path` pages --
        // it lands on this epoch's copies only. The published root and its
        // pages stay byte-identical for snapshot readers.
        if self.pool.is_frozen(self.root) {
            self.root = shadow_page(self.pool, self.root)?;
        }
        let mut path = Vec::new();
        let mut cur = self.root;
        let mut fences = LeafFences::NONE;
        loop {
            let (is_leaf, frozen_child) = {
                let r = self.pool.get(cur)?;
                let p = open_cached(&r, cur)?;
                if p.tree_id() != self.tree_id {
                    return Err(Error::Corrupt { page_no: cur, why: "page belongs to another tree" });
                }
                if p.kind() == PageKind::Leaf {
                    (true, None)
                } else {
                    let i = upper_bound(&p, key)?;
                    // Narrow the fences by this level's separators before
                    // stepping down. Child `i` holds the keys between slot
                    // `i - 1` (inclusive) and slot `i` (exclusive).
                    if i > 0 { fences.narrow_lower(validated_key(p.slot(i - 1))); }
                    if i < p.nentries() { fences.narrow_upper(validated_key(p.slot(i))); }
                    let child = child_at(self.pool, &p, i)?;
                    if self.pool.is_frozen(child) {
                        (false, Some((i, child)))
                    } else {
                        path.push(cur);
                        cur = child;
                        (false, None)
                    }
                }
            }; // `r` dropped here, before either looping again or taking get_mut
            if let Some((i, child)) = frozen_child {
                let fresh = shadow_page(self.pool, child)?;
                patch_child(self.pool, cur, i, fresh)?;
                path.push(cur);
                cur = fresh;
                continue;
            }
            if is_leaf {
                let w = self.pool.get_mut(cur)?;
                return Ok((w, path, fences));
            }
        }
    }

    /// Try to append `rec` to a leaf a `TagHints` slot named, skipping the
    /// descent. The caller has already established, IN MEMORY, that `key` sits
    /// inside the fences recorded when the slot was armed; this establishes the
    /// rest against the page itself.
    ///
    /// THE FENCE RULE, in full. `key` may go into this leaf without a descent
    /// when all of these hold:
    ///   * ROUTING, established by the caller in memory from the cached
    ///     fences: lower fence <= `key` < upper fence, the lower inclusive and
    ///     the upper STRICTLY exclusive. Strictly, because `upper_bound`
    ///     routes a key equal to a separator to the child on its RIGHT: a
    ///     record placed here instead stays visible to a scan and disappears
    ///     from every `get`. An absent upper fence means the tree's rightmost
    ///     leaf, which has no separator above it.
    ///   * IDENTITY: a live (unfrozen) leaf page of THIS tree whose
    ///     `next_leaf` is still the one the hint recorded. The link is what
    ///     makes a split of this very leaf -- the one event that subdivides
    ///     the cached interval -- visible without re-descending, and it is
    ///     also the evidence that the page number was not recycled underneath
    ///     the hint.
    ///   * ROOM for the record and its slot, and the leaf is not empty: an
    ///     empty leaf is no evidence a key belongs to it, the trap
    ///     `fast_path_leaf`'s check 5 documents.
    ///   * `key` IS NOT ALREADY THERE. A replacement has to retire the old
    ///     record's overflow pages and reclaim its payload bytes, which is
    ///     `insert_into_leaf`'s work, not this one's.
    ///
    /// AND NOTHING ABOUT POSITION. The record goes in at `lower_bound`'s slot,
    /// wherever that is, because the fences alone already say the key belongs
    /// to this leaf and nowhere else. Two earlier versions of this check asked
    /// for more and both were wrong to:
    ///   * "after the leaf's LAST record" never fires on a boundary leaf. One
    ///     leaf per keyspace boundary holds the end of one tag and the start
    ///     of the next, and for the tag below, that leaf is exactly where its
    ///     run ends -- its appends land BEFORE the higher tag's records. The
    ///     tag that most needs this hint (0x71 forward edges, with 0x72
    ///     reverse edges above them) armed nothing at all: measured 0 hits in
    ///     200,000 edge puts, 4.40 page accesses each.
    ///   * "after the last record OF ITS OWN TAG" fires on a boundary leaf but
    ///     not on a near-ascending run. The reverse-edge rows go to
    ///     destinations (i+1) and (i+7): the second is the tag's new maximum,
    ///     the first is six keys below it, so two puts in three landed before
    ///     a record of their own tag and refused a leaf they belonged in.
    ///     Measured: 4.357 page accesses per reverse row, unchanged.
    ///
    /// Returns the slot to insert at.
    fn fast_path_tag_leaf(&self, hint: u32, next: u32, key: &[u8], rec: &[u8])
        -> Result<Option<(PinnedWrite<'p>, usize)>> {
        if self.pool.is_frozen(hint) { return Ok(None); }
        let w = self.pool.get_mut(hint)?;
        let at = {
            let p = PageRef::open_resident(w.bytes(), hint)?;
            validate_records(&p)?;
            if p.kind() != PageKind::Leaf
                || p.tree_id() != self.tree_id
                || p.next_leaf() != next
                || p.free_space() < rec.len() + 4
                || p.nentries() == 0
            {
                None
            } else {
                // `lower_bound` puts `at` at the first record >= `key`, so
                // inserting there keeps the leaf sorted, and everything before
                // it is already strictly below.
                let at = lower_bound(&p, key)?;
                let fresh = at == p.nentries() || validated_key(p.slot(at)) != key;
                fresh.then_some(at)
            }
        };
        Ok(at.map(|at| (w, at)))
    }

    /// Try to insert directly into the cached last-written leaf, skipping
    /// the descent entirely. Every one of these five checks is load-bearing
    /// (Task 15 brief): a wrong hint may cost a fallback descent, but must
    /// never place a record wrongly or lose one (Law 3). On any failure the
    /// write guard, if taken, is dropped here and `None` is returned so the
    /// caller falls back to `descend_for_write`.
    ///
    /// SACRIFICE (Law 4): the hint can be stale (rightmost leaf changed,
    /// wrong tree, no room, key not actually the new maximum). Bought: an
    /// ascending-key workload — the common case for autoincrementing IDs and
    /// time-ordered writes — inserts with zero interior-page traffic instead
    /// of a full root-to-leaf walk per row.
    fn fast_path_leaf(&self, hint: u32, key: &[u8], rec: &[u8]) -> Result<Option<PinnedWrite<'p>>> {
        // check 0 (2f): a frozen hint belongs to the published epoch; fall
        // back to the shadowed descent, which relocates it properly.
        if self.pool.is_frozen(hint) { return Ok(None); }
        let w = self.pool.get_mut(hint)?;
        let fits = {
            let p = match PageRef::open_resident(w.bytes(), hint) {
                Ok(p) => p,
                Err(e) => return Err(e),
            };
            validate_records(&p)?;
            p.kind() == PageKind::Leaf                 // check 1
                && p.tree_id() == self.tree_id          // check 2
                && p.next_leaf() == 0                   // check 3: rightmost
                && p.free_space() >= rec.len() + 4       // check 4: room
                // check 5: strictly greater. An EMPTY leaf must NOT pass:
                // emptiness is not evidence the key belongs here. Delete every
                // key and the rightmost leaf is empty while separators still
                // route low keys left of it -- keys appended here become
                // reachable by scan but not by get (measured: 27/20,000 lost).
                && p.nentries() > 0
                && key > validated_key(p.slot(p.nentries() - 1))
        };
        Ok(if fits { Some(w) } else { None })
    }

    /// Insert `rec` at the end of an already-validated rightmost leaf. Every
    /// `fast_path_leaf` check that ran before this was called guarantees
    /// `insert_slot` fits, so there is no split path here.
    fn insert_fast(&mut self, mut w: PinnedWrite<'p>, rec: Vec<u8>) -> Result<()> {
        #[cfg(feature = "write-trace")]
        let trace_started = crate::write_trace::active().then(std::time::Instant::now);
        let leaf = w.page_no();
        let mut p = PageMut::reopen(w.bytes_mut());
        let at = p.nentries_pub();
        p.insert_slot(at, &rec)?;
        #[cfg(feature = "write-trace")]
        if trace_started.is_some() { crate::write_trace::value_copy(); }
        p.finalise(0);
        drop(p);
        self.last_leaf.set(Some(leaf));
        #[cfg(feature = "write-trace")]
        if let Some(started) = trace_started {
            crate::write_trace::add(crate::write_trace::Field::LeafInsert, started.elapsed());
        }
        Ok(())
    }

    pub fn insert(&mut self, key: &[u8], val: &[u8]) -> Result<()> {
        // The one choke point: put(), WAL replay and vacuum all insert here,
        // so spilling here means none of them can disagree about when a value
        // overflows. The KEY always stays in the leaf.
        #[cfg(feature = "write-trace")]
        let encode_started = crate::write_trace::active().then(std::time::Instant::now);
        let rec = if 4 + key.len() + val.len() > crate::page::MAX_RECORD_LEN {
            let (head, crc) = write_overflow(self.pool, val)?;
            enc_leaf_marker(key, &enc_marker(val.len() as u32, head, crc))
        } else {
            enc_leaf(key, val, self.pool.compact_cells())
        };
        #[cfg(feature = "write-trace")]
        if let Some(started) = encode_started {
            crate::write_trace::add(crate::write_trace::Field::RecordEncode, started.elapsed());
            if 4 + key.len() + val.len() <= crate::page::MAX_RECORD_LEN {
                crate::write_trace::value_copy();
            }
        }
        if rec.len() > crate::page::MAX_RECORD_LEN { return Err(Error::TooLarge); }

        // Append fast path: try the last leaf we wrote before paying for a
        // root-to-leaf descent (PostgreSQL's `RelationGetTargetBlock`).
        #[cfg(feature = "write-trace")]
        let descent_started = crate::write_trace::active().then(std::time::Instant::now);
        // The per-keyspace hint goes first, and when it claims this key the
        // whole-tree hint is not probed at all. Probing both would be worse
        // than probing neither: `last_leaf` names the rightmost leaf of the
        // TREE, so every insert under a lower tag would fail it, disarm it,
        // and force the next insert under the top tag to re-descend -- two
        // interleaved ascending runs would knock each other's hint out
        // forever. Whichever hint owns the key owns the probe.
        if let Some(tags) = self.tags {
            let claimed = tags.find(self.tree_id, key);
            if claimed.is_none() { tags.misses.set(tags.misses.get() + 1); }
            if let Some((hint, next)) = claimed {
                tags.attempts.set(tags.attempts.get() + 1);
                if let Some((w, at)) = self.fast_path_tag_leaf(hint, next, key, &rec)? {
                    tags.hits.set(tags.hits.get() + 1);
                    #[cfg(feature = "write-trace")]
                    if let Some(started) = descent_started {
                        crate::write_trace::add(crate::write_trace::Field::Descent, started.elapsed());
                    }
                    return self.insert_tag_fast(w, at, rec);
                }
                // Only THIS slot was wrong (the leaf filled, or a key arrived
                // out of order inside its own run). The other fifteen describe
                // other runs and stay; the descent below re-arms this one.
                tags.forget_leaf(self.tree_id, hint);
                let (w, path, fence) = self.descend_for_write_fenced(key)?;
                #[cfg(feature = "write-trace")]
                if let Some(started) = descent_started {
                    crate::write_trace::add(crate::write_trace::Field::Descent, started.elapsed());
                }
                return self.insert_into_leaf(w, path, key, rec, Some(fence));
            }
        }
        if let Some(hint) = self.last_leaf.get() {
            self.fast_path_attempts.set(self.fast_path_attempts.get() + 1);
            if let Some(w) = self.fast_path_leaf(hint, key, &rec)? {
                self.fast_path_hits.set(self.fast_path_hits.get() + 1);
                #[cfg(feature = "write-trace")]
                if let Some(started) = descent_started {
                    crate::write_trace::add(crate::write_trace::Field::Descent, started.elapsed());
                }
                return self.insert_fast(w, rec);
            }
            // PostgreSQL's `_bt_search_insert`, on ANY fast-path rejection,
            // calls `RelationSetTargetBlock(rel, InvalidBlockNumber)` --
            // forgets the hint rather than retrying it next time. Copy that:
            // one failure disarms it, so a random-order workload pays for
            // exactly one wasted probe (this one) and then nothing more,
            // instead of paying a `get_mut` plus a full CRC verify on every
            // single insert forever. It is re-armed below, in
            // `insert_into_leaf`, only when an insert actually lands on the
            // rightmost leaf.
            //
            // SACRIFICE (Law 4): a workload that keeps switching between
            // append order and random order pays one wasted probe at every
            // switch -- the disarm is not free, it is just no longer
            // permanent.
            self.last_leaf.set(None);
        }

        let (w, path, fence) = self.descend_for_write_fenced(key)?;
        #[cfg(feature = "write-trace")]
        if let Some(started) = descent_started {
            crate::write_trace::add(crate::write_trace::Field::Descent, started.elapsed());
        }
        self.insert_into_leaf(w, path, key, rec, Some(fence))
    }

    /// Place a record in a leaf a tag hint named, at the slot the probe
    /// found. The slot already describes this leaf, so nothing is re-armed;
    /// unlike `insert_fast` this must NOT touch `last_leaf`, because the leaf
    /// is rightmost of its own tag, not of the tree.
    fn insert_tag_fast(&mut self, mut w: PinnedWrite<'p>, at: usize, rec: Vec<u8>) -> Result<()> {
        #[cfg(feature = "write-trace")]
        let trace_started = crate::write_trace::active().then(std::time::Instant::now);
        let mut p = PageMut::reopen(w.bytes_mut());
        p.insert_slot(at, &rec)?;
        #[cfg(feature = "write-trace")]
        if trace_started.is_some() { crate::write_trace::value_copy(); }
        p.finalise(0);
        #[cfg(feature = "write-trace")]
        if let Some(started) = trace_started {
            crate::write_trace::add(crate::write_trace::Field::LeafInsert, started.elapsed());
        }
        Ok(())
    }

    fn insert_into_leaf(&mut self, mut w: PinnedWrite<'p>, path: Vec<u32>, key: &[u8], rec: Vec<u8>,
        fences: Option<LeafFences>) -> Result<()> {
        let leaf = w.page_no();
        #[cfg(feature = "write-trace")]
        let search_started = crate::write_trace::active().then(std::time::Instant::now);
        let (i, exists, is_rightmost, is_next, retired) = {
            // Not "just written": this guard came straight from
            // `descend_for_write`'s `get_mut`, which never verifies a CRC.
            // This is the one read that must.
            let p = PageRef::open_resident(w.bytes(), leaf)?;
            validate_records(&p)?;
            let i = lower_bound(&p, key)?;
            let exists = i < p.nentries()
                && validated_key(p.slot(i)) == key;
            // `descend_for_write` only ever hands back a write guard for a
            // page it already confirmed was a leaf (the interior branch
            // never breaks out of its loop), so `next_leaf == 0` here means
            // "rightmost leaf", not "leftmost child of an interior page" --
            // the ambiguity that field carries on a page whose kind hasn't
            // been checked yet. Read once, here, before anything below
            // mutates the page: neither `remove_slot`/`compact` nor
            // `insert_slot` ever touch this field, so it stays valid for the
            // whole function.
            let is_next = p.next_leaf();
            let is_rightmost = is_next == 0;

            let retired = if exists { replaced_overflow_pages(self.pool, p.slot(i))? } else { Vec::new() };
            (i, exists, is_rightmost, is_next, retired)
        };
        #[cfg(feature = "write-trace")]
        if let Some(started) = search_started {
            crate::write_trace::add(crate::write_trace::Field::LeafSearch, started.elapsed());
        }

        // Closed operation 1: if the key already exists, remove and compact
        // as one step, finalising before we ever ask whether the NEW record
        // fits. `compact` reclaims the old entry's payload bytes, not just
        // its slot; without this as a separate finalised step, an insert
        // that then failed to fit would leave the page dirty with a
        // checksum matching neither the old contents (already mutated) nor
        // the new (never written) — and the next `PageRef::open` on this
        // leaf, for ANY key, refuses the whole page.
        if exists {
            let mut p = PageMut::reopen(w.bytes_mut());
            p.remove_slot(i);
            p.compact();
            p.finalise(0);
            drop(p);
            for (no, birth) in retired { self.pool.free_shadow_page(no, birth)?; }
        }

        // Closed operation 2: decide room from the page's REAL free space —
        // not a sum over live entry lengths, which diverges from the truth
        // the moment anything has ever been deleted from this page — then
        // insert as its own finalised step. `i` is still the correct
        // insertion point: if the key existed, entry `i` is exactly what we
        // just removed, so nothing before or after it moved. No fresh
        // `PageRef::open` here: we either just finalised this page
        // ourselves (the `exists` branch) or verified it above and have not
        // touched it since — both leave the CRC known-good without paying
        // for another ~254ns verification pass.
        // remove_slot frees the slot entry, not the payload bytes; only
        // compact() recovers them. An emptied leaf can report nentries()==0 AND
        // free_space()==0 -- empty and full at once -- sending a single record
        // to the split path, which cannot halve one record (TooLarge on an
        // empty page). Compact and re-check before concluding there is no room.
        let mut room = PageMut::reopen(w.bytes_mut()).free_space() >= rec.len() + 4;
        if !room {
            let mut p = PageMut::reopen(w.bytes_mut());
            p.compact();
            p.finalise(0);
            drop(p);
            room = PageMut::reopen(w.bytes_mut()).free_space() >= rec.len() + 4;
        }
        if room {
            #[cfg(feature = "write-trace")]
            let insert_started = crate::write_trace::active().then(std::time::Instant::now);
            let mut p = PageMut::reopen(w.bytes_mut());
            p.insert_slot(i, &rec)?;
            #[cfg(feature = "write-trace")]
            if insert_started.is_some() { crate::write_trace::value_copy(); }
            p.finalise(0);
            // Arm only when this write actually landed on the rightmost
            // leaf. A descent can insert into ANY leaf -- most of them not
            // rightmost -- and re-arming unconditionally (the pre-Task-18
            // bug) is what let a random-order workload thrash forever: every
            // wrong hint just gets replaced with another wrong hint instead
            // of staying disarmed.
            if is_rightmost {
                self.last_leaf.set(Some(leaf));
            }
            // Arm the per-keyspace hint whether or not this leaf is the
            // tree's rightmost -- that is the whole point -- and wherever in
            // the leaf the record landed. The hint says "this tag's writes
            // are currently in this leaf, between these two separators",
            // which is true of any insert that did not split; the next key of
            // the tag is probed against the FENCES, so a hint armed from a
            // mid-leaf insert is not a worse guess than one armed from an
            // append. It is only not armed when the insert split, because a
            // split moves the separator this leaf sits under.
            if let (Some(tags), Some(fences)) = (self.tags, fences) {
                tags.arm(self.tree_id, key, leaf, is_next, fences);
            }
            #[cfg(feature = "write-trace")]
            if let Some(started) = insert_started {
                crate::write_trace::add(crate::write_trace::Field::LeafInsert, started.elapsed());
            }
            return Ok(());
        }
        #[cfg(feature = "write-trace")]
        let split_started = crate::write_trace::active().then(std::time::Instant::now);
        let result = self.split_leaf_and_insert(w, path, i, rec, key, fences);
        #[cfg(feature = "write-trace")]
        if let Some(started) = split_started {
            crate::write_trace::add(crate::write_trace::Field::Split, started.elapsed());
            crate::write_trace::value_copy();
        }
        result
    }

    /// Split dispatcher. Both bodies take the same decisions and write the
    /// same bytes; they differ only in how the records are MOVED (`_vec`
    /// copies every record into its own `Vec<u8>`, `_ref` points at them in
    /// place). Both are compiled under `cfg(test)` so the byte-equivalence
    /// oracle can run one against the other in a single build.
    fn split_leaf_and_insert(&mut self, w: PinnedWrite<'p>, path: Vec<u32>, at: usize, rec: Vec<u8>,
        key: &[u8], fences: Option<LeafFences>) -> Result<()> {
        // ONLY this leaf's fences. A split subdivides the interval of the leaf
        // it splits and inserts the new separator inside it; every other
        // leaf's interval is untouched, and a root split merely adds a level
        // above separators that do not move. Clearing the whole cache here
        // instead was measured at 1.410 page accesses per forward-edge insert
        // against 1.037 for this: a 0x40 leaf fills every fifteen documents,
        // and each of those splits was knocking out the edge run's hint.
        // (The neighbour redistribution inside DOES move other leaves'
        // separators, and clears their slots itself.)
        if let Some(t) = self.tags { t.forget_leaf(self.tree_id, w.page_no()); }
        #[cfg(not(feature = "slotref-split"))]
        { self.split_leaf_and_insert_vec(w, path, at, rec, key, fences) }
        #[cfg(feature = "slotref-split")]
        { self.split_leaf_and_insert_ref(w, path, at, rec, key, fences) }
    }

    #[cfg(any(test, not(feature = "slotref-split")))]
    #[allow(clippy::too_many_arguments)]
    fn split_leaf_and_insert_vec(&mut self, mut w: PinnedWrite<'p>, mut path: Vec<u32>, at: usize,
        rec: Vec<u8>, key: &[u8], fences: Option<LeafFences>) -> Result<()> {
        #[cfg(feature = "write-trace")]
        if crate::write_trace::active() { crate::write_trace::leaf_split(); }
        let leaf = w.page_no();
        // Collect, insert into the collected list, then redistribute. Bounded by
        // one page, so this is RAM proportional to the change, not the store.
        // One `PageRef::open` reads both the entries and `next_leaf` -- the old
        // shape spent a second full acquisition-and-verify on the same page just
        // to learn one more field of the header it had already opened.
        let (mut recs, old_next): (Vec<Vec<u8>>, u32) = {
            let p = PageRef::open_resident(w.bytes(), leaf)?;
            validate_records(&p)?;
            ((0..p.nentries()).map(|j| p.slot(j).to_vec()).collect(), p.next_leaf())
        };
        recs.insert(at, rec);
        // Split AT the insertion point when the left page ends up well filled.
        //
        // A 50/50 split of an ascending run abandons every left half at 50%
        // forever -- measured 0.458 utilisation, the file ~2x its needed size.
        // Splitting at the insertion point (SQLite balance_quick, InnoDB
        // sequential-insert) closes each left page full. "Append = last in
        // leaf" is too narrow when several keyspaces share the tree: the top
        // leaf of one space also holds the next space's keys, so ascending
        // inserts land BEFORE them, never last.
        //
        // The >= 2/3 guard is what separates workloads without detecting them:
        // ascending leaves the left ~full, scattered ~half -- and unguarded,
        // scattered amplification measurably worsened (2955.8 -> 3511.0
        // bytes/row), caught by a pinned test, so scattered keeps the
        // balanced split.
        let usable = PAGE_SIZE - crate::page::HEADER_LEN;
        let bytes = |r: &[Vec<u8>]| r.iter().map(|x| x.len() + 4).sum::<usize>();
        let left_bytes = bytes(&recs[..at.min(recs.len())]);
        // The unambiguous append signal: the new record is LAST in the leaf and
        // the leaf is RIGHTMOST (old_next == 0). Local byte-guards could not
        // separate the workloads -- left>=2/3 admitted scattered splits (file
        // 156 -> 182 MiB), and a small-right-count guard still left it at 174
        // (a random key is a new leaf-local max in ~1/nentries of splits, each
        // stranding a nearly-empty right page). Rightmost-append never fires
        // mid-tree under random keys, and is exactly SQLite's balance_quick
        // condition.
        //
        // `keyspace-append` widens "rightmost" from the TREE to the KEY TAG.
        // One collection is three ascending runs in one tree (D3/D4: vector
        // 0x60, row 0x40, external-key mapping 0x20), written one document at a
        // time, so only the top tag is ever the tree's rightmost leaf and the
        // other two never qualified -- see `keyspace_rightmost` below.
        let at_tail = at > 0
            && at == recs.len() - 1
            && left_bytes <= usable
            && bytes(&recs[at..]) <= usable;
        let at_point = at_tail
            && (old_next == 0 || self.keyspace_rightmost(&path, leaf, old_next, &recs[at])?);
        // SQLite's balance_nonroot uses neighboring child pages before growing
        // the tree. Reuse capacity in at most three existing leaves before
        // splitting. An append we have already decided to take skips it: moving
        // a closed run's records between siblings is the work this whole path
        // exists to avoid, and it cannot add capacity the append does not need.
        #[cfg(feature = "sqlite-balance")]
        if !at_point && (old_next != 0 || at + 1 != recs.len()) {
            if self.redistribute_neighbors_vec(&mut w, &path, &recs, old_next)? {
                return Ok(());
            }
        }
        let sp = if at_point {
            at
        } else if let Some(sp) = split_point(&recs, usable) {
            sp
        } else {
            // A large indivisible record between two existing runs can need
            // THREE leaves even though the total is below two-page capacity.
            // Greedy byte packing of one old page plus one new record needs
            // at most three groups; every individual cell was already bounded.
            let mut cuts=vec![0];let mut used=0;
            for (i,r) in recs.iter().enumerate() {
                if used+r.len()+4>usable {cuts.push(i);used=0;}
                used+=r.len()+4;
            }
            cuts.push(recs.len());
            if cuts.len()!=4 || cuts.windows(2).any(|w|w[0]==w[1]) {return Err(Error::TooLarge);}
            let mut middle=self.pool.allocate()?;let mid_no=middle.page_no();
            let mut right=self.pool.allocate()?;let right_no=right.page_no();
            let left_image=build_page(PageKind::Leaf,self.tree_id,leaf,&recs[..cuts[1]],mid_no)?;
            let mid_image=build_page(PageKind::Leaf,self.tree_id,mid_no,&recs[cuts[1]..cuts[2]],right_no)?;
            let right_image=build_page(PageKind::Leaf,self.tree_id,right_no,&recs[cuts[2]..],old_next)?;
            let mid_sep=validated_key(&recs[cuts[1]]).to_vec();let right_sep=validated_key(&recs[cuts[2]]).to_vec();
            middle.bytes_mut().copy_from_slice(&mid_image);right.bytes_mut().copy_from_slice(&right_image);w.bytes_mut().copy_from_slice(&left_image);
            drop((middle,right,w));self.last_leaf.set(None);
            self.insert_separator(&mut path,&mid_sep,mid_no)?;
            // The first fence can split its ancestors, so reacquire the second
            // fence's path from the new root instead of reusing stale parents.
            let (guard,mut new_path)=self.descend_for_write(&right_sep)?;drop(guard);
            self.insert_separator(&mut new_path,&right_sep,right_no)?;
            if old_next==0 {self.last_leaf.set(Some(right_no));}
            if at<cuts[1] {self.arm_after_split(key,fences,leaf,mid_no,|f|f.below(&mid_sep));}
            else if at<cuts[2] {self.arm_after_split(key,fences,mid_no,right_no,
                |f|f.above(&mid_sep).below(&right_sep));}
            else {self.arm_after_split(key,fences,right_no,old_next,|f|f.above(&right_sep));}
            return Ok(());
        };
        let right_recs = recs.split_off(sp);
        let sep = validated_key(&right_recs[0]).to_vec();

        let right_no = {
            let mut rw = self.pool.allocate()?;
            let no = rw.page_no();
            let page = build_page(PageKind::Leaf, self.tree_id, no, &right_recs, old_next)?;
            rw.bytes_mut().copy_from_slice(&page);
            no
        };

        {
            // Built before the frame is overwritten, so a failure cannot
            // leave the live leaf wiped.
            let page = build_page(PageKind::Leaf, self.tree_id, leaf, &recs, right_no)?;
            w.bytes_mut().copy_from_slice(&page);
        }
        drop(w); // release the leaf pin before recursing into the parent chain

        // The append hint: meaningful only when this split happened at the
        // tail of the leaf chain (`old_next == 0`), in which case `right_no`
        // is now the rightmost leaf and the next ascending insert should try
        // it first. A split elsewhere in the tree leaves whatever hint
        // already existed exactly as valid, or invalid, as it was — so it is
        // left untouched rather than being overwritten with a leaf that
        // isn't actually rightmost.
        if old_next == 0 {
            self.last_leaf.set(Some(right_no));
        }

        self.insert_separator(&mut path, &sep, right_no)?;
        if at < sp {
            self.arm_after_split(key, fences, leaf, right_no, |f| f.below(&sep));
        } else {
            self.arm_after_split(key, fences, right_no, old_next, |f| f.above(&sep));
        }
        Ok(())
    }

    /// L2.3-2: the same split, with every record left where it already is.
    ///
    /// Identical to `split_leaf_and_insert_vec` in every decision it takes and
    /// every byte it writes -- see the module comment above `SlotRef` and the
    /// `split_byte_equivalence` oracle. The difference is that the leaf's
    /// records are named by `SlotRef`s into one 4 KiB copy of the page image
    /// instead of being copied into one `Vec<u8>` each.
    #[cfg(any(test, feature = "slotref-split"))]
    #[allow(clippy::too_many_arguments)]
    fn split_leaf_and_insert_ref(&mut self, w: PinnedWrite<'p>, path: Vec<u32>, at: usize,
        rec: Vec<u8>, key: &[u8], fences: Option<LeafFences>) -> Result<()> {
        // Re-entrancy: nothing `split_core` can reach -- `insert_separator`,
        // `descend_for_write`, `shadow_page`, `redistribute_neighbors_ref` --
        // splits a LEAF, so these three borrows cannot nest. That is what lets
        // the separator key stay a borrow into scratch instead of a `Vec`.
        SPLIT_LEAF.with(|li| SPLIT_SLOTS.with(|sl| SPLIT_SCRATCH.with(|ss| {
            let mut leaf_img = li.borrow_mut();
            let mut slots = sl.borrow_mut();
            let mut sc = ss.borrow_mut();
            self.split_core(w, path, at, &rec, key, fences, &mut leaf_img, &mut slots, &mut sc)
        })))
    }

    #[cfg(any(test, feature = "slotref-split"))]
    #[allow(clippy::too_many_arguments)]
    fn split_core(
        &mut self,
        mut w: PinnedWrite<'p>,
        mut path: Vec<u32>,
        at: usize,
        rec: &[u8],
        key: &[u8],
        fences: Option<LeafFences>,
        leaf_img: &mut [u8],
        slots: &mut Vec<SlotRef>,
        sc: &mut SplitScratch,
    ) -> Result<()> {
        const P: usize = PAGE_SIZE;
        #[cfg(feature = "write-trace")]
        if crate::write_trace::active() { crate::write_trace::leaf_split(); }
        let leaf = w.page_no();

        // One `PageRef::open` reads the records' addresses and `next_leaf`.
        // The image is then copied into scratch: the write pin stays held, but
        // its bytes cannot be borrowed across the `&mut w` redistribution
        // needs, and one 4 KiB memcpy is cheaper than ~86 allocations.
        let old_next = {
            let p = PageRef::open_resident(w.bytes(), leaf)?;
            validate_records(&p)?;
            slots.clear();
            for i in 0..p.nentries() {
                let (off, len) = p.slot_bounds(i);
                slots.push(SlotRef { page_idx: frame::LEAF, off, len });
            }
            p.next_leaf()
        };
        leaf_img.copy_from_slice(w.bytes());
        slots.insert(at, SlotRef { page_idx: frame::REC, off: 0, len: rec.len() as u16 });
        let frames: [&[u8]; 2] = [leaf_img, rec];

        let usable = P - crate::page::HEADER_LEN;
        let bytes = |s: &[SlotRef]| s.iter().map(|x| x.len as usize + 4).sum::<usize>();
        let left_bytes = bytes(&slots[..at.min(slots.len())]);
        let at_tail = at > 0
            && at == slots.len() - 1
            && left_bytes <= usable
            && bytes(&slots[at..]) <= usable;
        let at_point = at_tail
            && (old_next == 0 || self.keyspace_rightmost(&path, leaf, old_next, rec)?);

        #[cfg(feature = "sqlite-balance")]
        if !at_point && (old_next != 0 || at + 1 != slots.len()) {
            if self.redistribute_neighbors_ref(&mut w, &path, leaf_img, rec, slots, old_next, sc)? {
                return Ok(());
            }
        }

        let sp = if at_point {
            at
        } else if let Some(sp) = split_point_ref(slots, usable) {
            sp
        } else {
            // The three-way cut: one indivisible record too large to pair with
            // its neighbours. Greedy byte packing, exactly as before; anything
            // other than two breaks is a record no arrangement can place.
            let mut cuts = [0usize; 4];
            let mut breaks = 0usize;
            let mut used = 0usize;
            for (i, s) in slots.iter().enumerate() {
                if used + s.len as usize + 4 > usable {
                    breaks += 1;
                    if breaks <= 2 { cuts[breaks] = i; }
                    used = 0;
                }
                used += s.len as usize + 4;
            }
            cuts[3] = slots.len();
            if breaks != 2 || cuts.windows(2).any(|c| c[0] == c[1]) { return Err(Error::TooLarge); }
            let mut middle = self.pool.allocate()?; let mid_no = middle.page_no();
            let mut right = self.pool.allocate()?; let right_no = right.page_no();
            build_page_into(&mut sc.out[0..P], PageKind::Leaf, self.tree_id, leaf,
                slots[..cuts[1]].iter().map(|x| rec_of(&frames, *x)), mid_no)?;
            build_page_into(&mut sc.out[P..2 * P], PageKind::Leaf, self.tree_id, mid_no,
                slots[cuts[1]..cuts[2]].iter().map(|x| rec_of(&frames, *x)), right_no)?;
            build_page_into(&mut sc.out[2 * P..3 * P], PageKind::Leaf, self.tree_id, right_no,
                slots[cuts[2]..].iter().map(|x| rec_of(&frames, *x)), old_next)?;
            let mid_sep = validated_key(rec_of(&frames, slots[cuts[1]]));
            let right_sep = validated_key(rec_of(&frames, slots[cuts[2]]));
            middle.bytes_mut().copy_from_slice(&sc.out[P..2 * P]);
            right.bytes_mut().copy_from_slice(&sc.out[2 * P..3 * P]);
            w.bytes_mut().copy_from_slice(&sc.out[0..P]);
            drop((middle, right, w));
            self.last_leaf.set(None);
            self.insert_separator(&mut path, mid_sep, mid_no)?;
            let (guard, mut new_path) = self.descend_for_write(right_sep)?;
            drop(guard);
            self.insert_separator(&mut new_path, right_sep, right_no)?;
            if old_next == 0 { self.last_leaf.set(Some(right_no)); }
            if at < cuts[1] { self.arm_after_split(key, fences, leaf, mid_no, |f| f.below(mid_sep)); }
            else if at < cuts[2] { self.arm_after_split(key, fences, mid_no, right_no,
                |f| f.above(mid_sep).below(right_sep)); }
            else { self.arm_after_split(key, fences, right_no, old_next, |f| f.above(right_sep)); }
            return Ok(());
        };

        let sep = validated_key(rec_of(&frames, slots[sp]));
        let right_no = {
            let mut rw = self.pool.allocate()?;
            let no = rw.page_no();
            build_page_into(&mut sc.out[P..2 * P], PageKind::Leaf, self.tree_id, no,
                slots[sp..].iter().map(|x| rec_of(&frames, *x)), old_next)?;
            rw.bytes_mut().copy_from_slice(&sc.out[P..2 * P]);
            no
        };
        // Built before the frame is overwritten, so a failure cannot leave the
        // live leaf wiped (btree.rs's atomicity rule for `build_page`).
        build_page_into(&mut sc.out[0..P], PageKind::Leaf, self.tree_id, leaf,
            slots[..sp].iter().map(|x| rec_of(&frames, *x)), right_no)?;
        w.bytes_mut().copy_from_slice(&sc.out[0..P]);
        drop(w);
        if old_next == 0 { self.last_leaf.set(Some(right_no)); }
        self.insert_separator(&mut path, sep, right_no)?;
        if at < sp {
            self.arm_after_split(key, fences, leaf, right_no, |f| f.below(sep));
        } else {
            self.arm_after_split(key, fences, right_no, old_next, |f| f.above(sep));
        }
        Ok(())
    }

    /// L2.3-2: `redistribute_neighbors` over borrowed records.
    ///
    /// Same windows in the same order, same `neighbor_cell_cuts`, same fit
    /// probe, same shadowing, same guard-before-install order. What changed:
    /// the window's records and the parent's separators are `SlotRef`s rather
    /// than `Vec<u8>`s, the four page images are built into reusable scratch,
    /// and the two or three separators that actually change are written into a
    /// small arena instead of one `Vec` each.
    ///
    /// PINNING. A `SlotRef` is only valid while the frame behind it is pinned,
    /// and `get_mut` below demands those same frames be unpinned. So each
    /// sibling is read-pinned ONE AT A TIME, validated, copied into scratch,
    /// and released; the splitting leaf's write pin is held throughout and its
    /// image was copied by the caller for the same reason.
    #[cfg(all(feature = "sqlite-balance", any(test, feature = "slotref-split")))]
    #[allow(clippy::too_many_arguments)]
    fn redistribute_neighbors_ref(
        &mut self,
        w: &mut PinnedWrite<'p>,
        path: &[u32],
        leaf_img: &[u8],
        rec: &[u8],
        current: &[SlotRef],
        current_next: u32,
        sc: &mut SplitScratch,
    ) -> Result<bool> {
        const P: usize = PAGE_SIZE;
        let Some(&parent_no) = path.last() else { return Ok(false) };
        let leaf = w.page_no();
        let SplitScratch {
            sib, parent, out, seps, win, pr0, pr, sizes, prefix, ends, needed, cuts, ids, newids,
        } = sc;

        let pos = {
            let r = self.pool.get(parent_no)?;
            let p = open_cached(&r, parent_no)?;
            if p.kind() != PageKind::Interior || p.tree_id() != self.tree_id {
                return Err(Error::Corrupt { page_no: parent_no, why: "redistribution parent identity" });
            }
            ids.clear();
            pr0.clear();
            ids.push(p.child0());
            for i in 0..p.nentries() {
                let (off, len) = p.slot_bounds(i);
                pr0.push(SlotRef { page_idx: frame::PARENT, off, len });
                ids.push(validated_child(p.slot(i)));
            }
            parent.copy_from_slice(&r[..]);
            ids.iter().position(|id| *id == leaf)
                .ok_or(Error::Corrupt { page_no: parent_no, why: "redistribution child missing" })?
        };
        let nchild = ids.len();

        let mut windows = [(0usize, 0usize); 3];
        let mut nw = 0;
        if nchild >= 3 { windows[nw] = (pos.saturating_sub(1).min(nchild - 3), 3); nw += 1; }
        if pos + 1 < nchild { windows[nw] = (pos, 2); nw += 1; }
        if pos > 0 { windows[nw] = (pos - 1, 2); nw += 1; }
        let usable = P - crate::page::HEADER_LEN;

        'windows: for &(start, count) in &windows[..nw] {
            // Gather the window. One sibling pinned at a time; its image is
            // copied into scratch before the pin is dropped.
            win.clear();
            let mut next = 0u32;
            let mut nsib = 0usize;
            for k in start..start + count {
                let no = ids[k];
                if no == leaf {
                    win.extend_from_slice(current);
                    next = current_next;
                    continue;
                }
                let r = self.pool.get(no)?;
                let p = open_cached(&r, no)?;
                if p.tree_id() != self.tree_id {
                    return Err(Error::Corrupt { page_no: no, why: "redistribution sibling identity" });
                }
                if p.kind() != PageKind::Leaf {
                // A grafted run enters the tree as a SUBTREE under a single
                // separator, so a leaf's sibling under the same parent can be
                // an interior page: a tree with a graft in it is no longer
                // uniformly deep. Redistribution moves leaf CELLS between
                // neighbours and has nothing to say about a subtree, so skip
                // this window and let the ordinary split run -- it never
                // needed the neighbours. Refusing here turned an ordinary
                // insert next to a bulk-built index into `Corrupt`, with
                // nothing actually corrupt (kernel/tests/range_graft.rs).
                    continue 'windows;
                }
                let idx = if nsib == 0 { frame::SIB0 } else { frame::SIB1 };
                sib[nsib * P..nsib * P + P].copy_from_slice(&r[..]);
                for i in 0..p.nentries() {
                    let (off, len) = p.slot_bounds(i);
                    win.push(SlotRef { page_idx: idx, off, len });
                }
                next = p.next_leaf();
                nsib += 1;
            }

            sizes.clear();
            sizes.extend(win.iter().map(|s| s.len as usize + 4));
            if !neighbor_cell_cuts_into(sizes, usable, count, prefix, ends, needed, cuts) { continue; }
            let groups = cuts.len() - 1;
            if groups > count { continue; }
            // `neighbor_cell_cuts` returns `max(existing, needed)` groups and
            // the line above rejects more than `existing`, so the window is
            // always repacked into exactly as many pages as it already had.
            debug_assert_eq!(groups, count);

            newids.clear();
            newids.extend_from_slice(ids);
            pr.clear();
            pr.extend_from_slice(pr0);

            // The separators this window rewrites, into the arena. Children
            // are the pre-shadow page numbers for now: the fit probe below
            // depends on the RECORD LENGTHS, which shadowing cannot change,
            // and shadowing must not happen before a window can still be
            // rejected.
            let mut arena = 0usize;
            {
                let f: [&[u8]; 5] = [leaf_img, rec, &sib[..P], &sib[P..2 * P], parent];
                for j in 1..groups {
                    let key = validated_key(rec_of(&f, win[cuts[j]]));
                    pr[start + j - 1] = push_sep(seps, &mut arena, key, newids[start + j]);
                }
                if start > 0 {
                    // The separator to the window's LEFT keeps its key and is
                    // re-pointed at whatever page now starts the window.
                    let key = validated_key(rec_of(&f, pr0[start - 1]));
                    pr[start - 1] = push_sep(seps, &mut arena, key, newids[start]);
                }
            }

            {
                let f: [&[u8]; 6] = [leaf_img, rec, &sib[..P], &sib[P..2 * P], parent, seps];
                match build_page_into(&mut out[3 * P..4 * P], PageKind::Interior, self.tree_id,
                        parent_no, pr.iter().map(|x| rec_of(&f, *x)), ids[0]) {
                    Ok(()) => {}
                    Err(Error::TooLarge) => continue,
                    Err(e) => return Err(e),
                }
            }

            // Past every `continue`: from here the window is committed, so
            // page-allocating side effects are allowed.
            for k in start..start + count {
                if newids[k] != leaf && self.pool.is_frozen(newids[k]) {
                    newids[k] = shadow_page(self.pool, newids[k])?;
                }
            }
            for j in 1..groups { patch_sep_child(seps, pr[start + j - 1], newids[start + j]); }
            if start > 0 { patch_sep_child(seps, pr[start - 1], newids[start]); }

            {
                let f: [&[u8]; 6] = [leaf_img, rec, &sib[..P], &sib[P..2 * P], parent, seps];
                for j in 0..groups {
                    let no = newids[start + j];
                    let nx = if j + 1 < groups { newids[start + j + 1] } else { next };
                    build_page_into(&mut out[j * P..(j + 1) * P], PageKind::Leaf, self.tree_id, no,
                        win[cuts[j]..cuts[j + 1]].iter().map(|x| rec_of(&f, *x)), nx)?;
                }
                build_page_into(&mut out[3 * P..4 * P], PageKind::Interior, self.tree_id, parent_no,
                    pr.iter().map(|x| rec_of(&f, *x)), newids[0])?;
            }

            // Acquire every fallible guard before installing any new image.
            let mut guards: [Option<(usize, PinnedWrite<'p>)>; 3] = [None, None, None];
            let mut g = 0;
            for j in 0..groups {
                if newids[start + j] != leaf {
                    guards[g] = Some((j, self.pool.get_mut(newids[start + j])?));
                    g += 1;
                }
            }
            let mut pw = self.pool.get_mut(parent_no)?;
            for slot in guards.iter_mut().flatten() {
                let j = slot.0;
                slot.1.bytes_mut().copy_from_slice(&out[j * P..(j + 1) * P]);
            }
            let me = pos - start;
            w.bytes_mut().copy_from_slice(&out[me * P..(me + 1) * P]);
            pw.bytes_mut().copy_from_slice(&out[3 * P..4 * P]);
            self.last_leaf.set(None);
            // Redistribution rewrote the separators BETWEEN these siblings, so
            // a hint on any of them describes an interval that has moved --
            // and unlike a split it can leave the sibling chain intact, so the
            // page-side check would not notice. Only the window is affected;
            // the rest of the cache is still exact.
            if let Some(t) = self.tags {
                for j in 0..groups { t.forget_leaf(self.tree_id, newids[start + j]); }
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Is this leaf the rightmost one of the NEW RECORD'S OWN key tag?
    ///
    /// D9's append split fires only at the tree's rightmost leaf. That is one
    /// leaf in the whole file, and D4 makes every feature a key tag inside the
    /// same tree, so a store with several ascending runs gets the append split
    /// for exactly one of them. `src/collections.rs` writes one document as
    /// three keys -- 0x60 vector, 0x40 row, 0x20 mapping -- each ascending in
    /// itself; the 0x40 and 0x20 runs always have a higher-tag leaf to their
    /// right, so both paid `redistribute_neighbors` (a clone of the parent's
    /// records plus up to three siblings' records, then up to four rebuilt page
    /// images) on every single leaf fill. A CPU sample of a 100K typed load put
    /// 23.3% of load time inside `split_leaf_and_insert`, about 45% of that in
    /// malloc/clone/free and 26% in `build_page` + `neighbor_cell_cuts`.
    ///
    /// The widened signal keeps D9's discipline exactly. The caller has already
    /// established that the new record is STRICTLY the largest in this leaf; so
    /// if the first key to this leaf's right carries a different tag, there is
    /// no key of this tag anywhere to the right, and the insert is an append to
    /// its own run in the same unambiguous sense D9 requires. A scattered
    /// workload still cannot reach here: a random key is the leaf-local maximum
    /// only rarely, and when it is, the tag test is the same one D9 already
    /// trusted.
    ///
    /// THE BOUNDARY LEAF. One leaf per keyspace holds the last key of one tag
    /// and the first key of the next. An ascending insert into that leaf lands
    /// BEFORE the higher tag's records, so `at == recs.len() - 1` is false and
    /// this is never consulted: the boundary leaf keeps the balanced split and
    /// the neighbour redistribution, unchanged. The shortcut costs one such
    /// leaf per keyspace boundary, which is where it belongs.
    ///
    /// The separator sitting in the parent ALREADY ON THE DESCENT PATH is this
    /// leaf's right neighbour's first key, so the ordinary case reads no page
    /// the insert had not already read. Only a leaf that is its parent's last
    /// child needs the sibling itself, and that leaf's right neighbour lives
    /// under another parent.
    ///
    /// SACRIFICE (Law 4): one buffer-pool `get` of the parent per split of a
    /// full leaf whose new record is its maximum, and for the last child of a
    /// parent one `get` of the right sibling, which may be a page read. Splits
    /// are one insert in tens; redistribution already opened this same parent.
    #[cfg(feature = "keyspace-append")]
    fn keyspace_rightmost(&self, path: &[u32], leaf: u32, old_next: u32, rec: &[u8]) -> Result<bool> {
        let Some(&tag) = validated_key(rec).first() else { return Ok(false) };
        // The parent separator is free: this descent just walked through it.
        if let Some(&parent) = path.last() {
            let r = self.pool.get(parent)?;
            let p = open_cached(&r, parent)?;
            if p.kind() == PageKind::Interior && p.tree_id() == self.tree_id {
                let n = p.nentries();
                let pos = if p.child0() == leaf {
                    Some(0)
                } else {
                    (0..n).find(|&i| validated_child(p.slot(i)) == leaf).map(|i| i + 1)
                };
                // `pos == n` is the parent's last child: its right neighbour is
                // under a different parent, so fall through to the sibling.
                if let Some(pos) = pos {
                    if pos < n {
                        return Ok(validated_key(p.slot(pos)).first() != Some(&tag));
                    }
                }
            }
        }
        let r = self.pool.get(old_next)?;
        let p = open_cached(&r, old_next)?;
        if p.kind() != PageKind::Leaf || p.tree_id() != self.tree_id || p.nentries() == 0 {
            return Ok(false);
        }
        Ok(validated_key(p.slot(0)).first() != Some(&tag))
    }

    #[cfg(not(feature = "keyspace-append"))]
    fn keyspace_rightmost(&self, _path: &[u32], _leaf: u32, _next: u32, _rec: &[u8]) -> Result<bool> {
        Ok(false)
    }

    /// Reuse neighboring capacity without adding a leaf to the window. If all
    /// neighbors are full, use the ordinary one-to-two split instead. This
    /// avoids rewriting unrelated full siblings merely to allocate a new leaf.
    /// New images are built before edits; frozen siblings are shadowed before
    /// changing their records. Only this parent and its children participate.
    #[cfg(feature = "sqlite-balance")]
    #[cfg(any(test, not(feature = "slotref-split")))]
    fn redistribute_neighbors_vec(&mut self, w: &mut PinnedWrite<'p>, path: &[u32], current: &[Vec<u8>], current_next: u32) -> Result<bool> {
        let Some(&parent)=path.last() else{return Ok(false);};
        let leaf=w.page_no();
        let (parent_recs, child_ids, pos)={
            let r=self.pool.get(parent)?;let p=open_cached(&r,parent)?;
            if p.kind()!=PageKind::Interior || p.tree_id()!=self.tree_id{return Err(Error::Corrupt{page_no:parent,why:"redistribution parent identity"});}
            let recs:Vec<Vec<u8>>=(0..p.nentries()).map(|i|p.slot(i).to_vec()).collect();
            let mut ids=vec![p.child0()];ids.extend(recs.iter().map(|r|validated_child(r)));
            let pos=ids.iter().position(|id|*id==leaf).ok_or(Error::Corrupt{page_no:parent,why:"redistribution child missing"})?;
            (recs,ids,pos)
        };
        let mut windows=Vec::new();
        if child_ids.len()>=3{windows.push((pos.saturating_sub(1).min(child_ids.len()-3),3));}
        if pos+1<child_ids.len(){windows.push((pos,2));}
        if pos>0{windows.push((pos-1,2));}
        let usable=PAGE_SIZE-crate::page::HEADER_LEN;
        'windows: for (start,count) in windows{
            let mut all=Vec::new();let mut next=0;
            for &no in &child_ids[start..start+count]{
                if no==leaf{all.extend(current.iter().cloned());next=current_next;}
                else{let r=self.pool.get(no)?;let p=open_cached(&r,no)?;
                    if p.tree_id()!=self.tree_id{return Err(Error::Corrupt{page_no:no,why:"redistribution sibling identity"});}
                    if p.kind()!=PageKind::Leaf{
                    // A grafted run enters the tree as a SUBTREE under a
                    // single separator, so a leaf's sibling under the same
                    // parent can be an interior page: a tree with a graft in
                    // it is no longer uniformly deep. Redistribution moves
                    // leaf CELLS between neighbours and has nothing to say
                    // about a subtree, so skip this window and let the
                    // ordinary split run -- it never needed the neighbours.
                    // Refusing here turned an ordinary insert next to a
                    // bulk-built index into `Corrupt`, with nothing actually
                    // corrupt (kernel/tests/range_graft.rs).
                        continue 'windows;
                    }
                    all.extend((0..p.nentries()).map(|i|p.slot(i).to_vec()));next=p.next_leaf();}
            }
            let sizes:Vec<usize>=all.iter().map(|r|r.len()+4).collect();
            let Some(cuts)=neighbor_cell_cuts(&sizes,usable,count) else { continue; };
            let groups=cuts.len()-1;
            if groups>count { continue; }
            let mut ids=child_ids.clone();
            let mut pr=parent_recs.clone();
            if groups>count{ids.insert(start+count,0);pr.insert(start+count-1,enc_interior(validated_key(&all[cuts[count]]),0));}
            for j in 1..groups{pr[start+j-1]=enc_interior(validated_key(&all[cuts[j]]),ids[start+j]);}
            match build_page(PageKind::Interior,self.tree_id,parent,&pr,child_ids[0]){Ok(_)=>{},Err(Error::TooLarge)=>continue,Err(e)=>return Err(e)}
            if groups>count{let mut fresh=self.pool.allocate()?;let no=fresh.page_no();let mut p=PageMut::init(fresh.bytes_mut(),PageKind::Leaf,self.tree_id,no);p.finalise(0);ids[start+count]=no;}
            for id in &mut ids[start..start+count]{if *id!=leaf && self.pool.is_frozen(*id){*id=shadow_page(self.pool,*id)?;}}
            let mut images=Vec::new();
            for j in 0..groups{
                let no=ids[start+j];let next=if j+1<groups{ids[start+j+1]}else{next};
                images.push(build_page(PageKind::Leaf,self.tree_id,no,&all[cuts[j]..cuts[j+1]],next)?);
                if start+j>0{
                    let k=if j>0{validated_key(&all[cuts[j]]).to_vec()}else{validated_key(&pr[start+j-1]).to_vec()};
                    pr[start+j-1]=enc_interior(&k,no);
                }
            }
            let parent_image=build_page(PageKind::Interior,self.tree_id,parent,&pr,ids[0])?;
            // Acquire every fallible guard before installing any new image.
            let mut guards=Vec::new();for j in 0..groups{if ids[start+j]!=leaf{guards.push((j,self.pool.get_mut(ids[start+j])?));}}
            let mut pw=self.pool.get_mut(parent)?;
            for (j,guard) in &mut guards{guard.bytes_mut().copy_from_slice(&images[*j]);}
            w.bytes_mut().copy_from_slice(&images[pos-start]);pw.bytes_mut().copy_from_slice(&parent_image);
            self.last_leaf.set(None);
            // See `redistribute_neighbors_ref`: the window's separators moved.
            if let Some(t)=self.tags {for j in 0..groups {t.forget_leaf(self.tree_id,ids[start+j]);}}
            return Ok(true);
        }
        Ok(false)
    }

    fn insert_separator(&mut self, path: &mut Vec<u32>, sep: &[u8], right: u32) -> Result<()> {
        let Some(parent) = path.pop() else {
            // The root split: build a new root above the old one.
            let left = self.root;
            let mut w = self.pool.allocate()?;
            let no = w.page_no();
            let mut p = PageMut::init(w.bytes_mut(), PageKind::Interior, self.tree_id, no);
            p.set_child0(left);                       // the old root
            p.insert_slot(0, &enc_interior(sep, right))?;
            p.finalise(0);
            drop(w);
            self.root = no;
            return Ok(());
        };

        let rec = enc_interior(sep, right);
        let (i, room) = {
            let r = self.pool.get(parent)?;
            let p = PageRef::open_resident(&r, parent)?;
            validate_records(&p)?;
            let i = upper_bound(&p, sep)?;
            // Interior pages never have entries removed in this task, so
            // this figure cannot yet have diverged from a summed
            // reconstruction — but the real free_space() is what's actually
            // true, and using it here keeps both room checks in the file
            // computing the same thing the same way.
            (i, p.free_space() >= rec.len() + 4)
        };

        if room {
            let mut w = self.pool.get_mut(parent)?;
            let mut p = PageMut::reopen(w.bytes_mut());
            // (sep, right) slots in ahead of the first entry whose key
            // exceeds sep. Whatever pointer already reached the left half still
            // reaches it, because the left half kept its page number.
            p.insert_slot(i, &rec)?;
            p.finalise(0);
            return Ok(());
        }

        // Split the interior page the same way.
        #[cfg(feature = "write-trace")]
        if crate::write_trace::active() { crate::write_trace::interior_split(); }
        let mut recs: Vec<Vec<u8>> = {
            let r = self.pool.get(parent)?;
            let p = PageRef::open_resident(&r, parent)?;
            validate_records(&p)?;
            (0..p.nentries()).map(|j| p.slot(j).to_vec()).collect()
        };
        recs.insert(i, rec);
        let old_child0 = {
            let r = self.pool.get(parent)?;
            let p = PageRef::open_resident(&r, parent)?;
            validate_records(&p)?;
            p.child0()
        };
        let sp = split_point(&recs, PAGE_SIZE - crate::page::HEADER_LEN)
            .ok_or(Error::TooLarge)?;
        let right_recs = recs.split_off(sp);
        // The right page's FIRST entry is promoted out of the slot array: its
        // key becomes the separator pushed up, and its child becomes the right
        // page's child0. This is the standard interior split.
        let up = validated_key(&right_recs[0]).to_vec();
        let right_child0 = validated_child(&right_recs[0]);

        let right_no = {
            let mut w = self.pool.allocate()?;
            let no = w.page_no();
            let page = build_page(
                PageKind::Interior, self.tree_id, no, &right_recs[1..], right_child0)?;
            w.bytes_mut().copy_from_slice(&page);
            no
        };
        {
            let page = build_page(
                PageKind::Interior, self.tree_id, parent, &recs, old_child0)?;
            let mut w = self.pool.get_mut(parent)?;
            w.bytes_mut().copy_from_slice(&page);
        }
        self.insert_separator(path, &up, right_no)
    }

    /// Delete one record and maintain sparsely occupied siblings locally.
    /// Scratch is bounded by two children plus their parent; published pages
    /// remain protected by the ordinary copy-on-write retirement protocol.
    pub fn delete(&mut self, key: &[u8]) -> Result<bool> {
        // A delete can empty a leaf, merge two, collapse the root, or free a
        // page that a later allocation hands to another leaf of this same
        // tree. The hinted fences survive none of that, and the page-identity
        // checks cannot tell a recycled leaf from the one that was armed.
        self.forget_tag_hints();
        // Read-only presence probe first, so a miss never shadows anything.
        let (leaf, _) = self.descend(key)?;
        let retired = {
            let r = self.pool.get(leaf)?;
            let p = open_cached(&r, leaf)?;
            let i = lower_bound(&p, key)?;
            if i >= p.nentries()
                || validated_key(p.slot(i)) != key
            {
                return Ok(false);
            }
            replaced_overflow_pages(self.pool, p.slot(i))?
        };
        // Hit: take the shadowed write descent (2f) and remove there.
        let (mut w, path) = self.descend_for_write(key)?;
        let i = {
            let pr = PageRef::open_resident_validated(w.bytes(), w.page_no())?;
            lower_bound(&pr, key)?
        };
        let mut p = PageMut::reopen(w.bytes_mut());
        p.remove_slot(i);
        p.finalise(0);
        drop(p);
        let leaf = w.page_no();
        drop(w);
        for (no, birth) in retired { self.pool.free_shadow_page(no, birth)?; }
        self.rebalance_after_delete(leaf, path)?;
        Ok(true)
    }

    /// At most two sibling images and their parent per level. Merge when they
    /// fit; otherwise redistribute only below one-third occupancy. The gap to
    /// half occupancy provides hysteresis for alternating delete/reinsert.
    fn rebalance_after_delete(&mut self, mut node: u32, mut path: Vec<u32>) -> Result<()> {
        let usable = PAGE_SIZE - crate::page::HEADER_LEN;
        loop {
            let (kind, used, entries, only_child, node_birth) = {
                let r = self.pool.get(node)?; let p = open_cached(&r, node)?;
                if p.tree_id() != self.tree_id || !matches!(p.kind(), PageKind::Leaf | PageKind::Interior) {
                    return Err(Error::Corrupt { page_no: node, why: "delete maintenance node identity" });
                }
                (p.kind(), (0..p.nentries()).map(|i|p.slot(i).len()+4).sum::<usize>(), p.nentries(), p.child0(), p.lsn())
            };
            if node == self.root {
                if kind == PageKind::Interior && entries == 0 {
                    // Verify the surviving child before unlinking the old root.
                    let r = self.pool.get(only_child)?; let child = open_cached(&r, only_child)?;
                    if child.tree_id()!=self.tree_id || !matches!(child.kind(),PageKind::Leaf|PageKind::Interior) {
                        return Err(Error::Corrupt {page_no:only_child,why:"collapsed root child identity"});
                    }
                    drop(r);
                    let birth=if self.pool.is_frozen(node) {node_birth}else{self.pool.write_generation()};
                    self.pool.free_shadow_page(node, birth)?;
                    self.root = only_child; self.last_leaf.set(None); node = only_child;
                    continue;
                }
                return Ok(());
            }
            if used >= usable / 3 { return Ok(()); }
            let parent = path.pop().ok_or(Error::Corrupt {page_no:node,why:"delete maintenance missing parent"})?;
            let (parent_recs, ids, pos) = {
                let r=self.pool.get(parent)?;let p=open_cached(&r,parent)?;
                if p.kind()!=PageKind::Interior || p.tree_id()!=self.tree_id {
                    return Err(Error::Corrupt {page_no:parent,why:"delete maintenance parent identity"});
                }
                let recs:Vec<Vec<u8>>=(0..p.nentries()).map(|i|p.slot(i).to_vec()).collect();
                let ids:Vec<u32>=(0..=p.nentries()).map(|i|child_at(self.pool,&p,i)).collect::<Result<_>>()?;
                let pos=ids.iter().position(|id|*id==node).ok_or(Error::Corrupt {page_no:parent,why:"delete maintenance child missing"})?;
                (recs,ids,pos)
            };
            if ids.len()==1 { node=parent; continue; }
            // Prefer a merge in either direction over rewriting two pages.
            let mut starts=Vec::with_capacity(2);
            if pos>0 {starts.push(pos-1);} if pos+1<ids.len() {starts.push(pos);}
            let mut changed=false;
            'attempt: for merge_only in [true,false] {
                for &start in &starts {
                    let left=ids[start];let right=ids[start+1];
                    // A grafted run enters the tree as a SUBTREE under one
                    // separator, so two children of the same parent need not
                    // be the same kind any more: an underfull leaf can sit
                    // beside a bulk-built subtree root. Merging or
                    // redistributing across that boundary is not defined --
                    // their records are not the same shape and they are not
                    // the same height -- so this PAIR is declined and the next
                    // candidate tried. Refusing outright failed an ordinary
                    // delete next to a bulk-built index with `Corrupt` and
                    // nothing corrupt; a declined pair only leaves a node
                    // underfull, which `!changed` already tolerates.
                    let (mut all, left_link) = {
                        let r=self.pool.get(left)?;let p=open_cached(&r,left)?;
                        if p.tree_id()!=self.tree_id {return Err(Error::Corrupt {page_no:left,why:"delete left sibling identity"});}
                        if p.kind()!=kind {continue;}
                        ((0..p.nentries()).map(|i|p.slot(i).to_vec()).collect::<Vec<_>>(),p.next_leaf())
                    };
                    let (right_link, right_birth) = {
                        let r=self.pool.get(right)?;let p=open_cached(&r,right)?;
                        if p.tree_id()!=self.tree_id {return Err(Error::Corrupt {page_no:right,why:"delete right sibling identity"});}
                        if p.kind()!=kind {continue;}
                        if kind==PageKind::Interior {all.push(enc_interior(validated_key(&parent_recs[start]),p.child0()));}
                        all.extend((0..p.nentries()).map(|i|p.slot(i).to_vec()));(p.next_leaf(),p.lsn())
                    };
                    let total:usize=all.iter().map(|r|r.len()+4).sum();let merge=total<=usable;
                    if merge_only!=merge {continue;}
                    let mut pr=parent_recs.clone();let mut children=ids.clone();
                    let (left_recs,right_recs,right_child0)=if merge {
                        pr.remove(start);children.remove(start+1);(all.clone(),Vec::new(),0)
                    } else if kind==PageKind::Leaf {
                        let Some(mid)=split_point(&all,usable) else {continue;};
                        pr[start]=enc_interior(validated_key(&all[mid]),right);
                        (all[..mid].to_vec(),all[mid..].to_vec(),right_link)
                    } else {
                        let mut prefix=0usize;let mut best=None;
                        for (i,rec) in all.iter().enumerate() {
                            let suffix=total-prefix-rec.len()-4;
                            if prefix<=usable && suffix<=usable {
                                let delta=prefix.abs_diff(suffix);
                                if best.is_none_or(|(_,old)|delta<old) {best=Some((i,delta));}
                            }
                            prefix+=rec.len()+4;
                        }
                        let Some((mid,_))=best else {continue;};
                        pr[start]=enc_interior(validated_key(&all[mid]),right);
                        (all[..mid].to_vec(),all[mid+1..].to_vec(),validated_child(&all[mid]))
                    };
                    // A longer replacement fence can overflow a variable-key
                    // parent. Decline that redistribution without touching it.
                    match build_page(PageKind::Interior,self.tree_id,parent,&pr,children[0]) {
                        Ok(_)=>{},Err(Error::TooLarge)=>continue,Err(e)=>return Err(e),
                    }
                    let left_next=if kind==PageKind::Interior {left_link}else if merge {right_link}else{right};
                    let mut left_image=build_page(kind,self.tree_id,left,&left_recs,left_next)?;
                    let mut right_image=if merge {None}else{Some(build_page(kind,self.tree_id,right,&right_recs,right_child0)?)};
                    // Shadow only pages that will be rewritten; a merged-away
                    // frozen sibling is retired directly, never overwritten.
                    let fresh_left=if self.pool.is_frozen(left) {shadow_page(self.pool,left)?}else{left};
                    let fresh_right=if !merge && self.pool.is_frozen(right) {shadow_page(self.pool,right)?}else{right};
                    children[start]=fresh_left;
                    if !merge {children[start+1]=fresh_right;}
                    for (i,&id) in children.iter().enumerate().skip(1) {
                        let key=validated_key(&pr[i-1]).to_vec();pr[i-1]=enc_interior(&key,id);
                    }
                    left_image[12..16].copy_from_slice(&fresh_left.to_le_bytes());
                    if kind==PageKind::Leaf && !merge {left_image[20..24].copy_from_slice(&fresh_right.to_le_bytes());}
                    if let Some(image)=right_image.as_mut(){image[12..16].copy_from_slice(&fresh_right.to_le_bytes());}
                    let parent_image=build_page(PageKind::Interior,self.tree_id,parent,&pr,children[0])?;
                    // Acquire every fallible guard and reserve retirement before
                    // installing any replacement image. Store fails closed on
                    // any error; no fallible operation follows the image copies.
                    let mut lw=self.pool.get_mut(fresh_left)?;
                    let mut rw=if merge {None}else{Some(self.pool.get_mut(fresh_right)?)};
                    let mut pw=self.pool.get_mut(parent)?;
                    if merge {
                        let birth=if self.pool.is_frozen(right) {right_birth}else{self.pool.write_generation()};
                        self.pool.free_shadow_page(right,birth)?;
                    }
                    lw.bytes_mut().copy_from_slice(&left_image);
                    if let (Some(w),Some(image))=(&mut rw,&right_image){w.bytes_mut().copy_from_slice(image);}
                    pw.bytes_mut().copy_from_slice(&parent_image);
                    self.last_leaf.set(None);changed=merge;
                    break 'attempt;
                }
            }
            if !changed {return Ok(());} node=parent;
        }
    }

    /// Scan from `from` forward, in key order.
    ///
    /// SINGLE WRITER. The iterator borrows the pool, not `self`, so the borrow
    /// checker will happily let you `insert` into this tree while a scan is live.
    /// Do not: the iterator re-reads its leaf on every `next`, so an insert that
    /// shifts entries in the leaf it is standing on makes it skip or repeat a
    /// row, with no compiler or runtime signal. The engine is single-writer by
    /// design, and this is where that assumption is cashed.
    /// Remove every key with `prefix`. Walks matching leaves via the
    /// parent path (a cleared leaf still receives the same descent -- the
    /// separators do not change -- so re-descending with the prefix would
    /// loop on the first emptied page forever; the advance must go THROUGH
    /// the parents, the RangeIter lesson). Each matching leaf is either
    /// CLEARED in one page write (every entry matches) or slot-trimmed.
    /// Cost: O(matching leaves) page writes + one read-descent each, not
    /// O(matching rows) tree operations. Emptied leaves stay allocated
    /// (the documented delete posture).
    pub fn delete_prefix(&mut self, prefix: &[u8]) -> Result<u64> {
        self.forget_tag_hints();
        let mut removed = 0u64;
        let mut cursor: Vec<u8> = prefix.to_vec();
        // 2n: the last leaf KEPT in the chain. Fully-cleared leaves after it
        // are unlinked from their parent and freed; the anchor's sibling
        // pointer is patched forward over each. The FIRST visited leaf is
        // never freed (its left neighbour is unknown), and a parent's last
        // child is never detached (no cascade; the leaf stays cleared).
        let mut anchor: Option<u32> = None;
        'leaves: loop {
            // read-descend to the candidate leaf, remembering the path
            let (leaf, mut path) = self.descend_with_path(&cursor)?;
            let (matches, all, past, retired) = {
                let r = self.pool.get(leaf)?;
                let p = open_cached(&r, leaf)?;
                let n = p.nentries();
                let mut idx = Vec::new();
                let mut past = false;
                let mut retired = Vec::new();
                for i in 0..n {
                    let k = validated_key(p.slot(i));
                    if k.starts_with(prefix) {
                        idx.push(i);
                        retired.extend(replaced_overflow_pages(self.pool, p.slot(i))?);
                    }
                    else if k > prefix { past = true; }
                }
                (idx.clone(), n > 0 && idx.len() == n, past, retired)
            };
            if !matches.is_empty() {
                removed += matches.len() as u64;
                let (mut w, wpath) = self.descend_for_write(&cursor)?;
                // The write descent may SHADOW the leaf (fresh or recycled
                // number) -- the numbers legitimately differ. What the slot
                // indices computed from the read descent require is CONTENT
                // congruence: the shadow is a byte-identical copy. Only
                // check when a shadow actually happened: reading the same
                // page while `w` write-pins it is itself a pin conflict.
                #[cfg(debug_assertions)]
                if w.page_no() != leaf {
                    let wn = { let p = PageMut::reopen(w.bytes_mut()); p.nentries_pub() };
                    let rn = { let r = self.pool.get(leaf)?; open_cached(&r, leaf)?.nentries() };
                    debug_assert_eq!(wn, rn,
                        "write-descent leaf diverged from the read-descent leaf");
                }
                if all {
                    // Whole-leaf clear MUST keep the sibling pointer: init
                    // resets the full header, and a zeroed next_leaf makes a
                    // MIDDLE leaf look rightmost -- the append fast path then
                    // writes keys past the leaf's true range and orphans
                    // committed subtrees (silent loss: a fold+insert+fold
                    // cycle dropped every folded segment; caught by 2n's
                    // reuse probe, present since 2h).
                    let no = w.page_no();
                    let tid = self.tree_id;
                    let nl = u32::from_le_bytes(w.bytes_mut()[20..24].try_into().unwrap());
                    let mut p = PageMut::init(w.bytes_mut(), PageKind::Leaf, tid, no);
                    p.set_next_leaf(nl);
                    p.finalise(0);
                    drop(w);
                    // 2n: detach and free it when safe -- an emptied leaf
                    // whose key range never refills (monotonic ids) is
                    // otherwise allocated forever. Stale read paths still
                    // find it cleared-with-chain, which walks treat as any
                    // other empty leaf.
                    let freed = match (anchor, wpath.last()) {
                        (Some(a), Some(&parent)) if a != no => {
                            match unlink_child(self.pool, self.tree_id, parent, no)? {
                                Some(pos) => {
                                    self.pool.free_page(no)?;
                                    let mut aw = self.pool.get_mut(a)?;
                                    let mut ap = PageMut::reopen(aw.bytes_mut());
                                    ap.set_next_leaf(nl);
                                    ap.finalise(0);
                                    // later iterations' read path may hold
                                    // saved child indices into this SAME
                                    // writable parent -- shift them left
                                    // past the removed position, or the
                                    // advance skips a child (rows survive
                                    // the delete; measured, not theorised).
                                    for e in path.iter_mut() {
                                        if e.0 == parent && e.1 >= pos && e.1 > 0 { e.1 -= 1; }
                                    }
                                    true
                                }
                                None => false,
                            }
                        }
                        _ => false,
                    };
                    if !freed { anchor = Some(no); }
                } else {
                    let mut p = PageMut::reopen(w.bytes_mut());
                    for &i in matches.iter().rev() { p.remove_slot(i); }
                    p.finalise(0);
                    anchor = Some(w.page_no());
                }
                for (no, birth) in retired { self.pool.free_shadow_page(no, birth)?; }
                self.last_leaf.set(None); // the hint may name a cleared leaf
            }
            if past { return Ok(removed); }
            // advance through the parents to the next leaf's first key
            loop {
                let (mut cur, mut idx) = loop {
                    let Some((page, i)) = path.pop() else { return Ok(removed) };
                    let n = {
                        let r = self.pool.get(page)?;
                        open_cached(&r, page)?.nentries()
                    };
                    if i < n { break (page, i + 1); }
                };
                // leftmost spine from the right sibling down to a leaf
                let first_key: Option<Vec<u8>> = loop {
                    let r = self.pool.get(cur)?;
                    let p = open_cached(&r, cur)?;
                    if p.kind() == PageKind::Leaf {
                        break if p.nentries() == 0 { None }
                              else { Some(validated_key(p.slot(0)).to_vec()) };
                    }
                    let child = child_at(self.pool, &p, idx)?;
                    path.push((cur, idx));
                    cur = child;
                    idx = 0;
                };
                match first_key {
                    Some(k) if k.starts_with(prefix) => { cursor = k; continue 'leaves; }
                    Some(_) => return Ok(removed),
                    None => continue, // empty leaf: keep advancing
                }
            }
        }
    }

    pub fn range(&self, from: &[u8]) -> Result<RangeIter<'p>> {
        let (leaf, idx, path) = self.descend_positioned(from)?;
        Ok(RangeIter {
            pool: self.pool, tree_id: self.tree_id, page: leaf, idx,
            done: false, leaves: 1, max_leaves: self.pool.page_count(),
            buf: std::collections::VecDeque::new(),
            served: 0,
            path,
            pin: None,
        })
    }

    /// Scan keys strictly below `to` in descending order.
    pub fn range_reverse(&self, to: &[u8]) -> Result<ReverseRangeIter<'p>> {
        let (leaf, path) = self.descend_with_path(to)?;
        let idx = { let r = self.pool.get(leaf)?; lower_bound(&open_cached(&r, leaf)?, to)? };
        Ok(ReverseRangeIter {
            pool: self.pool, tree_id: self.tree_id, page: leaf, idx,
            done: false, leaves: 1, max_leaves: self.pool.page_count(), path,
            pin: None, pending: None,
        })
    }
}

impl ReverseRangeIter<'_> {
    /// Step to the leaf immediately left of the current one through the saved
    /// parent path, then land just past its final slot.
    fn retreat(&mut self) -> Result<bool> {
        let (mut cur, mut idx_in_parent) = loop {
            let Some((page, i)) = self.path.pop() else { return Ok(false) };
            if i > 0 { break (page, i - 1); }
        };
        loop {
            let r = self.pool.get(cur)?;
            let p = open_cached(&r, cur)?;
            if p.tree_id() != self.tree_id {
                return Err(Error::Corrupt { page_no: cur, why: "page belongs to another tree" });
            }
            if p.kind() == PageKind::Leaf {
                self.page = cur;
                self.idx = p.nentries();
                break;
            }
            let n = p.nentries();
            let child_i = if idx_in_parent > n { n } else { idx_in_parent };
            let child = child_at(self.pool, &p, child_i)?;
            self.path.push((cur, child_i));
            cur = child;
            idx_in_parent = usize::MAX; // every later descent takes the rightmost child
        }
        self.leaves += 1;
        if self.leaves > self.max_leaves {
            return Err(Error::Corrupt {
                page_no: self.page, why: "reverse scan visits more leaves than the file holds" });
        }
        Ok(true)
    }

    /// The descending record the cursor is parked on, borrowed from the pinned
    /// leaf, with no allocation. Paired with [`ReverseRangeIter::step`] this is
    /// the PULL cursor the forward iterator already has: peek, use, step, peek.
    ///
    /// A query executor cannot live inside `for_each_ref`'s callback -- it has
    /// to interleave the walk with a heap, a work meter and a cancellation
    /// check -- and until this existed, every descending order had to
    /// materialise its whole range before it could rank it. Same leaf pin,
    /// same tree-id and cycle checks, same overflow resolution as the
    /// callback form.
    pub fn peek_ref(&mut self) -> Result<Option<(&[u8], &[u8])>> {
        self.position()?;
        self.current_ref()
    }

    /// Step past the record the last peek returned. Crossing into the leaf to
    /// the LEFT is left to the next peek, which retreats through the saved
    /// parent path when the slot index reaches the start of the leaf.
    pub fn step(&mut self) {
        if self.pending.take().is_some() {
            return;
        }
        self.idx = self.idx.saturating_sub(1);
    }

    fn position(&mut self) -> Result<()> {
        loop {
            if self.pending.is_some() || self.done {
                return Ok(());
            }
            if self.pin.is_none() {
                self.pin = Some(self.pool.get(self.page)?);
            }
            enum Step {
                Stay,
                Overflow { key: Vec<u8>, marker: Vec<u8> },
                PreviousLeaf,
                Corrupt,
            }
            let step = {
                let pin = self.pin.as_ref().unwrap();
                let p = open_cached(pin, self.page)?;
                if p.tree_id() != self.tree_id {
                    Step::Corrupt
                } else if self.idx > 0 {
                    let (key, value, is_marker) = validated_leaf(p.slot(self.idx - 1));
                    if is_marker {
                        Step::Overflow { key: key.to_vec(), marker: value.to_vec() }
                    } else {
                        Step::Stay
                    }
                } else {
                    Step::PreviousLeaf
                }
            };
            match step {
                Step::Corrupt => {
                    self.pin = None;
                    self.done = true;
                    return Err(Error::Corrupt {
                        page_no: self.page, why: "reverse-scan page belongs to another tree" });
                }
                Step::Stay => return Ok(()),
                Step::Overflow { key, marker } => {
                    // Step past it here: `step()` then only drops `pending`.
                    self.idx -= 1;
                    self.pin = None;
                    let value = read_overflow(self.pool, &marker)?;
                    self.pending = Some((key, value));
                    return Ok(());
                }
                Step::PreviousLeaf => {
                    self.pin = None;
                    if !self.retreat()? {
                        self.done = true;
                    }
                }
            }
        }
    }

    fn current_ref(&self) -> Result<Option<(&[u8], &[u8])>> {
        if let Some((key, value)) = self.pending.as_ref() {
            return Ok(Some((key.as_slice(), value.as_slice())));
        }
        if self.done {
            return Ok(None);
        }
        let Some(pin) = self.pin.as_ref() else {
            return Ok(None);
        };
        let p = open_cached(pin, self.page)?;
        if self.idx == 0 {
            return Ok(None);
        }
        let (key, value, is_marker) = validated_leaf(p.slot(self.idx - 1));
        debug_assert!(!is_marker, "peek parks overflow markers in `pending`");
        Ok(Some((key, value)))
    }

    /// Visit descending records as borrows into one pinned leaf. The cursor is
    /// bounded by the buffer pool; a caller stopping after `k` entries pays for
    /// only the pages containing those entries.
    pub fn for_each_ref(mut self, mut f: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
        self.pin = None;
        if let Some((key, value)) = self.pending.take() {
            if !f(&key, &value) { return Ok(()) }
        }
        loop {
            if self.done { return Ok(()) }
            let r = self.pool.get(self.page)?;
            let p = open_cached(&r, self.page)?;
            if p.tree_id() != self.tree_id {
                return Err(Error::Corrupt {
                    page_no: self.page, why: "reverse-scan page belongs to another tree" });
            }
            let mut overflow: Option<(Vec<u8>, Vec<u8>)> = None;
            while self.idx > 0 {
                self.idx -= 1;
                let rec = p.slot(self.idx);
                let (key, value, is_marker) = validated_leaf(rec);
                if is_marker {
                    overflow = Some((key.to_vec(), value.to_vec()));
                    break;
                }
                if !f(key, value) { return Ok(()) }
            }
            drop(r);
            if let Some((key, marker)) = overflow {
                let val = read_overflow(self.pool, &marker)?;
                if !f(&key, &val) { return Ok(()) }
                // Re-pin the same leaf at the already-decremented slot.
                continue;
            }
            if self.idx > 0 { continue }
            if !self.retreat()? { self.done = true; }
        }
    }
}

impl RangeIter<'_> {
    /// How many LEAVES this cursor has stepped through since it was opened.
    ///
    /// `advance` is the cursor's real unit of cost: it climbs the parent path
    /// until an ancestor has a child to the right and then re-descends the
    /// leftmost spine, so one step is one or more pager accesses and a long
    /// reach is many. A caller choosing between stepping this cursor forward
    /// and descending the tree afresh is choosing between LEAF STEPS and a
    /// tree height, and this is the only half of that it cannot work out for
    /// itself -- how far apart two keys are in leaves depends on how wide the
    /// records between them are, which is the caller's data and not its plan.
    pub fn leaves_stepped(&self) -> u32 {
        self.leaves
    }

    /// 2f: step to the next leaf through the parent path. Climb until an
    /// ancestor has a child to the right, step into it, then take the
    /// leftmost spine down to its first leaf. Returns false when every
    /// ancestor was exhausted -- the finished leaf was rightmost. Typical
    /// cost: one parent open and one leaf open, same as the old chain.
    fn advance(&mut self) -> Result<bool> {
        // Climb: drop exhausted levels.
        let (mut cur, mut idx_in_parent) = loop {
            let Some((page, i)) = self.path.pop() else { return Ok(false) };
            let n = {
                let r = self.pool.get(page)?;
                open_cached(&r, page)?.nentries()
            };
            // Children are child0 + one per slot: indices 0..=nentries.
            if i < n { break (page, i + 1); }
        };
        // Step right, then take the leftmost spine down to a leaf.
        loop {
            let r = self.pool.get(cur)?;
            let p = open_cached(&r, cur)?;
            if p.tree_id() != self.tree_id {
                return Err(Error::Corrupt { page_no: cur, why: "page belongs to another tree" });
            }
            if p.kind() == PageKind::Leaf {
                self.page = cur;
                self.idx = 0;
                break;
            }
            let child = child_at(self.pool, &p, idx_in_parent)?;
            self.path.push((cur, idx_in_parent));
            cur = child;
            idx_in_parent = 0; // leftmost from here down
        }
        self.leaves += 1;
        if self.leaves > self.max_leaves {
            return Err(Error::Corrupt { page_no: self.page, why: "scan visits more leaves than the file holds" });
        }
        Ok(true)
    }
}

impl RangeIter<'_> {
    /// Stop permanently, handing back the error that stopped us.
    ///
    /// Fusing on error is not tidiness. Returning `Some(Err(..))` without it
    /// leaves `page` and `idx` unchanged, so the next call retries the identical
    /// read and fails identically — forever. A consumer that logs and continues,
    /// or uses `filter_map(Result::ok)`, spins for good the first time a scan
    /// crosses an unreadable page. Fail loudly once; never fail forever.
    fn fail(&mut self, e: Error) -> Option<Result<(Vec<u8>, Vec<u8>)>> {
        self.done = true;
        Some(Err(e))
    }
}

impl RangeIter<'_> {
    /// Visit every remaining record WITHOUT allocating per entry: the callback
    /// gets the record's key and value as borrows into the pinned leaf, and
    /// iteration stops when it returns false.
    ///
    /// This exists because the allocating path costs ~34ns/key against
    /// SQLite's ~18 on covering scans -- two Vecs per row, paid even by
    /// count-only queries. One pin and one validation per leaf, zero allocs,
    /// same sibling-chain, cycle and tree-id checks as `next()`.
    pub fn for_each_ref(mut self, mut f: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
        self.pin = None;
        // Drain anything already buffered by earlier `next()` calls first.
        while let Some((k, v, is_marker)) = self.buf.pop_front() {
            if is_marker {
                let val = read_overflow(self.pool, &v)?;
                if !f(&k, &val) { return Ok(()); }
                continue;
            }
            if !f(&k, &v) { return Ok(()); }
        }
        loop {
            if self.done { return Ok(()); }
            let r = self.pool.get(self.page)?;
            let p = open_cached(&r, self.page)?;
            if p.tree_id() != self.tree_id {
                return Err(Error::Corrupt {
                    page_no: self.page, why: "sibling page belongs to another tree" });
            }
            let mut deferred: Vec<(Vec<u8>, Vec<u8>, bool)> = Vec::new();
            while self.idx < p.nentries() {
                let rec = p.slot(self.idx);
                self.idx += 1;
                let (key, value, flag) = validated_leaf(rec);
                if flag || !deferred.is_empty() {
                    // A marker cannot resolve while this leaf is pinned, and
                    // everything after it defers too so key order is kept.
                    deferred.push((key.to_vec(), value.to_vec(), flag));
                    continue;
                }
                if !f(key, value) { return Ok(()); }
            }
            drop(r);
            for (k, v, is_marker) in deferred {
                if is_marker {
                    let val = read_overflow(self.pool, &v)?;
                    if !f(&k, &val) { return Ok(()); }
                } else if !f(&k, &v) { return Ok(()); }
            }
            if !self.advance()? { return Ok(()); }
        }
    }

    /// Advance to the first remaining key >= `target` and return borrowed
    /// slices into the pinned leaf (or into a resolved overflow buffer).
    /// The iterator stays on that record so a later, still-greater target
    /// can resume without skipping it. `Ok(None)` means the range is empty
    /// or every remaining key is below `target`.
    ///
    /// Does not drain in-leaf records into `Vec`s. Overflow values still
    /// allocate, because they do not live in the pinned leaf. Reuses
    /// `advance()` and the validated-bit `open_cached` path that
    /// `for_each_ref` uses.
    pub fn peek_at_or_after(&mut self, target: &[u8]) -> Result<Option<(&[u8], &[u8])>> {
        self.position_at_or_after(target)?;
        self.current_ref()
    }

    /// The record the cursor is parked on, borrowed from the pinned leaf, with
    /// no seek and no allocation. Paired with [`RangeIter::step`] this makes a
    /// PULL cursor -- peek, use, step, peek -- that costs what `for_each_ref`
    /// costs while still letting the caller stop and resume. A query executor
    /// needs exactly that: it has to interleave the walk with a heap, a work
    /// meter and a cancellation check, none of which fit inside a callback.
    ///
    /// The empty target is below every key, so this is `peek_at_or_after`
    /// standing still: same leaf pin, same overflow-marker resolution.
    pub fn peek_ref(&mut self) -> Result<Option<(&[u8], &[u8])>> {
        self.position_at_or_after(&[])?;
        self.current_ref()
    }

    /// Step past the record the last peek returned, without materialising it.
    /// Crossing a leaf boundary is left to the next peek, which already climbs
    /// the parent path when the slot index runs past the end of the leaf.
    pub fn step(&mut self) {
        if self.buf.pop_front().is_some() {
            return;
        }
        self.idx += 1;
    }

    fn position_at_or_after(&mut self, target: &[u8]) -> Result<()> {
        loop {
            let skip_buf = matches!(self.buf.front(), Some((k, _, _)) if k.as_slice() < target);
            if skip_buf {
                self.buf.pop_front();
                continue;
            }
            break;
        }
        if matches!(self.buf.front(), Some((k, _, _)) if k.as_slice() >= target) {
            if let Some((_, v, true)) = self.buf.front() {
                let marker = v.clone();
                let (k, _, _) = self.buf.pop_front().unwrap();
                let val = read_overflow(self.pool, &marker)?;
                self.buf.push_front((k, val, false));
            }
            return Ok(());
        }

        loop {
            if self.done {
                self.pin = None;
                return Ok(());
            }
            if self.pin.is_none() {
                self.pin = Some(self.pool.get(self.page)?);
            }

            enum Step {
                Stay(usize),
                Overflow { idx: usize, key: Vec<u8>, marker: Vec<u8> },
                NextLeaf,
                Corrupt,
            }
            let step = {
                let pin = self.pin.as_ref().unwrap();
                let p = open_cached(pin, self.page)?;
                if p.tree_id() != self.tree_id {
                    Step::Corrupt
                } else {
                    let n = p.nentries();
                    let mut idx = self.idx;
                    if idx < n {
                        let (key0, _, _) = validated_leaf(p.slot(idx));
                        if key0 < target {
                            idx = lower_bound(&p, target)?.max(idx);
                        }
                    }
                    if idx < n {
                        let (key, value, is_marker) = validated_leaf(p.slot(idx));
                        if is_marker {
                            Step::Overflow { idx, key: key.to_vec(), marker: value.to_vec() }
                        } else {
                            Step::Stay(idx)
                        }
                    } else {
                        Step::NextLeaf
                    }
                }
            };
            match step {
                Step::Corrupt => {
                    self.pin = None;
                    self.done = true;
                    return Err(Error::Corrupt {
                        page_no: self.page,
                        why: "sibling page belongs to another tree",
                    });
                }
                Step::Stay(idx) => {
                    self.idx = idx;
                    return Ok(());
                }
                Step::Overflow { idx, key, marker } => {
                    self.idx = idx + 1;
                    self.pin = None;
                    let val = read_overflow(self.pool, &marker)?;
                    self.buf.push_front((key, val, false));
                    return Ok(());
                }
                Step::NextLeaf => {
                    self.pin = None;
                    if !self.advance()? {
                        self.done = true;
                    }
                }
            }
        }
    }

    fn current_ref(&self) -> Result<Option<(&[u8], &[u8])>> {
        if let Some((k, v, is_marker)) = self.buf.front() {
            debug_assert!(!*is_marker, "peek resolves overflow markers before yielding");
            return Ok(Some((k.as_slice(), v.as_slice())));
        }
        if self.done {
            return Ok(None);
        }
        let Some(pin) = self.pin.as_ref() else {
            return Ok(None);
        };
        let p = open_cached(pin, self.page)?;
        if self.idx >= p.nentries() {
            return Ok(None);
        }
        let (key, value, is_marker) = validated_leaf(p.slot(self.idx));
        debug_assert!(!is_marker, "in-leaf overflow must have been parked in buf");
        Ok(Some((key, value)))
    }
}

impl Iterator for RangeIter<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;
    fn next(&mut self) -> Option<Self::Item> {
        self.pin = None;
        loop {
            if let Some((k, v, is_marker)) = self.buf.pop_front() {
                if is_marker {
                    let val = match read_overflow(self.pool, &v) {
                        Ok(val) => val, Err(e) => return self.fail(e),
                    };
                    return Some(Ok((k, val)));
                }
                return Some(Ok((k, v)));
            }
            if self.done { return None; }
            let r = match self.pool.get(self.page) { Ok(r) => r, Err(e) => return self.fail(e) };
            let p = match open_cached(&r, self.page) { Ok(p) => p, Err(e) => return self.fail(e) };
            // `descend` checks this for the FIRST leaf only. Every later leaf is
            // reached by following a sibling pointer, and a well-formed page from
            // another tree passes its checksum perfectly well — five trees share
            // this file. Without this the scan decodes another tree's rows and
            // returns them as ours.
            if p.tree_id() != self.tree_id {
                let e = Error::Corrupt {
                    page_no: self.page, why: "sibling page belongs to another tree" };
                return self.fail(e);
            }
            const SHORT_SCAN: u32 = 8;
            if self.served < SHORT_SCAN {
                if self.idx < p.nentries() {
                    let rec = p.slot(self.idx);
                    self.idx += 1;
                    self.served += 1;
                    let (key, value, is_marker) = validated_leaf(rec);
                    let kv = (key.to_vec(), value.to_vec());
                    if is_marker {
                        drop(r);
                        let v = match read_overflow(self.pool, &kv.1) {
                            Ok(v) => v, Err(e) => return self.fail(e),
                        };
                        return Some(Ok((kv.0, v)));
                    }
                    return Some(Ok(kv));
                }
            } else {
                while self.idx < p.nentries() {
                    let rec = p.slot(self.idx);
                    self.idx += 1;
                    // markers buffered as markers (explicit flag -- an in-band
                    // tag would collide with real values of the same shape),
                    // resolved on pop: the chain walk must not run while this
                    // leaf is pinned
                    let (key, value, flag) = validated_leaf(rec);
                    self.buf.push_back((key.to_vec(), value.to_vec(), flag));
                }
            }
            drop(r);   // released BEFORE re-descending: one leaf pinned, ever
            // done is a STATE, not an exit: the buffer may hold this final
            // leaf's records, and returning here dropped them -- 163 keys of a
            // 20,000-key scan, caught by the ordering test within seconds of
            // the batching change.
            match self.advance() {
                Ok(true) => {}
                Ok(false) => { self.done = true; }
                Err(e) => return self.fail(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_pool;

    #[test]
    fn a_key_written_is_a_key_found() {
        let (pool, _d) = scratch_pool(64);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        t.insert(b"alpha", b"1").unwrap();
        t.insert(b"beta", b"2").unwrap();
        assert_eq!(t.get(b"alpha").unwrap().as_deref(), Some(&b"1"[..]));
        assert_eq!(t.get(b"beta").unwrap().as_deref(), Some(&b"2"[..]));
        assert_eq!(t.get(b"gamma").unwrap(), None);
    }

    #[test]
    fn a_later_write_replaces_an_earlier_one() {
        let (pool, _d) = scratch_pool(64);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        t.insert(b"k", b"first").unwrap();
        t.insert(b"k", b"second").unwrap();
        assert_eq!(t.get(b"k").unwrap().as_deref(), Some(&b"second"[..]));
    }

    /// Splitting is the whole point: more keys than one page can hold, in an
    /// order that guarantees splits, with a pool far smaller than the tree.
    #[test]
    fn fifty_thousand_scattered_keys_are_all_findable() {
        let (pool, _d) = scratch_pool(16);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        let n = 50_000u64;
        // A multiplicative hash scatters insert order without needing rand.
        let scatter = |i: u64| i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        for i in 0..n { t.insert(&scatter(i).to_be_bytes(), &i.to_le_bytes()).unwrap(); }
        for i in 0..n {
            let got = t.get(&scatter(i).to_be_bytes()).unwrap();
            assert_eq!(got.as_deref(), Some(&i.to_le_bytes()[..]), "key {i} missing");
        }
    }

    #[test]
    fn a_value_too_large_for_a_page_is_refused_not_panicked() {
        let (pool, _d) = scratch_pool(16);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        // An oversized VALUE now spills to an overflow chain and round-trips
        // exactly -- the refusal this test used to assert became a feature.
        let big = vec![7u8; 5000];
        t.insert(b"k", &big).unwrap();
        assert_eq!(t.get(b"k").unwrap().as_deref(), Some(big.as_slice()));
        // What must STILL be refused is a key that cannot fit a leaf record
        // even as a marker: keys never spill.
        let huge_key = vec![b'k'; crate::page::MAX_RECORD_LEN];
        assert!(matches!(t.insert(&huge_key, b"v"), Err(crate::Error::TooLarge)));
    }

    /// The exact path that used to corrupt a leaf: fill one leaf as tightly
    /// as this record shape allows without splitting, then replace one
    /// entry's value with a longer one whose extra bytes only fit once the
    /// old entry's payload is actually reclaimed (not just its slot). With
    /// the old `exists || room` shape this replace would remove the entry,
    /// fail to fit the new one, and never finalise — leaving the whole leaf
    /// unreadable. With the fix, this is an ordinary in-place growth.
    #[test]
    fn replacing_a_value_with_a_longer_one_on_a_full_leaf_keeps_every_key() {
        let (pool, _d) = scratch_pool(8);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();

        let capacity = crate::page::PAGE_SIZE - crate::page::HEADER_LEN;
        let key_len = 4usize;
        let small_val_len = 8usize;
        // Page-budget cost of one record: 4 (slot directory) + 2 (klen) +
        // key + 2 (vlen) + value.
        let small_cost = 8 + key_len + small_val_len;
        let n = capacity / small_cost;

        for i in 0..n as u32 {
            t.insert(&i.to_be_bytes(), &vec![0xABu8; small_val_len]).unwrap();
        }
        let root_before = t.root();

        // Grow the replacement so it exactly consumes what's left after the
        // other (n - 1) entries plus the reclaimed cost of the one being
        // replaced — the boundary the bug lived on.
        let free_after_fill = capacity - n * small_cost;
        let big_val_len = free_after_fill + small_cost - 8 - key_len;
        let big_val = vec![0xCDu8; big_val_len];
        t.insert(&0u32.to_be_bytes(), &big_val).unwrap();

        assert_eq!(t.root(), root_before, "an in-place replace must not split the leaf");
        assert_eq!(t.get(&0u32.to_be_bytes()).unwrap().as_deref(), Some(&big_val[..]));
        for i in 1..n as u32 {
            assert_eq!(
                t.get(&i.to_be_bytes()).unwrap().as_deref(),
                Some(&vec![0xABu8; small_val_len][..]),
                "key {i} missing after replacing an unrelated key"
            );
        }
    }

    /// 200 replacements of ONE key must never split its leaf. If `compact`
    /// were decorative (or absent), each replace would abandon its old
    /// payload without reclaiming it, `free_ptr` would march toward zero
    /// every time regardless of how much is actually live, and the leaf
    /// would exhaust its free space and split long before 200 iterations —
    /// even though at most one entry is ever live at once. A stable root is
    /// what proves compaction is actually reclaiming space, not just
    /// present in the source.
    #[test]
    fn repeated_replacement_of_one_key_does_not_exhaust_its_leaf() {
        let (pool, _d) = scratch_pool(8);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        t.insert(b"k", &[0u8; 20]).unwrap();
        let root_before = t.root();

        for i in 0..200u32 {
            let val = vec![i as u8; 20];
            t.insert(b"k", &val).unwrap();
        }

        assert_eq!(t.root(), root_before, "200 replacements of one key must not split its leaf");
        assert_eq!(t.get(b"k").unwrap().as_deref(), Some(&vec![199u8; 20][..]));
    }

    /// The pathological shape the byte-crossing heuristic got wrong: many small
    /// records plus one very large one landing near the cut. Cutting at the first
    /// prefix to cross half the bytes puts more than a page on the left.
    ///
    /// The big value's size (2500, not the 3000 originally specified) was
    /// picked by checking the arithmetic directly rather than by guessing: with
    /// 21 preceding 76-byte-cost small entries (1596 bytes) and 19 following
    /// (1444 bytes), a 3000-byte value costs 3016 in page-budget terms, and
    /// 1596 + 3016 = 4612 exceeds the 4056-byte usable page on EVERY possible
    /// cut, before or after the big record — no two-way split exists at all for
    /// that size, so `split_point` correctly returns `None` and the insert is
    /// correctly refused, which is not what this test is trying to demonstrate.
    /// 2500 costs 2516, low enough that a valid cut exists right before the big
    /// record (1596 / 3960, both under 4056), while still being large enough
    /// that the OLD "first prefix to cross half the bytes" heuristic picks the
    /// cut AFTER the big record instead (1596 + 2516 = 4112, over capacity) —
    /// so this size is still squarely in the region the fix is for.
    #[test]
    fn a_split_with_one_huge_record_among_many_small_ones_keeps_every_key() {
        let (pool, _d) = scratch_pool(32);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        let mut keys: Vec<[u8; 8]> = Vec::new();
        for i in 0..40u64 {
            let k = (i * 2).to_be_bytes();
            t.insert(&k, &[b'a'; 60]).unwrap();
            keys.push(k);
        }
        // A large value keyed to land in the middle of the run. The size is
        // derived, not chosen: see the doc comment above.
        let big = 41u64.to_be_bytes();
        t.insert(&big, &vec![b'Z'; 2500]).unwrap();
        keys.push(big);

        for k in &keys[..40] {
            let got = t.get(k).unwrap();
            assert!(got.is_some(), "key {k:?} lost across the split");
            // Length AND content: outright loss is not the only way a split can
            // damage a neighbour.
            assert_eq!(got.unwrap(), vec![b'a'; 60], "key {k:?} was corrupted by the split");
        }
        assert_eq!(t.get(&big).unwrap().unwrap(), vec![b'Z'; 2500]);
    }

    #[test]
    fn a_range_scan_returns_keys_in_order_across_leaf_boundaries() {
        let (pool, _d) = scratch_pool(8);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        let n = 20_000u64;
        let scatter = |i: u64| i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        for i in 0..n { t.insert(&scatter(i).to_be_bytes(), b"v").unwrap(); }

        let mut expect: Vec<[u8; 8]> = (0..n).map(|i| scatter(i).to_be_bytes()).collect();
        expect.sort();

        let got: Vec<Vec<u8>> = t.range(&[]).unwrap()
            .map(|r| r.unwrap().0).collect();
        assert_eq!(got.len(), expect.len());
        assert!(got.iter().zip(&expect).all(|(a, b)| a.as_slice() == b.as_slice()),
                "scan order must equal sorted order");
    }

    #[test]
    fn a_range_scan_from_a_midpoint_starts_there() {
        let (pool, _d) = scratch_pool(8);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        for i in 0..1000u64 { t.insert(&i.to_be_bytes(), b"v").unwrap(); }
        let first = t.range(&500u64.to_be_bytes()).unwrap().next().unwrap().unwrap().0;
        assert_eq!(first, 500u64.to_be_bytes().to_vec());
    }

    /// Law 1 for the scan path: peak memory is one leaf, whatever the result size.
    ///
    /// Assert on `peak_pins`, NOT on `frames_total`. `frames_total` is the pool's
    /// fixed capacity, set once at construction and never mutated, so an
    /// assertion that it did not change is true before the scan, true after it,
    /// and would still be true if the iterator pinned every leaf at once. It
    /// cannot fail. `peak_pins` is a real measurement.
    #[test]
    fn a_full_scan_pins_one_leaf_at_a_time() {
        let (pool, _d) = scratch_pool(8);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        for i in 0..50_000u64 { t.insert(&i.to_be_bytes(), b"v").unwrap(); }

        pool.reset_peak_pins();
        let before = pool.stats();
        let n = t.range(&[]).unwrap().count();
        let after = pool.stats();

        assert_eq!(n, 50_000);
        assert_eq!(after.peak_pins, 1, "a scan must hold exactly one leaf at a time");
        assert!(after.evictions > before.evictions, "50k keys over 8 frames must evict");
    }

    #[test]
    fn a_scan_starting_past_the_last_key_yields_nothing() {
        let (pool, _d) = scratch_pool(8);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        for i in 0..1000u64 { t.insert(&i.to_be_bytes(), b"v").unwrap(); }
        assert_eq!(t.range(&u64::MAX.to_be_bytes()).unwrap().count(), 0);
    }

    #[test]
    fn a_scan_of_an_empty_tree_yields_nothing() {
        let (pool, _d) = scratch_pool(8);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        assert_eq!(t.range(&[]).unwrap().count(), 0);
        assert_eq!(t.range(&42u64.to_be_bytes()).unwrap().count(), 0);
    }

    fn peek_owned(iter: &mut RangeIter<'_>, target: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
        iter.peek_at_or_after(target)
            .unwrap()
            .map(|(k, v)| (k.to_vec(), v.to_vec()))
    }

    fn collect_next(t: &BTree<'_>) -> Vec<(Vec<u8>, Vec<u8>)> {
        t.range(&[]).unwrap().collect::<Result<Vec<_>>>().unwrap()
    }

    /// `peek_at_or_after` on a sequence of ascending targets must return the
    /// same key/value as the first allocating `next()` record with key >= target.
    #[test]
    fn peek_at_or_after_matches_allocating_iterator_on_random_trees() {
        let (pool, _d) = scratch_pool(32);
        let last_leaf = Cell::new(None);
        let hits = Cell::new(0);
        let tries = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &hits, &tries).unwrap();
        let n = 800u64;
        let scatter = |i: u64| i.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes().to_vec();
        for i in 0..n {
            t.insert(&scatter(i), &i.to_le_bytes()).unwrap();
        }
        let all = collect_next(&t);
        assert_eq!(all.len(), n as usize);

        let mut targets: Vec<Vec<u8>> = Vec::new();
        targets.push(Vec::new());
        for (i, (k, _)) in all.iter().enumerate() {
            targets.push(k.clone());
            if i + 1 < all.len() {
                let mut mid = k.clone();
                if let Some(last) = mid.last_mut() {
                    *last = last.saturating_add(1);
                }
                if mid.as_slice() < all[i + 1].0.as_slice() {
                    targets.push(mid);
                }
            }
        }
        targets.push(vec![0xff; 16]);
        targets.sort();
        targets.dedup();

        let mut peek = t.range(&[]).unwrap();
        for target in &targets {
            let expected = all
                .iter()
                .find(|(k, _)| k.as_slice() >= target.as_slice())
                .cloned();
            assert_eq!(peek_owned(&mut peek, target), expected, "target {target:?}");
        }
    }

    #[test]
    fn peek_at_or_after_crosses_leaf_boundaries() {
        let (pool, _d) = scratch_pool(16);
        let last_leaf = Cell::new(None);
        let hits = Cell::new(0);
        let tries = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &hits, &tries).unwrap();
        let n = 2_000u64;
        for i in 0..n {
            t.insert(&i.to_be_bytes(), &i.to_le_bytes()).unwrap();
        }
        let all = collect_next(&t);
        assert_eq!(all.len(), n as usize);

        let mut peek = t.range(&[]).unwrap();
        for (k, v) in &all {
            assert_eq!(peek_owned(&mut peek, k).as_ref(), Some(&(k.clone(), v.clone())));
        }
        // Jumping onto a key that is not first in its leaf, then walking to the end.
        let mid = &all[all.len() / 2].0;
        let mut peek = t.range(&[]).unwrap();
        let got = peek_owned(&mut peek, mid);
        assert_eq!(got.as_ref(), Some(&all[all.len() / 2]));
        let last = &all[all.len() - 1].0;
        assert_eq!(peek_owned(&mut peek, last).as_ref(), Some(&all[all.len() - 1]));
    }

    #[test]
    fn peek_at_or_after_target_beyond_the_end_is_none() {
        let (pool, _d) = scratch_pool(8);
        let last_leaf = Cell::new(None);
        let hits = Cell::new(0);
        let tries = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &hits, &tries).unwrap();
        for i in 0..200u64 {
            t.insert(&i.to_be_bytes(), b"v").unwrap();
        }
        let mut peek = t.range(&[]).unwrap();
        assert!(peek_owned(&mut peek, &u64::MAX.to_be_bytes()).is_none());
        assert!(peek_owned(&mut peek, &u64::MAX.to_be_bytes()).is_none());
        let mut peek = t.range(&500u64.to_be_bytes()).unwrap();
        assert!(peek_owned(&mut peek, &u64::MAX.to_be_bytes()).is_none());
    }

    #[test]
    fn peek_at_or_after_on_an_empty_range_is_none() {
        let (pool, _d) = scratch_pool(8);
        let last_leaf = Cell::new(None);
        let hits = Cell::new(0);
        let tries = Cell::new(0);
        let t = BTree::create(&pool, 1, &last_leaf, &hits, &tries).unwrap();
        let mut peek = t.range(&[]).unwrap();
        assert!(peek_owned(&mut peek, b"").is_none());
        assert!(peek_owned(&mut peek, b"z").is_none());
        let mut peek = t.range(b"mid").unwrap();
        assert!(peek_owned(&mut peek, b"mid").is_none());
    }

    /// Sequential peeks of every key must pin each leaf once, not once per row.
    #[test]
    fn peek_at_or_after_pool_gets_are_per_leaf_not_per_row() {
        let (pool, _d) = scratch_pool(32);
        let last_leaf = Cell::new(None);
        let hits = Cell::new(0);
        let tries = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &hits, &tries).unwrap();
        let n = 3_000u64;
        for i in 0..n {
            t.insert(&i.to_be_bytes(), &i.to_le_bytes()).unwrap();
        }
        let keys: Vec<_> = collect_next(&t).into_iter().map(|(k, _)| k).collect();
        let before = pool.stats();
        let mut peek = t.range(&[]).unwrap();
        for k in &keys {
            assert!(peek_owned(&mut peek, k).is_some());
        }
        let after = pool.stats();
        let gets = (after.hits + after.misses) - (before.hits + before.misses);
        println!("peek_at_or_after over {n} rows: {gets} pool gets");
        assert!(
            gets < n / 8,
            "peek over {n} rows charged {gets} pool gets; expected O(leaves)"
        );
    }

    #[test]
    fn peek_at_or_after_does_not_allocate_per_row() {
        let (pool, _d) = scratch_pool(32);
        let last_leaf = Cell::new(None);
        let hits = Cell::new(0);
        let tries = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &hits, &tries).unwrap();
        let n = 2_000u64;
        for i in 0..n {
            t.insert(&i.to_be_bytes(), &i.to_le_bytes()).unwrap();
        }
        let (_, allocs, bytes) = crate::test_alloc::measured(|| {
            let mut peek = t.range(&[]).unwrap();
            for i in 0..n {
                let key = i.to_be_bytes();
                let hit = peek.peek_at_or_after(&key).unwrap();
                let (k, v) = hit.expect("key present");
                assert_eq!(k, key);
                assert_eq!(v, i.to_le_bytes());
            }
        });
        println!("peek_at_or_after over {n} rows: {allocs} allocations, {bytes} bytes");
        assert!(
            allocs < 32,
            "peek over {n} in-leaf rows allocated {allocs} times; expected O(1)"
        );
    }

    #[test]
    fn deleted_keys_are_gone_and_their_neighbours_are_not() {
        let (pool, _d) = scratch_pool(8);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        let n = 10_000u64;
        for i in 0..n { t.insert(&i.to_be_bytes(), &i.to_le_bytes()).unwrap(); }
        for i in (0..n).step_by(2) { assert!(t.delete(&i.to_be_bytes()).unwrap()); }

        for i in 0..n {
            let got = t.get(&i.to_be_bytes()).unwrap();
            if i % 2 == 0 { assert!(got.is_none(), "{i} should be gone"); }
            else { assert_eq!(got.as_deref(), Some(&i.to_le_bytes()[..]), "{i} was collateral"); }
        }
        // And the scan agrees with the point lookups.
        let scanned = t.range(&[]).unwrap().count();
        assert_eq!(scanned as u64, n / 2);
    }

    #[test]
    fn deleting_a_key_that_is_not_there_reports_so() {
        let (pool, _d) = scratch_pool(8);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        t.insert(b"a", b"1").unwrap();
        assert!(!t.delete(b"zzz").unwrap());
    }

    /// Deletion must leave the sibling chain walkable. A leaf emptied by
    /// deletion is not unlinked — it stays in the chain with zero entries — so
    /// the scan has to pass through it rather than stopping there.
    #[test]
    fn a_scan_still_walks_leaves_that_deletion_emptied() {
        let (pool, _d) = scratch_pool(16);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        for i in 0..5_000u64 { t.insert(&i.to_be_bytes(), b"v").unwrap(); }

        // Delete a long contiguous run, which should empty at least one leaf
        // outright while leaving keys on both sides of it.
        for i in 1_000..3_000u64 { assert!(t.delete(&i.to_be_bytes()).unwrap()); }

        let seen: Vec<u64> = t.range(&[]).unwrap()
            .map(|r| u64::from_be_bytes(r.unwrap().0.try_into().unwrap()))
            .collect();
        assert_eq!(seen.len(), 3_000, "keys on the far side of an emptied leaf must survive");
        assert_eq!(seen.first().copied(), Some(0));
        assert_eq!(seen.last().copied(), Some(4_999));
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "scan order must still be sorted");
    }

    /// The cycle guard, actually exercised. Without it this scan never returns.
    ///
    /// The guard is the one path in the scan with no other coverage: the two
    /// boundary tests are about where a scan starts, not about how it refuses to
    /// run forever. An unbounded loop on a damaged sibling pointer is how the
    /// predecessor engine died, so the guard needs a test that would notice its
    /// removal.
    #[test]
    fn a_cyclic_sibling_chain_is_inert_because_scans_descend() {
        let (pool, _d) = scratch_pool(16);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        for i in 0..5000u64 { t.insert(&i.to_be_bytes(), b"v").unwrap(); }

        // The leftmost leaf, and the one it points at.
        let first = { let (leaf, _) = t.descend(&[]).unwrap(); leaf };
        let second = {
            let r = pool.get(first).unwrap();
            PageRef::open_resident(&r, first).unwrap().next_leaf()
        };
        assert_ne!(second, 0, "this fixture needs at least two leaves");

        // Point the second leaf back at the first: A -> B -> A.
        {
            let mut w = pool.get_mut(second).unwrap();
            let mut p = PageMut::reopen(w.bytes_mut());
            p.set_next_leaf(first);
            p.finalise(0);
        }

        // 2f: scans advance by separator re-descent, never by the sibling
        // pointer, so the planted cycle must be INERT -- the scan completes,
        // terminates, and serves every key exactly once. (Before 2f this
        // fixture asserted the cycle was detected and refused; now the walk
        // that could meet it no longer exists. The leaves-vs-file-size guard
        // in `advance` still bounds a corrupt-interior descent loop.)
        let mut n = 0u64;
        for item in t.range(&[]).unwrap() {
            let (k, _) = item.expect("cycle in the DEAD sibling chain must not affect the scan");
            assert_eq!(k, n.to_be_bytes().to_vec(), "keys in order, exactly once");
            n += 1;
        }
        assert_eq!(n, 5000, "every key served exactly once despite the cycle");
    }

    // -- Task 15: one write guard per insert, and the append fast path --

    /// Ascending keys never require a split within this run (100 tiny
    /// records fit easily in one 4056-byte-usable leaf), so only the very
    /// first insert -- before `last_leaf` is ever set -- pays for a
    /// descent. Every insert after it satisfies all five fast-path checks:
    /// same leaf, same tree, rightmost (no split ever occurs), room, and a
    /// strictly increasing key. Asserting the exact count (not `> 0`) is
    /// what would catch a fast path that only fires sometimes.
    #[test]
    fn insert_ascending_uses_fast_path() {
        let (pool, _d) = scratch_pool(4);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        let n = 100u64;
        for i in 0..n { t.insert(&i.to_be_bytes(), b"v").unwrap(); }
        assert_eq!(
            t.fast_path_hits.get(), n - 1,
            "every insert but the first must hit the append fast path"
        );
    }

    /// Descending keys can never satisfy "strictly greater than the last
    /// key on the page" -- each new key is smaller than everything already
    /// there -- so the fast path must never fire, not even once after the
    /// first insert sets the hint.
    #[test]
    fn insert_descending_never_uses_fast_path() {
        let (pool, _d) = scratch_pool(4);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();
        let n = 100u64;
        for i in (0..n).rev() { t.insert(&i.to_be_bytes(), b"v").unwrap(); }
        assert_eq!(
            t.fast_path_hits.get(), 0,
            "a descending insert order must never satisfy the strictly-greater check"
        );
    }

    /// A hint left pointing at another tree's leaf must be refused by the
    /// tree_id check, not used -- five trees can share one file, and a page
    /// from another tree parses as a perfectly valid page. Build two trees
    /// in one store, force tree 1's hint onto tree 2's leaf, and confirm
    /// both trees come out uncorrupted.
    #[test]
    fn fast_path_rejects_wrong_tree() {
        let (pool, _d) = scratch_pool(8);
        let last_leaf1 = Cell::new(None);
        let fast_path_hits1 = Cell::new(0);
        let fast_path_attempts1 = Cell::new(0);
        let last_leaf2 = Cell::new(None);
        let fast_path_hits2 = Cell::new(0);
        let fast_path_attempts2 = Cell::new(0);
        let mut t1 = BTree::create(&pool, 1, &last_leaf1, &fast_path_hits1, &fast_path_attempts1).unwrap();
        let mut t2 = BTree::create(&pool, 2, &last_leaf2, &fast_path_hits2, &fast_path_attempts2).unwrap();
        t1.insert(b"a", b"1").unwrap();
        t2.insert(b"z", b"2").unwrap();

        // Force t1's hint onto t2's leaf, as a stale/corrupted hint would.
        let t2_leaf = t2.root();
        t1.last_leaf.set(Some(t2_leaf));

        // "zz" sorts after "z", t2's only (and last) key -- so the
        // ordering check alone would let this through. Only the tree_id
        // check stops it from landing on t2's leaf.
        t1.insert(b"zz", b"3").unwrap();

        assert_eq!(
            t1.fast_path_hits.get(), 0,
            "a hint pointing into another tree must fall back, not be used"
        );
        assert_eq!(t1.get(b"a").unwrap().as_deref(), Some(&b"1"[..]));
        assert_eq!(t1.get(b"zz").unwrap().as_deref(), Some(&b"3"[..]));
        assert_eq!(
            t2.get(b"z").unwrap().as_deref(), Some(&b"2"[..]),
            "t2 must be untouched by t1's misdirected hint"
        );
        assert_eq!(
            t2.range(&[]).unwrap().count(), 1,
            "t2 must not have gained a record that belonged to t1"
        );
    }

    /// After a split, the leaf that used to be rightmost has a right
    /// sibling and must never be used as an append target again, even for
    /// a key that legitimately belongs on it. Force the hint back onto that
    /// now-interior-ish (non-rightmost) leaf and confirm the insert still
    /// lands correctly, via fallback, with nothing else disturbed.
    #[test]
    fn fast_path_rejects_non_rightmost() {
        let (pool, _d) = scratch_pool(8);
        let last_leaf = Cell::new(None);
        let fast_path_hits = Cell::new(0);
        let fast_path_attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &fast_path_hits, &fast_path_attempts).unwrap();

        // Even keys only, so odd keys are legitimate "new, in-range" keys
        // with no split-order ambiguity. Enough of them (400, at ~17 bytes
        // of page-budget cost each) to force at least one split of the
        // single starting leaf.
        let n = 400u64;
        for i in 0..n { t.insert(&(i * 2).to_be_bytes(), b"v").unwrap(); }

        let (left_leaf, _) = t.descend(&0u64.to_be_bytes()).unwrap();
        let last_in_left = {
            let r = pool.get(left_leaf).unwrap();
            let p = PageRef::open_resident(&r, left_leaf).unwrap();
            assert_ne!(p.next_leaf(), 0, "fixture needs the left leaf to have a right sibling");
            u64::from_be_bytes(
                validated_key(p.slot(p.nentries() - 1))
                    .try_into()
                    .unwrap(),
            )
        };

        t.last_leaf.set(Some(left_leaf));
        let hits_before = t.fast_path_hits.get();

        // The odd key immediately after the left leaf's own last key: this
        // is the one candidate that is BOTH strictly greater than the last
        // key on the (stale) hinted leaf -- satisfying the ordering check
        // on its own -- AND still genuinely in-range for that leaf (it
        // sorts below the separator, since the next even key belongs to the
        // right leaf). Only the rightmost check can still catch it.
        let candidate = last_in_left + 1;
        t.insert(&candidate.to_be_bytes(), b"new").unwrap();

        assert_eq!(
            t.fast_path_hits.get(), hits_before,
            "a non-rightmost hint must never be used, even for a key that is both \
             in-range and greater than the hinted leaf's last key"
        );
        assert_eq!(
            t.get(&candidate.to_be_bytes()).unwrap().as_deref(), Some(&b"new"[..]),
            "the insert must still land correctly via fallback"
        );
        for i in 0..n {
            assert_eq!(
                t.get(&(i * 2).to_be_bytes()).unwrap().as_deref(), Some(&b"v"[..]),
                "key {i} corrupted by the stale hint"
            );
        }
    }

    /// The correctness net for both changes at once: the same 20k keys,
    /// inserted once in ascending order (exercising the fast path
    /// constantly) and once in a scattered order (exercising
    /// `descend_for_write` and splits constantly, since a scattered key is
    /// essentially never greater than the rightmost leaf's last key), must
    /// produce byte-identical range scans.
    #[test]
    fn random_order_matches_sequential() {
        let n = 20_000u64;
        let scatter = |i: u64| i.wrapping_mul(0x9E37_79B9_7F4A_7C15);

        let (pool_a, _da) = scratch_pool(16);
        let last_leaf_a = Cell::new(None);
        let fast_path_hits_a = Cell::new(0);
        let fast_path_attempts_a = Cell::new(0);
        let mut a = BTree::create(&pool_a, 1, &last_leaf_a, &fast_path_hits_a, &fast_path_attempts_a).unwrap();
        for i in 0..n { a.insert(&i.to_be_bytes(), &i.to_le_bytes()).unwrap(); }

        let (pool_b, _db) = scratch_pool(16);
        let last_leaf_b = Cell::new(None);
        let fast_path_hits_b = Cell::new(0);
        let fast_path_attempts_b = Cell::new(0);
        let mut b = BTree::create(&pool_b, 1, &last_leaf_b, &fast_path_hits_b, &fast_path_attempts_b).unwrap();
        let mut order: Vec<u64> = (0..n).collect();
        order.sort_by_key(|&i| scatter(i));
        for &i in &order { b.insert(&i.to_be_bytes(), &i.to_le_bytes()).unwrap(); }

        let seq_a: Vec<(Vec<u8>, Vec<u8>)> = a.range(&[]).unwrap().map(|r| r.unwrap()).collect();
        let seq_b: Vec<(Vec<u8>, Vec<u8>)> = b.range(&[]).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(seq_a.len(), n as usize);
        assert_eq!(
            seq_a, seq_b,
            "ascending vs scattered insertion order must produce identical range scans"
        );
    }

    // ---------------------------------------------------------- D9 per-keyspace append
    //
    // One tree holds several key TAGS (D3/D4: vectors, rows and external-key
    // mappings are keyspaces, not files). `src/collections.rs` writes one
    // document as three keys -- 0x60 vector, 0x40 row, 0x20 mapping -- and each
    // of those three runs is ASCENDING in itself. Only the highest tag is ever
    // the TREE's rightmost leaf, so D9's `next_leaf() == 0` guard fires for
    // 0x60 and never for 0x40 or 0x20.
    //
    // What that costs is WORK, not space: `redistribute_neighbors` keeps the
    // lower two runs densely packed (measured below), and pays for it with a
    // clone of the parent's records plus up to three siblings' records and up
    // to four rebuilt page images on every leaf fill, forever.

    /// `src/collections.rs`'s width-tagged big-endian integer component.
    fn ordered(n: u64) -> Vec<u8> {
        let b = n.to_be_bytes();
        let start = b.iter().position(|x| *x != 0).unwrap_or(7);
        let mut k = vec![0x80 + (8 - start) as u8];
        k.extend_from_slice(&b[start..]);
        k
    }

    /// One document's key for `tag`, ascending in `i`. Shaped exactly like
    /// `src/collections.rs`: `[tag][ordered(collection)][...]`.
    fn tagged(tag: u8, i: u64) -> Vec<u8> {
        let mut k = vec![tag];
        k.extend(ordered(1));
        match tag {
            // mapping: the caller's external key, a string
            0x20 => k.extend_from_slice(format!("key-{i:012}").as_bytes()),
            // row: the entity sequence
            0x40 => k.extend(ordered(i + 1)),
            // vector: the row key plus a field ordinal
            _ => {
                k.extend(ordered(i + 1));
                k.extend(ordered(0));
            }
        }
        k
    }

    /// The three keyspaces store very differently sized values, and that is the
    /// point: a mapping row is a few bytes, a document row a few hundred.
    fn tagged_value(tag: u8, i: u64) -> Vec<u8> {
        match tag {
            0x20 => ordered(i + 1),
            // A document, varying in length the way real documents do.
            0x40 => vec![b'd'; 200 + (i % 97) as usize * 2],
            // Eight f32 lanes.
            _ => vec![b'f'; 32],
        }
    }

    /// Insert `docs` documents, each as one key per tag in `tags`, in that
    /// order -- `src/collections.rs` writes vector, then row, then mapping.
    fn load_interleaved(t: &mut BTree<'_>, tags: &[u8], docs: u64) {
        for i in 0..docs {
            for tag in tags {
                t.insert(&tagged(*tag, i), &tagged_value(*tag, i)).unwrap();
            }
        }
    }

    /// Every leaf of the tree, as (tag of its first key, payload bytes used,
    /// whether the leaf mixes tags). Walks THROUGH THE PARENTS rather than the
    /// sibling chain, which is the only walk this file trusts.
    fn leaves(pool: &BufferPool, page_no: u32, out: &mut Vec<(u8, usize, bool)>) {
        let kids = {
            let r = pool.get(page_no).unwrap();
            let p = PageRef::open_resident(&r, page_no).unwrap();
            match p.kind() {
                PageKind::Leaf => {
                    if p.nentries() > 0 {
                        let tag = validated_key(p.slot(0))[0];
                        let mixed = (0..p.nentries()).any(|i| validated_key(p.slot(i))[0] != tag);
                        let used: usize = (0..p.nentries()).map(|i| p.slot(i).len() + 4).sum();
                        out.push((tag, used, mixed));
                    }
                    return;
                }
                PageKind::Interior => {
                    let mut kids = vec![p.child0()];
                    kids.extend((0..p.nentries()).map(|i| validated_child(p.slot(i))));
                    kids
                }
                other => panic!("unexpected page kind in the tree: {other:?}"),
            }
        };
        for k in kids {
            leaves(pool, k, out);
        }
    }

    /// Mean fill of the leaves whose keys all carry `tag`, and how many there
    /// are. Mixed-tag leaves are excluded: one per keyspace boundary exists by
    /// construction and it belongs to no single run.
    fn fill_of(all: &[(u8, usize, bool)], tag: u8) -> (f64, usize) {
        let usable = (PAGE_SIZE - crate::page::HEADER_LEN) as f64;
        let mine: Vec<usize> = all
            .iter()
            .filter(|(t, _, mixed)| *t == tag && !*mixed)
            .map(|(_, used, _)| *used)
            .collect();
        assert!(!mine.is_empty(), "no pure leaves for tag {tag:#04x}");
        (mine.iter().sum::<usize>() as f64 / mine.len() as f64 / usable, mine.len())
    }

    /// Buffer-pool page accesses per insert for one interleaved load.
    ///
    /// No `TagHints` attached, deliberately: this is D9's own measurement and
    /// the number below is its control. K1's per-keyspace HINT is measured
    /// separately, by `accesses_for_tag`, on a tree that has one.
    ///
    /// Page accesses are what this repo measures cost in (D14's 1.1 reads per
    /// hop, D13's 4.2 vs 90.3 reads per trace query). They are exact, they are
    /// already instrumented, and they do not move with the machine.
    fn accesses_per_insert(tags: &[u8], docs: u64) -> f64 {
        let (pool, _d) = scratch_pool(512);
        let last_leaf = Cell::new(None);
        let hits = Cell::new(0);
        let attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &hits, &attempts).unwrap();
        load_interleaved(&mut t, tags, docs);
        let s = pool.stats();
        (s.hits + s.misses) as f64 / (docs * tags.len() as u64) as f64
    }

    /// THE COST PROPERTY. An ascending run's per-insert page work must not
    /// depend on how many other keyspaces happen to sit to its right.
    ///
    /// Each of these three runs, loaded on its own, costs about one page access
    /// per insert. Interleaved into one tree they cost several, and most of the
    /// difference is not the deeper tree: it is `redistribute_neighbors`
    /// re-reading and rewriting a parent and up to three siblings on every fill
    /// of a 0x40 or 0x20 leaf, because neither run is the tree's rightmost.
    ///
    /// Measured on this fixture, 5,000 documents = 15,000 inserts:
    ///   `compact-cells,sqlite-balance`                  71,007 accesses = 4.734/insert
    ///   `compact-cells,sqlite-balance,keyspace-append`  60,882 accesses = 4.059/insert
    /// The single-tag arms are byte-identical between the two builds, which is
    /// the guard that nothing D9 already covered was disturbed.
    ///
    /// The bound below sits between those two figures. It is an absolute number
    /// because this fixture is deterministic -- no randomness, no timing -- and
    /// because the quantity it bounds is the one the CPU sample of the 100K
    /// typed load pointed at: 23.3% of load time inside `split_leaf_and_insert`.
    #[test]
    fn sharing_a_tree_must_not_multiply_an_ascending_runs_page_work() {
        const DOCS: u64 = 5_000;
        let mixed = accesses_per_insert(&[0x60, 0x40, 0x20], DOCS);
        let alone: Vec<f64> = [0x60u8, 0x40, 0x20]
            .iter()
            .map(|t| accesses_per_insert(std::slice::from_ref(t), DOCS))
            .collect();
        eprintln!("interleaved {mixed:.3} accesses/insert; alone {alone:.3?}");

        // Each run on its own already gets D9 and the append hint. This is the
        // control arm: it must not move, in either build.
        for (tag, a) in [0x60u8, 0x40, 0x20].iter().zip(&alone) {
            assert!(*a <= 1.50, "tag {tag:#04x} alone costs {a:.3} accesses/insert");
        }
        // This is the append optimization's acceptance bound. The balancing
        // control deliberately lacks it (4.734 in the measurements above).
        // Keep the standalone controls in every build.
        if cfg!(feature = "keyspace-append") {
            assert!(
                mixed <= 4.30,
                "interleaved keyspaces cost {mixed:.3} page accesses per insert, against {alone:.3?} \
                 for the same three runs loaded separately"
            );
        }
    }

    /// The density guard. `redistribute_neighbors` already packs these runs to
    /// ~0.96-0.99 of a page, so the append shortcut must not buy its cheaper
    /// splits with a fatter file -- which is the exact trade D9's own 2/3-fill
    /// discipline exists to refuse.
    #[cfg(feature = "sqlite-balance")]
    #[test]
    fn every_keyspace_packs_its_own_ascending_run() {
        const DOCS: u64 = 3_000;
        let (pool, _d) = scratch_pool(16);
        let last_leaf = Cell::new(None);
        let hits = Cell::new(0);
        let attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &hits, &attempts).unwrap();
        load_interleaved(&mut t, &[0x60, 0x40, 0x20], DOCS);

        let mut all = Vec::new();
        leaves(&pool, t.root, &mut all);
        let usable = PAGE_SIZE - crate::page::HEADER_LEN;
        let payload: usize = all.iter().map(|(_, used, _)| *used).sum();
        // The floor no policy can beat: the records themselves, wall to wall.
        let floor = payload.div_ceil(usable);
        let (top, top_n) = fill_of(&all, 0x60);
        let (row, row_n) = fill_of(&all, 0x40);
        let (map, map_n) = fill_of(&all, 0x20);
        eprintln!(
            "fill 0x20 {map:.3} ({map_n})  0x40 {row:.3} ({row_n})  0x60 {top:.3} ({top_n})  \
             leaves {} floor {floor}",
            all.len()
        );

        for (tag, f) in [(0x20u8, map), (0x40, row), (0x60, top)] {
            assert!(f >= 0.90, "tag {tag:#04x} packs its leaves only {f:.3} full");
        }
        // Measured: 292 leaves against a 281-leaf floor without the shortcut,
        // 294 with it. 6% is the room that leaves, and no more.
        assert!(
            (all.len() as f64) <= floor as f64 * 1.06,
            "{} leaves for a {floor}-leaf payload",
            all.len()
        );
    }

    // ------------------------------------------------ K1 per-keyspace append HINT
    //
    // D9 widened the append SPLIT to a key's own tag. The HINT stayed narrow:
    // `fast_path_leaf` believes a leaf only when `next_leaf() == 0`, the
    // rightmost leaf of the whole TREE, so a run behind a higher tag paid a
    // full root-to-leaf descent for every single row. Measured on the
    // relationship load (200K rows, 99,840 edges): 4.40 page accesses to write
    // one forward edge row and 4.35 to write its reverse, 95% of what was left
    // of the per-edge write once the endpoint and preflight reads were gone.
    //
    // `TagHints` remembers the leaf per (tree, tag) together with the FENCES
    // the arming descent walked, so the same append lands with one access.

    /// A tree that keeps per-keyspace hints, and the state they borrow.
    /// The `Cell`s must outlive the tree, so the caller owns them.
    struct Hinted {
        last_leaf: Cell<Option<u32>>,
        hits: Cell<u64>,
        attempts: Cell<u64>,
        tags: TagHints,
    }
    impl Hinted {
        fn new() -> Self {
            Hinted { last_leaf: Cell::new(None), hits: Cell::new(0), attempts: Cell::new(0),
                     tags: TagHints::default() }
        }
        fn tree<'p>(&'p self, pool: &'p BufferPool) -> BTree<'p> {
            BTree::create(pool, 1, &self.last_leaf, &self.hits, &self.attempts)
                .unwrap()
                .with_tags(&self.tags)
        }
    }

    /// Page accesses per insert for the run under `measured`, with the other
    /// tags interleaved between them, counted only after `warm` documents so
    /// the tree has its final height and the hint has been armed at least once.
    fn accesses_for_tag(tags: &[u8], measured: u8, docs: u64, warm: u64) -> f64 {
        let (pool, _d) = scratch_pool(512);
        let h = Hinted::new();
        let mut t = h.tree(&pool);
        let at = |p: &BufferPool| { let s = p.stats(); s.hits + s.misses };
        let (mut spent, mut counted) = (0u64, 0u64);
        for i in 0..docs {
            for tag in tags {
                let before = at(&pool);
                t.insert(&tagged(*tag, i), &tagged_value(*tag, i)).unwrap();
                if *tag == measured && i >= warm {
                    spent += at(&pool) - before;
                    counted += 1;
                }
            }
        }
        spent as f64 / counted.max(1) as f64
    }

    /// THE BUDGET. An ascending run under a tag that is NOT the tree's last
    /// must reach its leaf without descending.
    ///
    /// 0x71 is the forward-edge tag; 0x40 (rows) and 0x60 (vectors) are written
    /// between its keys exactly as the multimodel load writes them, and 0x71
    /// sorts after both, so nothing about it is the tree's rightmost leaf.
    ///
    /// One page access is the floor: the leaf itself has to be read. Splits
    /// happen about once per leaf-full of rows and cost a descent each, so the
    /// bound is a little above the floor rather than at it.
    #[test]
    fn an_ascending_run_behind_other_keyspaces_reaches_its_leaf_without_descending() {
        const DOCS: u64 = 1_000;
        let edges = accesses_for_tag(&[0x40, 0x60, 0x71], 0x71, DOCS, DOCS / 5);
        eprintln!("tag 0x71 behind 0x40 and 0x60: {edges:.3} page accesses per insert");
        assert!(
            edges <= 1.20,
            "an ascending run behind two other keyspaces costs {edges:.3} page accesses per \
             insert; before the per-keyspace hint it was the tree's full height"
        );
    }

    /// THE PLACEMENT PROPERTY. A key that does NOT fall inside the hinted
    /// leaf's fences must still land where a descent would put it.
    ///
    /// The tree is loaded with three ascending runs so every tag holds a live
    /// hint, then written scattered over the SAME tags -- every one of those
    /// keys finds a hint armed and almost none of them belongs to its leaf. A
    /// `BTreeMap` is the oracle: the full scan must equal it exactly, and every
    /// key must also be reachable by `get`, which is the half a wrong fence
    /// breaks first. A record appended past its separator stays visible to the
    /// scan and disappears from the descent.
    #[test]
    fn a_key_outside_the_hinted_leaf_still_lands_where_a_descent_would_put_it() {
        const ASCENDING: u64 = 1_500;
        const SCATTERED: u64 = 1_500;
        let (pool, _d) = scratch_pool(16);
        let h = Hinted::new();
        let mut t = h.tree(&pool);
        let mut model: std::collections::BTreeMap<Vec<u8>, Vec<u8>> = Default::default();
        let value = |tag: u8, i: u64| {
            let mut v = tagged_value(tag, i);
            v.extend_from_slice(format!("|{tag:#04x}/{i}").as_bytes());
            v
        };
        let tags = [0x20u8, 0x40, 0x60, 0x71, 0x72];
        for i in 0..ASCENDING {
            for tag in tags {
                let (k, v) = (tagged(tag, i), value(tag, i));
                t.insert(&k, &v).unwrap();
                model.insert(k, v);
            }
        }
        // Scattered, but INSIDE the range the ascending runs already cover, so
        // the hinted leaf is a plausible-looking neighbour rather than an
        // obvious miss -- and every third key is deliberately aimed just above
        // whatever leaf the hint currently names.
        for j in 0..SCATTERED {
            let i = j.wrapping_mul(0x9E37_79B9_7F4A_7C15) % ASCENDING;
            for tag in tags {
                let (k, v) = (tagged(tag, i), value(tag, i + 1));
                t.insert(&k, &v).unwrap();
                model.insert(k, v);
            }
        }
        let seen: Vec<(Vec<u8>, Vec<u8>)> = t.range(&[]).unwrap().map(|r| r.unwrap()).collect();
        let expect: Vec<(Vec<u8>, Vec<u8>)> = model.into_iter().collect();
        assert_eq!(seen.len(), expect.len(), "the scan returned a different number of keys");
        assert_eq!(seen, expect, "the scan is not every key exactly once in byte order");
        for (k, v) in &expect {
            assert_eq!(
                t.get(k).unwrap().as_deref(), Some(&v[..]),
                "a key the scan can see is not reachable by a descent: {k:?}"
            );
        }
    }

    /// THE SHAPE PROPERTY. Splitting under the hint must leave an ordinary
    /// tree: every leaf reachable through its parents, every leaf non-empty,
    /// keys strictly ascending across the whole walk, and the sibling chain
    /// agreeing with the parent walk. `insert_tag_fast` never splits -- it is
    /// the fallback descent that does -- so this is the guard that the hint
    /// does not quietly push records onto a page the split policy then
    /// mis-cuts.
    #[test]
    fn splitting_under_the_per_keyspace_hint_leaves_an_ordinary_tree() {
        const DOCS: u64 = 4_000;
        let (pool, _d) = scratch_pool(64);
        let h = Hinted::new();
        let mut t = h.tree(&pool);
        for i in 0..DOCS {
            for tag in [0x40u8, 0x60, 0x71] {
                t.insert(&tagged(tag, i), &tagged_value(tag, i)).unwrap();
            }
        }
        let mut all = Vec::new();
        leaves(&pool, t.root, &mut all);
        assert!(all.len() > 8, "the fixture did not split enough to test anything");

        // The parent walk and the sibling chain must describe the same tree.
        let scanned: Vec<Vec<u8>> = t.range(&[]).unwrap().map(|r| r.unwrap().0).collect();
        assert_eq!(scanned.len() as u64, DOCS * 3, "the walk lost or duplicated keys");
        for w in scanned.windows(2) {
            assert!(w[0] < w[1], "the walk is not strictly ascending at {:?}", w[0]);
        }
        for k in &scanned {
            assert!(t.get(k).unwrap().is_some(), "a walked key is not reachable by a descent");
        }
    }

    /// Every leaf of the tree, through its parents, as
    /// (page, first key, last key, `next_leaf`, free space).
    fn leaf_details(pool: &BufferPool, page_no: u32,
        out: &mut Vec<(u32, Vec<u8>, Vec<u8>, u32, usize)>) {
        let kids = {
            let r = pool.get(page_no).unwrap();
            let p = PageRef::open_resident(&r, page_no).unwrap();
            match p.kind() {
                PageKind::Leaf => {
                    if p.nentries() > 0 {
                        out.push((page_no,
                            validated_key(p.slot(0)).to_vec(),
                            validated_key(p.slot(p.nentries() - 1)).to_vec(),
                            p.next_leaf(), p.free_space()));
                    }
                    return;
                }
                PageKind::Interior => {
                    let mut kids = vec![p.child0()];
                    kids.extend((0..p.nentries()).map(|i| validated_child(p.slot(i))));
                    kids
                }
                other => panic!("unexpected page kind in the tree: {other:?}"),
            }
        };
        for k in kids { leaf_details(pool, k, out); }
    }

    /// THE FENCE IS EXCLUSIVE AT THE TOP. A key EQUAL to the separator above
    /// the hinted leaf belongs to the NEXT leaf, and the hint must refuse it.
    ///
    /// This is the one boundary a cached fence can get wrong in the silent
    /// direction. `upper_bound` routes a key equal to a separator to the child
    /// on its RIGHT, so a separator key is always the first key of the leaf to
    /// the right -- it already exists there. Accepting it on the hinted leaf
    /// appends a SECOND copy: the scan returns the key twice, and `get` keeps
    /// answering with the old value forever. Relaxing the comparison in
    /// `TagHints::find` from `key < upper` to `key <= upper` fails this test
    /// with "the key is in the tree twice".
    ///
    /// The hint is forced onto a chosen leaf the way the stale-hint tests above
    /// force `last_leaf`: a real workload reaches this state rarely, and a test
    /// that waits for it to happen by chance is testing nothing.
    #[test]
    fn a_key_equal_to_the_fence_belongs_to_the_next_leaf() {
        const DOCS: u64 = 3_000;
        let (pool, _d) = scratch_pool(64);
        let h = Hinted::new();
        let mut t = h.tree(&pool);
        // The 0x71 run is written SCATTERED here on purpose: an append split
        // closes each left page nearly full, and this test needs a leaf with
        // room in it. The hint is armed by hand below, so the write order of
        // the fixture decides nothing else.
        for i in 0..DOCS {
            let scattered = i.wrapping_mul(0x9E37_79B9_7F4A_7C15) % DOCS;
            t.insert(&tagged(0x40, i), &tagged_value(0x40, i)).unwrap();
            t.insert(&tagged(0x71, scattered), &tagged_value(0x71, scattered)).unwrap();
        }
        let mut all = Vec::new();
        leaf_details(&pool, t.root, &mut all);

        // A 0x71 leaf with room, whose right neighbour is also a 0x71 leaf.
        let by_page: std::collections::HashMap<u32, usize> =
            all.iter().enumerate().map(|(i, l)| (l.0, i)).collect();
        let chosen = all.iter().find(|l| {
            l.1[0] == 0x71 && l.2[0] == 0x71 && l.4 >= 64
                && by_page.get(&l.3).is_some_and(|&j| all[j].1[0] == 0x71)
        }).expect("the fixture has no 0x71 leaf with room beside another");
        let right = &all[by_page[&chosen.3]];
        let separator = right.1.clone();
        assert!(separator > chosen.2, "the fixture's leaves are not in order");

        // Arm the hint exactly as the arming descent would have, then hand it
        // the separator key.
        let mut fences = LeafFences::NONE;
        fences.narrow_lower(&chosen.1);
        fences.narrow_upper(&separator);
        assert!(fences.armable, "the fixture's separators must fit the inline fence");
        h.tags.arm(1, &chosen.2, chosen.0, chosen.3, fences);
        t.insert(&separator, b"replaced").unwrap();

        let seen: Vec<Vec<u8>> = t.range(&[]).unwrap().map(|r| r.unwrap().0).collect();
        assert_eq!(
            seen.iter().filter(|k| **k == separator).count(), 1,
            "the key is in the tree twice: the hint appended it below its own fence"
        );
        for w in seen.windows(2) {
            assert!(w[0] < w[1], "the scan is not strictly ascending at {:?}", w[0]);
        }
        assert_eq!(
            t.get(&separator).unwrap().as_deref(), Some(&b"replaced"[..]),
            "the descent still answers with the old value, so the new record went elsewhere"
        );
    }

    /// THE INVALIDATION PROPERTY. A delete can merge leaves, collapse the root
    /// and hand a freed page straight back to the allocator, so the fences a
    /// hint cached describe a shape that no longer exists. Writing the same
    /// ascending run again afterwards must place every key correctly.
    #[test]
    fn a_delete_forgets_the_fences_it_invalidated() {
        const DOCS: u64 = 1_200;
        let (pool, _d) = scratch_pool(16);
        let h = Hinted::new();
        let mut t = h.tree(&pool);
        let mut model: std::collections::BTreeMap<Vec<u8>, Vec<u8>> = Default::default();
        for i in 0..DOCS {
            for tag in [0x40u8, 0x71] {
                let (k, v) = (tagged(tag, i), tagged_value(tag, i));
                t.insert(&k, &v).unwrap();
                model.insert(k, v);
            }
        }
        // Empty most of the forward-edge run, which is where the hint sits.
        for i in 0..DOCS * 3 / 4 {
            let k = tagged(0x71, i);
            assert!(t.delete(&k).unwrap());
            model.remove(&k);
        }
        // And write it straight back, ascending, through whatever the delete
        // left behind.
        for i in 0..DOCS * 3 / 4 {
            let (k, v) = (tagged(0x71, i), tagged_value(0x71, i + 7));
            t.insert(&k, &v).unwrap();
            model.insert(k, v);
        }
        let seen: Vec<(Vec<u8>, Vec<u8>)> = t.range(&[]).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(seen, model.into_iter().collect::<Vec<_>>(),
            "the tree written after a delete is not the tree the model describes");
        for (k, v) in &seen {
            assert_eq!(t.get(k).unwrap().as_deref(), Some(&v[..]),
                "a key is in the scan but not reachable by a descent: {k:?}");
        }
    }

    // ------------------------------- K1 byte-equivalence: placement is unchanged
    //
    // Phase 1's rule for a speed change in the split/append path is that the
    // page IMAGES do not move (`split_byte_equivalence` below). The same rule
    // applies here, and more sharply: this cache decides which leaf a write
    // reaches WITHOUT descending, so if it ever reaches a different one than
    // the descent would, the file says so.
    //
    // The claim under test: the per-keyspace hint changes where a write
    // descends FROM, never where it lands. Run one deterministic multimodel
    // workload twice into two stores -- once with the cache, once with it
    // switched off -- and the two data files must be equal byte for byte,
    // every page image, after the final checkpoint.

    use crate::store::{Config, Store, SyncMode};
    use crate::io::IoMode;

    fn cfg() -> Config {
        Config { budget_bytes: 32 << 20, io: IoMode::Buffered, sync: SyncMode::Off }
    }

    /// `[tag][collection][sequence]` -- a data row, `src/collections.rs`'s shape.
    fn row_key(i: u64) -> Vec<u8> {
        let mut k = vec![0x40u8, 1];
        k.extend_from_slice(&i.to_be_bytes());
        k
    }
    /// `[tag][collection][sequence][field]` -- an embedding row.
    fn vec_key(i: u64) -> Vec<u8> {
        let mut k = vec![0x60u8, 1];
        k.extend_from_slice(&i.to_be_bytes());
        k.push(0);
        k
    }
    /// `[tag][first collection][first seq][ctx][type][last collection][last seq]`
    /// -- `src/graph_collections.rs`'s edge key, narrowed to one byte per
    /// small identity. 0x71 is the forward row (first = source), 0x72 the
    /// reverse (first = destination). Organizations live in collection 2, so
    /// their rows sort ABOVE every person's, exactly as in the bench.
    fn edge_key(tag: u8, a: (u8, u64), ty: u8, b: (u8, u64)) -> Vec<u8> {
        let mut k = vec![tag, a.0];
        k.extend_from_slice(&a.1.to_be_bytes());
        k.extend_from_slice(&[0, ty, b.0]);
        k.extend_from_slice(&b.1.to_be_bytes());
        k
    }

    /// THE WORKLOAD. Deterministic, no randomness, no timing: per document it
    /// writes a row, an embedding, three forward-edge rows and their three
    /// reverse rows, and every so often deletes one of each. The destinations
    /// reproduce the relationship load's shape -- `(i+1)` and `(i+7)` make two
    /// near-ascending reverse runs six keys apart, and `i % 100` makes a
    /// hundred hot organizations whose reverse rows are a hundred separate
    /// cold runs. 5,000 documents is 40,000 puts, far past the volume at which
    /// every tag's run splits and redistributes across the keyspace boundary
    /// it shares with the tag above it.
    fn multimodel_workload(s: &mut Store, docs: u64) {
        for i in 0..docs {
            s.put(&row_key(i), &vec![b'd'; 200 + (i % 97) as usize * 2]).unwrap();
            s.put(&vec_key(i), &vec![b'f'; 32]).unwrap();
            let people = [(1u8, (i + 1) % docs), (1, (i + 7) % docs)];
            for (n, dst) in people.iter().enumerate() {
                s.put(&edge_key(0x71, (1, i), 1, *dst), &[b'e', n as u8]).unwrap();
                s.put(&edge_key(0x72, *dst, 1, (1, i)), &[]).unwrap();
            }
            let org = (2u8, i % 100 + 1);
            s.put(&edge_key(0x71, (1, i), 2, org), b"m").unwrap();
            s.put(&edge_key(0x72, org, 2, (1, i)), &[]).unwrap();

            // Interleaved deletes: a merge, a freed page and a collapsed root
            // are what a cached fence cannot survive, so the oracle has to
            // contain them or it is not testing the invalidation at all.
            if i % 37 == 36 && i > 40 {
                s.delete(&row_key(i - 20)).unwrap();
                s.delete(&edge_key(0x71, (1, i - 13), 1, (1, (i - 12) % docs))).unwrap();
                s.delete(&edge_key(0x72, (1, (i - 12) % docs), 1, (1, i - 13))).unwrap();
            }
            if i % 256 == 255 { s.commit().unwrap(); }
            if i % 1024 == 1023 { s.checkpoint().unwrap(); }
        }
        s.commit().unwrap();
        s.checkpoint().unwrap();

        // THE UPDATE SWEEP, and it is here for the negative control. A key
        // EQUAL to a hinted leaf's upper fence is the one placement error a
        // cached fence can make silently, and it only ever arrives as a
        // REWRITE: the separator key already lives in the leaf to the right.
        // Rewriting every key in ascending order walks the cache onto each
        // leaf in turn and then hands it exactly that leaf's separator, at
        // every boundary in the tree. The values are shorter than the
        // originals so the leaf has the room a wrong placement would need --
        // a control that cannot fit its record proves nothing either.
        for i in 0..docs {
            s.put(&row_key(i), &vec![b'u'; 48]).unwrap();
            s.put(&vec_key(i), b"u").unwrap();
            s.put(&edge_key(0x71, (1, i), 1, (1, (i + 1) % docs)), b"U").unwrap();
            s.put(&edge_key(0x72, (1, (i + 1) % docs), 1, (1, i)), &[]).unwrap();
            if i % 256 == 255 { s.commit().unwrap(); }
        }
        s.commit().unwrap();
        s.checkpoint().unwrap();
    }

    /// Run the workload into a fresh store and return its data file, page by
    /// page, plus the rows a full scan sees.
    fn workload_file(docs: u64, hints: bool, relaxed: bool)
        -> (Vec<Vec<u8>>, Vec<(Vec<u8>, Vec<u8>)>, u64) {
        let d = tempfile::tempdir().unwrap();
        let mut s = Store::create(&d.path().join("s"), cfg()).unwrap();
        s.tag_hints().set_enabled(hints);
        s.tag_hints().set_relaxed_upper_fence(relaxed);
        multimodel_workload(&mut s, docs);
        let mut rows = Vec::new();
        s.scan(&[]).unwrap()
            .for_each_ref(|k, v| { rows.push((k.to_vec(), v.to_vec())); true })
            .unwrap();
        let served = s.tag_hints().hits();
        drop(s);
        let bytes = std::fs::read(d.path().join("s").join("data")).unwrap();
        (bytes.chunks(PAGE_SIZE).map(<[u8]>::to_vec).collect(), rows, served)
    }

    /// Report the first page and SLOT at which two files disagree, in the
    /// terms a placement bug is stated in: which leaf, which slot, which key,
    /// and how the two sides differ there.
    fn first_difference(a: &[Vec<u8>], b: &[Vec<u8>]) -> Option<String> {
        if a.len() != b.len() {
            return Some(format!("page COUNT differs: {} pages with the hint, {} without",
                a.len(), b.len()));
        }
        let records = |page: &[u8], no: u32| -> std::result::Result<Vec<Vec<u8>>, String> {
            let p = PageRef::open(page, no).map_err(|e| format!("unreadable: {e:?}"))?;
            Ok((0..p.nentries()).map(|i| p.slot(i).to_vec()).collect())
        };
        for (no, (pa, pb)) in a.iter().zip(b).enumerate() {
            if pa == pb { continue; }
            let no = no as u32;
            let (ra, rb) = match (records(pa, no), records(pb, no)) {
                (Ok(x), Ok(y)) => (x, y),
                (x, y) => return Some(format!("page {no}: {x:?} vs {y:?}")),
            };
            let header = pa[..crate::page::HEADER_LEN] != pb[..crate::page::HEADER_LEN];
            if ra == rb {
                return Some(format!(
                    "page {no} holds the same {} records but differs in its bytes \
                     (header differs: {header}) -- a difference that is NOT placement",
                    ra.len()));
            }
            let slot = (0..ra.len().max(rb.len()))
                .find(|&i| ra.get(i) != rb.get(i)).unwrap_or(0);
            let show = |r: Option<&Vec<u8>>| match r {
                Some(rec) => format!("key {:02x?} ({} bytes)", validated_key(rec), rec.len()),
                None => "no record".into(),
            };
            return Some(format!(
                "page {no} (a {:?}) first differs at SLOT {slot}: {} records with the hint, \
                 {} without\n  with the hint: {}\n  without:      {}",
                PageRef::open(pa, no).map(|p| p.kind()).unwrap(),
                ra.len(), rb.len(), show(ra.get(slot)), show(rb.get(slot))));
        }
        None
    }

    /// THE PROOF. Two stores, one workload, one difference: whether the
    /// per-keyspace cache was on. A hinted placement has to land in the same
    /// leaf AND the same slot a descent would have chosen, and a split that
    /// re-arms the cache afterwards must not change which half a record went
    /// to -- either would move a byte, and every byte is compared.
    #[test]
    fn the_hint_changes_where_a_write_descends_from_not_where_it_lands() {
        const DOCS: u64 = 5_000;   // 40,000 puts
        let (hinted, hinted_rows, served) = workload_file(DOCS, true, false);
        let (plain, plain_rows, none) = workload_file(DOCS, false, false);
        eprintln!("{} pages with the hint and {} without; the cache served {served} writes \
                   with it on and {none} with it off", hinted.len(), plain.len());
        // Neither arm may be vacuous: the control must really have the cache
        // off, and the candidate must really be using it.
        assert_eq!(none, 0, "the switch did not turn the cache off");
        assert!(served > DOCS * 4, "the cache served only {served} of this workload's writes");
        assert_eq!(hinted_rows, plain_rows, "the two stores do not hold the same rows");
        if let Some(why) = first_difference(&hinted, &plain) {
            panic!("the per-keyspace hint moved a record:\n{why}");
        }

        // THE NEGATIVE CONTROL. Without this the assertion above could pass
        // because the workload never armed a hint at all. Relaxing the upper
        // fence from `key < separator` to `key <= separator` is the smallest
        // real placement error this cache can make: the separator key already
        // lives in the NEXT leaf, so the relaxed rule appends a second copy
        // here. It must make the files differ.
        let (relaxed, relaxed_rows, _) = workload_file(DOCS, true, true);
        let moved = first_difference(&hinted, &relaxed);
        assert!(
            moved.is_some() || relaxed_rows != hinted_rows,
            "relaxing the upper fence changed nothing, so this workload never \
             exercised the fence and the comparison above is vacuous"
        );
        eprintln!("negative control: {}", moved.unwrap_or_else(|| "rows differ".into()));
    }

    /// The ordering oracle that rejected pair-packing and scattered-packing
    /// (docs/PAIR_PACKING.md, docs/SCATTERED_PACKING.md): a cheaper split policy
    /// is worth nothing if the tree stops answering exactly. Interleaved
    /// ascending runs FIRST, then a scattered phase over the same keyspaces, so
    /// the append shortcut and the balanced split both run against one tree.
    #[test]
    fn interleaved_then_scattered_keeps_every_key_exactly_once_in_order() {
        const DOCS: u64 = 2_000;
        const SCATTER: u64 = 2_000;
        let (pool, _d) = scratch_pool(16);
        let last_leaf = Cell::new(None);
        let hits = Cell::new(0);
        let attempts = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &hits, &attempts).unwrap();

        let mut expected: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let value = |tag: u8, i: u64| {
            let mut v = tagged_value(tag, i);
            v.extend_from_slice(format!("|{tag:#04x}/{i}").as_bytes());
            v
        };
        for i in 0..DOCS {
            for tag in [0x60u8, 0x40, 0x20] {
                let (k, v) = (tagged(tag, i), value(tag, i));
                t.insert(&k, &v).unwrap();
                expected.push((k, v));
            }
        }
        // A multiplicative hash scatters insert order without needing rand.
        for j in 0..SCATTER {
            let i = DOCS + j.wrapping_mul(0x9E37_79B9_7F4A_7C15) % 1_000_000;
            for tag in [0x20u8, 0x60, 0x40] {
                let (k, v) = (tagged(tag, i), value(tag, i));
                t.insert(&k, &v).unwrap();
                expected.push((k, v));
            }
        }
        expected.sort();
        expected.dedup_by(|a, b| a.0 == b.0);

        let seen: Vec<(Vec<u8>, Vec<u8>)> = t.range(&[]).unwrap().map(|r| r.unwrap()).collect();
        assert_eq!(seen.len(), expected.len(), "scan returned a different number of keys");
        assert_eq!(seen, expected, "full scan is not every key exactly once in byte order");
        for w in seen.windows(2) {
            assert!(w[0].0 < w[1].0, "scan is not strictly ascending at {:?}", w[0].0);
        }
        for (k, v) in &expected {
            assert_eq!(t.get(k).unwrap().as_deref(), Some(&v[..]), "point get disagrees for {k:?}");
        }
    }
}

/// L2.3-2 -- the byte-equivalence oracle for the allocation-free split.
///
/// The candidate changes only HOW a split moves bytes, never WHICH bytes it
/// writes: same `split_point`, same `neighbor_cell_cuts`, same `at_point`
/// decision, same page images, same separators. That is a claim about output,
/// so it is tested as one: every corpus case is run through
/// `split_leaf_and_insert_vec` (the record-per-`Vec` implementation) and
/// through `split_leaf_and_insert_ref` (the `SlotRef` one) in two independent
/// stores, and EVERY page of the two files is compared byte for byte -- which
/// covers separator keys too, since a separator is bytes on a parent page.
///
/// Both implementations are compiled under `cfg(test)` precisely so this can
/// run in ONE build; in a shipping build the `slotref-split` feature selects
/// one of them and the other is not compiled at all.
#[cfg(test)]
mod split_byte_equivalence {
    use super::*;
    use crate::test_support::scratch_pool;

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Impl { Vec, Ref }

    struct Case {
        name: &'static str,
        prefix: Vec<(Vec<u8>, Vec<u8>)>,
        last: (Vec<u8>, Vec<u8>),
    }

    /// `src/collections.rs`'s width-tagged big-endian integer component --
    /// the shape that produces `compact-cells` 0xff cells.
    fn ordered(n: u64) -> Vec<u8> {
        let b = n.to_be_bytes();
        let start = b.iter().position(|x| *x != 0).unwrap_or(7);
        let mut k = vec![0x80 + (8 - start) as u8];
        k.extend_from_slice(&b[start..]);
        k
    }

    fn tagged(tag: u8, i: u64) -> Vec<u8> {
        let mut k = vec![tag];
        k.extend(ordered(1));
        match tag {
            0x20 => k.extend_from_slice(format!("key-{i:012}").as_bytes()),
            0x40 => k.extend(ordered(i + 1)),
            _ => { k.extend(ordered(i + 1)); k.extend(ordered(0)); }
        }
        k
    }

    fn tagged_value(tag: u8, i: u64) -> Vec<u8> {
        match tag {
            0x20 => ordered(i + 1),
            0x40 => vec![b'd'; 200 + (i % 97) as usize * 2],
            _ => vec![b'f'; 32],
        }
    }

    fn scatter(i: u64) -> [u8; 8] { i.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes() }

    /// Load the prefix, then force the LAST record through the split path --
    /// whichever implementation is named. Returns every page of the resulting
    /// file, the root, and the full key/value sequence a scan sees.
    fn run(case: &Case, which: Impl) -> (Vec<Vec<u8>>, u32, Vec<(Vec<u8>, Vec<u8>)>, u32) {
        let (pool, _d) = scratch_pool(192);
        let last_leaf = Cell::new(None);
        let hits = Cell::new(0);
        let tries = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &hits, &tries).unwrap();
        for (k, v) in &case.prefix { t.insert(k, v).unwrap(); }

        let (k, v) = &case.last;
        let before = pool.page_count();
        let rec = enc_leaf(k, v, pool.compact_cells());
        let (w, path) = t.descend_for_write(k).unwrap();
        let leaf = w.page_no();
        let at = {
            let p = PageRef::open_resident(w.bytes(), leaf).unwrap();
            validate_records(&p).unwrap();
            lower_bound(&p, k).unwrap()
        };
        match which {
            Impl::Vec => t.split_leaf_and_insert_vec(w, path, at, rec, k, None).unwrap(),
            Impl::Ref => t.split_leaf_and_insert_ref(w, path, at, rec, k, None).unwrap(),
        }

        let root = t.root();
        let mut rows = Vec::new();
        t.range(&[]).unwrap()
            .for_each_ref(|k, v| { rows.push((k.to_vec(), v.to_vec())); true })
            .unwrap();
        let pages: Vec<Vec<u8>> = (0..pool.page_count())
            .map(|n| pool.get(n).unwrap().to_vec())
            .collect();
        (pages, root, rows, before)
    }

    /// The corpus. Each entry names one shape the split path has to handle:
    /// which branch it takes, what its records look like, and where in the
    /// tree the splitting leaf sits.
    fn corpus() -> Vec<Case> {
        let mut cases = Vec::new();

        // 1. Ascending into the TREE's rightmost leaf: D9's append split
        //    (at_point), left page closed full, right page holds one record.
        cases.push(Case {
            name: "ascending append at the tree's rightmost leaf",
            prefix: (0..64u64).map(|i| (tagged(0x40, i), tagged_value(0x40, i))).collect(),
            last: (tagged(0x40, 64), tagged_value(0x40, 64)),
        });

        // 2. Three keyspaces in one tree, written one document at a time --
        //    `src/collections.rs`'s exact shape. The 0x40 run is ascending
        //    but never at the tree's rightmost leaf, so this is the case
        //    `keyspace-append` widens and `redistribute_neighbors` owned
        //    before it.
        let mut prefix = Vec::new();
        for i in 0..90u64 {
            for tag in [0x60u8, 0x40, 0x20] { prefix.push((tagged(tag, i), tagged_value(tag, i))); }
        }
        cases.push(Case {
            name: "ascending row run under a higher keyspace",
            prefix,
            last: (tagged(0x40, 90), tagged_value(0x40, 90)),
        });

        // 3. Scattered keys: the balanced `split_point` cut, and the
        //    neighbour redistribution that fires when siblings have room.
        cases.push(Case {
            name: "scattered keys, balanced cut",
            prefix: (0..400u64).map(|i| (scatter(i).to_vec(), vec![b'x'; 90])).collect(),
            last: (scatter(400).to_vec(), vec![b'x'; 90]),
        });

        // 4. The new record lands FIRST in its leaf (at == 0).
        let mut prefix: Vec<(Vec<u8>, Vec<u8>)> =
            (10..70u64).map(|i| ((i * 4).to_be_bytes().to_vec(), vec![b'a'; 120])).collect();
        prefix.push((0u64.to_be_bytes().to_vec(), vec![b'a'; 120]));
        cases.push(Case {
            name: "new record first in its leaf",
            prefix,
            last: (1u64.to_be_bytes().to_vec(), vec![b'b'; 120]),
        });

        // 5. The boundary leaf: one leaf holding the top of tag 0x40 and the
        //    bottom of tag 0x60, so an ascending 0x40 insert is NOT last in
        //    the leaf and the append shortcut is never consulted.
        let mut prefix = Vec::new();
        for i in 0..40u64 { prefix.push((tagged(0x40, i), tagged_value(0x40, i))); }
        for i in 0..40u64 { prefix.push((tagged(0x60, i), tagged_value(0x60, i))); }
        cases.push(Case {
            name: "mixed-tag boundary leaf",
            prefix,
            last: (tagged(0x40, 40), tagged_value(0x40, 40)),
        });

        // 6. One indivisible record too large to pair with its neighbours:
        //    the three-way cut that allocates TWO pages and pushes TWO
        //    separators. Sizes taken from the pinned huge-record test.
        let mut prefix: Vec<(Vec<u8>, Vec<u8>)> =
            (0..40u64).map(|i| ((i * 2).to_be_bytes().to_vec(), vec![b'a'; 60])).collect();
        prefix.push((41u64.to_be_bytes().to_vec(), vec![b'Z'; 2500]));
        cases.push(Case {
            name: "one huge record among small ones",
            prefix,
            last: (42u64.to_be_bytes().to_vec(), vec![b'Y'; 2400]),
        });

        // 6b. The three-way cut actually taken. A ROOT leaf has no parent, so
        //     the neighbour redistribution cannot absorb the insert, and the
        //     sizes are chosen so that no single cut leaves both halves inside
        //     a page: 44 records of 74 page-bytes each with a 2514-byte record
        //     landing exactly in the middle. Left-of-big and right-of-big are
        //     each 1628 bytes, so big + either side is 4142 > 4056.
        let mut prefix: Vec<(Vec<u8>, Vec<u8>)> =
            (0..44u64).map(|i| ((i * 2).to_be_bytes().to_vec(), vec![b'a'; 60])).collect();
        prefix.sort();
        cases.push(Case {
            name: "three-way cut on a root leaf",
            prefix,
            last: (43u64.to_be_bytes().to_vec(), vec![b'Z'; 2500]),
        });

        // 7. Records close to the per-record limit: two or three to a leaf,
        //    so the cut has almost no freedom and every byte of slack shows.
        cases.push(Case {
            name: "records near the size limit",
            prefix: (0..12u64).map(|i| (i.to_be_bytes().to_vec(), vec![b'L'; 1300])).collect(),
            last: (12u64.to_be_bytes().to_vec(), vec![b'L'; 1300]),
        });

        // 8. Compact integer cells (the 0xff + width-tag encoding) with tiny
        //    values: hundreds of very short records in one leaf.
        cases.push(Case {
            name: "compact integer cells, many per leaf",
            prefix: (0..600u64).map(|i| (ordered(i + 1), vec![b'v'; 4])).collect(),
            last: (ordered(601), vec![b'v'; 4]),
        });

        // 9. A leaf that is its PARENT's last child: its right neighbour
        //    lives under a different parent, which is the branch
        //    `keyspace_rightmost` falls through to and the window
        //    `redistribute_neighbors` has to clamp.
        let mut prefix: Vec<(Vec<u8>, Vec<u8>)> =
            (0..900u64).map(|i| (scatter(i).to_vec(), vec![b'p'; 40])).collect();
        prefix.extend((0..300u64).map(|i| (tagged(0x60, i), tagged_value(0x60, i))));
        cases.push(Case {
            name: "deep tree, last child of a parent",
            prefix,
            last: (scatter(901).to_vec(), vec![b'p'; 40]),
        });

        cases
    }

    #[test]
    fn both_split_implementations_write_identical_pages_for_every_corpus_state() {
        let mut reached = [false; 3];
        for case in corpus() {
            let (a, root_a, rows_a, before) = run(&case, Impl::Vec);
            let (b, root_b, rows_b, _) = run(&case, Impl::Ref);
            // Which branch a case reached shows in how many pages the split
            // added: 0 = the neighbour redistribution absorbed it, 1 = an
            // ordinary or append split into one new leaf, more = the
            // three-way cut (two leaves, and a new root when the leaf that
            // split WAS the root). A corpus that stops reaching one of the
            // three stops testing it, silently -- hence the tally below.
            let added = a.len() as u32 - before;
            println!("{}: {added} pages added", case.name);
            reached[(added as usize).min(2)] = true;
            assert_eq!(root_a, root_b, "{}: root page differs", case.name);
            assert_eq!(rows_a, rows_b, "{}: visible rows differ", case.name);
            assert_eq!(a.len(), b.len(), "{}: page count differs", case.name);
            for (i, (x, y)) in a.iter().zip(b.iter()).enumerate() {
                assert!(x == y, "{}: page {i} differs byte for byte", case.name);
            }
        }
        // Redistribution does not exist in a build without sqlite-balance.
        // Byte identity and the remaining split coverage apply in every mode.
        assert_eq!(reached[0], cfg!(feature = "sqlite-balance"),
            "neighbour redistribution coverage must match the compiled policy");
        assert!(reached[1], "no corpus case reached the one-new-leaf split");
        assert!(reached[2], "no corpus case reached the three-way cut");
    }

    /// The comparison above is worthless unless it can FAIL. Feed the same
    /// harness two states that genuinely differ -- one byte of one value --
    /// and require it to notice.
    #[test]
    fn the_oracle_notices_a_one_byte_difference() {
        let mut a = corpus().remove(2);
        let mut b = corpus().remove(2);
        b.last.1[0] = b'z';
        assert_ne!(a.last.1, b.last.1);
        let pages_a = run(&a, Impl::Vec).0;
        let pages_b = run(&b, Impl::Vec).0;
        assert_ne!(pages_a, pages_b, "the oracle cannot tell two different splits apart");
        // ... and identical input through the same implementation must be
        // identical, or the harness itself is not deterministic.
        a.name = "determinism";
        assert_eq!(run(&a, Impl::Vec).0, run(&a, Impl::Vec).0);
    }
}

/// L2.3-2 -- how much the split path allocates, counted rather than argued.
///
/// The finding this candidate answers: on a Pi, a typed put spends ~46% of its
/// CPU in the split path, and a quarter of all cycles in malloc/free. A plain
/// leaf split copied every record on the page into its own `Vec<u8>`
/// (`p.slot(j).to_vec()`); a three-sibling redistribution did that for three
/// pages plus the parent's records. So the number to watch is not a timing, it
/// is an ALLOCATION COUNT, and it is measured here with the kernel's
/// `cfg(test)` counting allocator.
///
/// Both figures are printed, so the test doubles as the record of the before.
#[cfg(test)]
mod split_allocations {
    use super::*;
    use crate::test_alloc::measured;
    use crate::test_support::scratch_pool;

    /// Allocations inside ONE steady-state split -- not the first one. The
    /// first split of a process warms allocator free lists and (with the
    /// feature on) creates the one reusable scratch; a steady-state split is
    /// what a load actually spends its time in.
    ///
    /// The split that gets measured is a REAL one: keys are inserted through
    /// the ordinary path until one of them genuinely does not fit its leaf,
    /// and only that record is handed to the split directly, so the leaf is as
    /// full as it would be in a running store rather than as full as the test
    /// happened to leave it.
    fn split_allocations(scattered: bool, slotref: bool) -> (usize, usize) {
        let (pool, _d) = scratch_pool(192);
        let last_leaf = Cell::new(None);
        let hits = Cell::new(0);
        let tries = Cell::new(0);
        let mut t = BTree::create(&pool, 1, &last_leaf, &hits, &tries).unwrap();
        let val = vec![b'v'; 120];
        let mut i = 0u64;
        let mut next = move || {
            i += 1;
            if scattered { i.wrapping_mul(0x9E37_79B9_7F4A_7C15).to_be_bytes().to_vec() }
            else { i.to_be_bytes().to_vec() }
        };
        for _ in 0..2000 { let k = next(); t.insert(&k, &val).unwrap(); }

        /// Insert until a record does not fit its leaf; return that record's key.
        fn until_split(t: &mut BTree<'_>, next: &mut impl FnMut() -> Vec<u8>, val: &[u8]) -> Vec<u8> {
            loop {
                let k = next();
                let rec = enc_leaf(&k, val, t.pool.compact_cells());
                let (leaf, _) = t.descend(&k).unwrap();
                let room = {
                    let r = t.pool.get(leaf).unwrap();
                    let p = open_cached(&r, leaf).unwrap();
                    p.free_space() >= rec.len() + 4
                };
                if !room { return k; }
                t.insert(&k, val).unwrap();
            }
        }

        fn once<'p>(t: &mut BTree<'p>, k: &[u8], val: &[u8], slotref: bool, measure: bool) -> (usize, usize) {
            let rec = enc_leaf(k, val, t.pool.compact_cells());
            let (w, path) = t.descend_for_write(k).unwrap();
            let leaf = w.page_no();
            let at = {
                let p = PageRef::open_resident(w.bytes(), leaf).unwrap();
                validate_records(&p).unwrap();
                lower_bound(&p, k).unwrap()
            };
            let run = |t: &mut BTree<'p>| {
                if slotref { t.split_leaf_and_insert_ref(w, path, at, rec, k, None) }
                else { t.split_leaf_and_insert_vec(w, path, at, rec, k, None) }.unwrap()
            };
            if measure {
                let (_, count, bytes) = measured(|| run(t));
                (count, bytes)
            } else {
                run(t);
                (0, 0)
            }
        }
        // The first split warms; the second is the measurement.
        let warm = until_split(&mut t, &mut next, &val);
        once(&mut t, &warm, &val, slotref, false);
        let hot = until_split(&mut t, &mut next, &val);
        once(&mut t, &hot, &val, slotref, true)
    }

    #[test]
    fn a_steady_state_split_allocates_a_bounded_constant() {
        for scattered in [false, true] {
            let (n_vec, b_vec) = split_allocations(scattered, false);
            let (n_ref, b_ref) = split_allocations(scattered, true);
            println!(
                "{} split: record-per-Vec {n_vec} allocations / {b_vec} bytes; \
                 SlotRef {n_ref} allocations / {b_ref} bytes",
                if scattered { "scattered" } else { "ascending" }
            );
            // The before. A split of a leaf holding ~30 records of this size
            // allocates one Vec per record plus the page images; a
            // redistribution does it for a whole window.
            assert!(n_vec >= 20, "the record-per-Vec split should allocate per record, saw {n_vec}");
            // The after, and the residue is NAMED rather than waved at. A
            // redistribution allocates nothing at all (measured: 0). A split
            // that actually adds a page allocates exactly one thing: the
            // interior record `insert_separator` pushes into the parent
            // (`enc_interior`, key + 6 bytes). That belongs to the separator
            // promotion, not to moving the leaf's records, and it does not
            // grow with the number of records on the page -- which is the
            // property under test. `BufferPool::allocate` itself allocated
            // nothing here; if its page tables ever grow inside a measured
            // split this bound is where it will show.
            assert!(
                n_ref <= 1,
                "the SlotRef split should allocate at most the promoted separator, saw {n_ref}"
            );
        }
    }
}
