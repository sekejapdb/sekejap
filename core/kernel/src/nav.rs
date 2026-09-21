//! 2k: the vector navigation tier (FACT-02's graph stage).
//!
//! A newcomer's map. The scan tier reads EVERY fingerprint per query --
//! honest but linear. This tier gives each vector a handful of links to
//! its most-similar vectors, forming a small-world web (the Vamana family,
//! as in DiskANN); a query then WALKS: start at a central vector (the
//! medoid), repeatedly move toward whichever known neighbour looks closest
//! to the query, keep the best `ef` candidates, stop when no frontier
//! candidate can beat the worst kept result. Hops ~ logarithmic; a million
//! vectors answer in a few hundred row reads instead of a million.
//!
//! Everything is ordinary rows (D4): (0x11, field, id) -> [norm][code]
//! [links]. One web per FIELD -- links are bare ids, so a shared keyspace
//! would wire a 384-dim column's vectors into an 8-dim column's
//! neighbourhoods and both walks would read the other's rows as garbage.
//! The 2-bit fingerprint rides IN the nav row, so one read per visited
//! node yields both topology and ranking data. The exact tier (rescore)
//! still ranks the survivors: approximation can miss, never misrank.
//!
//! Freshness without an LSM: the fold records a WATERMARK as a fast path for
//! the ordinary increasing-id tail. Vectors written at-or-below it get an
//! explicit pending marker in the catalog. Queries merge both pending sets
//! with the walk, and folds consume both, so arbitrary caller ids remain
//! visible without an O(N) sweep. Deletes leave dangling links that walks
//! skip (missing row = dead), healed at fold.

use crate::graph::{Graph, Metric, CAT_NAV_PENDING};
use crate::keys;
use crate::vecquant::{self, AffineQuery};
use crate::{Error, Result};

/// Max neighbours kept per node. DiskANN's sweet spot region; the row
/// stays ~0.8KB at 2-bit/2048-pad codes.
pub const NAV_R: usize = 32;
/// Build-time beam width (bigger = better graph, slower fold).
pub const NAV_L_BUILD: usize = 64;
/// Pruning slack: a candidate is dropped if some kept neighbour is more
/// than ALPHA closer to it than the candidate is to the node (Vamana's
/// robust prune -- keeps links spread out instead of clustered).
/// Applied as ALPHA^2 because we compare SQUARED L2 -- alpha on squared
/// distances is sqrt(alpha) on true ones, which quietly weakened the
/// spread rule to 1.095 (recall 0.535 measured before the fix).
pub const NAV_ALPHA: f32 = 1.2;
/// The fold commits AND checkpoints every this-many inserts. A fold that
/// commits once holds every neighbour-row rewrite of the whole batch in
/// the WAL (~33 row images per insert: 15GB measured at 1M) -- the WAL
/// only truncates at a checkpoint, so the bound must checkpoint. Crash
/// mid-fold is already safe at ANY boundary: the watermark rides each
/// insert, so a reopened store simply resumes the fold above it.
pub const NAV_FOLD_CHECKPOINT_EVERY: u64 = 25_000;

/// Backlink lists may grow to this before being pruned back to NAV_R.
/// Pruning on EVERY overflow re-read ~33 full vectors per neighbour per
/// insert; letting lists run to 2R amortises that ~32x (Vamana batch
/// builds do the same). The row decoder already accepts n <= 2R.
pub const NAV_R_SLACK: usize = NAV_R * 2;

/// Map an f32 to order-preserving u64 bits (positive floats sort by raw
/// bits with the sign bit set; negatives flip ALL bits). The width
/// matters: f32::to_bits is u32, and a u64-width flip sets the upper 32
/// bits, ranking every NEGATIVE estimate above every positive one --
/// backwards. Negative L2 estimates (norm^2 - 2*dot omits |q|^2, so near
/// neighbours go negative) are exactly the BEST candidates; the inverted
/// order made recall FALL as the beam widened (0.495 -> 0.285 at 1M,
/// measured) because wider beams found, then evicted, more of them.
pub(crate) fn sortable(d: f32) -> u64 {
    let b = d.to_bits();
    (if d >= 0.0 { b | 0x8000_0000 } else { !b }) as u64
}

struct NavRow {
    norm: f32,
    code_off: usize,
    code_len: usize,
    neighbors: Vec<u64>,
}

