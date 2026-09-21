//! Recovery primitives and source-preserving salvage.
//!
//! Use [`recover_to`] for the E4 recovery workflow. It keeps the source intact,
//! follows verified published ancestry, streams named losses, and places raw
//! rootless evidence in a separate archive with no current-membership claim.
//!
//! The older [`recover`] function below is a low-level, in-place forensic
//! rebuild retained for kernel regression coverage. Its global leaf sweep can
//! include obsolete CoW versions and resurrect deletes. Generation/page-number
//! dedup does not establish current membership; sibling pointers can also be
//! stale. A broken overflow aborts that strict rebuild. It is NOT the safe E4
//! recovery API and must not be used to publish a repaired user database.
//!
//! See docs/RECOVERY_CONTRACT.md in the E4 root for the supported fault model,
//! distinctions between current survivors and candidates, and open law gates.

use crate::bulk::{pack_tree, ExternalSort};
use crate::budget::MemoryBudget;
use crate::io::{open_file, FileIo};
use crate::meta::Meta;
use crate::page::{PageKind, PageRef, PAGE_SIZE};
use crate::pool::BufferPool;
use crate::store::Config;
use crate::Result;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

mod safe;
pub use safe::{recover_to, RecoveryClass, SafeRecoveryReport};
mod reader;
pub use reader::{CandidateReader, LeafCandidate, LeafEvent, LeafScanReport};

/// A leaf that failed verification, and what can honestly be said about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LostLeaf {
    pub page_no: u32,
    /// Historical sibling hint from the rootless sweep. CoW sibling pointers
    /// can be stale: this is NOT an authenticated bound on current lost keys.
    /// The safe API reports bounds only from verified parent separators.
    pub after_key: Option<Vec<u8>>,
}

#[derive(Debug, Default)]
pub struct RecoveryReport {
    /// Total page reads attempted, across BOTH sweeps -- the first (which
    /// collects entries and losses) and the second (which walks the sibling
    /// chain to name each loss). Reflects actual I/O cost, not distinct pages
    /// in the file; a store with any losses is read twice.
    pub pages_scanned: u64,
    pub leaves_kept: u64,
    pub leaves_lost: u64,
    /// Entries actually present in the rebuilt tree -- after deduplication, so
    /// this is exactly what a caller's `scan` will count, never a raw sum of
    /// pushes into leaves that may have overlapped.
    pub entries_recovered: u64,
    /// One entry per lost leaf. See `LostLeaf`.
    pub lost_ranges: Vec<LostLeaf>,
    /// Bytes at the end of the file that do not make up a complete page
    /// (file length not a multiple of `PAGE_SIZE`) -- e.g. a crash mid-extend,
    /// or a full disk. Named, not classified: the page's own kind is
    /// unknowable, because its header may be exactly the part that is
    /// missing. NOT included in `leaves_lost` -- a page that was never even a
    /// candidate to be a leaf is not a leaf loss, it is a separate, honestly
    /// unclassifiable fact about the file.
    pub truncated_tail_bytes: u64,
    /// Where the write-ahead log was copied to, if it held damage `Wal::open`
    /// refuses to walk past (`wal::Stop::Damaged`). Every original byte is in
    /// that file; the live `wal` is reconstructed from independently verified
    /// committed regions. `None` means the log was fine and was not
    /// touched at all -- the ordinary case, and the one this module's doc
    /// comment promises.
    pub wal_quarantined: Option<std::path::PathBuf>,
    /// Bytes in the reconstructed live log. Ambiguous regions remain in the
    /// quarantined original for a repair tool or person to inspect.
    pub wal_bytes_kept: u64,
    /// Size of the log that was set aside, in full.
    pub wal_bytes_set_aside: u64,
}

/// The outcome of attempting to read and verify page `no`.
enum ReadOutcome<'b> {
    /// Read succeeded and the page verified.
    Verified(PageRef<'b>),
    /// The read succeeded, but the page failed to verify (bad CRC, identity,
    /// etc). `buf` genuinely holds THIS page's own (corrupted) bytes, so a
    /// raw kind byte read from it describes what the page actually claimed
    /// to be, even though the page as a whole cannot be trusted.
    ReadOk,
    /// The read itself failed. `buf` was never overwritten for this `no`, so
    /// it still holds whatever a PREVIOUS iteration (or nothing) left there
    /// -- not this page's bytes at all. Nothing about it, including a raw
    /// kind byte, may be trusted or used to classify this page.
    ReadFailed,
}

/// Read and verify page `no`, distinguishing "the read itself failed" from
/// "the read succeeded but the page did not verify" -- callers that fall back
/// to a raw kind byte on failure need to know which, because only the second
/// case leaves that byte meaningful.
fn read_page<'b>(file: &dyn FileIo, buf: &'b mut [u8], no: u64) -> ReadOutcome<'b> {
    if file.read_at(buf, no * PAGE_SIZE as u64).is_err() { return ReadOutcome::ReadFailed; }
    match PageRef::open(buf, no as u32) {
        Ok(p) => ReadOutcome::Verified(p),
        Err(_) => ReadOutcome::ReadOk,
    }
}

/// Read and verify page `no`. `None` covers both outcomes in which the page
/// cannot be used as itself (a failed read or a failed verification) --
/// collapsed to one outcome because callers here only ever need "is this
/// page trustworthy", never why it was not. (The one caller that DOES need
/// why -- the first sweep's lost-leaf classification, which falls back to a
/// raw kind byte -- uses `read_page` directly instead.)
fn read_verified<'b>(file: &dyn FileIo, buf: &'b mut [u8], no: u64) -> Option<PageRef<'b>> {
    match read_page(file, buf, no) {
        ReadOutcome::Verified(p) => Some(p),
        ReadOutcome::ReadOk | ReadOutcome::ReadFailed => None,
    }
}

/// Recovery deliberately shares the normal/verifier record decoder. A
/// recovery-only parser is most likely to be exercised by malformed bytes and
/// therefore must not have weaker bounds checks than the ordinary reader.
fn decode_leaf_record(rec: &[u8], page_no: u32) -> Result<(&[u8], &[u8], bool)> {
    match crate::verify::decode_record(rec, page_no, PageKind::Leaf)? {
        crate::verify::DecodedRecord::Leaf { key, value, overflow } => {
            Ok((key, value, overflow))
        }
        crate::verify::DecodedRecord::Interior { .. } => unreachable!(),
    }
}

