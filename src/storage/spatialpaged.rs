//! A spatial grid that accepts a point in place, instead of being rebuilt.
//!
//! # The structure this replaces
//!
//! `spatialgrid.bin` is one packed file: every node's spatial metadata sorted by
//! hash, then a directory of cells, then every cell's posting list back to back.
//! It is a good read layout — a cell lookup is a binary search and one slice.
//!
//! It also cannot absorb a single new point. Adding one member to a cell makes
//! that cell's run longer, so every run after it moves, so every directory offset
//! after it changes. There is no insert; there is only *rewrite*. So new points
//! accumulate in a RAM overlay and compaction folds the whole grid back.
//!
//! The fold is careful — `write_binary_merged` streams the mapped base against
//! the overlay, so RAM stays bounded — but it still **writes the entire file every
//! time**. Measured over a load, that phase runs at O(N^1.95): 340 ms at 500 000
//! rows, 5 050 ms at 2 million, because each fold costs the store while folds
//! happen ever more often. Extrapolated to fifty million rows it is roughly
//! three quarters of an hour, in one phase.
//!
//! That is Law 2 exactly inverted: the trigger is the change, the work is the
//! store. It is the same shape that was removed from nodes and adjacency, and
//! never applied here.
//!
//! # The layout
//!
//! Two [`PagedStore`]s — a B+tree from key to record id over slotted pages:
//!
//! | store | key | record |
//! |---|---|---|
//! | `spatial_meta` | node hash | that node's six `f64` bbox+centroid |
//! | `spatial_cells` | packed `(cy, cx)` | that cell's list of member hashes |
//!
//! Adding a point writes one meta record and rewrites one cell's list. Nothing
//! else moves, and there is no fold: the write *is* the durable state.
//!
//! # Sacrifices, named
//!
//! - **O(m²) to fill one cell one point at a time**, where m is the cell's
//!   occupancy — the list is a single record, so each insert rewrites it. The way
//!   out is [`insert_many`], which groups by cell and rewrites each once. Bulk
//!   loading must come through it, exactly as adjacency must use `add_many`.
//! - **Disk.** A B+tree entry per cell and per node against a packed array. The
//!   same trade adjacency made, for the same reason.
//! - **A cell read is a tree descent** rather than a binary search over a mapped
//!   array. One tree descent, then one record read.
//!
//! [`insert_many`]: PagedSpatial::insert_many

use super::pagedstore::PagedStore;
use crate::geo::SpatialMeta;
use std::collections::HashMap;
use std::io;
use std::path::Path;

/// crc32 over the rest, then the key this record belongs to.
///
/// Both halves earn their place: the checksum catches a flipped byte inside the
/// right record, and the key catches being handed the *wrong* record — which is
/// the failure that returned one cell's members as another's in the packed
/// format. Law 5 asks for exactly this pair.
const REC_HEADER: usize = 12;
/// Six `f64`: centroid lat/lon, then bbox min lat/lon and max lat/lon.
const META_BYTES: usize = 48;

fn crc(bytes: &[u8]) -> u32 {
    let mut h = crc32fast::Hasher::new();
    h.update(bytes);
    h.finalize()
}

/// Members per cell record.
///
/// # Why a cell is chunked and not one list
///
/// The first version stored a cell's whole membership in one record, the way
/// adjacency stores a node's whole edge list. Adjacency can afford that because
/// degree is a property of the graph and does not grow just because the store
/// does. **Cell occupancy does.** Two million rows over ten thousand cells is two
/// hundred members a cell, and rewriting a two-hundred-entry list to add one
/// member — every compaction, for every cell — is O(N²/cells) hiding inside an
/// O(change) trigger.
///
/// Measured: that version was *slower* than the packed file it replaced, by 3.8 s
/// at 500 000 rows and 14.9 s at two million. The fix is to bound what an append
/// touches, so a cell is a run of capped chunks and only the last one is rewritten.
const CHUNK_CAP: usize = 256;

/// `(cy, cx, chunk)` as one key: 32 bits each for the cell, 32 for the chunk
/// index. Sign is preserved by going through `u32`, and no two fields collide.
#[inline]
fn chunk_key(cy: i32, cx: i32, chunk: u32) -> u128 {
    ((cy as u32 as u128) << 64) | ((cx as u32 as u128) << 32) | chunk as u128
}