fn decode_row(v: &[u8], code_len: usize) -> Result<NavRow> {
    let count_at = 4usize.checked_add(code_len).ok_or(Error::Corrupt {
        page_no: 0,
        why: "navigation row code length overflows",
    })?;
    let header_end = count_at.checked_add(2).ok_or(Error::Corrupt {
        page_no: 0,
        why: "navigation row header length overflows",
    })?;
    let count_bytes = v.get(count_at..header_end).ok_or(Error::Corrupt {
        page_no: 0,
        why: "navigation row is shorter than its code and count",
    })?;
    let norm = f32::from_le_bytes(v.get(..4).ok_or(Error::Corrupt {
        page_no: 0,
        why: "navigation row has no norm",
    })?.try_into().unwrap());
    let n = u16::from_le_bytes(count_bytes.try_into().unwrap()) as usize;
    if n > NAV_R_SLACK {
        return Err(Error::Corrupt { page_no: 0, why: "navigation row has too many neighbours" });
    }
    let expected = n.checked_mul(8).and_then(|bytes| header_end.checked_add(bytes))
        .ok_or(Error::Corrupt { page_no: 0, why: "navigation row length overflows" })?;
    if v.len() != expected {
        return Err(Error::Corrupt { page_no: 0, why: "navigation row length is invalid" });
    }
    let mut neighbors = Vec::with_capacity(n);
    for i in 0..n {
        let o = header_end + i * 8;
        neighbors.push(u64::from_le_bytes(v[o..o + 8].try_into().unwrap()));
    }
    Ok(NavRow { norm, code_off: 4, code_len, neighbors })
}

fn encode_row(norm: f32, code: &[u8], neighbors: &[u64]) -> Vec<u8> {
    let mut v = Vec::with_capacity(4 + code.len() + 2 + neighbors.len() * 8);
    v.extend_from_slice(&norm.to_le_bytes());
    v.extend_from_slice(code);
    v.extend_from_slice(&(neighbors.len() as u16).to_le_bytes());
    for n in neighbors { v.extend_from_slice(&n.to_le_bytes()); }
    v
}

impl Graph {
    /// Append ids explicitly known to have been written behind the watermark.
    /// Empty pending sets cost one catalog seek, never a vector-range sweep.
    fn nav_pending_ids(&self, field: u64, out: &mut Vec<u64>) -> Result<()> {
        let prefix = keys::catalog_field(CAT_NAV_PENDING, field);
        self.store_ref().scan(&prefix)?.for_each_ref(|key, _| {
            if !key.starts_with(&prefix) { return false; }
            if key.len() == 25 { out.push(keys::u64_at(key, 17)); }
            true
        })
    }

    fn nav_code_len(&self, field: u64) -> usize {
        vecquant::code_len(vecquant::pad_dim(self.vec_dim(field) as usize),
                           self.vec_bits_pub(field))
    }

    /// Diagnostic walk: like the query path, but also returns every id the
    /// beam VISITED (estimated), not only the ef it kept. Separates "the
    /// walk never reached the region" from "reached it but ranked it out".
    pub fn nav_walk_diag(&self, field: u64, q: &[f32], ef: usize)
        -> Result<(Vec<u64>, std::collections::HashSet<u64>)>
    {
        let medoid = match self.vec_meta(field) {
            Some(m) if m.medoid != 0 => m.medoid,
            _ => return Ok((Vec::new(), Default::default())),
        };
        let Some(enc) = self.encoder(field) else { return Ok((Vec::new(), Default::default())) };
        let aq = enc.affine_query(q);
        let mut visited = std::collections::HashSet::new();
        let kept = self.nav_beam_inner(field, &aq, Metric::L2, ef, medoid, Some(&mut visited))?;
        Ok((kept.into_iter().map(|(_, id)| id).collect(), visited))
    }

    /// Beam walk over the nav rows: returns up to `ef` candidate ids by
    /// estimated distance. Reads ONE row per visited node.
    fn nav_beam(&self, field: u64, aq: &AffineQuery, metric: Metric, ef: usize, start: u64)
        -> Result<Vec<(f32, u64)>>
    {
        self.nav_beam_inner(field, aq, metric, ef, start, None)
    }