/// Overflow markers name pages in the SOURCE file. Repacking leaves alone
/// cannot preserve those addresses. Stream verified chunks into the new pool
/// and rewrite the marker, using one scratch page and at most one write guard.
/// Any failure leaves the original file untouched by the caller's publish gate.
fn copy_overflow(file: &dyn FileIo, pool: &BufferPool, marker: &[u8]) -> Result<Vec<u8>> {
    use crate::btree::{OV_CAP, OV_DATA, OV_NEXT, OV_USED};
    use crate::page::PageMut;
    let bad = |page_no, why| crate::Error::Corrupt { page_no, why };
    if marker.len() != 12 { return Err(bad(0, "recovery overflow marker wrong size")); }
    let total = u32::from_le_bytes(marker[0..4].try_into().unwrap()) as usize;
    let mut source = u32::from_le_bytes(marker[4..8].try_into().unwrap());
    let want_crc = u32::from_le_bytes(marker[8..12].try_into().unwrap());
    let expected_pages = total.div_ceil(OV_CAP).max(1);
    let file_pages = file.len()? / PAGE_SIZE as u64;
    let (mut seen, mut bytes, mut crc) = (0usize, 0usize, 0u32);
    let (mut head, mut previous) = (0u32, 0u32);
    let mut buf = [0u8; PAGE_SIZE];
    while source != 0 {
        seen += 1;
        if seen > expected_pages || source as u64 >= file_pages {
            return Err(bad(source, "recovery overflow chain cycles or exceeds file"));
        }
        file.read_at(&mut buf, source as u64 * PAGE_SIZE as u64)?;
        let page = PageRef::open(&buf, source)?;
        if page.kind() != PageKind::Overflow || page.tree_id() != 0 {
            return Err(bad(source, "recovery overflow chain reaches wrong page kind"));
        }
        let used = u16::from_le_bytes(buf[OV_USED..OV_USED + 2].try_into().unwrap()) as usize;
        let next = u32::from_le_bytes(buf[OV_NEXT..OV_NEXT + 4].try_into().unwrap());
        if used > OV_CAP || bytes.checked_add(used).is_none_or(|n| n > total) {
            return Err(bad(source, "recovery overflow length out of bounds"));
        }
        let chunk = &buf[OV_DATA..OV_DATA + used];
        crc = crc32c::crc32c_append(crc, chunk);
        bytes += used;
        let mut w = pool.allocate()?;
        let dest = w.page_no();
        let b = w.bytes_mut();
        PageMut::init(b, PageKind::Overflow, 0, dest).finalise(0);
        b[OV_USED..OV_USED + 2].copy_from_slice(&(used as u16).to_le_bytes());
        b[OV_DATA..OV_DATA + used].copy_from_slice(chunk);
        drop(w);
        if previous == 0 { head = dest; } else {
            let mut w = pool.get_mut(previous)?;
            w.bytes_mut()[OV_NEXT..OV_NEXT + 4].copy_from_slice(&dest.to_le_bytes());
        }
        previous = dest;
        source = next;
    }
    if seen != expected_pages || bytes != total || crc != want_crc {
        return Err(bad(0, "recovery overflow value fails manifest checksum"));
    }
    let mut relocated = marker.to_vec();
    relocated[4..8].copy_from_slice(&head.to_le_bytes());
    Ok(relocated)
}

/// Tag a value with (generation, origin page number) so a later
/// duplicate-key collapse can choose a winner. Since 2n step A, `seal`
/// stamps every page that reaches disk with the generation of the epoch
/// that wrote it, so the GENERATION is the primary recency signal -- it
/// stays correct when page numbers are recycled (the freelist), where the
/// old rule "higher page number = written later" silently inverts. Page
/// number remains the tie-break WITHIN a generation: fresh allocations in
/// one epoch are still monotonic, and one key has at most one current-
/// epoch leaf (a shadowed page is edited in place thereafter). Both fields
/// big-endian so the 12-byte prefix compares lexicographically.
fn tag(gen: u64, page_no: u32, val: &[u8]) -> Vec<u8> {
    let mut t = Vec::with_capacity(12 + val.len());
    t.extend_from_slice(&gen.to_be_bytes());
    t.extend_from_slice(&page_no.to_be_bytes());
    t.extend_from_slice(val);
    t
}