fn encode_meta(hash: u64, m: &SpatialMeta, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(REC_HEADER + META_BYTES);
    out.extend_from_slice(&0u32.to_le_bytes()); // checksum, filled below
    out.extend_from_slice(&hash.to_le_bytes());
    for v in [m.centroid_lat, m.centroid_lon, m.bbox_min_lat,
              m.bbox_min_lon, m.bbox_max_lat, m.bbox_max_lon] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    let sum = crc(&out[4..]);
    out[..4].copy_from_slice(&sum.to_le_bytes());
}

fn decode_meta(hash: u64, b: &[u8]) -> Option<SpatialMeta> {
    if b.len() != REC_HEADER + META_BYTES {
        return None;
    }
    if u32::from_le_bytes(b[0..4].try_into().ok()?) != crc(&b[4..]) {
        return None;
    }
    if u64::from_le_bytes(b[4..12].try_into().ok()?) != hash {
        return None;
    }
    let f = |i: usize| -> f64 {
        f64::from_le_bytes(b[REC_HEADER + i * 8..REC_HEADER + i * 8 + 8].try_into().unwrap())
    };
    Some(SpatialMeta {
        centroid_lat: f(0), centroid_lon: f(1),
        bbox_min_lat: f(2), bbox_min_lon: f(3),
        bbox_max_lat: f(4), bbox_max_lon: f(5),
    })
}

fn encode_cell(key: u128, members: &[u64], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(4 + 16 + members.len() * 8);
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&key.to_le_bytes());
    for h in members {
        out.extend_from_slice(&h.to_le_bytes());
    }
    let sum = crc(&out[4..]);
    out[..4].copy_from_slice(&sum.to_le_bytes());
}

/// crc32, then the full `(cy, cx, chunk)` key this run belongs to.
const CELL_HEADER: usize = 4 + 16;

fn decode_cell(key: u128, b: &[u8]) -> Option<Vec<u64>> {
    if b.len() < CELL_HEADER || (b.len() - CELL_HEADER) % 8 != 0 {
        return None;
    }
    if u32::from_le_bytes(b[0..4].try_into().ok()?) != crc(&b[4..]) {
        return None;
    }
    if u128::from_le_bytes(b[4..20].try_into().ok()?) != key {
        return None;
    }
    Some(
        b[CELL_HEADER..]
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect(),
    )
}

/// The spatial grid over paged records. See the module docs.
pub(crate) struct PagedSpatial {
    meta: PagedStore,
    cells: PagedStore,
    scratch: Vec<u8>,
}

impl PagedSpatial {
    pub(crate) fn open(dir: &Path, page_size: usize) -> io::Result<Self> {
        Ok(Self {
            meta: PagedStore::open_named(dir, "spatial_meta", page_size)?,
            cells: PagedStore::open_named(dir, "spatial_cells", page_size)?,
            scratch: Vec::new(),
        })
    }

    /// Nodes carrying spatial metadata.
    pub(crate) fn len(&self) -> u64 { self.meta.len() }

    /// Cells holding at least one member.
    pub(crate) fn cell_count(&self) -> u64 { self.cells.len() }

    pub(crate) fn sync(&mut self) -> io::Result<()> {
        self.meta.sync()?;
        self.cells.sync()
    }

    pub(crate) fn node_meta(&self, hash: u64) -> io::Result<Option<SpatialMeta>> {
        Ok(self.meta.with_value(hash as u128, |b| decode_meta(hash, b))?.flatten())
    }

    /// Member hashes of one cell. Empty when the cell is absent or its record
    /// cannot be vouched for — never another cell's members.
    pub(crate) fn cell_members(&self, cy: i32, cx: i32) -> io::Result<Vec<u64>> {
        let mut out = Vec::new();
        // Chunks are contiguous from zero, so the first absent one ends the cell.
        for chunk in 0u32.. {
            let k = chunk_key(cy, cx, chunk);
            match self.cells.with_value(k, |b| decode_cell(k, b))? {
                Some(Some(mut v)) => out.append(&mut v),
                _ => break,
            }
        }
        Ok(out)
    }