    fn nav_beam_inner(&self, field: u64, aq: &AffineQuery, metric: Metric, ef: usize, start: u64,
                      mut diag: Option<&mut std::collections::HashSet<u64>>)
        -> Result<Vec<(f32, u64)>>
    {
        let code_len = self.nav_code_len(field);
        let est = |row: &NavRow, v: &[u8]| -> f32 {
            let dot = vecquant::dot_est_affine2(row.norm, &v[row.code_off..row.code_off + row.code_len], aq);
            match metric {
                Metric::L2 | Metric::L1 => row.norm * row.norm - 2.0 * dot,
                Metric::Dot => -dot,
                Metric::Cosine => if row.norm > 0.0 { -dot / row.norm } else { 0.0 },
            }
        };
        let mut visited: std::collections::HashSet<u64> = std::collections::HashSet::new();
        // frontier: nearest-first (Reverse); kept: worst-first, bounded ef
        let mut frontier2: std::collections::BinaryHeap<std::cmp::Reverse<(u64, u64)>> =
            std::collections::BinaryHeap::new();
        let mut kept: std::collections::BinaryHeap<(u64, u64)> = std::collections::BinaryHeap::new();
        let seed = match self.store_ref().get(&keys::nav_key(field, start))? {
            Some(v) => v,
            None => return Ok(Vec::new()),
        };
        let row = decode_row(&seed, code_len)?;
        let d0 = est(&row, &seed);
        visited.insert(start);
        frontier2.push(std::cmp::Reverse((sortable(d0), start)));
        kept.push((sortable(d0), start));
        let mut dists: std::collections::HashMap<u64, f32> = std::collections::HashMap::new();
        dists.insert(start, d0);

        while let Some(std::cmp::Reverse((ds, id))) = frontier2.pop() {
            // stop when the closest frontier item cannot beat the worst kept
            if kept.len() >= ef {
                if let Some(&(worst, _)) = kept.peek() {
                    if ds > worst { break; }
                }
            }
            let Some(v) = self.store_ref().get(&keys::nav_key(field, id))? else { continue };
            let row = decode_row(&v, code_len)?;
            for &nb in &row.neighbors {
                if !visited.insert(nb) { continue; }
                if let Some(d) = diag.as_deref_mut() { d.insert(nb); }
                let Some(nv) = self.store_ref().get(&keys::nav_key(field, nb))? else { continue };
                let nrow = decode_row(&nv, code_len)?;
                let nd = est(&nrow, &nv);
                let nds = sortable(nd);
                let admit = kept.len() < ef || kept.peek().map(|&(w, _)| nds < w).unwrap_or(true);
                if admit {
                    kept.push((nds, nb));
                    if kept.len() > ef { kept.pop(); }
                    frontier2.push(std::cmp::Reverse((nds, nb)));
                    dists.insert(nb, nd);
                }
            }
        }
        let mut out: Vec<(f32, u64)> = kept.into_iter()
            .map(|(_, id)| (dists.get(&id).copied().unwrap_or(f32::MAX), id))
            .collect();
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        Ok(out)
    }

    /// Wire `id` into the graph (Vamana insert): beam-search its
    /// neighbourhood, robust-prune to NAV_R links, write the row, add
    /// pruned backlinks. Cost ∝ beam + degree, never ∝ store (Law 2).
    fn nav_insert(&mut self, field: u64, id: u64, medoid: u64, refresh: bool) -> Result<()> {
        let code_len = self.nav_code_len(field);
        let Some(vrow) = self.store_ref().get(&keys::vcode_key(field, id))? else { return Ok(()) };
        if vrow.len() != 4usize.saturating_add(code_len) {
            return Err(Error::Corrupt { page_no: 0, why: "vector code row length is invalid" });
        }
        let norm = f32::from_le_bytes(vrow[0..4].try_into().unwrap());
        let code = vrow[4..].to_vec();
        // the query IS this vector: reuse its exact f32 form for the walk
        let Some(v) = self.get_vec(field, id)? else { return Ok(()) };
        let Some(enc) = self.encoder(field) else { return Ok(()) };
        let aq = enc.affine_query(&v);

        let mut cands = if id == medoid && !refresh {
            Vec::new()
        } else {
            self.nav_beam(field, &aq, Metric::L2, NAV_L_BUILD, medoid)?
        };
        // A behind-watermark write may replace a folded vector. Its old row
        // can be reached by the beam, but it cannot be its own neighbour.
        cands.retain(|&(_, candidate)| candidate != id);
        // re-price candidates EXACTLY before pruning (estimates found them;
        // truth ranks them), all through one per-insert vector cache
        let mut vcache: std::collections::HashMap<u64, Vec<f32>> =
            std::collections::HashMap::new();
        vcache.insert(id, v.clone());
        for c in cands.iter_mut() {
            if let Some(d) = self.pair_dist_cached(field, &mut vcache, id, c.1)? { c.0 = d; }
        }
        let pruned = self.robust_prune(field, &mut vcache, id, &cands, code_len)?;
        self.store().put(&keys::nav_key(field, id), &encode_row(norm, &code, &pruned))?;
        // backlinks, pruned per neighbour
        for &nb in &pruned {
            let Some(nv) = self.store_ref().get(&keys::nav_key(field, nb))? else { continue };
            let mut nrow = decode_row(&nv, code_len)?;
            if nrow.neighbors.contains(&id) { continue; }
            nrow.neighbors.push(id);
            let links = if nrow.neighbors.len() > NAV_R_SLACK {
                let mut cds = Vec::with_capacity(nrow.neighbors.len());
                for &x in &nrow.neighbors {
                    cds.push((self.pair_dist_cached(field, &mut vcache, nb, x)?.unwrap_or(f32::MAX), x));
                }
                self.robust_prune(field, &mut vcache, nb, &cds, code_len)?
            } else { nrow.neighbors.clone() };
            let ncode = nv[4..4 + code_len].to_vec();
            self.store().put(&keys::nav_key(field, nb), &encode_row(nrow.norm, &ncode, &links))?;
        }
        Ok(())
    }

