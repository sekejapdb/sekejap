//! Independent verification for replacement trees.
//!
//! Recovery and bulk loading both build pages that are not authoritative yet.
//! This module reopens those bytes through a fresh, fixed-size pool and walks
//! the complete reachable graph before either caller publishes the root. The
//! verifier retains one page of separators per level (bounded by page size and
//! tree height), never a visited set proportional to the database.

use crate::btree::{OVERFLOW_VLEN, OV_CAP, OV_DATA, OV_NEXT, OV_USED};
use crate::budget::MemoryBudget;
use crate::io::{open_file, IoMode};
use crate::meta::Meta;
use crate::page::{PageKind, PageRef, PAGE_SIZE};
use crate::pool::BufferPool;
use crate::{Error, Result};
use std::path::Path;
use std::sync::Arc;

const VERIFY_FRAMES: usize = 16;
const MAX_TREE_DEPTH: usize = 64;

#[derive(Debug, Clone, Copy)]
pub(crate) struct VerifiedTree {
    pub rows: u64,
    pub pages: u64,
}

#[derive(Default)]
struct State {
    rows: u64,
    pages: u64,
    page_count: u32,
    expected_generation: Option<u64>,
}

struct Bounds {
    min: Option<Vec<u8>>,
    max: Option<Vec<u8>>,
    first_leaf: u32,
    last_leaf: u32,
    last_next: u32,
}

fn bad(page_no: u32, why: &'static str) -> Error {
    Error::Corrupt { page_no, why }
}

fn open_pool(path: &Path, mode: IoMode) -> Result<BufferPool> {
    let (file, _) = open_file(path, mode)?;
    let len = file.len()?;
    if len % PAGE_SIZE as u64 != 0 {
        return Err(bad(0, "replacement file is not page aligned"));
    }
    let pages = len / PAGE_SIZE as u64;
    if pages < 3 || pages > u32::MAX as u64 {
        return Err(bad(
            0,
            "replacement file page count is outside the format bound",
        ));
    }
    let budget = Arc::new(MemoryBudget::new(VERIFY_FRAMES * PAGE_SIZE));
    BufferPool::new(file.into(), budget, VERIFY_FRAMES)
}

/// Walk a PUBLISHED tree and prove its structure: every separator inside its
/// parent's range, every level strictly ordered, every page this tree's own,
/// the leaf chain intact. Row count is whatever the walk finds — this asks
/// "is the shape sound", not "does it hold the rows a build promised".
///
/// The graft path verifies its candidate subtree before publication, but
/// `install_graft` then rewrites the parent path, and nothing re-read THAT.
/// A boundary anchored one child left of a stale separator produced a tree
/// whose root disagreed with its own leaves — full scans saw the keys, seeks
/// skipped them. This is the check that catches that class at its source.
pub fn verify_published_tree(path: &Path, mode: IoMode, root: u32, tree_id: u16)
    -> Result<(u64, u64)>
{
    let pool = open_pool(path, mode)?;
    let mut rows = 0u64;
    let mut pages = 0u64;
    let depth_limit = MAX_TREE_DEPTH as u32;
    published_walk(&pool, root, tree_id, None, None, 0, depth_limit, &mut rows, &mut pages)?;
    Ok((rows, pages))
}

