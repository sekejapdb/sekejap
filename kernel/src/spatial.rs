//! 2i: space as a key discipline (FACT-04, D31).
//!
//! A newcomer's map. To index locations in a btree we need a way to turn
//! two dimensions (lon, lat) into ONE sortable number such that places
//! near each other in space usually get nearby numbers -- then "everything
//! in this box" becomes a few contiguous key ranges. A Hilbert curve does
//! exactly that (better locality than the simpler Z-order: the curve never
//! makes the big diagonal jumps Z does). We port PostGIS's own Hilbert
//! implementation -- notable because PostGIS, the reference R-tree system,
//! uses this very ordering to BUILD its R-trees; curve order is the best
//! layout the R-tree camp knows, so we store in curve order directly.
//!
//! Two fixed levels: FINE cells (16 bits per axis, ~600 m at the equator)
//! for points and small shapes; COARSE (8 bits per axis) for anything
//! whose bounding box would need more than MAX_CELLS fine cells. A
//! geometry posts to at most MAX_CELLS cells -- write cost O(1) per
//! geometry, ever (Law 2).
//!
//! Values carry the geometry's bounding box as four f32s rounded OUTWARD
//! (also PostGIS's trick): a float box strictly containing the double box
//! filters candidates without touching the payload; a degenerate box
//! (xmin==xmax, ymin==ymax) IS a point, so point workloads answer exact
//! distances straight from the posting -- zero payload reads.

/// Hilbert index of an (x, y) cell on a 2^bits x 2^bits grid -- the
/// classic level-parameterized xy2d walk (public domain; PostGIS uses the
/// same curve family via a 32-bit bit-scan for its sorted R-tree builds,
/// which inspired this keyspace). The bit-scan variant is fixed to 32-bit
/// grids; our cells live at 8- and 16-bit levels, and a cell's index must
/// be computed AT ITS LEVEL or aligned squares stop being contiguous runs
/// -- the oracle test below caught exactly that with a scaled-shift
/// shortcut. O(bits) per call, index-time only.
pub fn cell_hilbert(cx: u32, cy: u32, bits: u8) -> u64 {
    let n: u64 = 1u64 << bits;
    let (mut x, mut y) = (cx as u64, cy as u64);
    let mut d: u64 = 0;
    let mut s: u64 = n / 2;
    while s > 0 {
        let rx = if (x & s) > 0 { 1u64 } else { 0 };
        let ry = if (y & s) > 0 { 1u64 } else { 0 };
        d += s * s * ((3 * rx) ^ ry);
        // rotate
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - (x & (s - 1)) | (x & !(2 * s - 1));
                y = s - 1 - (y & (s - 1)) | (y & !(2 * s - 1));
            }
            std::mem::swap(&mut x, &mut y);
        }
        s /= 2;
    }
    d
}

pub const LEVEL_FINE: u8 = 12;
pub const LEVEL_COARSE: u8 = 8;
/// Geometries too big for MAX_CELLS coarse cells (continent scale) post
/// ONE entry in the world bucket, which every query also scans. Sound
/// (never missed), bounded (one posting), cheap (members are rare and
/// bbox-filtered). The corner-clip shortcut this replaces missed interior
/// queries -- caught by the coarse-fallback oracle test.
pub const LEVEL_WORLD: u8 = 0;
/// A geometry posts to at most this many cells (Law 2's bound).
pub const MAX_CELLS: usize = 8;

/// Quantize lon in [-180,180], lat in [-90,90] to `bits`-per-axis cells.
pub fn cell_of(lon: f64, lat: f64, bits: u8) -> (u32, u32) {
    let n = (1u64 << bits) as f64;
    let cx = (((lon + 180.0) / 360.0) * n).floor().clamp(0.0, n - 1.0) as u32;
    let cy = (((lat + 90.0) / 180.0) * n).floor().clamp(0.0, n - 1.0) as u32;
    (cx, cy)
}