    /// EXACT L2^2 between two stored vectors -- build-time only, through a
    /// per-insert cache (the uncached form re-read vectors O(kept x cands)
    /// times: 12ms per insert, measured). Estimates find candidates;
    /// exact distances rank and prune them (the DiskANN posture).
    fn pair_dist_cached(&self, field: u64, cache: &mut std::collections::HashMap<u64, Vec<f32>>,
                        a: u64, b: u64) -> Result<Option<f32>> {
        for id in [a, b] {
            if !cache.contains_key(&id) {
                let Some(vector) = self.get_vec(field, id)? else { return Ok(None) };
                cache.insert(id, vector);
            }
        }
        let (Some(va), Some(vb)) = (cache.get(&a), cache.get(&b)) else { return Ok(None) };
        Ok(Some(va.iter().zip(vb).map(|(x, y)| (x - y) * (x - y)).sum()))
    }

    /// Vamana's robust prune: keep the closest candidate, drop any other
    /// candidate that some kept neighbour dominates (ALPHA * d(kept, cand)
    /// < d(node, cand)); repeat to NAV_R.
    fn robust_prune(&self, field: u64, vcache: &mut std::collections::HashMap<u64, Vec<f32>>,
                    _node: u64, cands: &[(f32, u64)], code_len: usize)
        -> Result<Vec<u64>>
    {
        let mut sorted: Vec<(f32, u64)> = cands.to_vec();
        sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
        sorted.dedup_by_key(|c| c.1);
        let mut kept: Vec<(f32, u64)> = Vec::new();
        let _ = code_len;
        'cand: for &(d, c) in &sorted {
            if kept.len() >= NAV_R { break; }
            for &(_, k) in &kept {
                let Some(dkc) = self.pair_dist_cached(field, vcache, k, c)? else { continue };
                if NAV_ALPHA * NAV_ALPHA * dkc < d { continue 'cand; }
            }
            kept.push((d, c));
        }
        Ok(kept.into_iter().map(|(_, id)| id).collect())
    }

    /// Fold: wire the increasing-id tail plus explicit out-of-order pending
    /// ids into the graph, in id order. Cost ∝ new vectors, not the field.
    pub fn fold_nav(&mut self, field: u64) -> Result<u64> {
        let Some(mut meta) = self.vec_meta(field) else { return Ok(0) };
        let mut stragglers: Vec<u64> = Vec::new();
        self.nav_pending_ids(field, &mut stragglers)?;
        stragglers.sort_unstable();
        stragglers.dedup();
        let mut pending = stragglers.clone();
        // The contiguous tail remains the fast path and preserves existing
        // dock/bulk-loaded stores, which have no per-vector pending markers.
        if let Some(next) = meta.watermark.checked_add(1) {
            let from = keys::vcode_key(field, next);
            let it = self.store_ref().scan(&from)?;
            it.for_each_ref(|key, _| {
                if key.len() != 17 || key[0] != keys::TAG_VCODE
                    || keys::u64_at(key, 1) != field { return false; }
                pending.push(keys::u64_at(key, 9));
                true
            })?;
        }
        pending.sort_unstable();
        pending.dedup();
        let n = pending.len() as u64;
        let mut since = 0u64;
        for id in pending {
            let m = match meta.medoid {
                0 => {
                    meta.medoid = id;
                    self.set_vec_meta(field, meta)?;
                    id
                }
                m => m,
            };
            let refresh = stragglers.binary_search(&id).is_ok();
            self.nav_insert(field, id, m, refresh)?;
            if refresh {
                self.store().delete(
                    &keys::catalog_field_item(CAT_NAV_PENDING, field, id))?;
            }
            meta.watermark = meta.watermark.max(id);
            self.set_vec_meta(field, meta)?;
            since += 1;
            if since >= self.nav_fold_every {
                self.commit()?;
                self.checkpoint()?; // truncates the WAL: disk high-water ∝ interval
                since = 0;
            }
        }
        self.commit()?;
        Ok(n)
    }