/// The key-order half of `walk`, without its sibling-chain requirement.
///
/// A freshly packed candidate's `next_leaf` pointers form one exact chain, and
/// `walk` checks that. A LIVE tree's do not: copy-on-write moves a modified
/// leaf to a new page, leaving its neighbour's stored pointer naming the stale
/// version — which is precisely why `RangeIter` advances through the parent
/// path and never follows `next_leaf`. Demanding the chain here would report
/// healthy trees as corrupt. What must hold, and what this proves, is the
/// property the graft defect violated: every key sits inside the interval its
/// ancestors' separators claim for it.
#[allow(clippy::too_many_arguments)]
fn published_walk(
    pool: &BufferPool,
    page_no: u32,
    tree_id: u16,
    lower: Option<&[u8]>,
    upper: Option<&[u8]>,
    depth: u32,
    depth_limit: u32,
    rows: &mut u64,
    pages: &mut u64,
) -> Result<()> {
    if depth > depth_limit {
        return Err(bad(page_no, "published tree exceeds the format depth bound"));
    }
    let read = pool.get(page_no)?;
    let page = crate::page::PageRef::open(&read, page_no)?;
    if page.tree_id() != tree_id {
        return Err(bad(page_no, "published page belongs to another tree"));
    }
    *pages += 1;
    if page.kind() == PageKind::Leaf {
        let mut previous: Option<Vec<u8>> = None;
        for index in 0..page.nentries() {
            let record = page.slot(index);
            let key = match decode_record(record, page_no, PageKind::Leaf)? {
                DecodedRecord::Leaf { key, value, overflow } => {
                    if overflow {
                        let mut state = State { page_count: pool.page_count(), ..State::default() };
                        verify_overflow(pool, value, &mut state)?;
                    }
                    key
                },
                _ => return Err(bad(page_no, "leaf holds an interior record")),
            };
            if let Some(previous) = &previous {
                if key <= previous.as_slice() {
                    return Err(bad(page_no, "published leaf keys are not strictly ordered"));
                }
            }
            if lower.is_some_and(|bound| key < bound) {
                return Err(bad(page_no, "published leaf key is below its separator"));
            }
            if upper.is_some_and(|bound| key >= bound) {
                return Err(bad(page_no, "published leaf key is at or above its separator"));
            }
            previous = Some(key.to_vec());
            *rows += 1;
        }
        return Ok(());
    }
    if page.kind() != PageKind::Interior {
        return Err(bad(page_no, "published tree reaches a non-tree page"));
    }
    let mut separators: Vec<(Vec<u8>, u32)> = Vec::with_capacity(page.nentries());
    for index in 0..page.nentries() {
        let record = page.slot(index);
        let DecodedRecord::Interior { key, child } =
            decode_record(record, page_no, PageKind::Interior)?
        else {
            return Err(bad(page_no, "interior page holds a leaf record"));
        };
        if separators.last().is_some_and(|(previous, _)| key <= previous.as_slice()) {
            return Err(bad(page_no, "published separators are not strictly ordered"));
        }
        if lower.is_some_and(|bound| key < bound) || upper.is_some_and(|bound| key >= bound) {
            return Err(bad(page_no, "published separator is outside its parent range"));
        }
        separators.push((key.to_vec(), child));
    }
    for index in 0..=separators.len() {
        let child = if index == 0 { page.child0() } else { separators[index - 1].1 };
        let child_lower = if index == 0 { lower } else { Some(separators[index - 1].0.as_slice()) };
        let child_upper = if index == separators.len() { upper } else { Some(separators[index].0.as_slice()) };
        published_walk(pool, child, tree_id, child_lower, child_upper,
            depth + 1, depth_limit, rows, pages)?;
    }
    Ok(())
}

pub(crate) fn verify_file(
    path: &Path,
    mode: IoMode,
    root: u32,
    tree_id: u16,
    expected_rows: u64,
) -> Result<VerifiedTree> {
    let pool = open_pool(path, mode)?;
    verify_tree(&pool, root, tree_id, expected_rows, None, None, 0)
}

/// Verify an unpublished graft candidate through a fresh file handle and
/// fixed-size pool.  Unlike a whole replacement tree, the candidate's final
/// leaf legitimately points at the first standing leaf to its right.
pub(crate) fn verify_range_file(
    path: &Path,
    mode: IoMode,
    root: u32,
    tree_id: u16,
    expected_rows: u64,
    expected_min: &[u8],
    expected_max: &[u8],
    expected_next: u32,
) -> Result<VerifiedTree> {
    verify_range_file_generation(path, mode, root, tree_id, expected_rows,
        expected_min, expected_max, expected_next, None)
}

pub(crate) fn verify_range_file_generation(
    path: &Path,
    mode: IoMode,
    root: u32,
    tree_id: u16,
    expected_rows: u64,
    expected_min: &[u8],
    expected_max: &[u8],
    expected_next: u32,
    expected_generation: Option<u64>,
) -> Result<VerifiedTree> {
    let pool = open_pool(path, mode)?;
    verify_tree_generation(
        &pool,
        root,
        tree_id,
        expected_rows,
        Some(expected_min),
        Some(expected_max),
        expected_next,
        expected_generation,
    )
}