    /// The last chunk of a cell and how full it is — where an append goes.
    fn tail_chunk(&self, cy: i32, cx: i32) -> io::Result<(u32, Vec<u64>)> {
        let mut prev: (u32, Vec<u64>) = (0, Vec::new());
        for chunk in 0u32.. {
            let k = chunk_key(cy, cx, chunk);
            match self.cells.get(k)? {
                Some(b) => prev = (chunk, decode_cell(k, &b).unwrap_or_default()),
                None => break,
            }
        }
        Ok(prev)
    }

    /// Add one point. Prefer [`insert_many`] for more than a handful — see the
    /// O(m²) sacrifice in the module docs.
    ///
    /// [`insert_many`]: PagedSpatial::insert_many
    pub(crate) fn insert(
        &mut self,
        hash: u64,
        meta: &SpatialMeta,
        cells: &[(i32, i32)],
    ) -> io::Result<()> {
        self.insert_many(std::slice::from_ref(&(hash, meta.clone(), cells.to_vec())))
    }

    /// Add many points, rewriting each touched cell exactly once.
    ///
    /// Grouping is what turns O(m²) into O(m): filling a cell with m members one
    /// at a time rewrites its list m times.
    pub(crate) fn insert_many(
        &mut self,
        items: &[(u64, SpatialMeta, Vec<(i32, i32)>)],
    ) -> io::Result<()> {
        self.insert_many_inner(items, true)
    }

