//! Page 0: the superblock. Roots of every tree, and the format version.

use crate::page::{PageKind, PageMut, PageRef};
use crate::pool::BufferPool;
use crate::{Error, Result};

pub const META_PAGE: u32 = 0;
/// 2f: the second commit slot. Generation g lives in page g % 2, so writing
/// generation g+1 only ever overwrites the slot of g-1 -- the newest
/// published root is never touched by the write that supersedes it, and a
/// torn slot write fails its page checksum and loses to the other slot.
/// (LMDB's dual meta page; its pick is by txnid, ours by generation, and
/// unlike LMDB every slot is protected by the ordinary page checksum.)
pub const META_PAGE_B: u32 = 1;
pub const MAX_TREES: usize = 8;
/// Highest logical superblock version this engine reads. Versions 1 (plain
/// cells) and 2 (compact cells) are both supported in every build. The page
/// header's own version (byte 4) guards the physical page layout; THIS guards
/// the meaning of what the pages contain — key encodings, payload format,
/// keyspace tags. A database stamped with a newer value is refused on open,
/// never guessed at.
pub const SUPPORTED_FORMAT_VERSION: u16 = 2;
const FUTURE_FORMAT: &str = "database format version 3+ is newer than this engine reads; open it with the engine version that created it";
const ZERO_FORMAT: &str = "database format version 0 is not one this engine writes";
/// Version a newly created store is stamped with. The `compact-cells` cargo
/// feature still decides only this default; opening never restamps it.
pub const FORMAT_VERSION: u16 = if cfg!(feature="compact-cells") {2}else{1};
/// Optional second meta-page slot, never a user-tree row. Covered by the page
/// checksum. A normal checkpoint reinitializes the slot without this marker.
pub(crate) const LIMITED: u16 = 0x8000;
const SALVAGED: &[u8] = b"sekejap-salvaged-v1";

#[derive(Debug, Clone, Copy)]
pub struct Meta {
    pub format_version: u16,
    pub roots: [u32; MAX_TREES],
    /// LSN high-water mark, persisted so a rotation followed by a restart does
    /// not renumber log records from 1. Nothing today compares a page's `lsn`
    /// against a record's, but a later idempotence check would be silently
    /// wrong if LSNs repeated, and that is not a defect worth discovering from
    /// a corrupted store.
    pub next_lsn: u64,
    /// Monotone publication counter (2f). Chooses the live slot on open and
    /// the victim slot on write. 0 = the create-time publication.
    pub generation: u64,
}

impl Meta {
    pub(crate) fn mark_salvaged(pool: &BufferPool) -> Result<()> {
        let mut w = pool.get_mut(META_PAGE)?;
        let mut p = PageMut::reopen(w.bytes_mut());
        p.insert_slot(1, SALVAGED)?;
        p.finalise(0);
        Ok(())
    }

    pub(crate) fn is_salvaged(pool: &BufferPool) -> Result<bool> {
        let r = pool.get(META_PAGE)?;
        let p = PageRef::open_resident(&r, META_PAGE)?;
        let meta = Self::from_page(&p)?;
        Ok(meta.generation == 0 && p.nentries() == 2 && p.slot(1) == SALVAGED)
    }

    pub fn write(&self, pool: &BufferPool) -> Result<()> {
        let mut rec = Vec::with_capacity(10 + MAX_TREES * 4);
        rec.extend_from_slice(&(self.format_version | if pool.resource_limits().is_some() { LIMITED } else { 0 }).to_le_bytes());
        for r in self.roots { rec.extend_from_slice(&r.to_le_bytes()); }
        rec.extend_from_slice(&self.next_lsn.to_le_bytes());
        rec.extend_from_slice(&self.generation.to_le_bytes());
        Self::write_record(pool, &rec, META_PAGE)
    }

    /// Write this Meta into the slot its generation selects (2f). The caller
    /// flushes data pages and BARRIERS before calling, and flushes again
    /// after: the flip must reach the medium only once everything it names
    /// is already there.
    pub fn write_slot(&self, pool: &BufferPool) -> Result<()> {
        let mut rec = Vec::with_capacity(18 + MAX_TREES * 4);
        rec.extend_from_slice(&(self.format_version | if pool.resource_limits().is_some() { LIMITED } else { 0 }).to_le_bytes());
        for r in self.roots { rec.extend_from_slice(&r.to_le_bytes()); }
        rec.extend_from_slice(&self.next_lsn.to_le_bytes());
        rec.extend_from_slice(&self.generation.to_le_bytes());
        let slot = if self.generation % 2 == 0 { META_PAGE } else { META_PAGE_B };
        Self::write_record(pool, &rec, slot)
    }