/// Verify a packed graft candidate in the LIVE pool, before anything points
/// at it.
///
/// `verify_range_file` reopens the data file through a second handle because
/// the pages it checks were written straight to the medium, outside the log:
/// there, the reopen is what makes the check independent of the writer that
/// produced the bytes. A page-WAL graft's pages are ordinary logged pages that
/// have not reached the data file at all yet, so there is nothing to reopen —
/// the same walk runs over the pool.
///
/// SACRIFICE (Law 4): this walk trusts the pool's page images rather than a
/// second read of the medium, so it cannot catch a fault introduced between
/// the pool and the disk. It is not asked to: the graft publishes through the
/// ordinary commit, so those bytes cross the medium boundary as WAL frames
/// with their own checksums and are verified on the way back like every other
/// page (Law 5 unchanged). What this catches is the class the packer itself
/// can produce — a short separator level, an unreachable subtree, a key
/// outside its separator's interval, a broken leaf chain — before a single
/// page of the standing tree is rewritten.
pub(crate) fn verify_range_pool(
    pool: &BufferPool,
    root: u32,
    tree_id: u16,
    expected_rows: u64,
    expected_min: &[u8],
    expected_max: &[u8],
    expected_next: u32,
) -> Result<VerifiedTree> {
    verify_tree_generation(pool, root, tree_id, expected_rows, Some(expected_min),
        Some(expected_max), expected_next, None)
}

pub(crate) fn verify_rebuild(
    path: &Path,
    mode: IoMode,
    expected_meta: &Meta,
    expected_rows: u64,
) -> Result<VerifiedTree> {
    let pool = open_pool(path, mode)?;
    let meta = Meta::read_latest(&pool)?;
    if meta.format_version != expected_meta.format_version
        || meta.roots != expected_meta.roots
        || meta.next_lsn != expected_meta.next_lsn
        || meta.generation != expected_meta.generation
    {
        return Err(bad(
            0,
            "reopened replacement manifest does not match the rebuild",
        ));
    }
    verify_tree(&pool, meta.roots[0], 1, expected_rows, None, None, 0)
}