    fn insert_many_inner(
        &mut self,
        items: &[(u64, SpatialMeta, Vec<(i32, i32)>)],
        dedup: bool,
    ) -> io::Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut by_cell: HashMap<(i32, i32), Vec<u64>> = HashMap::new();
        let mut scratch = std::mem::take(&mut self.scratch);
        // Ascending by key. These are B+tree writes, and arriving in key order
        // turns a random descent per row into a walk that stays on the pages it
        // just touched. The caller hands them over in hash-map order, which is
        // the worst possible order for a tree.
        let mut order: Vec<usize> = (0..items.len()).collect();
        order.sort_unstable_by_key(|&i| items[i].0);
        for &i in &order {
            let (hash, meta, cells) = &items[i];
            // Append is the fast path and must stay bounded, so it does not scan
            // for an existing copy. Only a point the store already knows about can
            // produce a duplicate, and one probe answers that — so a re-insert
            // pays the scan and a fresh insert does not.
            if dedup && self.meta.get(*hash as u128)?.is_some() {
                self.purge_from_cells(*hash, cells, &mut scratch)?;
            }
            encode_meta(*hash, meta, &mut scratch);
            self.meta.put(*hash as u128, &scratch)?;
            for (cy, cx) in cells {
                by_cell.entry((*cy, *cx)).or_default().push(*hash);
            }
        }
        // Same reason, for the cell records.
        let mut cell_order: Vec<(i32, i32)> = by_cell.keys().copied().collect();
        cell_order.sort_unstable_by_key(|(cy, cx)| ((*cy as u32 as u64) << 32) | *cx as u32 as u64);
        for (cy, cx) in cell_order {
            let adding = by_cell.remove(&(cy, cx)).unwrap_or_default();
            // Only the tail chunk is touched, so an append costs CHUNK_CAP rather
            // than the cell's whole occupancy. That bound is the entire point.
            let (mut chunk, mut tail) = self.tail_chunk(cy, cx)?;
            for h in adding {
                if tail.len() >= CHUNK_CAP {
                    encode_cell(chunk_key(cy, cx, chunk), &tail, &mut scratch);
                    self.cells.put(chunk_key(cy, cx, chunk), &scratch)?;
                    chunk += 1;
                    tail = Vec::with_capacity(CHUNK_CAP);
                }
                tail.push(h);
            }
            encode_cell(chunk_key(cy, cx, chunk), &tail, &mut scratch);
            self.cells.put(chunk_key(cy, cx, chunk), &scratch)?;
        }
        self.scratch = scratch;
        Ok(())
    }

    /// Drop `hash` from the chunks of the given cells, leaving the run's shape
    /// alone. Used when a point is written again: the append would otherwise put
    /// a second copy in the same cell.
    fn purge_from_cells(
        &mut self,
        hash: u64,
        cells: &[(i32, i32)],
        scratch: &mut Vec<u8>,
    ) -> io::Result<()> {
        for (cy, cx) in cells {
            for chunk in 0u32.. {
                let k = chunk_key(*cy, *cx, chunk);
                let Some(bytes) = self.cells.get(k)? else { break };
                let Some(mut list) = decode_cell(k, &bytes) else { continue };
                let before = list.len();
                list.retain(|h| *h != hash);
                if list.len() != before {
                    encode_cell(k, &list, scratch);
                    self.cells.put(k, scratch)?;
                    break;
                }
            }
        }
        Ok(())
    }

    /// Retire a point: drop its metadata and remove it from the cells it occupied.
    pub(crate) fn remove(&mut self, hash: u64, cells: &[(i32, i32)]) -> io::Result<bool> {
        let had = self.meta.delete(hash as u128)?;
        let mut scratch = std::mem::take(&mut self.scratch);
        for (cy, cx) in cells {
            for chunk in 0u32.. {
                let k = chunk_key(*cy, *cx, chunk);
                let Some(bytes) = self.cells.get(k)? else { break };
                let Some(mut list) = decode_cell(k, &bytes) else { continue };
                let before = list.len();
                list.retain(|h| *h != hash);
                if list.len() == before {
                    continue;
                }
                // The chunk is rewritten in place, holes and all. Compacting the
                // run would renumber every chunk after it, which is the rewrite
                // this structure exists to avoid; an empty middle chunk costs one
                // record and must NOT be deleted, because chunk zero-to-first-gap
                // is how a cell's extent is found.
                encode_cell(k, &list, &mut scratch);
                self.cells.put(k, &scratch)?;
                break;
            }
        }
        self.scratch = scratch;
        Ok(had)
    }

    /// Every `(hash, meta)` in the store, for a rebuild or an audit.
    pub(crate) fn for_each_meta(&self, mut f: impl FnMut(u64, SpatialMeta) -> bool) -> io::Result<()> {
        let mut keys: Vec<u128> = Vec::new();
        self.meta.for_each_key(|k, _| { keys.push(k); true })?;
        for k in keys {
            let hash = k as u64;
            if let Some(m) = self.node_meta(hash)? {
                if !f(hash, m) {
                    break;
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(lat: f64, lon: f64) -> SpatialMeta {
        SpatialMeta {
            centroid_lat: lat, centroid_lon: lon,
            bbox_min_lat: lat, bbox_min_lon: lon,
            bbox_max_lat: lat, bbox_max_lon: lon,
        }
    }

    fn store() -> (tempfile::TempDir, PagedSpatial) {
        let dir = tempfile::TempDir::new().unwrap();
        let s = PagedSpatial::open(dir.path(), 4096).unwrap();
        (dir, s)
    }

    #[test]
    fn a_point_is_readable_after_it_is_written() {
        let (_d, mut s) = store();
        s.insert(7, &meta(-8.8, 115.1), &[(-880, 11510)]).unwrap();
        let got = s.node_meta(7).unwrap().expect("meta must read back");
        assert!((got.centroid_lat - -8.8).abs() < 1e-12);
        assert_eq!(s.cell_members(-880, 11510).unwrap(), vec![7]);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn a_cell_holds_every_member_put_in_it() {
        let (_d, mut s) = store();
        let items: Vec<_> = (0..50u64)
            .map(|i| (i, meta(-8.8, 115.1), vec![(-880, 11510)]))
            .collect();
        s.insert_many(&items).unwrap();
        let mut got = s.cell_members(-880, 11510).unwrap();
        got.sort_unstable();
        assert_eq!(got, (0..50).collect::<Vec<u64>>());
    }

    #[test]
    fn re_inserting_a_point_does_not_duplicate_it_in_its_cell() {
        let (_d, mut s) = store();
        s.insert(7, &meta(-8.8, 115.1), &[(-880, 11510)]).unwrap();
        s.insert(7, &meta(-8.8, 115.1), &[(-880, 11510)]).unwrap();
        assert_eq!(s.cell_members(-880, 11510).unwrap(), vec![7]);
    }

    #[test]
    fn removing_a_point_takes_it_out_of_its_cells_and_its_meta() {
        let (_d, mut s) = store();
        s.insert(7, &meta(-8.8, 115.1), &[(-880, 11510), (-880, 11511)]).unwrap();
        s.insert(8, &meta(-8.8, 115.1), &[(-880, 11510)]).unwrap();
        assert!(s.remove(7, &[(-880, 11510), (-880, 11511)]).unwrap());
        assert!(s.node_meta(7).unwrap().is_none());
        assert_eq!(s.cell_members(-880, 11510).unwrap(), vec![8]);
        // The cell 7 alone occupied is gone, not left empty.
        assert!(s.cell_members(-880, 11511).unwrap().is_empty());
    }

    #[test]
    fn everything_survives_a_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        {
            let mut s = PagedSpatial::open(dir.path(), 4096).unwrap();
            let items: Vec<_> = (0..200u64)
                .map(|i| (i, meta(-8.8 + i as f64 * 0.01, 115.1), vec![(i as i32, 7)]))
                .collect();
            s.insert_many(&items).unwrap();
            s.sync().unwrap();
        }
        let s = PagedSpatial::open(dir.path(), 4096).unwrap();
        assert_eq!(s.len(), 200, "metadata was lost across the reopen");
        for i in 0..200u64 {
            assert!(s.node_meta(i).unwrap().is_some(), "node {i} lost its meta");
            assert_eq!(s.cell_members(i as i32, 7).unwrap(), vec![i]);
        }
    }

    /// The property the packed format could not offer: a record proves whose it
    /// is, so a misdirected read is refused rather than answered.
    #[test]
    fn a_record_belonging_to_another_key_is_refused() {
        let mut buf = Vec::new();
        encode_cell(chunk_key(1, 2, 0), &[10, 11], &mut buf);
        assert_eq!(decode_cell(chunk_key(1, 2, 0), &buf), Some(vec![10, 11]));
        assert_eq!(decode_cell(chunk_key(9, 9, 0), &buf), None, "wrong cell was served");

        let mut m = Vec::new();
        encode_meta(42, &meta(1.0, 2.0), &mut m);
        assert!(decode_meta(42, &m).is_some());
        assert!(decode_meta(43, &m).is_none(), "another node's meta was served");
    }

    #[test]
    fn a_flipped_byte_is_refused_not_served() {
        let mut buf = Vec::new();
        encode_cell(chunk_key(1, 2, 0), &[10, 11], &mut buf);
        let n = buf.len();
        buf[n - 1] ^= 0xFF;
        assert_eq!(decode_cell(chunk_key(1, 2, 0), &buf), None, "damage went unnoticed");
    }

    /// The cost that matters: adding to a store that already holds a lot must not
    /// cost more than adding to an empty one. This is Law 2 for this structure.
    #[test]
    fn inserting_costs_the_same_at_every_size() {
        let (_d, mut s) = store();
        let batch = |from: u64| -> Vec<(u64, SpatialMeta, Vec<(i32, i32)>)> {
            (from..from + 2_000)
                .map(|i| (i, meta(-8.8, 115.1), vec![(i as i32 % 500, 3)]))
                .collect()
        };
        s.insert_many(&batch(0)).unwrap();
        let t0 = std::time::Instant::now();
        s.insert_many(&batch(2_000)).unwrap();
        let early = t0.elapsed();

        for k in 2..12u64 {
            s.insert_many(&batch(k * 2_000)).unwrap();
        }
        let t1 = std::time::Instant::now();
        s.insert_many(&batch(12 * 2_000)).unwrap();
        let late = t1.elapsed();

        // Generous: this asserts a shape, not a constant, and CI machines are
        // noisy. A rebuild-shaped cost would be many times over, not 4x.
        assert!(
            late < early * 4 + std::time::Duration::from_millis(20),
            "insert cost grew with the store: {early:?} early against {late:?} late \
             — that is the rebuild shape this structure exists to remove"
        );
    }
}
