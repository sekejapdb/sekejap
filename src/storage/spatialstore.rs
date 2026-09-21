//! # Spatial index — finding things by location, on disk
//!
//! To answer "which venues are within 5 km of this point" without checking every
//! row, sekejap lays a **grid** over the map and records which items fall in each
//! cell. A spatial query then only inspects the handful of cells the query area
//! covers. This file is the disk-first form of that grid.
//!
//! Disk-first (mmap-served) spatial grid: the cell index and per-node spatial
//! metadata served straight from a memory-mapped `spatialgrid.bin`, so paged mode
//! need not rebuild the resident `SpatialGrid` (cells + meta HashMaps) on open.
//!
//! Layout (`SKGRID01`, all little-endian, fixed-size records → binary search):
//!   [0..8]    MAGIC "SKGRID01"
//!   [8..12]   version u32
//!   [12..20]  cell_size f64
//!   [20..24]  node_count u32
//!   meta:     node_count × [hash u64 | 6× f64]   (56 B), sorted by hash
//!   [+4]      cell_count u32
//!   cell dir: cell_count × [cy i32 | cx i32 | off u64 | len u32]  (20 B), sorted by (cy,cx)
//!   [+8]      blob_len u64
//!   blob:     concatenated u64 posting arrays; a cell's `off` is relative to blob start
use crate::geo::SpatialMeta;
use crate::storage::mmap::MmapView;
use std::path::Path;

const MAGIC: &[u8; 8] = b"SKGRID01";
/// Unframed posting runs. Readable, and served exactly as it always was.
const VERSION_PLAIN: u32 = 1;
/// Each posting run is preceded by `[cy i32][cx i32][crc32 u32]`, so a run can
/// be checked to belong to the cell that was asked for. See `geo::CELL_FRAME`.
const VERSION_FRAMED: u32 = 2;
const META_REC: usize = 8 + 6 * 8; // 56
const DIR_REC: usize = 4 + 4 + 8 + 4; // 20

#[derive(Clone)]
pub(crate) struct MappedSpatialGrid {
    view: MmapView,
    cell_size: f64,
    node_count: usize,
    meta_off: usize,
    cell_count: usize,
    dir_off: usize,
    blob_off: usize,
    blob_len: usize,
    /// Whether posting runs carry `[cy][cx][crc32]`. False for stores written
    /// before the framed format; those keep their old behaviour rather than
    /// being declared corrupt.
    framed: bool,
}