    /// Graph-accelerated nearest: beam walk over the graph, scan tier for the
    /// increasing-id head and explicit out-of-order pending set, then exact
    /// rescore over the union. Falls back to pure scan when no graph exists.
    pub fn nearest_nav(&self, field: u64, q: &[f32], k: usize, metric: Metric, oversample: usize)
        -> Result<Vec<(u64, f32)>>
    {
        let meta = match self.vec_meta(field) {
            Some(m) if m.medoid != 0 => m,
            _ => return self.nearest(field, q, k, metric, oversample),
        };
        let Some(enc) = self.encoder(field) else {
            return self.nearest(field, q, k, metric, oversample);
        };
        let aq = enc.affine_query(q);
        let ef = (k * oversample).max(k).max(64);
        let mut cands: Vec<u64> = self.nav_beam(field, &aq, metric, ef, meta.medoid)?
            .into_iter().map(|(_, id)| id).collect();
        let mut stragglers = Vec::new();
        self.nav_pending_ids(field, &mut stragglers)?;
        for id in stragglers {
            // An overwrite may still be reachable through its old nav row.
            // Avoid returning the same id twice after exact rescoring.
            if !cands.contains(&id) { cands.push(id); }
        }
        // Head: codes above the watermark, contiguous within this field.
        if let Some(next) = meta.watermark.checked_add(1) {
            let head_from = keys::vcode_key(field, next);
            let it = self.store_ref().scan(&head_from)?;
            it.for_each_ref(|key, _| {
                if key.len() != 17 || key[0] != keys::TAG_VCODE
                    || keys::u64_at(key, 1) != field { return false; }
                cands.push(keys::u64_at(key, 9));
                true
            })?;
        }
        self.rescore(field, &cands, q, metric, k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Config, Store};

    /// The beam orders candidates by these keys: more-negative estimate =
    /// better = smaller key. The u64-width flip bug ranked all negatives
    /// worst; this pins strict monotonicity across the sign boundary.
    #[test]
    fn sortable_orders_floats_across_the_sign_boundary() {
        let seq = [-3.5f32, -1.0, -0.25, 0.0, 0.25, 1.0, 3.5];
        for w in seq.windows(2) {
            assert!(sortable(w[0]) < sortable(w[1]),
                    "{} must sort below {}", w[0], w[1]);
        }
    }

    #[test]
    fn malformed_navigation_rows_are_errors_not_missing_nodes() {
        assert!(matches!(decode_row(&[0; 3], 8), Err(Error::Corrupt { .. })));

        let mut trailing = encode_row(1.0, &[0; 8], &[7]);
        trailing.push(0);
        assert!(matches!(decode_row(&trailing, 8), Err(Error::Corrupt { .. })));
    }

    /// Pins the alpha semantics: distances here are SQUARED L2, so the
    /// spread rule must use ALPHA^2. Geometry chosen so that a candidate
    /// sits in the band between sqrt(1.2)-slack and 1.2-slack: the correct
    /// rule keeps it, the squared-alpha-as-is bug drops it.
    #[test]
    fn robust_prune_alpha_acts_on_true_distances() {
        let d = tempfile::TempDir::new().unwrap();
        let g = Graph::new(Store::create(d.path(), Config::default()).unwrap()).unwrap();
        let mut vc: std::collections::HashMap<u64, Vec<f32>> = std::collections::HashMap::new();
        vc.insert(10, vec![1.0, 0.0]);          // c1: nearest, always kept
        vc.insert(20, vec![0.68375, 1.10567]);  // c2: d(p,.)=1.3, d(c1,.)=1.15
        vc.insert(30, vec![1.05, 0.0]);         // c3: dominated by c1, must drop
        let cands = vec![
            (1.0f32, 10u64),      // d(p,c1)^2
            (1.69, 20),           // 1.3^2
            (1.1025, 30),         // 1.05^2
        ];
        let kept = g.robust_prune(0, &mut vc, 0, &cands, 0).unwrap();
        // 1.2 * d(c1,c2) = 1.38 > 1.3         -> c2 survives the spread rule
        // 1.2 * d(c1,c3) = 0.06 < 1.05        -> c3 dropped
        assert_eq!(kept, vec![10, 20], "alpha must apply to true distances, not squared");
    }
}