    /// The page half of `write`, split from the encoding half so a test can
    /// hand it a record `insert_slot` will actually refuse and then look at
    /// what the page was left as. Without this seam the error path below is
    /// unreachable from any test -- a real `Meta` record is 42 bytes and
    /// `insert_slot` cannot fail on it -- and an unreachable path with no
    /// test is how it came to be wrong in the first place.
    fn write_record(pool: &BufferPool, rec: &[u8], slot: u32) -> Result<()> {
        let mut w = pool.get_mut(slot)?;
        let mut p = PageMut::init(w.bytes_mut(), PageKind::Meta, 0, slot);
        let result = p.insert_slot(0, rec).and_then(|_| {
            if let Some(l) = pool.resource_limits() { p.insert_slot(1, &l.encode())?; }
            Ok(())
        });
        // Finalise regardless of whether the insert above succeeded (Task 17
        // review, F9). `pool.get_mut` already marked this page dirty and
        // `PageMut::init` already zeroed it, so on the `?`-would-be error
        // path the page was left initialised, dirty, and UNfinalised -- its
        // checksum field still whatever `init` zeroed it to, not a value
        // that actually describes the bytes now on the page. A later
        // `flush_all`, or an ordinary eviction, must never publish a dirty
        // page whose checksum does not match its own content; finalising
        // here closes that regardless of which branch is taken.
        //
        // NOT safe to publish, though (Task 17 re-review, R2, corrected from
        // this comment's earlier claim otherwise): a CRC-valid page with
        // zero slots is exactly the shape `from_page` below refuses rather
        // than reads. Finalising is what keeps a reader from being handed a
        // page whose checksum LIES about its content; it was never what made
        // the empty content itself meaningful.
        p.finalise(0);
        result
    }

    /// Decode a `Meta` from an already-verified page. Pure -- no
    /// `BufferPool` involved -- so a caller holding a raw, already-CRC-
    /// checked `PageRef` over a single buffer can read it without needing a
    /// whole pool wrapped around one page. `read` below is a thin wrapper
    /// over this.
    ///
    /// Fallible (Task 17 re-review, R2): a page can CRC-verify -- its own
    /// bytes are exactly what was last written -- while still holding zero
    /// slots, which `Meta::write`'s own error path can produce (see its doc
    /// comment) and nothing stops a future writer from producing another
    /// way. `PageRef::slot(0)` on such a page returns an EMPTY slice, not an
    /// error -- verification and "has a slot 0 at all" are different
    /// questions -- so the previous, infallible version of this function
    /// indexed straight into it and panicked. A malformed superblock is
    /// exactly the kind of damage this crate's own `recover()` exists to
    /// repair; a decoder that panics on it instead of returning `Err` takes
    /// that repair path down with it, which is precisely what R2 measured.
    pub fn from_page(p: &PageRef) -> Result<Meta> {
        if p.nentries() == 0 {
            return Err(Error::Corrupt { page_no: p.page_no(), why: "meta page has no slots" });
        }
        let rec = p.slot(0);
        if rec.len() < 2 {
            return Err(Error::Corrupt { page_no: p.page_no(), why: "meta record too short for format_version" });
        }
        let format_version = u16::from_le_bytes([rec[0], rec[1]]);
        let base_version = format_version & !LIMITED;
        // The version prefix belongs to the admission envelope. A future
        // version may change roots, record length or extensions, so reject it
        // before applying any supported-version payload rules. Otherwise an
        // intact future slot could be mistaken for damage and lose to a sibling.
        if base_version == 0 || base_version > SUPPORTED_FORMAT_VERSION {
            return Err(Error::Corrupt {
                page_no: p.page_no(),
                why: if base_version == 0 { ZERO_FORMAT } else { FUTURE_FORMAT },
            });
        }
        if p.nentries() > 1 && (p.nentries() != 2 || (p.slot(1) != SALVAGED && crate::limits::ResourceLimits::decode(p.slot(1)).is_err())) {
            return Err(Error::Corrupt { page_no: p.page_no(), why: "invalid meta extension" });
        }
        let base = 2 + MAX_TREES * 4;
        if rec.len() < base {
            return Err(Error::Corrupt {
                page_no: p.page_no(),
                why: "meta record too short for format_version and roots",
            });
        }
        let mut roots = [0u32; MAX_TREES];
        for (i, slot) in roots.iter_mut().enumerate() {
            *slot = u32::from_le_bytes(rec[2 + i * 4..6 + i * 4].try_into().unwrap());
        }
        let next_lsn = if rec.len() >= base + 8 {
            u64::from_le_bytes(rec[base..base + 8].try_into().unwrap())
        } else { 0 };
        let generation = if rec.len() >= base + 16 {
            u64::from_le_bytes(rec[base + 8..base + 16].try_into().unwrap())
        } else { 0 };
        if format_version & LIMITED != 0 {
            if p.nentries() != 2 { return Err(Error::Corrupt { page_no: p.page_no(), why: "missing resource policy" }); }
            crate::limits::ResourceLimits::decode(p.slot(1))?;
        }
        if format_version & LIMITED == 0 && p.nentries() == 2 && p.slot(1) != SALVAGED {
            return Err(Error::Corrupt { page_no: p.page_no(), why: "resource policy without required format flag" });
        }
        Ok(Meta { format_version, roots, next_lsn, generation })
    }