/// Outward-rounded f32 bounding box -- PostGIS `box2df_from_gbox_p`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BoxF {
    pub xmin: f32, pub xmax: f32, pub ymin: f32, pub ymax: f32,
}
fn next_down(v: f64) -> f32 {
    let f = v as f32;
    if (f as f64) > v { f32::from_bits(if f > 0.0 { f.to_bits() - 1 } else { f.to_bits() + 1 }) } else { f }
}
fn next_up(v: f64) -> f32 {
    let f = v as f32;
    if (f as f64) < v { f32::from_bits(if f >= 0.0 { f.to_bits() + 1 } else { f.to_bits() - 1 }) } else { f }
}
impl BoxF {
    pub fn from_f64(xmin: f64, xmax: f64, ymin: f64, ymax: f64) -> BoxF {
        BoxF { xmin: next_down(xmin), xmax: next_up(xmax),
               ymin: next_down(ymin), ymax: next_up(ymax) }
    }
    pub fn intersects(&self, o: &BoxF) -> bool {
        self.xmin <= o.xmax && self.xmax >= o.xmin
            && self.ymin <= o.ymax && self.ymax >= o.ymin
    }
    /// Degenerate box = the geometry is a point (its exact coordinates).
    pub fn as_point(&self) -> Option<(f64, f64)> {
        if self.xmin == self.xmax && self.ymin == self.ymax {
            Some((self.xmin as f64, self.ymin as f64))
        } else { None }
    }
    pub fn encode(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..4].copy_from_slice(&self.xmin.to_le_bytes());
        b[4..8].copy_from_slice(&self.xmax.to_le_bytes());
        b[8..12].copy_from_slice(&self.ymin.to_le_bytes());
        b[12..16].copy_from_slice(&self.ymax.to_le_bytes());
        b
    }
    pub fn decode(b: &[u8]) -> Option<BoxF> {
        if b.len() < 16 { return None; }
        Some(BoxF {
            xmin: f32::from_le_bytes(b[0..4].try_into().unwrap()),
            xmax: f32::from_le_bytes(b[4..8].try_into().unwrap()),
            ymin: f32::from_le_bytes(b[8..12].try_into().unwrap()),
            ymax: f32::from_le_bytes(b[12..16].try_into().unwrap()),
        })
    }
}

/// The cells a bbox covers at `bits` per axis, capped: returns None when
/// the cover would exceed `max` cells (caller drops to a coarser level).
pub fn cover_cells(xmin: f64, xmax: f64, ymin: f64, ymax: f64, bits: u8, max: usize)
    -> Option<Vec<(u32, u32)>>
{
    let (x0, y0) = cell_of(xmin, ymin, bits);
    let (x1, y1) = cell_of(xmax, ymax, bits);
    let w = (x1 - x0 + 1) as usize;
    let h = (y1 - y0 + 1) as usize;
    if w.saturating_mul(h) > max { return None; }
    let mut out = Vec::with_capacity(w * h);
    for cy in y0..=y1 {
        for cx in x0..=x1 {
            out.push((cx, cy));
        }
    }
    Some(out)
}

