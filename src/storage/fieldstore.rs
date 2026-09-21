//! # Scalar (btree) field index — fast `WHERE x = / < / BETWEEN` and `ORDER BY`
//!
//! When you filter or sort on a column, scanning every row is slow. A btree index
//! keeps that column's values in *sorted* order mapping value → the ids of rows
//! that have it, so an equality or range lookup is a quick search and `ORDER BY`
//! is just reading the tree in order. This file is the on-disk, memory-mapped form
//! of that index: one file per `(collection, field)`, with the bulk posting lists
//! (linear in the row count) living in the mmap — reclaimable OS page cache —
//! instead of a heap `BTreeMap`. So a reopened paged database answers indexed
//! queries with bounded RAM no matter how big the table is.
//!
//! One file per `(collection, field)`: `fieldidx_<coll_hash>_<field>.bin`. The
//! posting lists (the linear-in-N bulk) live in the mmap — reclaimable page
//! cache — instead of the heap `BTreeMap`, so a reopened paged DB serves indexed
//! queries with bounded RAM regardless of dataset size.
//!
//! ## File format
//! ```text
//! [0..8)    magic  "SKFIDX\0\0"
//! [8..16)   nkeys  u64
//! [16..16+nkeys*24)  key directory (24 B/entry), sorted by key ascending:
//!     key_off  u64   absolute offset of the encoded key
//!     key_len  u32   encoded-key length
//!     post_off u64   absolute offset of the posting run (8-aligned)
//!     post_cnt u32   number of u64 postings
//! keys blob:      FieldKey::encode() bytes, back to back
//! postings blob:  raw little-endian u64 node hashes (sorted), 8-aligned start
//! ```
//! Keys are recovered by decode-and-compare during binary search, so the byte
//! layout need not be order-preserving.
#![allow(dead_code)] // wired into compact()/open()/query in a follow-up edit

use std::io::{self, Write};
use std::ops::Bound;
use std::path::Path;

use crate::FieldKey;

/// Unframed posting runs — stores written before the checksum. Still read, and
/// still served exactly as they always were.
const MAGIC_PLAIN: [u8; 8] = *b"SKFIDX\0\0";
/// Each posting run is preceded by `[key_crc32 u32][post_crc32 u32]`.
///
/// # Why a run carries its key
///
/// The directory says "this key's rows live at `post_off`, and there are
/// `post_cnt` of them". The run said nothing about which key it belonged to, so
/// a damaged `post_off` returned another key's rows: `WHERE city = 'Paris'`
/// answered with Lyon's, silently and indistinguishably from a correct result.
///
/// The first word ties the run to its key — the CRC of that key's encoded bytes,
/// which the directory also locates — and the second catches damage inside the
/// run itself. Both are checked before the postings are believed.
///
/// The frame is eight bytes, which preserves the 8-alignment the raw `u64` reads
/// below depend on.
///
/// The sacrifice: eight bytes per key, two CRCs per run that is read, and a
/// durable format change.
const MAGIC_FRAMED: [u8; 8] = *b"SKFIDX\0\x01";
/// Bytes in front of each posting run in [`MAGIC_FRAMED`] files.
const POST_FRAME: usize = 8;

/// Marks a file that carries a superseded-hash trailer.
///
/// # Why a sidecar needs one
///
/// A field sidecar used to be rewritten whole at every compaction — the base
/// streamed through into a new file so the delta could be folded in. That is
/// O(store) work behind a change-sized trigger: 100 ms per compaction at 500 000
/// rows, 207 ms at two million, total N^1.70.
///
/// Keeping the base and writing the delta beside it removes that, but only if a
/// reader can tell which of the base's postings the delta has made stale. A row
/// whose value goes from A to B leaves `A -> hash` in the base, and the delta
/// stores `B -> hash` under a **different key**, so no key-local check can see
/// it. The delta therefore carries the set of hashes it supersedes.
///
/// It is a **sorted array in the file**, binary-searched from the mmap, not a
/// resident `HashSet`. A resident one would be RAM proportional to the change
/// since the last full merge, which is exactly the growth Law 1 forbids and
/// which `GINIndex::slot_of` was already caught doing.
const SUP_MAGIC: [u8; 8] = *b"SKFSUP01";
/// `[count u64][SUP_MAGIC 8]` at the very end, so the section is found by
/// reading backwards without trusting anything in the payload.
const SUP_TAIL: usize = 8 + 8;

/// Append the sorted superseded-hash section. `hashes` need not be sorted.
pub(crate) fn append_superseded(buf: &mut Vec<u8>, hashes: &mut Vec<u64>) {
    hashes.sort_unstable();
    hashes.dedup();
    for h in hashes.iter() {
        buf.extend_from_slice(&h.to_le_bytes());
    }
    buf.extend_from_slice(&(hashes.len() as u64).to_le_bytes());
    buf.extend_from_slice(&SUP_MAGIC);
}
const HEADER_LEN: usize = 16; // magic(8) + nkeys(8)
const DIR_ENTRY: usize = 24; // key_off u64, key_len u32, post_off u64, post_cnt u32

/// Read a little-endian `u64` at `o`, or `0` if the file does not reach that far.
///
/// Every offset handed to these comes out of the file — the directory entry that
/// says where a key or a posting list lives. They indexed the mapped bytes
/// directly, so one flipped byte in a `fieldidx_*.bin` aborted the process:
/// eleven of thirteen corruptions of a single such file crashed, most of them
/// here and the rest in `key_at`.
///
/// Zero is not a *good* answer, but it is a value the callers already handle —
/// an offset of zero points at the magic, whose bytes decode as an empty or
/// absent key — where a panic is an abort nobody can catch. See
/// `tests/corrupt_files.rs`.
#[inline]
fn rd_u64(b: &[u8], o: usize) -> u64 {
    b.get(o..o + 8).map_or(0, |x| u64::from_le_bytes(x.try_into().unwrap()))
}
/// The `u32` counterpart — see [`rd_u64`].
#[inline]
fn rd_u32(b: &[u8], o: usize) -> u32 {
    b.get(o..o + 4).map_or(0, |x| u32::from_le_bytes(x.try_into().unwrap()))
}

/// mmap-or-owned backing (mirrors the private `Backing` in `topology.rs`).
///
/// `Clone`-able so a read snapshot can share this field index: the mmap is shared
/// (an `Arc` bump via [`MmapView`]) and the retained fd is shared via `Arc<File>`
/// (kept only to hold the file open; the mapping itself outlives it on unix).
#[derive(Clone)]
enum Backing {
    #[cfg(unix)]
    Map {
        _file: std::sync::Arc<std::fs::File>,
        map: super::mmap::MmapView,
    },
    Owned(Vec<u8>),
}

impl Backing {
    fn open(path: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            let file = std::fs::File::open(path)?;
            let len = file.metadata()?.len() as usize;
            if let Some(map) = super::mmap::MmapView::try_new(&file, len) {
                return Ok(Backing::Map { _file: std::sync::Arc::new(file), map });
            }
        }
        Ok(Backing::Owned(std::fs::read(path)?))
    }

    fn bytes(&self) -> &[u8] {
        match self {
            #[cfg(unix)]
            Backing::Map { map, .. } => map.slice(0, map.len()).unwrap_or(&[]),
            Backing::Owned(v) => v.as_slice(),
        }
    }
}