    pub(crate) fn read_limits(pool: &BufferPool) -> Result<Option<crate::limits::ResourceLimits>> {
        let meta = Self::read_latest(pool)?;
        if meta.format_version & LIMITED == 0 { return Ok(None); }
        let no = (meta.generation % 2) as u32;
        let r = pool.get(no)?;
        let p = PageRef::open_resident(&r, no)?;
        Ok(Some(crate::limits::ResourceLimits::decode(p.slot(1))?))
    }

    pub fn read(pool: &BufferPool) -> Result<Meta> {
        let r = pool.get(META_PAGE)?;
        let p = PageRef::open_resident(&r, META_PAGE)?;
        Self::from_page(&p)
    }

    /// 2f: read BOTH slots, adopt the newest valid one. A slot that fails
    /// its page checksum or does not parse is normally the loser -- a torn
    /// flip leaves the previous publication standing. An intact unsupported
    /// logical version must refuse instead of falling back to stale metadata.
    /// A page-1 slot that is a valid page of any OTHER kind is a pre-2f
    /// file and is refused outright: silently adopting slot 0 would let the
    /// next checkpoint overwrite a live tree page.
    pub fn read_latest(pool: &BufferPool) -> Result<Meta> {
        let read_slot = |page: u32| -> Result<Meta> {
            let r = match pool.get(page) {
                // PageRef checks the physical version before its checksum.
                // Distinguish an intact unsupported page from a damaged copy
                // before deciding whether the sibling may be used instead.
                Err(Error::Corrupt { why: "unknown format version", .. }) => {
                    let mut bytes = [0; crate::page::PAGE_SIZE];
                    pool.file_ref().read_at(&mut bytes, u64::from(page) * crate::page::PAGE_SIZE as u64)?;
                    let why = if crate::page::checksum(&bytes) == u32::from_le_bytes(bytes[36..40].try_into().unwrap()) {
                        "unknown format version"
                    } else { "checksum mismatch" };
                    return Err(Error::Corrupt { page_no: page, why });
                }
                other => other?,
            };
            let p = PageRef::open_resident(&r, page)?;
            if p.kind() != PageKind::Meta {
                return Err(Error::Corrupt { page_no: page, why: "slot is not a meta page" });
            }
            Self::from_page(&p)
        };
        let a = read_slot(META_PAGE);
        let b = read_slot(META_PAGE_B);
        for result in [&a, &b] {
            if let Err(Error::Corrupt { page_no, why }) = result {
                if *why == FUTURE_FORMAT || *why == ZERO_FORMAT || *why == "unknown format version" {
                    return Err(Error::Corrupt { page_no: *page_no, why });
                }
            }
        }
        if let Err(Error::Corrupt { why: "slot is not a meta page", .. }) = &b {
            return Err(Error::Corrupt {
                page_no: META_PAGE_B,
                why: "page 1 is not a meta slot: pre-2f file layout, rebuild via bulk load",
            });
        }
        match (a, b) {
            (Ok(a), Ok(b)) => Ok(if a.generation >= b.generation { a } else { b }),
            (Ok(a), Err(_)) => Ok(a),
            (Err(_), Ok(b)) => Ok(b),
            (Err(e), Err(_)) => Err(e),
        }
    }