/// Persist an ordinary checkpoint's derived hint AFTER its new root is durable.
/// The preceding generation's hint is already invalid for that root, so a torn
/// overwrite sacrifices reuse knowledge only. Recovery must keep using the
/// independently verified replacement protocol below: it renumbers live pages.
/// No filename is replaced. Only creation needs a directory barrier; content
/// synchronization remains unconditional, just as in `publish_freelist`.
pub(crate) fn persist_checkpoint_freelist(
    dir: &Path,
    bytes: &[u8],
    directory_file: &dyn crate::io::FileIo,
) -> Result<()> {
    use std::io::Write;
    let path = dir.join("free");
    let (mut file, created) = match std::fs::OpenOptions::new().write(true).open(&path) {
        Ok(file) => (file, false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound =>
            (std::fs::OpenOptions::new().write(true).create_new(true).open(&path)?, true),
        Err(e) => return Err(e.into()),
    };
    // The whole-body CRC also covers the generation. A short write, stale tail,
    // truncation or failed sync cannot create an unchecked reusable-page list.
    file.write_all(bytes)?;
    file.set_len(bytes.len() as u64)?;
    crate::write_stats::add(crate::write_stats::Phase::Sidecar, bytes.len() as u64);
    file.sync_all()?;
    if created { directory_file.sync_dir()?; }
    Ok(())
}

/// Publish a reusable-page sidecar beside the standing file: write and flush a
/// candidate, optionally reopen and verify it, then rename. Recovery requests
/// independent verification because this empty sidecar must become durable
/// before rebuilt page numbers are published. Ordinary checkpoint does not:
/// the framed CRC and generation gate make any bad candidate leak-only on
/// reopen, and a second full walk would repeat work proportional to old frees.
/// The caller supplies the data file's directory handle so it can order this
/// publication against a metadata flip or a recovery rename.
pub(crate) fn publish_freelist(
    dir: &Path,
    bytes: Vec<u8>,
    expected_generation: u64,
    page_count: u32,
    directory_file: &dyn crate::io::FileIo,
    verify_candidate: bool,
    sync_directory_now: bool,
) -> Result<()> {
    let tmp = dir.join("free.tmp");
    let published = dir.join("free");
    let result = (|| {
        std::fs::write(&tmp, &bytes)?;
        crate::write_stats::add(crate::write_stats::Phase::Sidecar, bytes.len() as u64);
        std::fs::File::open(&tmp)?.sync_all()?;
        drop(bytes);
        if verify_candidate {
            let reopened = std::fs::read(&tmp)?;
            if !crate::pool::BufferPool::verify_free(
                &reopened,
                expected_generation,
                page_count,
            ) {
                return Err(bad(0, "reopened freelist candidate failed verification"));
            }
        }
        std::fs::rename(&tmp, &published)?;
        if sync_directory_now {
            directory_file.sync_dir()?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn verify_tree(
    pool: &BufferPool,
    root: u32,
    tree_id: u16,
    expected_rows: u64,
    expected_min: Option<&[u8]>,
    expected_max: Option<&[u8]>,
    expected_next: u32,
) -> Result<VerifiedTree> {
    verify_tree_generation(pool, root, tree_id, expected_rows, expected_min,
        expected_max, expected_next, None)
}

fn verify_tree_generation(
    pool: &BufferPool,
    root: u32,
    tree_id: u16,
    expected_rows: u64,
    expected_min: Option<&[u8]>,
    expected_max: Option<&[u8]>,
    expected_next: u32,
    expected_generation: Option<u64>,
) -> Result<VerifiedTree> {
    let mut state = State {
        page_count: pool.page_count(),
        expected_generation,
        ..State::default()
    };
    let bounds = walk(pool, root, tree_id, None, None, true, 0, &mut state)?;
    if bounds.last_next != expected_next {
        return Err(bad(
            bounds.last_leaf,
            if expected_next == 0 {
                "rightmost leaf points past the verified tree"
            } else {
                "rightmost leaf does not name the graft continuation"
            },
        ));
    }
    if state.rows != expected_rows {
        return Err(bad(
            root,
            "replacement row count does not match its manifest",
        ));
    }
    if expected_min.is_some_and(|want| bounds.min.as_deref() != Some(want)) {
        return Err(bad(root, "replacement minimum does not match its manifest"));
    }
    if expected_max.is_some_and(|want| bounds.max.as_deref() != Some(want)) {
        return Err(bad(root, "replacement maximum does not match its manifest"));
    }
    Ok(VerifiedTree {
        rows: state.rows,
        pages: state.pages,
    })
}

fn visit(state: &mut State, page_no: u32) -> Result<()> {
    if page_no < 2 || page_no >= state.page_count {
        return Err(bad(page_no, "replacement graph points outside the file"));
    }
    state.pages = state
        .pages
        .checked_add(1)
        .ok_or_else(|| bad(page_no, "replacement page count overflow"))?;
    if state.pages > state.page_count as u64 {
        return Err(bad(page_no, "replacement graph cycles or repeats pages"));
    }
    Ok(())
}

fn walk(
    pool: &BufferPool,
    page_no: u32,
    tree_id: u16,
    lower: Option<&[u8]>,
    upper: Option<&[u8]>,
    is_root: bool,
    depth: usize,
    state: &mut State,
) -> Result<Bounds> {
    if depth > MAX_TREE_DEPTH {
        return Err(bad(
            page_no,
            "replacement tree exceeds the format depth bound",
        ));
    }
    visit(state, page_no)?;
    let r = pool.get(page_no)?;
    let page = PageRef::open_resident(&r, page_no)?;
    if state.expected_generation.is_some_and(|want| page.lsn() != want) {
        return Err(bad(page_no, "replacement page belongs to another build generation"));
    }
    if page.tree_id() != tree_id {
        return Err(bad(page_no, "replacement page belongs to another tree"));
    }

    match page.kind() {
        PageKind::Leaf => verify_leaf(pool, &page, lower, upper, is_root, state),
        PageKind::Interior => {
            if page.nentries() == 0 {
                return Err(bad(
                    page_no,
                    "replacement interior page has fewer than two children",
                ));
            }
            let child0 = page.child0();
            let mut entries: Vec<(Vec<u8>, u32)> = Vec::with_capacity(page.nentries());
            for i in 0..page.nentries() {
                let record = decode_record(page.slot(i), page_no, PageKind::Interior)?;
                let DecodedRecord::Interior { key, child } = record else { unreachable!() };
                if i > 0 && entries[i - 1].0.as_slice() >= key {
                    return Err(bad(
                        page_no,
                        "replacement separators are not strictly ordered",
                    ));
                }
                if lower.is_some_and(|bound| key < bound) || upper.is_some_and(|bound| key >= bound)
                {
                    return Err(bad(
                        page_no,
                        "replacement separator is outside its parent range",
                    ));
                }
                entries.push((key.to_vec(), child));
            }
            drop(r);

            let mut combined: Option<Bounds> = None;
            for index in 0..=entries.len() {
                let child = if index == 0 {
                    child0
                } else {
                    entries[index - 1].1
                };
                let child_lower = if index == 0 {
                    lower
                } else {
                    Some(entries[index - 1].0.as_slice())
                };
                let child_upper = if index == entries.len() {
                    upper
                } else {
                    Some(entries[index].0.as_slice())
                };
                let got = walk(
                    pool,
                    child,
                    tree_id,
                    child_lower,
                    child_upper,
                    false,
                    depth + 1,
                    state,
                )?;
                if index > 0 && got.min.as_deref() != child_lower {
                    return Err(bad(
                        child,
                        "replacement child minimum does not match its separator",
                    ));
                }
                if let Some(acc) = &combined {
                    if acc.last_next != got.first_leaf {
                        return Err(bad(
                            acc.last_leaf,
                            "replacement leaf linkage disagrees with the root graph",
                        ));
                    }
                }
                combined = Some(match combined {
                    None => got,
                    Some(acc) => Bounds {
                        min: acc.min,
                        max: got.max,
                        first_leaf: acc.first_leaf,
                        last_leaf: got.last_leaf,
                        last_next: got.last_next,
                    },
                });
            }
            combined.ok_or_else(|| bad(page_no, "replacement interior page has no children"))
        }
        _ => Err(bad(
            page_no,
            "replacement root graph reaches a non-tree page",
        )),
    }
}

fn verify_leaf(
    pool: &BufferPool,
    page: &PageRef<'_>,
    lower: Option<&[u8]>,
    upper: Option<&[u8]>,
    is_root: bool,
    state: &mut State,
) -> Result<Bounds> {
    let page_no = page.page_no();
    if page.nentries() == 0 && !is_root {
        return Err(bad(page_no, "replacement contains an empty non-root leaf"));
    }
    let mut min = None;
    let mut max: Option<Vec<u8>> = None;
    for i in 0..page.nentries() {
        let record = decode_record(page.slot(i), page_no, PageKind::Leaf)?;
        let DecodedRecord::Leaf { key, value, overflow } = record else { unreachable!() };
        if max.as_deref().is_some_and(|previous| previous >= key) {
            return Err(bad(
                page_no,
                "replacement leaf keys are not strictly ordered",
            ));
        }
        if lower.is_some_and(|bound| key < bound) || upper.is_some_and(|bound| key >= bound) {
            return Err(bad(
                page_no,
                "replacement leaf key is outside its parent range",
            ));
        }
        if overflow {
            verify_overflow(pool, value, state)?;
        }
        if min.is_none() {
            min = Some(key.to_vec());
        }
        max = Some(key.to_vec());
        state.rows = state
            .rows
            .checked_add(1)
            .ok_or_else(|| bad(page_no, "replacement row count overflow"))?;
    }
    Ok(Bounds {
        min,
        max,
        first_leaf: page_no,
        last_leaf: page_no,
        last_next: page.next_leaf(),
    })
}

/// The one decoder for records stored in B-tree page slots. Ordinary reads,
/// replacement verification, and recovery all enter through this function;
/// none may index a disk-derived key length, value length, marker, or child
/// pointer first. Keeping the decoder here extends the independent verifier's
/// boundary instead of creating a recovery-only parser that can drift.
#[derive(Debug, Clone, Copy)]
pub enum DecodedRecord<'a> {
    Leaf { key: &'a [u8], value: &'a [u8], overflow: bool },
    Interior { key: &'a [u8], child: u32 },
}

pub fn decode_record(
    rec: &[u8],
    page_no: u32,
    kind: PageKind,
) -> Result<DecodedRecord<'_>> {
    // Both compact families decode unconditionally. The encoding belongs to
    // the stored database, not to the build: gating these here made a binary
    // compiled without `compact-cells` refuse pages written by one compiled
    // with it, and disagree with the fast validated helpers in `btree.rs`,
    // which never gated the integer family at all.
    if kind==PageKind::Leaf && rec.first()==Some(&0xff)
        && rec.get(1).is_some_and(|b|(0x81..=0x88).contains(b)) {
        let end=2+(rec[1]-0x80)as usize;
        let key=rec.get(1..end).ok_or_else(||bad(page_no,"compact integer key crosses its slot"))?;
        return Ok(DecodedRecord::Leaf{key,value:&rec[end..],overflow:false});
    }
    if kind==PageKind::Leaf && rec.get(1).is_some_and(|b|b&0xf0==0x40) {
        let end=2+(u16::from_le_bytes([rec[0],rec[1]])&0x0fff) as usize;
        let key=rec.get(2..end).ok_or_else(||bad(page_no,"compact key crosses its slot"))?;
        return Ok(DecodedRecord::Leaf{key,value:&rec[end..],overflow:false});
    }
    let klen_bytes = rec
        .get(..2)
        .ok_or_else(|| bad(page_no, "record has no key length"))?;
    let klen = u16::from_le_bytes(klen_bytes.try_into().unwrap()) as usize;
    let key_end = 2usize
        .checked_add(klen)
        .ok_or_else(|| bad(page_no, "record key boundary overflow"))?;
    let key = rec
        .get(2..key_end)
        .ok_or_else(|| bad(page_no, "record key crosses its slot"))?;

    if kind == PageKind::Interior {
        let end = key_end
            .checked_add(4)
            .ok_or_else(|| bad(page_no, "interior child boundary overflow"))?;
        if end != rec.len() {
            return Err(bad(page_no, "interior record length does not match its slot"));
        }
        let child = u32::from_le_bytes(rec[key_end..end].try_into().unwrap());
        return Ok(DecodedRecord::Interior { key, child });
    }
    if kind != PageKind::Leaf {
        return Err(bad(page_no, "non-tree page contains a tree record"));
    }

    let vlen_end = key_end
        .checked_add(2)
        .ok_or_else(|| bad(page_no, "leaf value boundary overflow"))?;
    let vlen_bytes = rec
        .get(key_end..vlen_end)
        .ok_or_else(|| bad(page_no, "leaf record has no value length"))?;
    let vlen = u16::from_le_bytes(vlen_bytes.try_into().unwrap());
    if vlen == OVERFLOW_VLEN {
        let end = vlen_end
            .checked_add(12)
            .ok_or_else(|| bad(page_no, "overflow marker boundary overflow"))?;
        let marker = rec
            .get(vlen_end..end)
            .ok_or_else(|| bad(page_no, "overflow marker crosses its slot"))?;
        if end != rec.len() {
            return Err(bad(page_no, "overflow marker has trailing bytes"));
        }
        return Ok(DecodedRecord::Leaf { key, value: marker, overflow: true });
    }
    let end = vlen_end
        .checked_add(vlen as usize)
        .ok_or_else(|| bad(page_no, "leaf value boundary overflow"))?;
    if end != rec.len() {
        return Err(bad(page_no, "leaf value length does not match its slot"));
    }
    Ok(DecodedRecord::Leaf { key, value: &rec[vlen_end..end], overflow: false })
}

fn verify_overflow(pool: &BufferPool, marker: &[u8], state: &mut State) -> Result<()> {
    let total = u32::from_le_bytes(marker[0..4].try_into().unwrap()) as usize;
    let mut page_no = u32::from_le_bytes(marker[4..8].try_into().unwrap());
    let want_crc = u32::from_le_bytes(marker[8..12].try_into().unwrap());
    let expected_pages = total.div_ceil(OV_CAP).max(1);
    let mut seen = 0usize;
    let mut bytes = 0usize;
    let mut crc = 0u32;
    while page_no != 0 {
        seen = seen
            .checked_add(1)
            .ok_or_else(|| bad(page_no, "overflow page count overflow"))?;
        if seen > expected_pages {
            return Err(bad(
                page_no,
                "replacement overflow chain cycles or is too long",
            ));
        }
        visit(state, page_no)?;
        let r = pool.get(page_no)?;
        let page = PageRef::open_resident(&r, page_no)?;
        if state.expected_generation.is_some_and(|want| page.lsn() != want) {
            return Err(bad(page_no, "replacement overflow page belongs to another build generation"));
        }
        if page.kind() != PageKind::Overflow || page.tree_id() != 0 {
            return Err(bad(
                page_no,
                "replacement overflow chain reaches the wrong page kind",
            ));
        }
        let used = u16::from_le_bytes(r[OV_USED..OV_USED + 2].try_into().unwrap()) as usize;
        let next = u32::from_le_bytes(r[OV_NEXT..OV_NEXT + 4].try_into().unwrap());
        if used > OV_CAP || bytes.checked_add(used).is_none_or(|n| n > total) {
            return Err(bad(page_no, "replacement overflow length is out of bounds"));
        }
        let chunk = &r[OV_DATA..OV_DATA + used];
        crc = if bytes == 0 {
            crc32c::crc32c(chunk)
        } else {
            crc32c::crc32c_append(crc, chunk)
        };
        bytes += used;
        page_no = next;
    }
    if seen != expected_pages || bytes != total || crc != want_crc {
        return Err(bad(
            0,
            "replacement overflow value fails its manifest checksum",
        ));
    }
    Ok(())
}