/// Serialize a heap btree (`FieldKey -> sorted node hashes`) to the on-disk
/// format at `path`, via a temp file + atomic rename.
/// Write the index from a key-ordered stream, holding none of it.
///
/// [`write`] takes the whole index as a `BTreeMap` and builds the directory, the
/// key blob and the posting blob beside it, so writing a column costs memory
/// proportional to that column. Measured at fold time: 6 MB at 100 000 rows,
/// 92 MB at a million, on its way to gigabytes at the sizes this database is
/// built for. Law 1 does not exempt maintenance — "the occasion is exactly when
/// the database is largest".
///
/// The directory lives at the front of the file but its offsets are only known
/// once every key has been sized, so the stream is consumed twice: once to
/// measure, once to write. `make_iter` therefore has to be able to produce the
/// same sequence twice, and the caller owes that guarantee.
///
/// The three regions — directory, keys, postings — are filled through three
/// handles on the same file, each with its own cursor and its own small buffer.
/// Nothing proportional to the column is ever resident; peak memory is one
/// key's posting list plus 192 KB of buffers.
/// `[crc32(encoded key)][crc32(postings)]`, the eight bytes in front of a run.
fn frame_for(key_bytes: &[u8], post_bytes: &[u8]) -> [u8; POST_FRAME] {
    let mut kh = crc32fast::Hasher::new();
    kh.update(key_bytes);
    let mut ph = crc32fast::Hasher::new();
    ph.update(post_bytes);
    let mut out = [0u8; POST_FRAME];
    out[0..4].copy_from_slice(&kh.finalize().to_le_bytes());
    out[4..8].copy_from_slice(&ph.finalize().to_le_bytes());
    out
}

pub(crate) fn write_merged<F, I>(path: &Path, mut make_iter: F) -> io::Result<()>
where
    F: FnMut() -> I,
    I: Iterator<Item = (FieldKey, Vec<u64>)>,
{
    use std::io::{Seek, SeekFrom};

    // Pass 1 — sizes only. Nothing is kept.
    let (mut nkeys, mut key_bytes, mut npost) = (0usize, 0usize, 0usize);
    let mut kbuf: Vec<u8> = Vec::new();
    for (k, ids) in make_iter() {
        kbuf.clear();
        k.encode(&mut kbuf);
        nkeys += 1;
        key_bytes += kbuf.len();
        npost += ids.len();
    }

    let dir_start = HEADER_LEN;
    let keys_start = dir_start + nkeys * DIR_ENTRY;
    let mut post_start = keys_start + key_bytes;
    post_start += (8 - (post_start % 8)) % 8; // 8-aligned, as `write` does
    let total = post_start + npost * 8 + nkeys * POST_FRAME;

    let tmp = path.with_extension("bin.tmp");
    // Sized up front so the three cursors write into a file that already exists
    // at full length, and so the alignment padding is zero rather than a hole.
    std::fs::File::create(&tmp)?.set_len(total as u64)?;
    let open = || std::fs::OpenOptions::new().write(true).open(&tmp);

    {
        let mut h = open()?;
        h.write_all(&MAGIC_FRAMED)?;
        h.write_all(&(nkeys as u64).to_le_bytes())?;
        h.flush()?;
    }

    let mut dw = std::io::BufWriter::with_capacity(64 << 10, open()?);
    let mut kw = std::io::BufWriter::with_capacity(64 << 10, open()?);
    let mut pw = std::io::BufWriter::with_capacity(64 << 10, open()?);
    dw.seek(SeekFrom::Start(dir_start as u64))?;
    kw.seek(SeekFrom::Start(keys_start as u64))?;
    pw.seek(SeekFrom::Start(post_start as u64))?;

    // Pass 2 — write. The offsets are running totals, so they stay correct only
    // if this pass sees exactly what the first one did.
    let (mut koff, mut poff) = (keys_start as u64, post_start as u64);
    let mut seen = 0usize;
    for (k, ids) in make_iter() {
        kbuf.clear();
        k.encode(&mut kbuf);
        dw.write_all(&koff.to_le_bytes())?;
        dw.write_all(&(kbuf.len() as u32).to_le_bytes())?;
        dw.write_all(&poff.to_le_bytes())?;
        dw.write_all(&(ids.len() as u32).to_le_bytes())?;
        kw.write_all(&kbuf)?;
        koff += kbuf.len() as u64;
        let mut body: Vec<u8> = Vec::with_capacity(ids.len() * 8);
        for &id in &ids {
            body.extend_from_slice(&id.to_le_bytes());
        }
        pw.write_all(&frame_for(&kbuf, &body))?;
        pw.write_all(&body)?;
        poff += POST_FRAME as u64 + (ids.len() as u64) * 8;
        seen += 1;
    }
    dw.flush()?;
    kw.flush()?;
    pw.flush()?;
    drop(dw);
    drop(kw);
    drop(pw);

    // A source that answered differently the second time would leave a directory
    // pointing at bytes that were never written — an index that reads as garbage
    // rather than as missing. Refuse to rename it into place.
    if seen != nkeys {
        let _ = std::fs::remove_file(&tmp);
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("field index source yielded {seen} keys on the write pass but {nkeys} on the sizing pass"),
        ));
    }

    open()?.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// Segment files for one `(collection, field)`, newest first.
///
/// Named `fieldidx_<coll>_<field>.s<NNN>.bin` beside the base
/// `fieldidx_<coll>_<field>.bin`. Ids ascend, so sorting names sorts by age.
pub(crate) fn segment_paths(dir: &Path, stem: &str) -> Vec<(u64, std::path::PathBuf)> {
    let prefix = format!("{stem}.s");
    let mut out: Vec<(u64, std::path::PathBuf)> = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { return out };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        let Some(rest) = name.strip_prefix(&prefix) else { continue };
        let Some(id) = rest.strip_suffix(".bin").and_then(|d| d.parse::<u64>().ok()) else {
            continue;
        };
        out.push((id, e.path()));
    }
    out.sort_by_key(|(id, _)| *id);
    out
}

/// Open the base with every segment stacked on it, newest outermost.
///
/// Each segment filters the ones under it through its own superseded array, and
/// because the chain is built oldest-inward, a posting from the base passes
/// through every segment's filter on its way out. That is what makes a row that
/// moved keys appear only under its newest one.
pub(crate) fn open_stack(dir: &Path, stem: &str) -> io::Result<Option<MappedFieldStore>> {
    let base_path = dir.join(format!("{stem}.bin"));
    let mut acc = MappedFieldStore::open_disk(&base_path)?;
    for (_, p) in segment_paths(dir, stem) {
        match MappedFieldStore::open_disk(&p)? {
            Some(mut seg) => {
                seg.older = acc.map(Box::new);
                acc = Some(seg);
            }
            // A segment that cannot be read is left out rather than served; the
            // rows it held come back on the next full merge from the store.
            None => {}
        }
    }
    Ok(acc)
}