    /// Initialise slot B as an EMPTY Meta page (valid page, no record):
    /// recognisably a slot -- so `read_latest` never mistakes this for a
    /// pre-2f file -- but never adoptable until a real flip writes it.
    pub fn init_slot_b(pool: &BufferPool) -> Result<()> {
        let mut w = pool.get_mut(META_PAGE_B)?;
        PageMut::init(w.bytes_mut(), PageKind::Meta, 0, META_PAGE_B).finalise(0);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::scratch_pool;

    /// Shipping makes the on-disk format a contract, and a contract needs a
    /// border guard: a database stamped with a NEWER logical format must be
    /// refused outright — a v1 engine silently opening a v2 file would misread
    /// every keyspace whose encoding changed, and "silently wrong" is the one
    /// failure Law 5 exists to forbid. The page-level version at byte 4 guards
    /// the physical layout; this guards the logical one.
    #[test]
    fn a_future_format_version_is_refused_not_misread() {
        let (pool, _d) = scratch_pool(8);
        let future = SUPPORTED_FORMAT_VERSION + 1;
        let m = Meta { format_version: future, roots: [7, 0, 0, 0, 0, 0, 0, 0], next_lsn: 1, generation: 0 };
        m.write(&pool).unwrap();
        pool.flush_all(crate::io::Barrier::Data).unwrap();
        let err = Meta::read(&pool).expect_err("a future format must not open");
        let text = err.to_string();
        assert!(text.contains(&format!("format version {future}")),
            "the refusal must name the file's version: {text}");
    }

    #[test]
    fn the_superblock_round_trips() {
        let (pool, _d) = scratch_pool(8);
        let m = Meta { format_version: 1, roots: [7, 0, 0, 0, 0, 0, 0, 0], next_lsn: 4242, generation: 0 };
        m.write(&pool).unwrap();
        pool.flush_all(crate::io::Barrier::Data).unwrap();

        let got = Meta::read(&pool).unwrap();
        assert_eq!(got.format_version, 1);
        assert_eq!(got.roots[0], 7);
        assert_eq!(got.next_lsn, 4242, "the LSN high-water mark must survive a round trip");
    }

    /// `Meta::read` must tolerate a superblock written before `next_lsn` existed,
    /// or a format upgrade refuses every database the previous version wrote.
    #[test]
    fn a_superblock_written_before_next_lsn_existed_still_loads() {
        let (pool, _d) = scratch_pool(8);
        {
            // The old layout: version + roots, and nothing after it.
            let mut w = pool.get_mut(META_PAGE).unwrap();
            let mut p = crate::page::PageMut::init(
                w.bytes_mut(), crate::page::PageKind::Meta, 0, META_PAGE);
            let mut rec = 1u16.to_le_bytes().to_vec();
            for r in [9u32, 0, 0, 0, 0, 0, 0, 0] { rec.extend_from_slice(&r.to_le_bytes()); }
            p.insert_slot(0, &rec).unwrap();
            p.finalise(0);
        }
        let got = Meta::read(&pool).unwrap();
        assert_eq!(got.roots[0], 9);
        assert_eq!(got.next_lsn, 0, "a missing field reads as zero, not as an error");
    }

    /// Task 17 re-review, R2. A CRC-valid page with zero slots -- exactly
    /// what `Meta::write`'s own error path can leave behind -- must be
    /// refused with `Err`, not panic. `PageRef::slot(0)` on such a page
    /// returns an empty slice rather than erroring, so `Meta::from_page`
    /// (and `read`, which goes through it) must check `nentries()` itself
    /// before indexing anything out of it.
    #[test]
    fn an_empty_but_crc_valid_meta_page_is_refused_not_panicked() {
        let (pool, _d) = scratch_pool(8);
        {
            let mut w = pool.get_mut(META_PAGE).unwrap();
            let mut p = crate::page::PageMut::init(
                w.bytes_mut(), crate::page::PageKind::Meta, 0, META_PAGE);
            // Deliberately no insert_slot -- zero entries, but still
            // finalised (CRC-valid), exactly like `Meta::write`'s own
            // error path.
            p.finalise(0);
        }
        match Meta::read(&pool) {
            Err(crate::Error::Corrupt { .. }) => {}
            other => panic!("expected Err(Corrupt), got {other:?}"),
        }
    }

    /// The other half of R2/F9, and the one nothing pinned: `write`'s error
    /// path must leave page 0 CRC-VALID, not merely refuse. `pool.get_mut`
    /// has already marked the frame dirty and `PageMut::init` has already
    /// zeroed it by the time `insert_slot` can fail, so a `?` there would
    /// leave a dirty page whose checksum field describes bytes that are no
    /// longer on it -- and an ordinary eviction, or the next `flush_all`,
    /// would publish exactly that. Verified by reading the page back through
    /// `PageRef::open`, which is the check a real reader performs; with the
    /// `finalise(0)` deleted this fails with `Corrupt { why: "checksum
    /// mismatch" }` instead of the empty-page refusal below.
    #[test]
    fn a_refused_meta_record_still_leaves_a_checksum_that_matches_the_page() {
        let (pool, _d) = scratch_pool(8);
        // Larger than a page: `insert_slot` cannot possibly take it.
        let huge = vec![0u8; crate::page::PAGE_SIZE];
        match Meta::write_record(&pool, &huge, META_PAGE) {
            Err(crate::Error::TooLarge) => {}
            other => panic!("expected Err(TooLarge), got {other:?}"),
        }
        let r = pool.get(META_PAGE).unwrap();
        crate::page::PageRef::open_resident(&r, META_PAGE)
            .expect("a refused write must leave page 0 verifiable, not carrying a stale checksum");
        drop(r);
        // And what it left is empty, which `from_page` refuses rather than
        // decodes -- the two halves of the same guarantee.
        match Meta::read(&pool) {
            Err(crate::Error::Corrupt { .. }) => {}
            other => panic!("expected Err(Corrupt) from the empty page, got {other:?}"),
        }
    }
}
