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
use crate::pool::{BufferPool, PinnedWrite};
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
        Ok(BTree { pool, tree_id, root: no, last_leaf, fast_path_hits, fast_path_attempts })
    }

    pub fn open(
        pool: &'p BufferPool,
        tree_id: u16,
        root: u32,
        last_leaf: &'p Cell<Option<u32>>,
        fast_path_hits: &'p Cell<u64>,
        fast_path_attempts: &'p Cell<u64>,
    ) -> Self {
        BTree { pool, tree_id, root, last_leaf, fast_path_hits, fast_path_attempts }
    }

    pub fn root(&self) -> u32 { self.root }

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
                return Ok((w, path));
            }
        }
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

        let (w, path) = self.descend_for_write(key)?;
        #[cfg(feature = "write-trace")]
        if let Some(started) = descent_started {
            crate::write_trace::add(crate::write_trace::Field::Descent, started.elapsed());
        }
        self.insert_into_leaf(w, path, key, rec)
    }

    fn insert_into_leaf(&mut self, mut w: PinnedWrite<'p>, path: Vec<u32>, key: &[u8], rec: Vec<u8>) -> Result<()> {
        let leaf = w.page_no();
        #[cfg(feature = "write-trace")]
        let search_started = crate::write_trace::active().then(std::time::Instant::now);
        let (i, exists, is_rightmost, retired) = {
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
            let is_rightmost = p.next_leaf() == 0;
            let retired = if exists { replaced_overflow_pages(self.pool, p.slot(i))? } else { Vec::new() };
            (i, exists, is_rightmost, retired)
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
            #[cfg(feature = "write-trace")]
            if let Some(started) = insert_started {
                crate::write_trace::add(crate::write_trace::Field::LeafInsert, started.elapsed());
            }
            return Ok(());
        }
        #[cfg(feature = "write-trace")]
        let split_started = crate::write_trace::active().then(std::time::Instant::now);
        let result = self.split_leaf_and_insert(w, path, i, rec);
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
    fn split_leaf_and_insert(&mut self, w: PinnedWrite<'p>, path: Vec<u32>, at: usize, rec: Vec<u8>) -> Result<()> {
        #[cfg(not(feature = "slotref-split"))]
        { self.split_leaf_and_insert_vec(w, path, at, rec) }
        #[cfg(feature = "slotref-split")]
        { self.split_leaf_and_insert_ref(w, path, at, rec) }
    }

    #[cfg(any(test, not(feature = "slotref-split")))]
    fn split_leaf_and_insert_vec(&mut self, mut w: PinnedWrite<'p>, mut path: Vec<u32>, at: usize, rec: Vec<u8>) -> Result<()> {
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

        self.insert_separator(&mut path, &sep, right_no)
    }

    /// L2.3-2: the same split, with every record left where it already is.
    ///
    /// Identical to `split_leaf_and_insert_vec` in every decision it takes and
    /// every byte it writes -- see the module comment above `SlotRef` and the
    /// `split_byte_equivalence` oracle. The difference is that the leaf's
    /// records are named by `SlotRef`s into one 4 KiB copy of the page image
    /// instead of being copied into one `Vec<u8>` each.
    #[cfg(any(test, feature = "slotref-split"))]
    fn split_leaf_and_insert_ref(&mut self, w: PinnedWrite<'p>, path: Vec<u32>, at: usize, rec: Vec<u8>) -> Result<()> {
        // Re-entrancy: nothing `split_core` can reach -- `insert_separator`,
        // `descend_for_write`, `shadow_page`, `redistribute_neighbors_ref` --
        // splits a LEAF, so these three borrows cannot nest. That is what lets
        // the separator key stay a borrow into scratch instead of a `Vec`.
        SPLIT_LEAF.with(|li| SPLIT_SLOTS.with(|sl| SPLIT_SCRATCH.with(|ss| {
            let mut leaf_img = li.borrow_mut();
            let mut slots = sl.borrow_mut();
            let mut sc = ss.borrow_mut();
            self.split_core(w, path, at, &rec, &mut leaf_img, &mut slots, &mut sc)
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
        self.insert_separator(&mut path, sep, right_no)
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

        for &(start, count) in &windows[..nw] {
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
                if p.kind() != PageKind::Leaf || p.tree_id() != self.tree_id {
                    return Err(Error::Corrupt { page_no: no, why: "redistribution sibling identity" });
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
        for (start,count) in windows{
            let mut all=Vec::new();let mut next=0;
            for &no in &child_ids[start..start+count]{
                if no==leaf{all.extend(current.iter().cloned());next=current_next;}
                else{let r=self.pool.get(no)?;let p=open_cached(&r,no)?;
                    if p.kind()!=PageKind::Leaf || p.tree_id()!=self.tree_id{return Err(Error::Corrupt{page_no:no,why:"redistribution sibling identity"});}
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
            self.last_leaf.set(None);return Ok(true);
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
                    let (mut all, left_link) = {
                        let r=self.pool.get(left)?;let p=open_cached(&r,left)?;
                        if p.kind()!=kind || p.tree_id()!=self.tree_id {return Err(Error::Corrupt {page_no:left,why:"delete left sibling identity"});}
                        ((0..p.nentries()).map(|i|p.slot(i).to_vec()).collect::<Vec<_>>(),p.next_leaf())
                    };
                    let (right_link, right_birth) = {
                        let r=self.pool.get(right)?;let p=open_cached(&r,right)?;
                        if p.kind()!=kind || p.tree_id()!=self.tree_id {return Err(Error::Corrupt {page_no:right,why:"delete right sibling identity"});}
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
        let (leaf, path) = self.descend_with_path(from)?;
        let idx = { let r = self.pool.get(leaf)?; lower_bound(&open_cached(&r, leaf)?, from)? };
        Ok(RangeIter {
            pool: self.pool, tree_id: self.tree_id, page: leaf, idx,
            done: false, leaves: 1, max_leaves: self.pool.page_count(),
            buf: std::collections::VecDeque::new(),
            served: 0,
            path,
        })
    }

    /// Scan keys strictly below `to` in descending order.
    pub fn range_reverse(&self, to: &[u8]) -> Result<ReverseRangeIter<'p>> {
        let (leaf, path) = self.descend_with_path(to)?;
        let idx = { let r = self.pool.get(leaf)?; lower_bound(&open_cached(&r, leaf)?, to)? };
        Ok(ReverseRangeIter {
            pool: self.pool, tree_id: self.tree_id, page: leaf, idx,
            done: false, leaves: 1, max_leaves: self.pool.page_count(), path,
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

    /// Visit descending records as borrows into one pinned leaf. The cursor is
    /// bounded by the buffer pool; a caller stopping after `k` entries pays for
    /// only the pages containing those entries.
    pub fn for_each_ref(mut self, mut f: impl FnMut(&[u8], &[u8]) -> bool) -> Result<()> {
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
}

impl Iterator for RangeIter<'_> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;
    fn next(&mut self) -> Option<Self::Item> {
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
            Impl::Vec => t.split_leaf_and_insert_vec(w, path, at, rec).unwrap(),
            Impl::Ref => t.split_leaf_and_insert_ref(w, path, at, rec).unwrap(),
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
                if slotref { t.split_leaf_and_insert_ref(w, path, at, rec) }
                else { t.split_leaf_and_insert_vec(w, path, at, rec) }.unwrap()
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