/// Write a **delta segment**: the entries written since the last one, plus the
/// hashes whose base postings they supersede.
///
/// The base sidecar is left alone, which is the whole point — folding the delta
/// into it is the O(store) work this removes.
/// Write one segment beside a base, superseding the hashes it names.
///
/// Not on the write path. Segmenting the delta instead of folding it into the
/// base was measured slower at every size that matters and is reverted; see the
/// note in `CoreDB::compact`. The *reader* side stays, because a database
/// written while the experiment was live still has segments on disk and must
/// converge rather than be stranded, and the reader cannot be tested without a
/// writer. Keep it here, keep it out of `compact`.
pub(crate) fn write_segment<F, I>(
    path: &Path,
    make_iter: F,
    superseded: &mut Vec<u64>,
) -> io::Result<()>
where
    F: FnMut() -> I,
    I: Iterator<Item = (FieldKey, Vec<u64>)>,
{
    write_merged(path, make_iter)?;
    if superseded.is_empty() {
        return Ok(());
    }
    let mut buf: Vec<u8> = Vec::new();
    append_superseded(&mut buf, superseded);
    let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
    f.write_all(&buf)?;
    f.sync_all()
}

pub(crate) fn write(
    path: &Path,
    btree: &std::collections::BTreeMap<FieldKey, Vec<u64>>,
) -> io::Result<()> {
    let nkeys = btree.len();
    let dir_start = HEADER_LEN;
    let keys_start = dir_start + nkeys * DIR_ENTRY;

    // Pass 1: build the keys blob and remember each key's absolute offset/len.
    let mut keys_blob: Vec<u8> = Vec::new();
    let mut key_locs: Vec<(u64, u32)> = Vec::with_capacity(nkeys);
    for k in btree.keys() {
        let off = keys_start as u64 + keys_blob.len() as u64;
        k.encode(&mut keys_blob);
        let len = (keys_start as u64 + keys_blob.len() as u64 - off) as u32;
        key_locs.push((off, len));
    }

    // Postings blob starts 8-aligned so raw u64 reads never straddle awkwardly.
    let mut post_start = keys_start + keys_blob.len();
    post_start += (8 - (post_start % 8)) % 8;
    let mut post_blob: Vec<u8> = Vec::new();
    let mut post_locs: Vec<(u64, u32)> = Vec::with_capacity(nkeys);
    let mut kbuf: Vec<u8> = Vec::new();
    for (k, ids) in btree.iter() {
        // Each run begins at a multiple of 8: the blob start is aligned, the
        // frame is 8 bytes and the postings are 8*n.
        let off = post_start as u64 + post_blob.len() as u64;
        kbuf.clear();
        k.encode(&mut kbuf);
        let mut body: Vec<u8> = Vec::with_capacity(ids.len() * 8);
        for &id in ids {
            body.extend_from_slice(&id.to_le_bytes());
        }
        post_blob.extend_from_slice(&frame_for(&kbuf, &body));
        post_blob.extend_from_slice(&body);
        post_locs.push((off, ids.len() as u32));
    }

    let mut dir: Vec<u8> = Vec::with_capacity(nkeys * DIR_ENTRY);
    for i in 0..nkeys {
        let (koff, klen) = key_locs[i];
        let (poff, pcnt) = post_locs[i];
        dir.extend_from_slice(&koff.to_le_bytes());
        dir.extend_from_slice(&klen.to_le_bytes());
        dir.extend_from_slice(&poff.to_le_bytes());
        dir.extend_from_slice(&pcnt.to_le_bytes());
    }

    let tmp = path.with_extension("bin.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&MAGIC_FRAMED)?;
        f.write_all(&(nkeys as u64).to_le_bytes())?;
        f.write_all(&dir)?;
        f.write_all(&keys_blob)?;
        // pad up to the aligned postings start
        let pad = post_start - (keys_start + keys_blob.len());
        if pad > 0 {
            f.write_all(&vec![0u8; pad])?;
        }
        f.write_all(&post_blob)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// A memory-mapped btree field index. Lookups decode postings into owned `Vec`s
/// (transient — dropped after use); the retained bytes are the reclaimable mmap.
#[derive(Clone)]
pub(crate) struct MappedFieldStore {
    backing: Backing,
    nkeys: usize,
    /// Whether posting runs carry `[key_crc][post_crc]`. False for stores written
    /// before the framed format, which keep their old behaviour rather than being
    /// declared corrupt.
    framed: bool,
    /// `(offset, count)` of the sorted superseded-hash array, when present.
    superseded: Option<(usize, usize)>,
    /// The segment this one was written on top of.
    ///
    /// A delta segment plus the base it supersedes reads as one index, so callers
    /// keep asking a single `MappedFieldStore` and the merge lives here — beside
    /// the format and the tests — instead of in every query path.
    older: Option<Box<MappedFieldStore>>,
}

impl MappedFieldStore {
    /// Open `path` if it exists and has a valid header; `Ok(None)` otherwise.
    pub(crate) fn open_disk(path: &Path) -> io::Result<Option<Self>> {
        if !path.exists() {
            return Ok(None);
        }
        let backing = Backing::open(path)?;
        let b = backing.bytes();
        let framed = b.len() >= HEADER_LEN && b[0..8] == MAGIC_FRAMED;
        if b.len() < HEADER_LEN || (b[0..8] != MAGIC_PLAIN && !framed) {
            return Ok(None);
        }
        // Capped by the file. `nkeys` bounds every binary search and every loop in
        // this store, and it was believed: a corrupt header could claim
        // `u64::MAX` keys, so a search would compute a midpoint far past the end
        // and read there. Each key needs a `DIR_ENTRY` in the directory, so the
        // file's own length says how many there can be.
        let entries_possible = b.len().saturating_sub(HEADER_LEN) / DIR_ENTRY;
        let nkeys = (rd_u64(b, 8) as usize).min(entries_possible);
        // Read backwards: the section is found without trusting the payload.
        let superseded = if b.len() >= SUP_TAIL && b[b.len() - 8..] == SUP_MAGIC {
            let cnt_at = b.len() - SUP_TAIL;
            let count = rd_u64(b, cnt_at) as usize;
            count
                .checked_mul(8)
                .and_then(|len| cnt_at.checked_sub(len))
                .map(|off| (off, count))
        } else {
            None
        };
        Ok(Some(Self { backing, nkeys, framed, superseded, older: None }))
    }

    #[inline]
    /// Distinct keys **in this segment alone**.
    ///
    /// Deliberately not chained. `key_at`/`postings_at` are own-only and index
    /// into this segment's directory, and the fold walks them by index to stay
    /// streaming — materialising the base cost 92 MB at a million rows. Making
    /// `len` chained while those stayed own-only made that walk both overrun its
    /// directory and miss every segment underneath. Use [`stack_len`] for the
    /// whole chain, and [`stack_iter`] to walk it.
    ///
    /// [`stack_len`]: MappedFieldStore::stack_len
    /// [`stack_iter`]: MappedFieldStore::stack_iter
    pub(crate) fn len(&self) -> usize {
        self.nkeys
    }

    /// Upper bound on distinct keys across the chain — an exact count would have
    /// to walk every run to see which keys they share.
    pub(crate) fn stack_len(&self) -> usize {
        self.nkeys + self.older.as_ref().map_or(0, |o| o.stack_len())
    }

    /// A shared empty store, so a caller with no base can still ask for a
    /// stack iterator instead of special-casing `None` at every use.
    pub(crate) fn empty_ref(
        cell: &'static std::sync::OnceLock<MappedFieldStore>,
    ) -> &'static MappedFieldStore {
        cell.get_or_init(|| MappedFieldStore {
            backing: Backing::Owned(Vec::new()),
            nkeys: 0,
            framed: false,
            superseded: None,
            older: None,
        })
    }

    /// The segments of this chain, newest first.
    fn levels(&self) -> Vec<&MappedFieldStore> {
        let mut v = vec![self];
        let mut cur = self;
        while let Some(o) = cur.older.as_deref() {
            v.push(o);
            cur = o;
        }
        v
    }

    /// Streaming merge over the whole chain, in key order.
    ///
    /// Ascending only, which is what the fold needs. Holds one posting list per
    /// level, never the run — the property the index-walking fold had and must
    /// keep.
    pub(crate) fn stack_iter(&self) -> StackIter<'_> {
        let levels = self.levels();
        let heads: Vec<Option<FieldKey>> = levels
            .iter()
            .map(|l| (l.len() > 0).then(|| l.key_at(0)))
            .collect();
        let hiders: Vec<Vec<&MappedFieldStore>> = (0..levels.len())
            .map(|i| {
                levels[..i]
                    .iter()
                    .copied()
                    .filter(|h| h.superseded_len() > 0)
                    .collect()
            })
            .collect();
        StackIter {
            cursors: levels.iter().map(|l| (*l, 0usize)).collect(),
            levels,
            heads,
            hiders,
        }
    }

    /// Read a delta segment stacked on the base it was written against.
    ///
    /// `None` for either half is handled: with no delta this is just the base,
    /// and with no base it is just the delta.
    pub(crate) fn open_chain(base: &Path, delta: &Path) -> io::Result<Option<Self>> {
        let b = Self::open_disk(base)?;
        let d = Self::open_disk(delta)?;
        Ok(match (b, d) {
            (Some(b), Some(mut d)) => {
                d.older = Some(Box::new(b));
                Some(d)
            }
            (Some(b), None) => Some(b),
            (None, d) => d,
        })
    }

    /// Postings this segment hides from older ones.
    fn hides(&self, id: u64) -> bool {
        self.supersedes(id)
    }

    /// Postings for an exact key, across this segment and everything under it.
    pub(crate) fn get_eq(&self, k: &FieldKey) -> Option<Vec<u64>> {
        // Unchained is the overwhelmingly common case and must behave exactly as
        // it always did — including the order postings were stored in, which a
        // test pins deliberately. Only a merge needs to sort.
        let Some(_) = self.older.as_ref() else { return self.get_eq_own(k) };
        let mut ids = self.get_eq_own(k).unwrap_or_default();
        if let Some(o) = &self.older {
            ids.extend(
                o.get_eq(k).unwrap_or_default().into_iter().filter(|id| !self.hides(*id)),
            );
        }
        if ids.is_empty() {
            return None;
        }
        ids.sort_unstable();
        ids.dedup();
        Some(ids)
    }

    /// Candidate postings for a range, across the chain. Membership matters here,
    /// order does not — `iter_kv` is what preserves key order.
    pub(crate) fn range_postings(&self, lo: Bound<&FieldKey>, hi: Bound<&FieldKey>) -> Vec<u64> {
        if self.older.is_none() {
            return self.range_postings_own(lo, hi);
        }
        let mut ids = self.range_postings_own(lo, hi);
        if let Some(o) = &self.older {
            ids.extend(
                o.range_postings(lo, hi).into_iter().filter(|id| !self.hides(*id)),
            );
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// All `(key, postings)` in key order, across the chain.
    ///
    /// Key order is load-bearing — ORDER BY, MIN and MAX read this — so the two
    /// runs are merged as sorted runs rather than concatenated and re-sorted.
    pub(crate) fn iter_kv(&self, rev: bool) -> Vec<(FieldKey, Vec<u64>)> {
        let own = self.iter_kv_own(rev);
        let Some(o) = &self.older else { return own };
        let old = o.iter_kv(rev);

        let mut out: Vec<(FieldKey, Vec<u64>)> = Vec::with_capacity(own.len() + old.len());
        let (mut i, mut j) = (0usize, 0usize);
        while i < own.len() || j < old.len() {
            use std::cmp::Ordering;
            let ord = match (own.get(i), old.get(j)) {
                (Some((a, _)), Some((b, _))) => {
                    let o = a.cmp(b);
                    if rev { o.reverse() } else { o }
                }
                (Some(_), None) => Ordering::Less,
                (None, Some(_)) => Ordering::Greater,
                (None, None) => break,
            };
            let (key, mut ids) = match ord {
                Ordering::Less => { let (k, v) = own[i].clone(); i += 1; (k, v) }
                Ordering::Greater => {
                    let (k, v) = old[j].clone();
                    j += 1;
                    (k, v.into_iter().filter(|id| !self.hides(*id)).collect())
                }
                Ordering::Equal => {
                    let (k, mut v) = own[i].clone();
                    i += 1;
                    v.extend(old[j].1.iter().copied().filter(|id| !self.hides(*id)));
                    j += 1;
                    (k, v)
                }
            };
            // A row sits under one value at a time, so the halves should not both
            // list it — but a segment written twice can, and a duplicate posting
            // is double-counted by every aggregate downstream.
            ids.sort_unstable();
            ids.dedup();
            if !ids.is_empty() {
                out.push((key, ids));
            }
        }
        out
    }

    /// Whether this segment supersedes `hash` — i.e. an older segment's posting
    /// for it is stale and must not be returned.
    ///
    /// A binary search over the mapped array. Nothing is held resident, which is
    /// the point: the set is bounded by the change but it does not need to be in
    /// memory to be consulted.
    pub(crate) fn supersedes(&self, hash: u64) -> bool {
        let Some((off, count)) = self.superseded else { return false };
        let b = self.backing.bytes();
        let (mut lo, mut hi) = (0usize, count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let at = off + mid * 8;
            if at + 8 > b.len() {
                return false;
            }
            match rd_u64(b, at).cmp(&hash) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return true,
            }
        }
        false
    }

    /// How many hashes this segment supersedes — for deciding when to merge.
    pub(crate) fn superseded_len(&self) -> usize {
        self.superseded.map_or(0, |(_, c)| c)
    }

    pub(crate) fn key_at(&self, i: usize) -> FieldKey {
        let b = self.backing.bytes();
        let e = HEADER_LEN + i * DIR_ENTRY;
        let koff = rd_u64(b, e) as usize;
        let klen = rd_u32(b, e + 8) as usize;
        // The offset and length are the file's, and slicing with them directly
        // aborted the process on a corrupt directory entry. A key that does not
        // fit inside the file is not a key; `FieldKey::decode` of nothing is the
        // same absent-key answer this store gives for a hole in the directory.
        FieldKey::decode(b.get(koff..koff + klen).unwrap_or(&[]))
    }

    /// Rows for the `i`th key.
    ///
    /// Empty when the run cannot be trusted — a misdirected offset, or damage
    /// inside the run. Returning nothing is the same absent answer this store
    /// already gives for a hole in the directory, and it is the only safe one:
    /// the alternative is another key's rows, returned confidently.
    pub(crate) fn postings_at(&self, i: usize) -> Vec<u64> {
        let b = self.backing.bytes();
        let e = HEADER_LEN + i * DIR_ENTRY;
        let poff = rd_u64(b, e + 12) as usize;
        // Capped by what remains after the offset: every posting is eight bytes,
        // so a count larger than that cannot be real. Uncapped, a corrupt `cnt`
        // both reserved for billions of entries and walked `poff + j * 8` off the
        // end of the mapping.
        let start = if self.framed { poff + POST_FRAME } else { poff };
        let room = b.len().saturating_sub(start) / 8;
        let declared = rd_u32(b, e + 20) as usize;
        let cnt = declared.min(room);

        if self.framed {
            let koff = rd_u64(b, e) as usize;
            let klen = rd_u32(b, e + 8) as usize;
            let (key_bytes, frame, body) = match (
                b.get(koff..koff + klen),
                b.get(poff..poff + POST_FRAME),
                b.get(start..start + cnt * 8),
            ) {
                (Some(k), Some(f), Some(v)) => (k, f, v),
                _ => return Vec::new(),
            };
            // A truncated run cannot match its checksum, so a capped count is
            // refused rather than partly served.
            if cnt != declared || frame != frame_for(key_bytes, body) {
                return Vec::new();
            }
        }
        (0..cnt).map(|j| rd_u64(b, start + j * 8)).collect()
    }

    /// Binary search for `target`; `Ok(i)` exact, `Err(i)` insertion point.
    fn search(&self, target: &FieldKey) -> Result<usize, usize> {
        let (mut lo, mut hi) = (0usize, self.nkeys);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match self.key_at(mid).cmp(target) {
                std::cmp::Ordering::Less => lo = mid + 1,
                std::cmp::Ordering::Greater => hi = mid,
                std::cmp::Ordering::Equal => return Ok(mid),
            }
        }
        Err(lo)
    }

    /// Postings for an exact key match.
    fn get_eq_own(&self, k: &FieldKey) -> Option<Vec<u64>> {
        self.search(k).ok().map(|i| self.postings_at(i))
    }

    /// The `[start, end)` index window covered by `(lo, hi)` bounds.
    fn window(&self, lo: Bound<&FieldKey>, hi: Bound<&FieldKey>) -> (usize, usize) {
        let start = match lo {
            Bound::Unbounded => 0,
            Bound::Included(k) => match self.search(k) {
                Ok(i) | Err(i) => i,
            },
            Bound::Excluded(k) => match self.search(k) {
                Ok(i) => i + 1,
                Err(i) => i,
            },
        };
        let end = match hi {
            Bound::Unbounded => self.nkeys,
            Bound::Included(k) => match self.search(k) {
                Ok(i) => i + 1,
                Err(i) => i,
            },
            Bound::Excluded(k) => match self.search(k) {
                Ok(i) | Err(i) => i,
            },
        };
        (start, end.max(start))
    }

    /// Concatenated postings for all keys in `(lo, hi)`.
    fn range_postings_own(&self, lo: Bound<&FieldKey>, hi: Bound<&FieldKey>) -> Vec<u64> {
        let (start, end) = self.window(lo, hi);
        let mut out = Vec::new();
        for i in start..end {
            out.extend(self.postings_at(i));
        }
        out
    }

    /// All `(key, postings)` pairs in ascending key order (for GROUP BY / DISTINCT
    /// / ORDER BY index scans). `rev` walks descending.
    fn iter_kv_own(&self, rev: bool) -> Vec<(FieldKey, Vec<u64>)> {
        let idxs: Vec<usize> = if rev {
            (0..self.nkeys).rev().collect()
        } else {
            (0..self.nkeys).collect()
        };
        idxs.into_iter()
            .map(|i| (self.key_at(i), self.postings_at(i)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FieldKey;
    use std::collections::BTreeMap;

    fn num(f: f64) -> FieldKey {
        FieldKey::from_f64(f)
    }
    fn s(x: &str) -> FieldKey {
        FieldKey::Str(x.to_string())
    }

    /// Round-trip a key through encode/decode.
    fn rt(k: FieldKey) -> FieldKey {
        let mut buf = Vec::new();
        k.encode(&mut buf);
        FieldKey::decode(&buf)
    }

    #[test]
    fn codec_roundtrip_all_variants() {
        for k in [
            FieldKey::Null,
            FieldKey::Bool(true),
            FieldKey::Bool(false),
            num(0.0),
            num(-1.5),
            num(3.14159265358979),
            num(f64::MAX),
            num(f64::MIN),
            num(1e300),
            num(-0.0),
            s(""),
            s("hits"),
            s("a string with spaces and 日本語 🚀"),
            s(&"x".repeat(5000)),
        ] {
            assert_eq!(rt(k.clone()), k, "roundtrip failed for {k:?}");
        }
    }

    #[test]
    fn codec_ordering_across_types() {
        // Null < Bool < Number < Str, preserved through encode/decode.
        let ordered = [
            FieldKey::Null,
            FieldKey::Bool(false),
            FieldKey::Bool(true),
            num(-100.0),
            num(0.0),
            num(100.0),
            s("a"),
            s("b"),
        ];
        for w in ordered.windows(2) {
            assert!(rt(w[0].clone()) < rt(w[1].clone()), "{:?} !< {:?}", w[0], w[1]);
        }
    }

    fn build(entries: &[(FieldKey, Vec<u64>)]) -> (tempfile::TempDir, MappedFieldStore) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fieldidx_test.bin");
        let mut bt: BTreeMap<FieldKey, Vec<u64>> = BTreeMap::new();
        for (k, v) in entries {
            bt.insert(k.clone(), v.clone());
        }
        write(&path, &bt).unwrap();
        let store = MappedFieldStore::open_disk(&path).unwrap().unwrap();
        (dir, store)
    }

    #[test]
    fn empty_index() {
        let (_d, store) = build(&[]);
        assert_eq!(store.len(), 0);
        assert_eq!(store.get_eq(&num(1.0)), None);
        assert!(store.range_postings(Bound::Unbounded, Bound::Unbounded).is_empty());
        assert!(store.iter_kv(false).is_empty());
    }

    #[test]
    fn single_key() {
        let (_d, store) = build(&[(num(42.0), vec![7, 8, 9])]);
        assert_eq!(store.len(), 1);
        assert_eq!(store.get_eq(&num(42.0)), Some(vec![7, 8, 9]));
        assert_eq!(store.get_eq(&num(41.0)), None);
        assert_eq!(store.get_eq(&num(43.0)), None);
    }

    #[test]
    fn get_eq_hits_and_misses() {
        let (_d, store) = build(&[
            (num(1.0), vec![10]),
            (num(5.0), vec![20, 21]),
            (num(9.0), vec![30, 31, 32]),
            (s("zebra"), vec![99]),
        ]);
        assert_eq!(store.get_eq(&num(1.0)), Some(vec![10]));
        assert_eq!(store.get_eq(&num(5.0)), Some(vec![20, 21]));
        assert_eq!(store.get_eq(&num(9.0)), Some(vec![30, 31, 32]));
        assert_eq!(store.get_eq(&s("zebra")), Some(vec![99]));
        // misses
        assert_eq!(store.get_eq(&num(2.0)), None);
        assert_eq!(store.get_eq(&num(100.0)), None);
        assert_eq!(store.get_eq(&s("aardvark")), None);
        assert_eq!(store.get_eq(&FieldKey::Null), None);
    }

    /// The authoritative oracle: range_postings must equal the same window over
    /// the source BTreeMap. Exhaustively checks every bound combination.
    #[test]
    fn range_matches_btreemap_oracle() {
        let entries: Vec<(FieldKey, Vec<u64>)> = (0..40)
            .map(|i| (num(i as f64), vec![i as u64 * 1000, i as u64 * 1000 + 1]))
            .collect();
        let mut bt: BTreeMap<FieldKey, Vec<u64>> = BTreeMap::new();
        for (k, v) in &entries {
            bt.insert(k.clone(), v.clone());
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fieldidx_range.bin");
        write(&path, &bt).unwrap();
        let store = MappedFieldStore::open_disk(&path).unwrap().unwrap();

        let probes = [-1.0, 0.0, 0.5, 5.0, 20.0, 39.0, 39.5, 100.0];
        let check = |lb: Bound<&FieldKey>, hb: Bound<&FieldKey>, ctx: &str| {
            let expected: Vec<u64> = bt
                .range((lb, hb))
                .flat_map(|(_, v)| v.iter().copied())
                .collect();
            let got = store.range_postings(lb, hb);
            assert_eq!(got, expected, "mismatch {ctx}");
        };
        for &v in &probes {
            let k = num(v);
            // Single-sided ranges are always valid for BTreeMap regardless of value.
            check(Bound::Excluded(&k), Bound::Unbounded, &format!("> {v}"));
            check(Bound::Included(&k), Bound::Unbounded, &format!(">= {v}"));
            check(Bound::Unbounded, Bound::Excluded(&k), &format!("< {v}"));
            check(Bound::Unbounded, Bound::Included(&k), &format!("<= {v}"));
        }
        check(Bound::Unbounded, Bound::Unbounded, "full");
        // Two-sided ranges only when lo < hi (BTreeMap panics on start > end;
        // range_postings tolerates it, but the executor never emits such ranges).
        for &lo in &probes {
            for &hi in &probes {
                if lo >= hi {
                    continue;
                }
                let (lk, hk) = (num(lo), num(hi));
                check(Bound::Included(&lk), Bound::Excluded(&hk), &format!("[{lo},{hi})"));
                check(Bound::Excluded(&lk), Bound::Included(&hk), &format!("({lo},{hi}]"));
                check(Bound::Included(&lk), Bound::Included(&hk), &format!("[{lo},{hi}]"));
                check(Bound::Excluded(&lk), Bound::Excluded(&hk), &format!("({lo},{hi})"));
            }
        }
        // Degenerate lo>hi against the store directly (oracle would panic): empty.
        let (a, b) = (num(30.0), num(5.0));
        assert!(store.range_postings(Bound::Included(&a), Bound::Excluded(&b)).is_empty());
    }

    #[test]
    fn iter_kv_order_and_rev() {
        let (_d, store) = build(&[
            (num(3.0), vec![3]),
            (num(1.0), vec![1]),
            (num(2.0), vec![2]),
        ]);
        let fwd = store.iter_kv(false);
        assert_eq!(fwd.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(), vec![num(1.0), num(2.0), num(3.0)]);
        let rev = store.iter_kv(true);
        assert_eq!(rev.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(), vec![num(3.0), num(2.0), num(1.0)]);
    }

    #[test]
    fn mixed_type_ordering_on_disk() {
        let (_d, store) = build(&[
            (s("b"), vec![5]),
            (num(2.0), vec![3]),
            (FieldKey::Bool(true), vec![2]),
            (FieldKey::Null, vec![1]),
            (s("a"), vec![4]),
        ]);
        // On-disk order follows FieldKey Ord: Null < Bool < Number < Str.
        let keys: Vec<FieldKey> = store.iter_kv(false).into_iter().map(|(k, _)| k).collect();
        assert_eq!(keys, vec![FieldKey::Null, FieldKey::Bool(true), num(2.0), s("a"), s("b")]);
    }

    /// Varying key lengths make keys_blob a non-multiple of 8, stressing the
    /// postings-blob alignment padding. Large u64s exercise full-width reads.
    #[test]
    fn alignment_and_large_u64() {
        let entries: Vec<(FieldKey, Vec<u64>)> = vec![
            (s("x"), vec![u64::MAX, 0, 1]),
            (s("yy"), vec![u64::MAX - 1]),
            (s("zzz"), vec![1 << 63, (1 << 63) + 7]),
            (s("wwww"), vec![12345678901234567, 9]),
            (num(1.0), vec![u64::MAX / 2]),
        ];
        let (_d, store) = build(&entries);
        assert_eq!(store.get_eq(&s("x")), Some(vec![u64::MAX, 0, 1]));
        assert_eq!(store.get_eq(&s("zzz")), Some(vec![1 << 63, (1 << 63) + 7]));
        assert_eq!(store.get_eq(&num(1.0)), Some(vec![u64::MAX / 2]));
    }

    #[test]
    fn large_posting_lists() {
        let big: Vec<u64> = (0..50_000u64).map(|i| i.wrapping_mul(2654435761)).collect();
        let (_d, store) = build(&[(num(1.0), big.clone()), (num(2.0), vec![42])]);
        assert_eq!(store.get_eq(&num(1.0)), Some(big));
        assert_eq!(store.get_eq(&num(2.0)), Some(vec![42]));
    }

    #[test]
    fn persistence_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fieldidx_persist.bin");
        let mut bt: BTreeMap<FieldKey, Vec<u64>> = BTreeMap::new();
        bt.insert(num(1.0), vec![1, 2, 3]);
        bt.insert(s("hi"), vec![9]);
        write(&path, &bt).unwrap();
        drop(bt);
        // Fresh open, no in-memory state.
        let store = MappedFieldStore::open_disk(&path).unwrap().unwrap();
        assert_eq!(store.get_eq(&num(1.0)), Some(vec![1, 2, 3]));
        assert_eq!(store.get_eq(&s("hi")), Some(vec![9]));
    }

    #[test]
    fn missing_and_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        // missing → Ok(None)
        let missing = dir.path().join("nope.bin");
        assert!(MappedFieldStore::open_disk(&missing).unwrap().is_none());
        // corrupt/short → Ok(None)
        let bad = dir.path().join("bad.bin");
        std::fs::write(&bad, b"not a real header").unwrap();
        assert!(MappedFieldStore::open_disk(&bad).unwrap().is_none());
        // empty file → Ok(None)
        let empty = dir.path().join("empty.bin");
        std::fs::write(&empty, b"").unwrap();
        assert!(MappedFieldStore::open_disk(&empty).unwrap().is_none());
    }
}

#[cfg(test)]
mod corruption_tests {
    use super::*;
    use crate::FieldKey;
    use std::collections::BTreeMap;

    fn two_key_index() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fieldidx_test.bin");
        let mut bt: BTreeMap<FieldKey, Vec<u64>> = BTreeMap::new();
        bt.insert(FieldKey::Str("lyon".into()), vec![101, 102, 103]);
        bt.insert(FieldKey::Str("paris".into()), vec![201, 202]);
        write(&path, &bt).unwrap();
        (dir, path)
    }

    /// A key's rows must belong to that key.
    ///
    /// The directory locates a run; the run used to say nothing about whose it
    /// was. Repointing one key's `post_off` at another's therefore answered
    /// `WHERE city = 'paris'` with lyon's rows — confidently, and
    /// indistinguishably from a correct answer.
    #[test]
    fn a_damaged_posting_offset_never_serves_another_keys_rows() {
        let (_tmp, path) = two_key_index();
        let clean = MappedFieldStore::open_disk(&path).unwrap().unwrap();
        assert_eq!(clean.len(), 2);
        let lyon = clean.postings_at(0);
        let paris = clean.postings_at(1);
        assert_ne!(lyon, paris, "keys must differ or the test proves nothing");

        // Point key 1's run at key 0's run.
        let mut b = std::fs::read(&path).unwrap();
        let e0 = HEADER_LEN;
        let e1 = HEADER_LEN + DIR_ENTRY;
        let poff0 = rd_u64(&b, e0 + 12);
        let cnt0 = rd_u32(&b, e0 + 20);
        b[e1 + 12..e1 + 20].copy_from_slice(&poff0.to_le_bytes());
        b[e1 + 20..e1 + 24].copy_from_slice(&cnt0.to_le_bytes());
        std::fs::write(&path, &b).unwrap();

        let damaged = MappedFieldStore::open_disk(&path).unwrap().unwrap();
        let served = damaged.postings_at(1);
        assert_ne!(served, lyon, "key 1 was served key 0's rows {lyon:?}");
        assert!(served.is_empty(), "an untrustworthy run must be refused, not partly served");
    }

    /// Damage inside a run — what the key identity alone cannot catch, and the
    /// reason the frame carries two checksums.
    #[test]
    fn a_flipped_byte_inside_a_run_is_refused() {
        let (_tmp, path) = two_key_index();
        let clean = MappedFieldStore::open_disk(&path).unwrap().unwrap();
        let before = clean.postings_at(0);
        assert!(!before.is_empty());

        let mut b = std::fs::read(&path).unwrap();
        let poff = rd_u64(&b, HEADER_LEN + 12) as usize;
        b[poff + POST_FRAME] ^= 0xFF; // first posting byte, past the frame
        std::fs::write(&path, &b).unwrap();

        let damaged = MappedFieldStore::open_disk(&path).unwrap().unwrap();
        assert!(
            damaged.postings_at(0).is_empty(),
            "a run failing its CRC must be refused, not served"
        );
    }

    /// The checksum must not reject valid data — a fix that does that is worse
    /// than the bug it closes, and no corruption test would catch it.
    #[test]
    fn an_undamaged_index_still_answers_every_key() {
        let (_tmp, path) = two_key_index();
        let g = MappedFieldStore::open_disk(&path).unwrap().unwrap();
        assert_eq!(g.get_eq(&FieldKey::Str("lyon".into())), Some(vec![101, 102, 103]));
        assert_eq!(g.get_eq(&FieldKey::Str("paris".into())), Some(vec![201, 202]));
        let kv = g.iter_kv(false);
        assert_eq!(kv.len(), 2, "iteration lost a key");
        assert!(kv.iter().all(|(_, v)| !v.is_empty()), "iteration lost a run");
    }

    /// `write_merged` is the other writer and must produce the same framing.
    #[test]
    fn the_merged_writer_produces_readable_framed_runs() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fieldidx_merged.bin");
        let rows = || {
            vec![
                (FieldKey::Str("alpha".into()), vec![1u64, 2]),
                (FieldKey::Str("beta".into()), vec![3u64]),
            ]
            .into_iter()
        };
        write_merged(&path, rows).unwrap();
        let g = MappedFieldStore::open_disk(&path).unwrap().unwrap();
        assert_eq!(g.get_eq(&FieldKey::Str("alpha".into())), Some(vec![1, 2]));
        assert_eq!(g.get_eq(&FieldKey::Str("beta".into())), Some(vec![3]));
    }

    /// An index written before the framed format keeps working. Refusing to read
    /// an existing store because it predates a checksum is data loss dressed up
    /// as safety.
    #[test]
    fn a_plain_magic_index_still_opens_and_reads() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fieldidx_plain.bin");
        let mut kbuf = Vec::new();
        FieldKey::Str("solo".into()).encode(&mut kbuf);
        let keys_start = HEADER_LEN + DIR_ENTRY;
        let mut post_start = keys_start + kbuf.len();
        post_start += (8 - (post_start % 8)) % 8;

        let mut b: Vec<u8> = Vec::new();
        b.extend_from_slice(&MAGIC_PLAIN);
        b.extend_from_slice(&1u64.to_le_bytes());
        b.extend_from_slice(&(keys_start as u64).to_le_bytes());
        b.extend_from_slice(&(kbuf.len() as u32).to_le_bytes());
        b.extend_from_slice(&(post_start as u64).to_le_bytes());
        b.extend_from_slice(&2u32.to_le_bytes());
        b.extend_from_slice(&kbuf);
        while b.len() < post_start { b.push(0); }
        b.extend_from_slice(&7u64.to_le_bytes());
        b.extend_from_slice(&8u64.to_le_bytes());
        std::fs::write(&path, &b).unwrap();

        let g = MappedFieldStore::open_disk(&path)
            .unwrap()
            .expect("a pre-checksum index must still open");
        assert_eq!(g.get_eq(&FieldKey::Str("solo".into())), Some(vec![7, 8]));
    }
}