fn untag(t: &[u8]) -> (&[u8], &[u8]) {
    (&t[0..12], &t[12..])
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Low-level in-place forensic rebuild; prefer [`recover_to`].
///
/// WARNING: this rootless path can resurrect deleted/obsolete rows and its
/// historical loss hints are not authoritative. Retained for kernel regression
/// coverage, not for publishing current user data.
///
/// Sweeps the file once, keeping every leaf that still verifies (correct CRC,
/// correct identity), and separately counting every page that CLAIMS to be a
/// leaf but does not verify -- only that page kind costs a loss, because an
/// interior page is fully re-derivable from the leaves and a damaged one is
/// simply skipped for free.
///
/// Two leaves can genuinely agree on the same key: page reclamation is
/// deferred in this engine (`Store::bulk_load`'s doc comment), so a whole-tree
/// replacement leaves the previous tree's leaves allocated, intact, and CRC
/// valid, just unreachable from the current root. A sweep that does not consult
/// the (possibly damaged) root cannot tell "unreachable" from "current" by
/// structure alone, so duplicates are resolved by (generation, page number) (see `tag`)
/// before the fresh tree is packed, since `pack_tree` itself now refuses to
/// pack a duplicate key outright.
///
/// Law 3: the old file is never touched until a complete, verified replacement
/// exists. The rebuild is written to a fresh temp file and only then swapped
/// in with a rename.
///
/// SACRIFICE (Law 4): after the existing build and full barrier, recovery pays
/// one additional sequential tree traversal through a fresh 16-page pool.
/// This is recovery-only work; query and write paths are unchanged.
pub fn recover(dir: &Path, cfg: Config) -> Result<RecoveryReport> {
    if matches!(crate::limits::read(dir), Ok(Some(_))) {
        return Err(crate::Error::ResourceLimit("in-place repair disabled for constrained stores; recover_to a separately provisioned destination"));
    }
    let data = dir.join("data");
    let (file, _) = open_file(&data, cfg.io)?;
    recover_impl(&*file, dir, cfg)
}

/// The actual sweep and rebuild, taking the file to read as a `&dyn FileIo`
/// rather than opening it itself. Exists so a test can inject a read failure
/// on a specific page without needing a real faulty disk -- `recover`'s own
/// `open_file` call is the only thing this split moves out of the fallible
/// path a test can reach; everything else (the two sweeps, the rebuild, the
/// rename) is unchanged.
fn recover_impl(file: &dyn FileIo, dir: &Path, cfg: Config) -> Result<RecoveryReport> {
    recover_impl_with_before_publish(file, dir, cfg, |_| Ok(()))
}

/// Test seam at the exact Law-3 boundary: the replacement is complete and
/// flushed, but the original has not been renamed over yet.
fn recover_impl_with_before_publish<F>(
    file: &dyn FileIo,
    dir: &Path,
    cfg: Config,
    before_publish: F,
) -> Result<RecoveryReport>
where
    F: FnOnce(&Path) -> Result<()>,
{
    let data = dir.join("data");
    let len = file.len()?;
    let total = len / PAGE_SIZE as u64;

    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let scratch = std::env::temp_dir().join(format!("kernel-recover-{}-{}", std::process::id(), seq));
    // A third of the configured budget -- the fraction the pool below does
    // NOT take (it takes two thirds, matching `Store::build`'s convention).
    // A fixed arena disconnected from `cfg.budget_bytes` is not a bound at
    // all: every caller in this crate's own tests configures 16-32 MiB, which
    // a hard-coded 64 MiB arena would have exceeded outright, on the one path
    // that runs when the store is largest and most damaged.
    let arena = (cfg.budget_bytes / 3).max(4 << 20);
    let mut sorter = ExternalSort::new(&scratch, arena)?;

    let mut rep = RecoveryReport::default();
    rep.truncated_tail_bytes = len % PAGE_SIZE as u64;
    let mut max_lsn: u64 = 0;
    // Page numbers of every leaf that failed to verify, in the order the
    // sweep found them -- small (bounded by how much is actually lost, not by
    // the store), so this is fine to hold in full.
    let mut lost_pages: Vec<u32> = Vec::new();

    let mut buf = vec![0u8; PAGE_SIZE];
    for no in 1..total {
        rep.pages_scanned += 1;
        let page = match read_page(file, &mut buf, no) {
            ReadOutcome::Verified(p) => p,
            ReadOutcome::ReadOk => {
                // The read succeeded, so `buf` genuinely holds this page's
                // own (corrupted) bytes -- a raw kind byte read from it
                // describes what the page actually claimed to be. Only count
                // it as a lost LEAF if it claims to be one; a damaged
                // interior page costs nothing, because interiors are wholly
                // derivable from the leaves that survive.
                let kind = u16::from_le_bytes([buf[6], buf[7]]);
                if kind == PageKind::Leaf as u16 {
                    rep.leaves_lost += 1;
                    lost_pages.push(no as u32);
                }
                continue;
            }
            ReadOutcome::ReadFailed => {
                // The read itself failed: `buf` was never touched for this
                // `no` and still holds whatever a PREVIOUS iteration left in
                // it -- not this page's bytes at all. Reading a kind byte
                // from stale data and skipping the page because it happens
                // to say "Interior" is exactly the silent undercount Law 5
                // forbids: a page that may genuinely have been a leaf would
                // be dropped with no counter touched and no entry in
                // `lost_ranges`. Over-reporting is tolerable; this is not, so
                // an unreadable page is ALWAYS counted as a lost leaf,
                // regardless of what the stale buffer says.
                rep.leaves_lost += 1;
                lost_pages.push(no as u32);
                continue;
            }
        };
        // Trustworthy only because this page just verified: a corrupted page's
        // `lsn` field could be garbage, so only verified pages may contribute
        // to the LSN floor the rebuilt superblock will publish. (Today every
        // production page carries `lsn == 0` regardless -- see page.rs -- so
        // this is a no-op until that field is wired up, and correct either way.)
        max_lsn = max_lsn.max(page.lsn());
        if page.kind() != PageKind::Leaf { continue; }
        rep.leaves_kept += 1;

        let n = page.nentries();
        for i in 0..n {
            let rec = page.slot(i);
            let (k, v, is_marker) = decode_leaf_record(rec, page.page_no())?;
            // v is the slot's stored value: for an overflow record that is
            // the 12-byte marker, and the flag must survive recovery or the
            // rebuilt tree would hold the marker bytes as a LITERAL value.
            sorter.push_flagged(k.to_vec(), tag(page.lsn(), page.page_no(), v), is_marker)?;
        }
    }

    // Second sweep: name each loss from the sibling chain, not from page
    // order. `lost_set` is only as large as `lost_pages` -- O(losses), never
    // a map of every leaf's `next_leaf`, which would be RAM proportional to
    // the store in the one path that runs when the store is largest and most
    // damaged.
    let lost_set: HashSet<u32> = lost_pages.iter().copied().collect();
    let mut after_by_page: HashMap<u32, Vec<u8>> = HashMap::with_capacity(lost_pages.len());
    if !lost_set.is_empty() {
        for no in 1..total {
            rep.pages_scanned += 1;
            let Some(page) = read_verified(file, &mut buf, no) else { continue };
            if page.kind() != PageKind::Leaf { continue; }
            if !lost_set.contains(&page.next_leaf()) { continue; }
            let n = page.nentries();
            // An empty leaf has no key to honestly offer as a bound, even
            // though it structurally is the predecessor -- Law 5 asks what
            // was lost, not what probably was, so this leaf simply does not
            // supply a bound rather than inventing one from nothing.
            if n == 0 { continue; }
            let (last_k, _, _) = decode_leaf_record(page.slot(n - 1), page.page_no())?;
            // If more than one surviving leaf's `next_leaf` names the same
            // lost page (not possible from an intact sibling chain, but this
            // sweep does not assume the chain is intact), the later one found
            // wins; either is an equally honest single predecessor.
            after_by_page.insert(page.next_leaf(), last_k.to_vec());
        }
    }
    for page_no in lost_pages {
        rep.lost_ranges.push(LostLeaf { page_no, after_key: after_by_page.get(&page_no).cloned() });
    }

    // Rebuild into a fresh file, then swap. Law 3: nothing about the damaged
    // file is touched until the replacement is complete and verified.
    let tmp = dir.join("data.rebuild");
    let _ = std::fs::remove_file(&tmp);
    let mut entries_recovered = 0u64;
    let next_lsn = max_lsn.checked_add(1).ok_or(crate::Error::Corrupt {
        page_no: 0,
        why: "recovered LSN high-water mark is exhausted",
    })?;
    let mut rebuilt_roots = [0u32; crate::meta::MAX_TREES];
    // Wrapped in an immediately-invoked closure so any `?` failure inside can
    // be caught and `tmp` cleaned up before the error propagates -- without
    // this, a failure partway through (e.g. `pack_tree` hitting a page too
    // large, or the pool failing to allocate) leaves a half-built
    // `data.rebuild` sitting on disk: litter that looks like real state to
    // anyone inspecting the directory later.
    let build: Result<()> = (|| {
        let (nf, _) = open_file(&tmp, cfg.io)?;
        let budget = Arc::new(MemoryBudget::new(cfg.budget_bytes));
        let frames = ((cfg.budget_bytes * 2 / 3) / PAGE_SIZE).max(16);
        let pool = BufferPool::new(nf.into(), budget, frames)?;
        let _meta_page = pool.allocate()?; // page 0 is the superblock; BTree::create
        drop(_meta_page); // asserts a root is never page 0 -- reserve it first.
        let _slot_b = pool.allocate()?;    // page 1 is meta slot B (2f)
        drop(_slot_b);
        Meta::init_slot_b(&pool)?;

        let mut runs = sorter.finish()?;
        let merged = runs.iter()?;

        // Collapse adjacent equal keys, keeping the highest (generation,
        // page number) origin (see `tag`'s doc comment).
        // `merged` is sorted by key, so duplicates of the same key are always
        // adjacent -- this needs only the current winner in memory, never the
        // whole store (Law 1).
        struct Dedup<I> { inner: I, winner: Option<(Vec<u8>, Vec<u8>, bool)>, done: bool }
        impl<I: Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>>> Iterator for Dedup<I> {
            type Item = Result<(Vec<u8>, Vec<u8>, bool)>;
            fn next(&mut self) -> Option<Self::Item> {
                if self.done { return None; }
                loop {
                    match self.inner.next() {
                        Some(Ok((k, tagged_v, m))) => {
                            match &mut self.winner {
                                None => self.winner = Some((k, tagged_v, m)),
                                Some((wk, wv, wm)) if *wk == k => {
                                    let (worder, _) = untag(wv);
                                    let (order, _) = untag(&tagged_v);
                                    if order > worder { *wv = tagged_v; *wm = m; }
                                }
                                Some(_) => {
                                    let (out_k, out_v, out_m) = self.winner.take().unwrap();
                                    self.winner = Some((k, tagged_v, m));
                                    let (_, real_v) = untag(&out_v);
                                    return Some(Ok((out_k, real_v.to_vec(), out_m)));
                                }
                            }
                        }
                        Some(Err(e)) => { self.done = true; return Some(Err(e)); }
                        None => {
                            self.done = true;
                            return self.winner.take().map(|(k, v, m)| {
                                let (_, real_v) = untag(&v);
                                Ok((k, real_v.to_vec(), m))
                            });
                        }
                    }
                }
            }
        }
        // A salvaged source may have lost rows while its aggregate remains
        // checksum-valid. Derived SQL counts are never evidence of salvage
        // completeness. The marker also tells ordinary WAL replay not to
        // resurrect old summaries on this generation-zero salvaged source.
        let recovered = Dedup { inner: merged, winner: None, done: false }
            .filter(|item| !matches!(item, Ok((key, _, _))
                if crate::keys::is_field_aggregate_key(key)));
        let relocated = recovered.map(|item| {
            let (key, value, overflow) = item?;
            let value = if overflow { copy_overflow(file, &pool, &value)? } else { value };
            Ok((key, value, overflow))
        });
        let counting = CountingIter { inner: relocated, count: &mut entries_recovered };
        // Same scratch the sort used: `pack_tree` spills one file per tree
        // level there and unlinks each as it is consumed, so nothing of its
        // outlives this call even on the error path below.
        let root = pack_tree(&pool, 1, counting, 0.9, &scratch)?;

        // The LSN floor for the rebuilt store. It must be strictly above every
        // LSN any surviving (verified) page carried, or a future record could
        // repeat one already embedded in a page -- exactly the hazard
        // `Meta::next_lsn`'s own doc comment names. The old log itself is not
        // a source of truth here: it describes writes to a file that, after a
        // sweep, may not even have the same page-to-content mapping any more.
        rebuilt_roots[0] = root;
        Meta { format_version: crate::meta::FORMAT_VERSION, roots: rebuilt_roots, next_lsn, generation: 0 }.write(&pool)?;
        Meta::mark_salvaged(&pool)?;
        // Always the strongest barrier, regardless of `cfg.sync`: Law 3 says
        // the replacement must be verified durable before the rename that
        // exposes it, and a repair run happens rarely enough that it should
        // not inherit a throughput-motivated durability trade-off made for
        // ordinary per-write traffic.
        pool.flush_all(crate::io::Barrier::Full)?;
        Ok(())
    })();
    if let Err(e) = build {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = before_publish(&tmp).and_then(|_| {
        let expected = Meta {
            format_version: crate::meta::FORMAT_VERSION,
            roots: rebuilt_roots,
            next_lsn,
            generation: 0,
        };
        let verified = crate::verify::verify_rebuild(&tmp, cfg.io, &expected, entries_recovered)?;
        debug_assert_eq!(verified.rows, entries_recovered);
        debug_assert!(verified.pages > 0);
        Ok(())
    }) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    // Recovery has changed every page number. Publish a verified EMPTY list
    // before publishing those new numbers. If power fails here, the standing
    // data file either sees its own old list or rejects generation 0 and leaks;
    // if it fails after the data rename, the rebuilt file sees the matching
    // empty list. No instant exposes rebuilt data beside old reusable numbers.
    let rebuilt_pages = std::fs::metadata(&tmp)?.len() / PAGE_SIZE as u64;
    let rebuilt_pages = u32::try_from(rebuilt_pages).map_err(|_| crate::Error::Corrupt {
        page_no: 0,
        why: "rebuilt file has too many pages for its freelist bound",
    })?;
    let empty_free = BufferPool::empty_free(0);
    crate::verify::publish_freelist(dir, empty_free, 0, rebuilt_pages, file, true, true)?;
    std::fs::rename(&tmp, &data)?;
    let (f2, _) = open_file(&data, cfg.io)?;
    f2.sync_dir()?;

    // The log, if it is the thing that is damaged. `Wal::open` refuses a
    // log whose walk stopped for a reason that is not an ending (see
    // `wal::Stop`), and that refusal is only half a design: a store that
    // cannot be opened and cannot be repaired is exactly as unrecoverable
    // as one that was deleted, which is what Law 5 forbids. This is the
    // other half.
    quarantine_damaged_wal(dir, cfg, &mut rep)?;

    // Otherwise the write-ahead log is deliberately NOT deleted here. Its records are
    // logical (`insert`/`delete` by key, per `Store::apply`) and name no
    // pages, so they replay onto the rebuilt tree exactly as well as onto
    // the old one -- and the log holds precisely the writes that were
    // committed but never checkpointed, which is to say the writes that are
    // NOT in the pages this sweep could find. Deleting it would be this
    // repair path discarding committed data it could have restored, having
    // decided what to keep by reading and then removing what it did not see
    // -- Law 3's exact subject. `Store::open`'s ordinary log replay applies
    // it to the rebuilt tree the next time this directory is opened; replay
    // is idempotent for both record kinds and `Wal::open` already truncates
    // a damaged tail, so nothing here needs to special-case it.

    rep.entries_recovered = entries_recovered;
    Ok(rep)
}

/// Copy the whole log aside, byte for byte, verify the copy by read-back hash,
/// and reconstruct safely resynchronised committed regions as the live log.
///
/// Law 3 first: nothing is deleted and nothing is overwritten in place. The
/// full copy is written and fsynced BEFORE the live log is touched, so at
/// every instant from here on there is at least one complete copy of every
/// original byte on disk. A crash midway leaves either the original alone or
/// the original plus its copy; neither loses anything, and re-running
/// `recover()` simply makes another copy.
///
/// Law 5 second: what makes the refusal survivable is that this runs on
/// exactly the images `Wal::open` refuses, and afterwards those images open.
/// Ambiguous frames are not replayed and not thrown away: they remain in
/// `wal.corrupt.N`, where a repair tool has the whole file and the offset
/// that stopped the walk.
///
/// A missing log is a proved absence and succeeds.  A log that exists but
/// cannot be inspected is uncertainty and propagates: it may contain a
/// committed tail that is absent from the rebuilt pages, so reporting success
/// would strand precisely the data recovery is responsible for finding.
/// Nothing has touched that log when inspection fails.
fn quarantine_damaged_wal(dir: &Path, cfg: Config, rep: &mut RecoveryReport) -> Result<()> {
    let wal = dir.join("wal");
    if !wal.exists() { return Ok(()); }
    let scan = crate::wal::Wal::inspect(&wal, cfg.io)?;
    match scan.stop {
        crate::wal::Stop::End(_) => return Ok(()),
        crate::wal::Stop::Damaged { .. } => {}
    }

    // First free name, so a second repair never overwrites the evidence the
    // first one preserved.
    let mut n = 0u32;
    let aside = loop {
        let c = dir.join(format!("wal.corrupt.{n}"));
        if !c.exists() { break c; }
        n = n.checked_add(1).ok_or(crate::Error::TooLarge)?;
    };
    let total = std::fs::metadata(&wal)?.len();
    let source_hash = crate::wal::hash_prefix(&wal, total)?;
    std::fs::copy(&wal, &aside)?;
    std::fs::File::open(&aside)?.sync_all()?;
    if std::fs::metadata(&aside)?.len() != total
        || crate::wal::hash_prefix(&aside, total)? != source_hash
    {
        let _ = std::fs::remove_file(&aside);
        return Err(crate::Error::CorruptWal {
            offset: scan.end,
            why: "quarantined WAL copy failed independent read-back hashing",
        });
    }

    // Reconstruct beside the original and rename only after an independent
    // parser and hash pass agree with the salvage writer.
    let tmp = dir.join("wal.rebuild");
    let _ = std::fs::remove_file(&tmp);
    let salvaged = crate::wal::Wal::salvage_committed(&wal, &tmp)?;
    let verified = crate::wal::Wal::inspect(&tmp, cfg.io)?;
    if verified.end != salvaged.bytes
        || !matches!(verified.stop, crate::wal::Stop::End(_))
        || std::fs::metadata(&tmp)?.len() != salvaged.bytes
        || crate::wal::hash_prefix(&tmp, salvaged.bytes)? != salvaged.hash
    {
        let _ = std::fs::remove_file(&tmp);
        return Err(crate::Error::CorruptWal {
            offset: scan.end,
            why: "reconstructed WAL failed independent parse and hash verification",
        });
    }
    std::fs::rename(&tmp, &wal)?;
    let (f, _) = open_file(&wal, crate::io::IoMode::Buffered)?;
    f.sync_dir()?;

    rep.wal_quarantined = Some(aside);
    rep.wal_bytes_kept = salvaged.bytes;
    rep.wal_bytes_set_aside = total;
    Ok(())
}

/// Counts every item that passes through, so `entries_recovered` reflects
/// exactly what was packed -- after deduplication -- rather than a raw push
/// count that could include items later collapsed away.
struct CountingIter<'a, I> { inner: I, count: &'a mut u64 }
impl<'a, I: Iterator<Item = Result<(Vec<u8>, Vec<u8>, bool)>>> Iterator for CountingIter<'a, I> {
    type Item = Result<(Vec<u8>, Vec<u8>, bool)>;
    fn next(&mut self) -> Option<Self::Item> {
        let item = self.inner.next();
        if let Some(Ok(_)) = &item { *self.count += 1; }
        item
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::IoMode;
    use crate::store::{Config, Store, SyncMode};

    fn cfg() -> Config { Config { budget_bytes: 16 << 20, io: IoMode::Buffered, sync: SyncMode::Off } }

    /// The falsification: force `recover` to treat a damaged INTERIOR page as
    /// a loss, the same as a leaf. If interiors truly cost nothing (they are
    /// wholly derivable from the leaves), then wrongly counting one as a loss
    /// must be something a test could catch -- proving the real implementation
    /// relies on the distinction rather than merely asserting it in a comment.
    /// This test does not call the wrong-counting code path (that would defeat
    /// its own purpose); it exists so the counterfactual is checked by hand in
    /// the report, alongside a positive assertion that a leaf loss above is
    /// real. See the task report for the actual before/after run.
    #[test]
    fn a_damaged_interior_page_costs_nothing() {
        let d = tempfile::tempdir().unwrap();
        let n = 5_000u64;
        { let mut s = Store::create(d.path(), cfg()).unwrap();
          s.bulk_load((0..n).map(|i| (i.to_be_bytes().to_vec(), b"v".to_vec()))).unwrap();
          s.commit().unwrap(); s.checkpoint().unwrap(); }

        let path = d.path().join("data");
        let mut bytes = std::fs::read(&path).unwrap();
        let ps = PAGE_SIZE;
        let mut wrecked_interior = false;
        for p in 1..bytes.len() / ps {
            let kind = u16::from_le_bytes([bytes[p * ps + 6], bytes[p * ps + 7]]);
            if kind == PageKind::Interior as u16 && !wrecked_interior {
                bytes[p * ps + 100] ^= 0xff;
                wrecked_interior = true;
            }
        }
        assert!(wrecked_interior, "the fixture needs at least one interior page");
        std::fs::write(&path, &bytes).unwrap();

        let report = recover(d.path(), cfg()).unwrap();
        assert_eq!(report.leaves_lost, 0, "a damaged interior page must cost no leaf loss");
        assert_eq!(report.entries_recovered, n, "and no data may be missing either");
    }

    /// Pins the duplicate-key resolution directly. `Store::bulk_load` never
    /// reclaims the previous tree's pages, so bulk-loading twice over
    /// overlapping keys leaves BOTH generations' leaves intact and CRC-valid
    /// in the file -- a stale one at lower page numbers, a current one at
    /// higher page numbers, both surviving the sweep. Recovery must keep the
    /// current value, not the first one it happens to see.
    #[test]
    fn a_duplicate_key_across_two_surviving_generations_keeps_the_newer_one() {
        let d = tempfile::tempdir().unwrap();
        let n = 500u64;
        let mut s = Store::create(d.path(), cfg()).unwrap();
        s.bulk_load((0..n).map(|i| (i.to_be_bytes().to_vec(), b"stale".to_vec()))).unwrap();
        // The stale tree's leaves are now allocated at low page numbers and
        // are never reclaimed -- confirmed below.
        s.bulk_load((0..n).map(|i| (i.to_be_bytes().to_vec(), b"current".to_vec()))).unwrap();
        s.commit().unwrap(); s.checkpoint().unwrap();
        drop(s);

        // Sanity: the file really does hold leaves from both generations,
        // i.e. this test exercises what it claims to.
        let bytes = std::fs::read(d.path().join("data")).unwrap();
        let mut stale_leaves = 0;
        let mut current_leaves = 0;
        for p in 1..bytes.len() / PAGE_SIZE {
            let b = &bytes[p * PAGE_SIZE..(p + 1) * PAGE_SIZE];
            if let Ok(pr) = PageRef::open(b, p as u32) {
                if pr.kind() == PageKind::Leaf && pr.nentries() > 0 {
                    let (_, v, _) = decode_leaf_record(pr.slot(0), pr.page_no()).unwrap();
                    if v == b"stale" { stale_leaves += 1; }
                    if v == b"current" { current_leaves += 1; }
                }
            }
        }
        assert!(stale_leaves > 0 && current_leaves > 0,
                "fixture must retain both generations' leaves unreclaimed");

        let report = recover(d.path(), cfg()).unwrap();
        assert_eq!(report.entries_recovered, n,
                   "duplicates must collapse to one entry per key, not {}",
                   stale_leaves + current_leaves);

        let reopened = Store::open(d.path(), cfg()).unwrap();
        for i in (0..n).step_by(37) {
            assert_eq!(
                reopened.get(&i.to_be_bytes()).unwrap().as_deref(),
                Some(&b"current"[..]),
                "key {i} must resolve to the newer generation, not the stale one"
            );
        }
    }

    /// Locate every non-empty leaf's page number and last key in a raw file
    /// buffer, in page-number order. Test helper only.
    fn leaves_with_last_keys(bytes: &[u8]) -> Vec<(u32, Vec<u8>)> {
        let mut out = Vec::new();
        for p in 1..bytes.len() / PAGE_SIZE {
            let b = &bytes[p * PAGE_SIZE..(p + 1) * PAGE_SIZE];
            if let Ok(pr) = PageRef::open(b, p as u32) {
                if pr.kind() == PageKind::Leaf && pr.nentries() > 0 {
                    let (last_k, _, _) =
                        decode_leaf_record(pr.slot(pr.nentries() - 1), pr.page_no()).unwrap();
                    out.push((p as u32, last_k.to_vec()));
                }
            }
        }
        out
    }

    /// Break a page's CRC by flipping a payload byte, leaving its header
    /// (including `kind`) intact -- the same technique the integration test
    /// uses, so a broken leaf still reads as a Leaf by its raw kind byte.
    fn wreck(bytes: &mut [u8], page_no: u32) {
        let base = page_no as usize * PAGE_SIZE;
        bytes[base + 50] ^= 0xff;
    }

    /// Overwrite a page's `next_leaf` field and re-SEAL it (checksum is
    /// seal's job, not finalise's -- finalise only writes the lsn field),
    /// preserving the page's generation stamp so the forged pointer is
    /// indistinguishable from a real one. The old form finalise(0) only
    /// passed because the forge happened to be a byte-level no-op (the
    /// pointer already named its packed neighbour and lsn was already 0);
    /// generation stamping (2n step A) made it a real change and exposed
    /// the stale CRC.
    fn forge_next_leaf(bytes: &mut [u8], page_no: u32, target: u32) {
        let base = page_no as usize * PAGE_SIZE;
        let page = &mut bytes[base..base + PAGE_SIZE];
        let gen = u64::from_le_bytes(page[24..32].try_into().unwrap());
        let mut p = crate::page::PageMut::reopen(page);
        p.set_next_leaf(target);
        crate::page::seal(page, gen);
    }

    /// The falsification for the sibling-chain lost-range naming: point a
    /// surviving leaf's `next_leaf` at a page about to be destroyed, and
    /// confirm `after_key` reports that leaf's last key -- proving the bound
    /// really is read from the chain. Then destroy the pointing leaf too, and
    /// confirm `after_key` falls back to `None` rather than a page-order
    /// guess, since nothing verifiable points at the lost page any more.
    #[test]
    fn a_lost_leafs_after_key_comes_from_the_sibling_chain_not_page_order() {
        let d = tempfile::tempdir().unwrap();
        let n = 2_000u64;
        { let mut s = Store::create(d.path(), cfg()).unwrap();
          s.bulk_load((0..n).map(|i| (i.to_be_bytes().to_vec(), b"v".to_vec()))).unwrap();
          s.commit().unwrap(); s.checkpoint().unwrap(); }

        let path = d.path().join("data");
        let mut bytes = std::fs::read(&path).unwrap();

        let leaves = leaves_with_last_keys(&bytes);
        assert!(leaves.len() >= 2, "fixture needs at least two non-empty leaves");
        let (pointer_no, pointer_last_key) = leaves[0].clone();
        let (victim_no, _) = leaves[1].clone();
        assert_ne!(pointer_no, victim_no);

        // Forge pointer -> victim, destroy victim. `recover` rebuilds `data`
        // in place, so this exact byte state is captured (cloned) BEFORE
        // either half runs `recover` -- each half gets its own fresh
        // directory over the same starting bytes, rather than the second
        // half accidentally reading back the first half's already-rebuilt
        // file.
        forge_next_leaf(&mut bytes, pointer_no, victim_no);
        wreck(&mut bytes, victim_no);
        let bytes_for_half_b = bytes.clone();

        // Half A: one surviving leaf's next_leaf names the lost page ->
        // after_key must be that leaf's real last key.
        std::fs::write(&path, &bytes).unwrap();
        let report = recover(d.path(), cfg()).unwrap();
        let victim_entry = report.lost_ranges.iter().find(|l| l.page_no == victim_no)
            .expect("the victim page must be reported as a loss");
        assert_eq!(
            victim_entry.after_key,
            Some(pointer_last_key.clone()),
            "after_key must come from the leaf whose next_leaf names the lost page"
        );

        // Half B: also destroy the pointing leaf, in a fresh directory over
        // the SAME starting bytes. Its forged next_leaf is still physically
        // present, but it can no longer be trusted (it fails its own CRC),
        // so the second sweep must not see it -- after_key must fall back to
        // None, not silently keep reporting the stale bound.
        let d2 = tempfile::tempdir().unwrap();
        let mut bytes2 = bytes_for_half_b;
        wreck(&mut bytes2, pointer_no);
        std::fs::write(d2.path().join("data"), &bytes2).unwrap();

        let report2 = recover(d2.path(), cfg()).unwrap();
        let victim_entry2 = report2.lost_ranges.iter().find(|l| l.page_no == victim_no)
            .expect("the victim page must still be reported as a loss");
        assert_eq!(
            victim_entry2.after_key, None,
            "after_key must be None once the only pointer to the lost page is itself unreadable"
        );
    }

    /// A file truncated mid-page (crash mid-extend, a full disk) must be
    /// named, not silently dropped from the floor-divided page count.
    #[test]
    fn a_truncated_tail_is_named_not_silently_dropped() {
        let d = tempfile::tempdir().unwrap();
        let n = 2_000u64;
        { let mut s = Store::create(d.path(), cfg()).unwrap();
          s.bulk_load((0..n).map(|i| (i.to_be_bytes().to_vec(), b"v".to_vec()))).unwrap();
          s.commit().unwrap(); s.checkpoint().unwrap(); }

        let path = d.path().join("data");
        let full_len = std::fs::metadata(&path).unwrap().len();
        assert_eq!(full_len % PAGE_SIZE as u64, 0, "fixture must start page-aligned");

        // Truncate 137 bytes into the last page -- neither a whole page nor
        // nothing.
        let short_by = 137u64;
        let truncated_len = full_len - short_by;
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_len(truncated_len).unwrap();
        drop(f);

        let report = recover(d.path(), cfg()).unwrap();
        assert_eq!(
            report.truncated_tail_bytes,
            PAGE_SIZE as u64 - short_by,
            "the dangling bytes at the end of the file must be named exactly"
        );
        // The tail is unclassifiable (its own header may be the missing
        // part), so it must never inflate leaves_lost -- no other page was
        // damaged, so this must stay exactly zero.
        assert_eq!(report.leaves_lost, 0, "a truncated tail must not be counted as a lost leaf");
        assert_eq!(report.entries_recovered, n, "every complete page's data must still be recovered");
    }

    /// Delegates every `FileIo` method to a real, opened file, except that
    /// `read_at` fails outright at one specific offset -- a deterministic,
    /// portable way to force "the read itself failed" without needing a
    /// faulty disk.
    struct FailAt { inner: Box<dyn FileIo>, fail_offset: u64 }
    impl FileIo for FailAt {
        fn requires_alignment(&self) -> bool { self.inner.requires_alignment() }
        fn read_at(&self, buf: &mut [u8], off: u64) -> Result<()> {
            if off == self.fail_offset {
                return Err(std::io::Error::new(std::io::ErrorKind::Other, "injected read failure").into());
            }
            self.inner.read_at(buf, off)
        }
        fn write_at(&self, buf: &[u8], off: u64) -> Result<()> { self.inner.write_at(buf, off) }
        fn sync_data(&self) -> Result<()> { self.inner.sync_data() }
        fn sync_full(&self) -> Result<()> { self.inner.sync_full() }
        fn sync_full_primitive(&self) -> &'static str { self.inner.sync_full_primitive() }
        fn sync_dir(&self) -> Result<()> { self.inner.sync_dir() }
        fn len(&self) -> Result<u64> { self.inner.len() }
        fn set_len(&self, n: u64) -> Result<()> { self.inner.set_len(n) }
    }

    /// The falsification for item 4: force a read failure on a page whose
    /// PREDECESSOR (in scan order) is a genuine, undamaged interior page, and
    /// confirm the failed page is counted as a lost leaf regardless -- not
    /// silently skipped because the stale buffer (still holding the
    /// predecessor's bytes) says "Interior".
    ///
    /// Two bulk loads in a row lay out pages as [gen-1 leaves][gen-1
    /// interiors][gen-2 leaves][gen-2 interiors] (`pack_tree` allocates all
    /// of a generation's leaves before any of its interiors), so the
    /// boundary between gen-1's LAST interior page and gen-2's FIRST leaf
    /// page is exactly "predecessor is interior, this page is a real leaf" --
    /// no forged bytes needed, just picking the right natural page.
    #[test]
    fn an_unreadable_page_is_counted_regardless_of_the_stale_buffer_it_follows() {
        let d = tempfile::tempdir().unwrap();
        let n = 3_000u64;
        let mut s = Store::create(d.path(), cfg()).unwrap();
        s.bulk_load((0..n).map(|i| (i.to_be_bytes().to_vec(), b"gen1".to_vec()))).unwrap();
        s.bulk_load((n..2 * n).map(|i| (i.to_be_bytes().to_vec(), b"gen2".to_vec()))).unwrap();
        s.commit().unwrap(); s.checkpoint().unwrap();
        drop(s);

        let path = d.path().join("data");
        let bytes = std::fs::read(&path).unwrap();
        let total = bytes.len() / PAGE_SIZE;
        let kind_of = |p: usize| -> Option<PageKind> {
            PageRef::open(&bytes[p * PAGE_SIZE..(p + 1) * PAGE_SIZE], p as u32).ok().map(|pr| pr.kind())
        };
        let victim = (2..total as u32)
            .find(|&p| kind_of(p as usize - 1) == Some(PageKind::Interior)
                    && kind_of(p as usize) == Some(PageKind::Leaf))
            .expect("two generations must produce an interior-then-leaf boundary");

        let (real_file, _) = crate::io::open_file(&path, IoMode::Buffered).unwrap();
        let failing = FailAt { inner: real_file, fail_offset: victim as u64 * PAGE_SIZE as u64 };
        let report = recover_impl(&failing, d.path(), cfg()).unwrap();

        assert!(
            report.lost_ranges.iter().any(|l| l.page_no == victim),
            "page {victim} (predecessor is Interior) failed to read and must be counted as lost, \
             not silently skipped because the stale buffer said Interior"
        );

        // Falsification: the OLD behaviour -- classify a read failure from
        // whatever `buf` still holds (the predecessor's real Interior bytes,
        // since a failed read never overwrites `buf`) -- would have skipped
        // this exact page. Reproduce that classification by hand against the
        // SAME stale buffer this sweep would have seen, to show the
        // counterfactual is real rather than assumed.
        let buf = &bytes[(victim as usize - 1) * PAGE_SIZE..victim as usize * PAGE_SIZE];
        let stale_kind = u16::from_le_bytes([buf[6], buf[7]]);
        assert_eq!(
            stale_kind,
            PageKind::Interior as u16,
            "sanity: the stale buffer a failed read would have left behind really does say Interior"
        );
        let old_behaviour_would_count_it = stale_kind == PageKind::Leaf as u16;
        assert!(
            !old_behaviour_would_count_it,
            "the old kind-byte-fallback logic would NOT have counted page {victim} -- \
             confirming this is a genuine fix, not a no-op"
        );
    }

    /// A malformed superblock is the damage `recover()` exists to service,
    /// so `recover()` must degrade on it, never abort. This overwrites page
    /// 0 with a finalised-but-EMPTY Meta page -- CRC-valid, zero slots,
    /// exactly the shape `Meta::write`'s error path leaves behind -- and
    /// confirms the repair still returns `Ok` and finds every leaf. (Task 17
    /// re-review, R2: an earlier version of `Meta::from_page` panicked on
    /// this page rather than returning `Err`, which took `recover()` down
    /// with it. `from_page`'s own unit test pins the decoder; this pins the
    /// repair path that has to survive whatever the decoder says.)
    #[test]
    fn recover_does_not_panic_on_an_empty_but_crc_valid_meta_page() {
        let d = tempfile::tempdir().unwrap();
        let n = 500u64;
        { let mut s = Store::create(d.path(), cfg()).unwrap();
          s.bulk_load((0..n).map(|i| (i.to_be_bytes().to_vec(), b"v".to_vec()))).unwrap();
          s.commit().unwrap(); s.checkpoint().unwrap(); }

        let path = d.path().join("data");
        let mut bytes = std::fs::read(&path).unwrap();
        {
            let page0 = &mut bytes[0..PAGE_SIZE];
            let mut p = crate::page::PageMut::init(page0, PageKind::Meta, 0, 0);
            // Deliberately no insert_slot -- zero entries, but still
            // finalised (CRC-valid).
            p.finalise(0);
        }
        std::fs::write(&path, &bytes).unwrap();

        let report = recover(d.path(), cfg())
            .expect("recover() must degrade on a malformed superblock, not panic");
        assert!(report.leaves_kept > 0, "the surviving leaves must still be found and repacked");
        assert_eq!(report.entries_recovered, n, "every leaf's entries survive an empty page 0");
    }

    #[test]
    fn a_corrupted_rebuild_never_replaces_the_original_data_file() {
        let d = tempfile::tempdir().unwrap();
        {
            let mut s = Store::create(d.path(), cfg()).unwrap();
            s.bulk_load((0..2_000u64).map(|i| {
                (i.to_be_bytes().to_vec(), i.to_le_bytes().to_vec())
            })).unwrap();
        }
        let data = d.path().join("data");
        let original = std::fs::read(&data).unwrap();
        let (file, _) = open_file(&data, cfg().io).unwrap();

        let result = recover_impl_with_before_publish(&*file, d.path(), cfg(), |fresh| {
            let mut bytes = std::fs::read(fresh)?;
            bytes[PAGE_SIZE * 2 + 100] ^= 0x80;
            std::fs::write(fresh, bytes)?;
            Ok(())
        });

        assert!(result.is_err(), "a rebuild corrupted before publication must be refused");
        assert_eq!(std::fs::read(&data).unwrap(), original,
                   "the original data file must remain byte-for-byte authoritative");
        let s = Store::open(d.path(), cfg()).unwrap();
        assert_eq!(s.get(&1999u64.to_be_bytes()).unwrap().as_deref(),
                   Some(&1999u64.to_le_bytes()[..]));
    }

    #[test]
    fn an_unreadable_wal_is_an_error_while_no_wal_is_success() {
        let damaged = tempfile::tempdir().unwrap();
        {
            let mut s = Store::create(damaged.path(), cfg()).unwrap();
            s.put(b"published", b"yes").unwrap();
            s.commit().unwrap();
            s.checkpoint().unwrap();
            s.put(b"committed-tail", b"must not be stranded").unwrap();
            s.commit().unwrap();
        }
        let wal = damaged.path().join("wal");
        std::fs::rename(&wal, damaged.path().join("wal.committed-tail")).unwrap();
        std::fs::create_dir(&wal).unwrap();
        let result = recover(damaged.path(), cfg());
        assert!(result.is_err(), "an unreadable committed log must not be reported as recovered");

        let healthy = tempfile::tempdir().unwrap();
        {
            let mut s = Store::create(healthy.path(), cfg()).unwrap();
            s.put(b"published", b"yes").unwrap();
            s.commit().unwrap();
            s.checkpoint().unwrap();
        }
        std::fs::remove_file(healthy.path().join("wal")).unwrap();
        assert!(recover(healthy.path(), cfg()).is_ok(), "there is nothing to inspect when no log exists");
    }
}
