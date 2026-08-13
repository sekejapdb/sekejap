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
const VERSION: u32 = 1;
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
        if len < 24 || &b[0..8] != MAGIC || rd_u32(b, 8) != VERSION {
            return Ok(None);
        }
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
        Ok(Some(Self { view, cell_size, node_count, meta_off, cell_count, dir_off, blob_off, blob_len }))
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
                let n = rd_u32(dir, o + 16) as usize;
                let blob = self.view.slice(self.blob_off, self.blob_len)?;
                let mut out = Vec::with_capacity(n);
                for i in 0..n {
                    let p = off + i * 8;
                    if p + 8 > blob.len() { break; }
                    out.push(rd_u64(blob, p));
                }
                return Some(out);
            } else if k < key { lo = mid as isize + 1; } else { hi = mid as isize - 1; }
        }
        None
    }
}