#[cfg(test)]
mod segment_tests {
    use super::*;
    use crate::FieldKey;

    fn k(x: &str) -> FieldKey { FieldKey::Str(x.to_string()) }

    /// A delta segment must carry both its entries and the hashes it supersedes,
    /// and the supersede check must not hold anything resident.
    #[test]
    fn a_delta_segment_round_trips_entries_and_supersedes() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fieldidx_x.d.bin");
        let rows = || vec![(k("paris"), vec![7u64, 9]), (k("rome"), vec![11u64])].into_iter();
        let mut sup = vec![42u64, 7, 42]; // unsorted, with a duplicate
        write_segment(&path, rows, &mut sup).unwrap();

        let seg = MappedFieldStore::open_disk(&path).unwrap().unwrap();
        assert_eq!(seg.get_eq(&k("paris")), Some(vec![7, 9]));
        assert_eq!(seg.get_eq(&k("rome")), Some(vec![11]));
        assert_eq!(seg.superseded_len(), 2, "duplicates should collapse");
        assert!(seg.supersedes(7), "7 was superseded");
        assert!(seg.supersedes(42), "42 was superseded");
        assert!(!seg.supersedes(9), "9 was not superseded");
        assert!(!seg.supersedes(0));
        assert!(!seg.supersedes(u64::MAX));
    }

    /// The whole point: a delta segment stacked on a base reads as one index,
    /// and the row whose value moved appears ONLY under its new key.
    ///
    /// This is the case no key-local check can get right. Row 2 was `paris` and
    /// is now `rome`; the base still says `paris -> 2`, and nothing about the key
    /// `paris` reveals that. Only the segment's superseded set does.
    #[test]
    fn a_chained_segment_hides_the_rows_it_superseded() {
        let dir = tempfile::TempDir::new().unwrap();
        let base_path = dir.path().join("fieldidx_c.bin");
        let delta_path = dir.path().join("fieldidx_c.d.bin");

        let mut bt: std::collections::BTreeMap<FieldKey, Vec<u64>> = Default::default();
        bt.insert(k("lyon"), vec![5]);
        bt.insert(k("paris"), vec![1, 2, 3]);
        write(&base_path, &bt).unwrap();

        // Row 2 moved from paris to rome.
        let rows = || vec![(k("rome"), vec![2u64])].into_iter();
        write_segment(&delta_path, rows, &mut vec![2u64]).unwrap();

        let ix = MappedFieldStore::open_chain(&base_path, &delta_path).unwrap().unwrap();

        assert_eq!(ix.get_eq(&k("paris")), Some(vec![1, 3]), "row 2 must be gone from paris");
        assert_eq!(ix.get_eq(&k("rome")), Some(vec![2]), "row 2 must appear under rome");
        assert_eq!(ix.get_eq(&k("lyon")), Some(vec![5]), "an untouched key must survive");

        let all = ix.range_postings(Bound::Unbounded, Bound::Unbounded);
        assert_eq!(all, vec![1, 2, 3, 5], "every live row exactly once");

        let kv = ix.iter_kv(false);
        let keys: Vec<String> = kv.iter().map(|(k, _)| format!("{k:?}")).collect();
        assert_eq!(kv.len(), 3, "three live keys, got {keys:?}");
        assert_eq!(kv[0].1, vec![5], "lyon first in key order");
        assert_eq!(kv[1].1, vec![1, 3], "paris without the moved row");
        assert_eq!(kv[2].1, vec![2], "rome with it");

        let rev = ix.iter_kv(true);
        assert_eq!(rev.len(), 3);
        assert_eq!(rev[0].1, vec![2], "descending order must reverse too");
    }

    /// A segment with nothing superseded must still read, and must claim nothing.
    #[test]
    fn a_segment_with_no_supersedes_claims_none() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fieldidx_y.d.bin");
        let rows = || vec![(k("solo"), vec![1u64])].into_iter();
        write_segment(&path, rows, &mut Vec::new()).unwrap();

        let seg = MappedFieldStore::open_disk(&path).unwrap().unwrap();
        assert_eq!(seg.get_eq(&k("solo")), Some(vec![1]));
        assert_eq!(seg.superseded_len(), 0);
        assert!(!seg.supersedes(1));
    }

    /// An ordinary sidecar, written before segments existed, has no trailer and
    /// must not be read as if it had one.
    #[test]
    fn a_plain_sidecar_has_no_supersede_section() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fieldidx_z.bin");
        let mut bt: std::collections::BTreeMap<FieldKey, Vec<u64>> = Default::default();
        bt.insert(k("a"), vec![1, 2]);
        write(&path, &bt).unwrap();

        let base = MappedFieldStore::open_disk(&path).unwrap().unwrap();
        assert_eq!(base.get_eq(&k("a")), Some(vec![1, 2]));
        assert_eq!(base.superseded_len(), 0);
        assert!(!base.supersedes(1), "a plain sidecar supersedes nothing");
    }
}