fn rd_u32(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }
fn rd_u64(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }
fn rd_i32(b: &[u8], o: usize) -> i32 { i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }
fn rd_f64(b: &[u8], o: usize) -> f64 { f64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

impl MappedSpatialGrid {
    pub(crate) fn open_disk(path: &Path) -> std::io::Result<Option<Self>> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len() as usize;
        let view = match MmapView::try_new(&file, len) { Some(v) => v, None => return Ok(None) };
        let b = match view.slice(0, len) { Some(s) => s, None => return Ok(None) };
        let version = rd_u32(b, 8);
        if len < 24 || &b[0..8] != MAGIC
            || (version != VERSION_PLAIN && version != VERSION_FRAMED)
        {
            return Ok(None);
        }
        let framed = version == VERSION_FRAMED;
        let cell_size = rd_f64(b, 12);
        let node_count = rd_u32(b, 20) as usize;
        let meta_off = 24;
        let after_meta = meta_off + node_count * META_REC;
        if after_meta + 4 > len { return Ok(None); }
        let cell_count = rd_u32(b, after_meta) as usize;
        let dir_off = after_meta + 4;
        let after_dir = dir_off + cell_count * DIR_REC;
        if after_dir + 8 > len { return Ok(None); }
        let blob_len = rd_u64(b, after_dir) as usize;
        let blob_off = after_dir + 8;
        if blob_off + blob_len > len { return Ok(None); }
        Ok(Some(Self { view, cell_size, node_count, meta_off, cell_count, dir_off, blob_off, blob_len, framed }))
    }

    /// Postings at `off`, but only if they belong to `want` and survive their
    /// checksum.
    ///
    /// The directory is not evidence about the blob. It says where a cell's run
    /// begins; the run says which cell it is. When those disagree the read is
    /// refused, because serving the postings anyway is how one cell's members are
    /// returned as another's — silently, and indistinguishably from a correct
    /// answer. `None` here means "this cell cannot be read", which a caller can
    /// act on; a wrong list is not.
    fn read_run(&self, blob: &[u8], off: usize, declared: usize, want: (i32, i32)) -> Option<Vec<u64>> {
        // `declared` comes out of the file, so it is capped by what the blob can
        // hold before it sizes anything — a corrupt count would otherwise reserve
        // billions of entries before any bounds check could run.
        let read_postings = |start: usize, n: usize| -> Vec<u64> {
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                let p = start + i * 8;
                if p + 8 > blob.len() { break }
                out.push(rd_u64(blob, p));
            }
            out
        };

        if !self.framed {
            let room = blob.len().saturating_sub(off) / 8;
            return Some(read_postings(off, declared.min(room)));
        }

        if off + crate::geo::CELL_FRAME > blob.len() {
            return None;
        }
        if (rd_i32(blob, off), rd_i32(blob, off + 4)) != want {
            return None;
        }
        let want_crc = rd_u32(blob, off + 8);
        let start = off + crate::geo::CELL_FRAME;
        let room = blob.len().saturating_sub(start) / 8;
        // A truncated run cannot match its checksum, so capping here refuses it
        // rather than quietly serving the part that survived.
        let n = declared.min(room);
        let body = blob.get(start..start + n * 8)?;
        let mut h = crc32fast::Hasher::new();
        h.update(body);
        if h.finalize() != want_crc || n != declared {
            return None;
        }
        Some(read_postings(start, n))
    }

    pub(crate) fn cell_size(&self) -> f64 { self.cell_size }
    pub(crate) fn len(&self) -> usize { self.node_count }

    /// Spatial metadata for a node — binary search the sorted-by-hash meta array.
    pub(crate) fn node_meta(&self, hash: u64) -> Option<SpatialMeta> {
        let b = self.view.slice(self.meta_off, self.node_count * META_REC)?;
        let (mut lo, mut hi) = (0isize, self.node_count as isize - 1);
        while lo <= hi {
            let mid = ((lo + hi) / 2) as usize;
            let o = mid * META_REC;
            let h = rd_u64(b, o);
            if h == hash {
                return Some(SpatialMeta {
                    centroid_lat: rd_f64(b, o + 8),
                    centroid_lon: rd_f64(b, o + 16),
                    bbox_min_lat: rd_f64(b, o + 24),
                    bbox_min_lon: rd_f64(b, o + 32),
                    bbox_max_lat: rd_f64(b, o + 40),
                    bbox_max_lon: rd_f64(b, o + 48),
                });
            } else if h < hash { lo = mid as isize + 1; } else { hi = mid as isize - 1; }
        }
        None
    }

    /// Number of cells in the directory.
    pub(crate) fn cell_count(&self) -> usize { self.cell_count }

    /// The `i`th meta record, in the file's order — which is ascending by hash.
    ///
    /// `node_meta` binary-searches for one node; this walks them in order, so a
    /// fold can merge the base with its overlay as two sorted runs instead of
    /// rebuilding the whole grid to write it.
    pub(crate) fn meta_at(&self, i: usize) -> Option<(u64, SpatialMeta)> {
        if i >= self.node_count { return None }
        let b = self.view.slice(self.meta_off, self.node_count * META_REC)?;
        let o = i * META_REC;
        Some((
            rd_u64(b, o),
            SpatialMeta {
                centroid_lat: rd_f64(b, o + 8),
                centroid_lon: rd_f64(b, o + 16),
                bbox_min_lat: rd_f64(b, o + 24),
                bbox_min_lon: rd_f64(b, o + 32),
                bbox_max_lat: rd_f64(b, o + 40),
                bbox_max_lon: rd_f64(b, o + 48),
            },
        ))
    }

    /// The `i`th cell, in the file's order — ascending by `(cy, cx)`.
    ///
    /// Postings are capped by what the blob can actually hold, for the same
    /// reason `cell_members` caps them: `n` comes out of the file, and a corrupt
    /// one would otherwise reserve billions of entries before any bounds check
    /// could run.
    pub(crate) fn cell_at(&self, i: usize) -> Option<((i32, i32), Vec<u64>)> {
        if i >= self.cell_count { return None }
        let dir = self.view.slice(self.dir_off, self.cell_count * DIR_REC)?;
        let o = i * DIR_REC;
        let key = (rd_i32(dir, o), rd_i32(dir, o + 4));
        let off = rd_u64(dir, o + 8) as usize;
        let blob = self.view.slice(self.blob_off, self.blob_len)?;
        let declared = rd_u32(dir, o + 16) as usize;
        Some((key, self.read_run(blob, off, declared, key)?))
    }

    /// Node hashes in cell `(cy, cx)` — binary search the sorted-by-(cy,cx) dir,
    /// then read the posting run from the blob.
    pub(crate) fn cell_members(&self, cy: i32, cx: i32) -> Option<Vec<u64>> {
        let dir = self.view.slice(self.dir_off, self.cell_count * DIR_REC)?;
        let key = (cy, cx);
        let (mut lo, mut hi) = (0isize, self.cell_count as isize - 1);
        while lo <= hi {
            let mid = ((lo + hi) / 2) as usize;
            let o = mid * DIR_REC;
            let k = (rd_i32(dir, o), rd_i32(dir, o + 4));
            if k == key {
                let off = rd_u64(dir, o + 8) as usize;
                let blob = self.view.slice(self.blob_off, self.blob_len)?;
                let declared = rd_u32(dir, o + 16) as usize;
                return self.read_run(blob, off, declared, key);
            } else if k < key { lo = mid as isize + 1; } else { hi = mid as isize - 1; }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geo::{SpatialGrid, SpatialMeta};

    fn meta(lat: f64, lon: f64) -> SpatialMeta {
        SpatialMeta {
            centroid_lat: lat,
            centroid_lon: lon,
            bbox_min_lat: lat,
            bbox_min_lon: lon,
            bbox_max_lat: lat,
            bbox_max_lon: lon,
        }
    }

    /// Two cells far enough apart that they never share one.
    fn two_cell_grid() -> SpatialGrid {
        SpatialGrid::build(
            [
                (11u64, meta(-8.80, 115.10)),
                (12u64, meta(-8.80, 115.10)),
                (21u64, meta(-8.20, 115.90)),
                (22u64, meta(-8.20, 115.90)),
            ]
            .into_iter(),
        )
    }

    fn write_to_temp(grid: &SpatialGrid) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("spatialgrid.bin");
        let mut buf: Vec<u8> = Vec::new();
        grid.write_binary(&mut buf).unwrap();
        std::fs::write(&path, &buf).unwrap();
        (dir, path)
    }

    /// A cell's postings must belong to the cell that was asked for.
    ///
    /// The directory entry carries `(cy, cx, off, len)` and the blob run it points
    /// at used to carry nothing at all — no identity, no checksum. Flipping `off`
    /// to another cell's run therefore served that cell's node hashes as this
    /// one's: a wrong answer, silently, which is the failure Law 5 exists to
    /// forbid. Recorded as open in `.workbench/STABLE.md` before this test.
    #[test]
    fn a_damaged_cell_offset_never_serves_another_cells_postings() {
        let grid = two_cell_grid();
        let (_tmp, path) = write_to_temp(&grid);
        let clean = MappedSpatialGrid::open_disk(&path).unwrap().unwrap();
        assert_eq!(clean.cell_count(), 2, "test needs two distinct cells");

        let (key_a, members_a) = clean.cell_at(0).unwrap();
        let (_key_b, members_b) = clean.cell_at(1).unwrap();
        assert_ne!(members_a, members_b, "cells must differ or the test proves nothing");

        // Point cell 0's directory entry at cell 1's run: exactly what one
        // flipped byte in `off` can do.
        let mut bytes = std::fs::read(&path).unwrap();
        let node_count = rd_u32(&bytes, 20) as usize;
        let after_meta = 24 + node_count * META_REC;
        let dir_off = after_meta + 4;
        let off_b = rd_u64(&bytes, dir_off + DIR_REC + 8);
        let len_b = rd_u32(&bytes, dir_off + DIR_REC + 16);
        bytes[dir_off + 8..dir_off + 16].copy_from_slice(&off_b.to_le_bytes());
        bytes[dir_off + 16..dir_off + 20].copy_from_slice(&len_b.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();

        let damaged = MappedSpatialGrid::open_disk(&path).unwrap().unwrap();
        let served = damaged.cell_members(key_a.0, key_a.1).unwrap_or_default();
        assert_ne!(
            served, members_b,
            "cell {key_a:?} was served cell 1's postings {members_b:?} — a damaged \
             offset produced a confidently wrong answer"
        );
        assert!(
            served.is_empty(),
            "a run that fails its identity check must be refused, not partly served"
        );
    }

    /// The fix must not reject good data. A checksum that refuses valid reads is
    /// a worse bug than the one it closes, and would not show up in the
    /// corruption test at all.
    #[test]
    fn every_cell_of_an_undamaged_grid_still_reads() {
        let grid = two_cell_grid();
        let (_tmp, path) = write_to_temp(&grid);
        let g = MappedSpatialGrid::open_disk(&path).unwrap().unwrap();
        assert_eq!(g.cell_count(), 2);
        let mut seen: Vec<u64> = Vec::new();
        for i in 0..g.cell_count() {
            let (key, members) = g.cell_at(i).expect("clean cell must read");
            assert!(!members.is_empty(), "cell {key:?} came back empty");
            assert_eq!(
                g.cell_members(key.0, key.1).as_deref(),
                Some(members.as_slice()),
                "cell_at and cell_members disagree for {key:?}"
            );
            seen.extend(members);
        }
        seen.sort_unstable();
        assert_eq!(seen, vec![11, 12, 21, 22], "some node was lost");
    }

    /// Damage *inside* a run — the case identity alone cannot catch, and the
    /// reason the frame carries a CRC as well as a cell.
    #[test]
    fn a_flipped_byte_inside_a_run_is_refused_not_served() {
        let grid = two_cell_grid();
        let (_tmp, path) = write_to_temp(&grid);
        let clean = MappedSpatialGrid::open_disk(&path).unwrap().unwrap();
        let (key, before) = clean.cell_at(0).unwrap();

        let mut bytes = std::fs::read(&path).unwrap();
        let node_count = rd_u32(&bytes, 20) as usize;
        let after_meta = 24 + node_count * META_REC;
        let dir_off = after_meta + 4;
        let after_dir = dir_off + 2 * DIR_REC;
        let blob_off = after_dir + 8;
        // First posting byte of cell 0, just past its frame.
        let target = blob_off + crate::geo::CELL_FRAME;
        bytes[target] ^= 0xFF;
        std::fs::write(&path, &bytes).unwrap();

        let damaged = MappedSpatialGrid::open_disk(&path).unwrap().unwrap();
        let served = damaged.cell_members(key.0, key.1);
        assert_ne!(
            served.as_deref(),
            Some(before.as_slice()),
            "the damaged byte was not noticed at all"
        );
        assert_eq!(
            served, None,
            "a run failing its CRC must be refused; serving {served:?} is a wrong \
             answer wearing a correct one's clothes"
        );
    }

    /// A grid written before the framed format keeps working. Refusing to read an
    /// existing store because it predates a checksum would be data loss dressed
    /// up as safety.
    #[test]
    fn a_version_one_grid_still_opens_and_reads() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("spatialgrid.bin");
        let mut b: Vec<u8> = Vec::new();
        b.extend_from_slice(MAGIC);
        b.extend_from_slice(&VERSION_PLAIN.to_le_bytes());
        b.extend_from_slice(&0.01f64.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // no meta records
        b.extend_from_slice(&1u32.to_le_bytes()); // one cell
        b.extend_from_slice(&7i32.to_le_bytes());
        b.extend_from_slice(&9i32.to_le_bytes());
        b.extend_from_slice(&0u64.to_le_bytes()); // off
        b.extend_from_slice(&2u32.to_le_bytes()); // two postings
        b.extend_from_slice(&16u64.to_le_bytes()); // blob_len, unframed
        b.extend_from_slice(&41u64.to_le_bytes());
        b.extend_from_slice(&42u64.to_le_bytes());
        std::fs::write(&path, &b).unwrap();

        let g = MappedSpatialGrid::open_disk(&path)
            .unwrap()
            .expect("a version 1 grid must still open");
        assert_eq!(g.cell_members(7, 9), Some(vec![41, 42]));
    }
}