/// Decompose a query bbox into Hilbert key RANGES at one level: a quadtree
/// refinement over Hilbert quadrants, emitting one [lo, hi] run for every
/// square fully inside the box, dropping squares fully outside, and
/// splitting the ones that straddle the box's edge. `max_ranges` caps the
/// output; the refinement spends that budget where it buys the most.
///
/// The budget is spent WASTE-FIRST. Every straddling square carries the
/// number of its cells that lie outside the box; the square with the most
/// outside cells is always the next one split, because that split removes
/// the most cells the scan would otherwise read and throw away. When one
/// more split could push the count of runs past the budget, every square
/// still straddling is emitted whole -- a slightly larger scan, filtered
/// exactly per posting, never a miss. A depth-first descent that checked
/// its budget against its own stack was the earlier shape of this: three
/// pending siblings per level ate fifty of sixty-four slots at sixteen
/// bits, so it stopped refining at the fourth level and read five times the
/// box for a 50 km radius.
pub fn cover_ranges(xmin: f64, xmax: f64, ymin: f64, ymax: f64, bits: u8,
                    max_ranges: usize) -> Vec<(u64, u64)>
{
    use std::collections::BinaryHeap;
    let (qx0, qy0) = cell_of(xmin, ymin, bits);
    let (qx1, qy1) = cell_of(xmax, ymax, bits);
    let budget = max_ranges.max(4);
    // An aligned power-of-two square of side `size` at cell (x, y); its
    // cells are one contiguous hilbert run starting at the least of its
    // corners (see `aligned_squares_are_contiguous_hilbert_runs`).
    #[derive(PartialEq, Eq, PartialOrd, Ord)]
    struct Straddling { waste: u64, size: u32, x: u32, y: u32 }
    let run = |x: u32, y: u32, size: u32| -> (u64, u64) {
        let (x1, y1) = (x + size - 1, y + size - 1);
        let corners = [
            cell_hilbert(x, y, bits), cell_hilbert(x1, y, bits),
            cell_hilbert(x, y1, bits), cell_hilbert(x1, y1, bits),
        ];
        let lo = *corners.iter().min().unwrap();
        (lo, lo + (size as u64) * (size as u64) - 1)
    };
    // How many of the square's cells fall inside the box: `None` when
    // none do, so the square is dropped.
    let inside = |x: u32, y: u32, size: u32| -> Option<u64> {
        let (x1, y1) = (x + size - 1, y + size - 1);
        if x1 < qx0 || x > qx1 || y1 < qy0 || y > qy1 { return None; }
        let w = (x1.min(qx1) - x.max(qx0) + 1) as u64;
        let h = (y1.min(qy1) - y.max(qy0) + 1) as u64;
        Some(w * h)
    };
    let mut out: Vec<(u64, u64)> = Vec::new();
    let mut straddling: BinaryHeap<Straddling> = BinaryHeap::new();
    let full = 1u32 << bits;
    let mut place = |x: u32, y: u32, size: u32, out: &mut Vec<(u64, u64)>,
                     straddling: &mut BinaryHeap<Straddling>| {
        if let Some(cells) = inside(x, y, size) {
            let total = (size as u64) * (size as u64);
            if cells == total {
                out.push(run(x, y, size));
            } else {
                straddling.push(Straddling { waste: total - cells, size, x, y });
            }
        }
    };
    place(0, 0, full, &mut out, &mut straddling);
    // A split replaces one run by at most four, so it is affordable while
    // three more runs still fit under the budget.
    while out.len() + straddling.len() + 3 <= budget {
        let Some(worst) = straddling.pop() else { break };
        let half = worst.size / 2;
        for (dx, dy) in [(0, 0), (half, 0), (0, half), (half, half)] {
            place(worst.x + dx, worst.y + dy, half, &mut out, &mut straddling);
        }
    }
    out.extend(straddling.into_iter().map(|s| run(s.x, s.y, s.size)));
    // merge adjacent/overlapping runs so the scan count stays small
    out.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (lo, hi) in out {
        match merged.last_mut() {
            Some(last) if lo <= last.1 + 1 => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hilbert must be a bijection on small grids and neighbours must be
    /// adjacent along the curve for at least one axis step (the locality
    /// property Z-order lacks at quadrant seams).
    #[test]
    fn hilbert_is_a_bijection_on_an_8x8_grid() {
        let bits = 3u8;
        let mut seen = std::collections::HashSet::new();
        for y in 0..8u32 {
            for x in 0..8u32 {
                let h = cell_hilbert(x, y, bits);
                assert!(seen.insert(h), "collision at ({x},{y})");
            }
        }
        assert_eq!(seen.len(), 64);
        // consecutive curve positions differ by exactly one grid step
        let mut by_h: Vec<(u64, (u32, u32))> = (0..8u32)
            .flat_map(|y| (0..8u32).map(move |x| (cell_hilbert(x, y, 3), (x, y))))
            .collect();
        by_h.sort();
        for w in by_h.windows(2) {
            let ((_, (x0, y0)), (_, (x1, y1))) = (w[0], w[1]);
            let d = x0.abs_diff(x1) + y0.abs_diff(y1);
            assert_eq!(d, 1, "curve jumps from ({x0},{y0}) to ({x1},{y1})");
        }
    }

    /// Every cell inside a query rect is covered by some emitted range,
    /// and no range is unbounded nonsense -- against brute force.
    #[test]
    fn cover_ranges_cover_exactly_against_bruteforce() {
        let bits = 6u8; // 64x64 world: brute force is cheap
        let cases = [
            (-180.0, 180.0, -90.0, 90.0),
            (-1.0, 1.0, -1.0, 1.0),
            (10.0, 11.5, -20.0, -19.2),
            (100.0, 179.9, 50.0, 89.9),
            (-0.001, 0.001, -0.001, 0.001),
        ];
        for (xmin, xmax, ymin, ymax) in cases {
            let ranges = cover_ranges(xmin, xmax, ymin, ymax, bits, 32);
            assert!(ranges.len() <= 40, "range budget blown: {}", ranges.len());
            let (qx0, qy0) = cell_of(xmin, ymin, bits);
            let (qx1, qy1) = cell_of(xmax, ymax, bits);
            for cy in qy0..=qy1 {
                for cx in qx0..=qx1 {
                    let h = cell_hilbert(cx, cy, bits);
                    assert!(ranges.iter().any(|&(lo, hi)| lo <= h && h <= hi),
                            "cell ({cx},{cy}) h={h} not covered for box {:?}",
                            (xmin, xmax, ymin, ymax));
                }
            }
        }
    }

    /// The cover's over-fetch is bounded by its RANGE budget, not by how deep
    /// the recursion happens to be when the budget check first trips. A box
    /// of 262x262 cells (a 50 km radius at 16 bits) under a budget of 64
    /// ranges must cover at most 1.5x its own cells, and a box of 47x47
    /// (10 km) and 9x17 (2 km) the same, with never more ranges than the
    /// budget. Before this was pinned the 50 km cover spent 5.2x its box and
    /// only 14 of its 64 ranges: the check counted the DFS stack -- three
    /// pending siblings per level, fifty of the sixty-four -- and emitted
    /// whole quadrants far larger than the box.
    #[test]
    fn cover_over_fetch_is_bounded_by_the_range_budget() {
        let bits = 16u8;
        let (lon, lat) = (107.6f64, -6.9f64);
        for (half_lon, half_lat, budget, ceiling) in
            [(0.45, 0.45, 64, 1.5), (0.09, 0.09, 64, 1.5), (0.018, 0.018, 64, 1.6)]
        {
            let (xmin, xmax, ymin, ymax) = (lon - half_lon, lon + half_lon, lat - half_lat, lat + half_lat);
            let ranges = cover_ranges(xmin, xmax, ymin, ymax, bits, budget);
            assert!(ranges.len() <= budget, "{} ranges over a budget of {budget}", ranges.len());
            let (qx0, qy0) = cell_of(xmin, ymin, bits);
            let (qx1, qy1) = cell_of(xmax, ymax, bits);
            let box_cells = (qx1 - qx0 + 1) as u64 * (qy1 - qy0 + 1) as u64;
            let covered: u64 = ranges.iter().map(|(lo, hi)| hi - lo + 1).sum();
            assert!(
                covered as f64 <= ceiling * box_cells as f64,
                "a {}x{} box was covered with {covered} cells ({:.2}x) by {} ranges",
                qx1 - qx0 + 1, qy1 - qy0 + 1, covered as f64 / box_cells as f64, ranges.len()
            );
        }
    }

    /// The quadrant-run assumption itself: an aligned power-of-two square
    /// is one contiguous hilbert interval.
    #[test]
    fn aligned_squares_are_contiguous_hilbert_runs() {
        let bits = 6u8;
        for size_log in 1..=5u32 {
            let size = 1u32 << size_log;
            for qy in (0..64).step_by(size as usize) {
                for qx in (0..64).step_by(size as usize) {
                    let mut hs: Vec<u64> = (0..size).flat_map(|dy| {
                        (0..size).map(move |dx| cell_hilbert(qx + dx, qy + dy, bits))
                    }).collect();
                    hs.sort_unstable();
                    let lo = hs[0];
                    for (i, h) in hs.iter().enumerate() {
                        assert_eq!(*h, lo + i as u64,
                                   "square at ({qx},{qy}) size {size} not contiguous");
                    }
                }
            }
        }
    }

    #[test]
    fn outward_rounding_always_contains_the_double_box() {
        for &(a, b) in &[(0.1f64, 0.2f64), (-179.99999, 179.99999),
                          (37.42421356237, 37.42421356238), (-0.0, 0.0)] {
            let bx = BoxF::from_f64(a, b, a, b);
            assert!((bx.xmin as f64) <= a && (bx.xmax as f64) >= b);
            assert!((bx.ymin as f64) <= a && (bx.ymax as f64) >= b);
        }
    }
}

/// Typed geometry -- the kernel's only geometry language. Coordinates are
/// (lon, lat) pairs in WGS84 degrees (GeoJSON axis order); rings are
/// implicitly closed. GeoJSON <-> Geom conversion lives above the kernel.
#[derive(Clone, Debug, PartialEq)]
pub enum Geom {
    Point(f64, f64),
    LineString(Vec<[f64; 2]>),
    Polygon(Vec<Vec<[f64; 2]>>),
    MultiPoint(Vec<[f64; 2]>),
    MultiLineString(Vec<Vec<[f64; 2]>>),
    MultiPolygon(Vec<Vec<Vec<[f64; 2]>>>),
}

impl Geom {
    pub fn bbox(&self) -> Option<(f64, f64, f64, f64)> {
        let mut b: Option<(f64, f64, f64, f64)> = None;
        let mut add = |x: f64, y: f64| {
            b = Some(match b {
                None => (x, x, y, y),
                Some((x0, x1, y0, y1)) => (x0.min(x), x1.max(x), y0.min(y), y1.max(y)),
            });
        };
        match self {
            Geom::Point(x, y) => add(*x, *y),
            Geom::LineString(c) | Geom::MultiPoint(c) =>
                c.iter().for_each(|p| add(p[0], p[1])),
            Geom::Polygon(rs) | Geom::MultiLineString(rs) =>
                rs.iter().flatten().for_each(|p| add(p[0], p[1])),
            Geom::MultiPolygon(ps) =>
                ps.iter().flatten().flatten().for_each(|p| add(p[0], p[1])),
        }
        b
    }

    /// Outer rings as [[lat, lon]] (geomath's internal layout) for PIP.
    pub fn rings_latlon(&self) -> Vec<Vec<[f64; 2]>> {
        let flip = |r: &Vec<[f64; 2]>| r.iter().map(|p| [p[1], p[0]]).collect();
        match self {
            Geom::Polygon(rs) => rs.iter().take(1).map(flip).collect(),
            Geom::MultiPolygon(ps) =>
                ps.iter().filter_map(|rs| rs.first()).map(|r| flip(r)).collect(),
            _ => Vec::new(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        fn coords(v: &mut Vec<u8>, c: &[[f64; 2]]) {
            v.extend_from_slice(&(c.len() as u32).to_le_bytes());
            for p in c {
                v.extend_from_slice(&p[0].to_le_bytes());
                v.extend_from_slice(&p[1].to_le_bytes());
            }
        }
        fn ringsets(v: &mut Vec<u8>, rs: &[Vec<[f64; 2]>]) {
            v.extend_from_slice(&(rs.len() as u32).to_le_bytes());
            for r in rs { coords(v, r); }
        }
        let mut v = Vec::new();
        match self {
            Geom::Point(x, y) => { v.push(1); v.extend_from_slice(&x.to_le_bytes()); v.extend_from_slice(&y.to_le_bytes()); }
            Geom::LineString(c) => { v.push(2); coords(&mut v, c); }
            Geom::Polygon(rs) => { v.push(3); ringsets(&mut v, rs); }
            Geom::MultiPoint(c) => { v.push(4); coords(&mut v, c); }
            Geom::MultiLineString(rs) => { v.push(5); ringsets(&mut v, rs); }
            Geom::MultiPolygon(ps) => {
                v.push(6);
                v.extend_from_slice(&(ps.len() as u32).to_le_bytes());
                for rs in ps { ringsets(&mut v, rs); }
            }
        }
        v
    }

    pub fn decode(b: &[u8]) -> Option<Geom> {
        fn f64_at(b: &[u8], p: &mut usize) -> Option<f64> {
            let v = f64::from_le_bytes(b.get(*p..*p + 8)?.try_into().ok()?);
            *p += 8; Some(v)
        }
        fn u32_at(b: &[u8], p: &mut usize) -> Option<u32> {
            let v = u32::from_le_bytes(b.get(*p..*p + 4)?.try_into().ok()?);
            *p += 4; Some(v)
        }
        fn coords(b: &[u8], p: &mut usize) -> Option<Vec<[f64; 2]>> {
            let n = u32_at(b, p)? as usize;
            if n > b.len() / 16 + 1 { return None; } // bound off disk (L5)
            let mut c = Vec::with_capacity(n);
            for _ in 0..n { c.push([f64_at(b, p)?, f64_at(b, p)?]); }
            Some(c)
        }
        fn ringsets(b: &[u8], p: &mut usize) -> Option<Vec<Vec<[f64; 2]>>> {
            let n = u32_at(b, p)? as usize;
            if n > b.len() / 4 + 1 { return None; }
            let mut rs = Vec::with_capacity(n);
            for _ in 0..n { rs.push(coords(b, p)?); }
            Some(rs)
        }
        let mut p = 1usize;
        match *b.first()? {
            1 => Some(Geom::Point(f64_at(b, &mut p)?, f64_at(b, &mut p)?)),
            2 => Some(Geom::LineString(coords(b, &mut p)?)),
            3 => Some(Geom::Polygon(ringsets(b, &mut p)?)),
            4 => Some(Geom::MultiPoint(coords(b, &mut p)?)),
            5 => Some(Geom::MultiLineString(ringsets(b, &mut p)?)),
            6 => {
                let n = u32_at(b, &mut p)? as usize;
                if n > b.len() / 4 + 1 { return None; }
                let mut ps = Vec::with_capacity(n);
                for _ in 0..n { ps.push(ringsets(b, &mut p)?); }
                Some(Geom::MultiPolygon(ps))
            }
            _ => None,
        }
    }
}