/// Streaming k-way merge across a chain of segments. See
/// [`MappedFieldStore::stack_iter`].
pub(crate) struct StackIter<'a> {
    levels: Vec<&'a MappedFieldStore>,
    cursors: Vec<(&'a MappedFieldStore, usize)>,
    /// The key each level is currently sitting on.
    ///
    /// Cached because `key_at` DECODES a `FieldKey` from the mapped bytes, and a
    /// `Str` key allocates. Asking every level for its head key on every output
    /// key — twice, once to find the smallest and once to see who has it — made
    /// this merge allocate `2 x levels` strings per key and cost more than the
    /// whole-file rewrite it was meant to replace.
    heads: Vec<Option<FieldKey>>,
    /// For each level, the levels above it that actually supersede anything.
    ///
    /// Precomputed. Building this per output key allocated a `Vec` for every key
    /// in the index, which is the kind of cost that turns a merge meant to save
    /// work into one that adds it.
    hiders: Vec<Vec<&'a MappedFieldStore>>,
}

impl<'a> Iterator for StackIter<'a> {
    type Item = (FieldKey, Vec<u64>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // Smallest key still unread on any level, from the cached heads.
            let mut best: Option<&FieldKey> = None;
            for h in self.heads.iter().flatten() {
                if best.is_none_or(|b| h < b) {
                    best = Some(h);
                }
            }
            let key = best?.clone();

            // Every level holding it contributes, newest first. A posting from a
            // lower level is dropped when any higher one supersedes it — which is
            // how a row that moved keys appears only under its newest.
            let mut ids: Vec<u64> = Vec::new();
            for idx in 0..self.cursors.len() {
                if self.heads[idx].as_ref() != Some(&key) {
                    continue;
                }
                let (lvl, i) = self.cursors[idx];
                let hidden_by = &self.hiders[idx];
                if hidden_by.is_empty() {
                    // Nothing above supersedes anything, so no filter is needed —
                    // the common case by far during a load.
                    ids.extend(lvl.postings_at(i));
                } else {
                    ids.extend(
                        lvl.postings_at(i)
                            .into_iter()
                            .filter(|id| !hidden_by.iter().any(|h| h.supersedes(*id))),
                    );
                }
                let next = i + 1;
                self.cursors[idx].1 = next;
                self.heads[idx] = (next < lvl.len()).then(|| lvl.key_at(next));
            }
            ids.sort_unstable();
            ids.dedup();
            // A key whose every row has moved away is skipped, not yielded empty.
            if !ids.is_empty() {
                return Some((key, ids));
            }
        }
    }
}
