//! In-memory HNSW (Hierarchical Navigable Small World) graph.
//!
//! Adapted from HyperHNSW (sekejap-full) for single-threaded use with
//! pluggable vector storage via the [`VectorAccess`] trait. No extra
//! dependencies.
//!
//! # Algorithm
//! Standard HNSW as described in Malkov & Yashunin (2018):
//! - Random multi-layer graph — exponentially fewer nodes at higher layers.
//! - Greedy layer descent to find the entry point for the base layer.
//! - Beam search at layer 0 to find final k-NN results.
//! - Bidirectional edge wiring + diversity-heuristic pruning.
//!
//! # Atomicity
//! `HnswGraph::build` constructs the entire graph into a local value and
//! returns it only on completion.  The caller stores it with a single
//! assignment, so the main store is never partially modified on error.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::vector::access::{QuantAccess, VectorAccess};
use crate::vector::quant::l2_u8;
use crate::vector::Distance;

// ── Candidate types ───────────────────────────────────────────────────────────

/// Min-heap element: smallest distance = highest priority.
#[derive(Clone, PartialEq)]
struct MinCand {
    id: u64,
    dist: f32,
}
impl Eq for MinCand {}
impl PartialOrd for MinCand {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for MinCand {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse so BinaryHeap (which is max by default) acts as min-heap.
        other
            .dist
            .partial_cmp(&self.dist)
            .unwrap_or(Ordering::Equal)
            .then_with(|| self.id.cmp(&other.id))
    }
}

/// Max-heap element: largest distance = highest priority (evict the farthest).
#[derive(Clone, PartialEq)]
struct MaxCand {
    id: u64,
    dist: f32,
}
impl Eq for MaxCand {}
impl PartialOrd for MaxCand {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for MaxCand {
    fn cmp(&self, other: &Self) -> Ordering {
        self.dist
            .partial_cmp(&other.dist)
            .unwrap_or(Ordering::Equal)
            .then_with(|| other.id.cmp(&self.id))
    }
}

// ── PRNG (no external dep) ────────────────────────────────────────────────────

/// Xorshift64 — maps a (node_id, counter) pair to a float in (0, 1).
/// Deterministic for the same seed.  Good enough for level selection.
#[inline]
fn random_unit(seed: u64) -> f64 {
    let mut x = seed ^ 0x9e3779b97f4a7c15;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    x = x.wrapping_mul(2685821657736338717);
    // Map 53 random bits to (0, 1).
    let f = (x >> 11) as f64 / (1u64 << 53) as f64;
    f.max(1e-15) // guard against exact 0 → -ln(0) = ∞
}

// ── HnswGraph ─────────────────────────────────────────────────────────────────

/// In-memory HNSW graph for approximate nearest-neighbour search.
///
/// Implements the Hierarchical Navigable Small World algorithm (Malkov &
/// Yashunin, 2018) with diversity-heuristic neighbour pruning.
///
/// Node IDs are `u64` slug hashes. Vectors are **not** stored inside the
/// graph — they are accessed through any type implementing [`VectorAccess`],
/// keeping the graph lightweight.
///
/// # Construction
///
/// - **Bulk**: [`HnswGraph::build()`] constructs the full graph from a set of
///   vectors. Best for initial data loads.
/// - **Incremental**: [`HnswGraph::empty()`] + repeated [`HnswGraph::insert()`]
///   adds one node at a time in O(log n). Best for online inserts.
///
/// # Thread safety
///
/// The graph itself is not `Sync`. For concurrent access, wrap it in the
/// [`Engine`](crate::engine::Engine) (behind the `engine` feature flag)
/// which provides RwLock-based read/write separation.
#[derive(Serialize, Deserialize, Clone)]
pub struct HnswGraph {
    m: usize,
    m_max0: usize,
    level_mult: f64,
    /// node_id → layers[0..=max_level], each layer a list of neighbour IDs.
    nodes: HashMap<u64, Vec<Vec<u64>>>,
    /// (node_id, max_level) entry point.
    entry_point: Option<(u64, usize)>,
}

impl HnswGraph {
    /// Create an empty HNSW graph with the given connectivity parameter `m`.
    ///
    /// Use this when you plan to insert nodes incrementally via
    /// [`insert()`](Self::insert). For bulk construction, prefer
    /// [`build()`](Self::build) which calls this internally.
    ///
    /// # Parameters
    ///
    /// - `m`: max bidirectional connections per node per layer.
    ///   Clamped to a minimum of 2. Recommended range: 8–32 (16 is a good
    ///   default). Higher values improve recall at the cost of memory.
    pub fn empty(m: usize) -> Self {
        Self::new(m)
    }

    fn new(m: usize) -> Self {
        let m = m.max(2);
        Self {
            m,
            m_max0: 2 * m,
            level_mult: 1.0 / (m as f64).ln(),
            nodes: HashMap::new(),
            entry_point: None,
        }
    }

    fn m_max(&self, level: usize) -> usize {
        if level == 0 { self.m_max0 } else { self.m }
    }

    fn pick_level(&self, node_id: u64) -> usize {
        let seed = node_id.wrapping_add((self.nodes.len() as u64).wrapping_mul(6364136223846793005));
        let r = random_unit(seed);
        (-r.ln() * self.level_mult) as usize
    }

    // ── Public API ────────────────────────────────────────────────────────────

    /// Build a new HNSW graph from all entries in `field_vecs`.
    ///
    /// The caller's data is never modified — the graph is fully constructed
    /// in a local value and returned only when complete.
    ///
    /// # Parameters
    /// - `m`: max connections per node (recommended 8–32; 16 is a good default)
    /// - `ef_construction`: beam width during build (recommended 100–400; 200 is good)
    pub fn build<D: Distance, V: VectorAccess>(
        field_vecs: &V,
        m: usize,
        ef_construction: usize,
    ) -> Self
    where
        V: IterableVectors,
    {
        let mut graph = Self::new(m);
        for (node_id, _) in field_vecs.iter_vectors() {
            graph.insert_node::<D, V>(node_id, field_vecs, ef_construction);
        }
        graph
    }

    /// Fully-dense build: contiguous vector data + a `Vec`-indexed graph using
    /// dense internal ids (0..n), so distance evals and neighbour lookups are pure
    /// array indexing — NO `HashMap` probes and NO scattered pointer-chases (which
    /// made the old `HashMap`-keyed build scale super-linearly once data exceeds
    /// cache). `flat` is `n*dim` row-major; `ids[dense] = node hash`. The result is
    /// converted back to the public `HashMap`-keyed graph. Same algorithm/params →
    /// same recall as [`build`], just cache-friendly.
    pub fn build_dense<D: Distance>(
        flat: &[f32],
        dim: usize,
        ids: &[u64],
        m: usize,
        ef_construction: usize,
    ) -> Self {
        let n = ids.len();
        let mut g = Self::new(m);
        if n == 0 {
            return g;
        }
        let m_max0 = 2 * m;
        let level_mult = 1.0 / (m as f64).ln();
        let vget = |a: u32| -> &[f32] { &flat[a as usize * dim..(a as usize + 1) * dim] };

        // dense adjacency: node → layers → neighbour dense ids
        let mut nodes: Vec<Vec<Vec<u32>>> = Vec::with_capacity(n);
        let mut entry: (u32, usize) = (0, 0);

        for i in 0..n as u32 {
            let qv = vget(i);
            let seed = ids[i as usize].wrapping_add((i as u64).wrapping_mul(6364136223846793005));
            let level = (-random_unit(seed).ln() * level_mult) as usize;
            nodes.push(vec![Vec::new(); level + 1]);
            if i == 0 {
                entry = (0, level);
                continue;
            }
            let (mut cur, top) = entry;
            let mut cur_d = D::eval(qv, vget(cur));
            // Phase 1: greedy descent to level+1 (ef=1).
            for l in ((level + 1)..=top).rev() {
                loop {
                    let mut improved = false;
                    if let Some(nbs) = nodes[cur as usize].get(l) {
                        for &nb in nbs {
                            let d = D::eval(qv, vget(nb));
                            if d < cur_d {
                                cur_d = d;
                                cur = nb;
                                improved = true;
                            }
                        }
                    }
                    if !improved {
                        break;
                    }
                }
            }
            // Phase 2: connect at each shared level.
            for l in (0..=level.min(top)).rev() {
                let cands = search_layer_dense::<D>(&nodes, flat, dim, qv, cur, ef_construction, l);
                let m_l = if l == 0 { m_max0 } else { m };
                let chosen = select_neighbors_dense::<D>(&cands, m_l, flat, dim);
                nodes[i as usize][l] = chosen.clone();
                // Back-links (+ prune the neighbour's list if over capacity).
                for &nb in &chosen {
                    let mm = if l == 0 { m_max0 } else { m };
                    let layers = &mut nodes[nb as usize];
                    if l < layers.len() {
                        layers[l].push(i);
                        if layers[l].len() > mm {
                            let nbv = &flat[nb as usize * dim..(nb as usize + 1) * dim];
                            let mut scored: Vec<(f32, u32)> = layers[l]
                                .iter()
                                .map(|&x| (D::eval(nbv, &flat[x as usize * dim..(x as usize + 1) * dim]), x))
                                .collect();
                            scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
                            scored.truncate(mm);
                            layers[l] = scored.into_iter().map(|(_, x)| x).collect();
                        }
                    }
                }
                if let Some(c) = cands.first() {
                    cur = c.1;
                }
            }
            if level > top {
                entry = (i, level);
            }
        }

        // Convert dense → public HashMap-keyed graph.
        g.entry_point = Some((ids[entry.0 as usize], entry.1));
        for i in 0..n {
            let mapped: Vec<Vec<u64>> = nodes[i]
                .iter()
                .map(|layer| layer.iter().map(|&d| ids[d as usize]).collect())
                .collect();
            g.nodes.insert(ids[i], mapped);
        }
        g
    }

    /// Parallel dense build (rayon) — the same dense algorithm as [`build_dense`],
    /// but inserts run concurrently across cores. Each node's adjacency is behind an
    /// `RwLock`; a node picked as a neighbour before its own insert completes just
    /// receives a back-link (merged, deduped — never overwritten), which HNSW tolerates.
    /// Falls back to the sequential dense build below the rayon-overhead threshold.
    pub fn build_dense_parallel<D: Distance>(
        flat: &[f32],
        dim: usize,
        ids: &[u64],
        m: usize,
        ef_construction: usize,
    ) -> Self {
        use rayon::prelude::*;
        use std::sync::RwLock;
        let n = ids.len();
        if n < 4000 {
            return Self::build_dense::<D>(flat, dim, ids, m, ef_construction);
        }
        let m_max0 = 2 * m;
        let level_mult = 1.0 / (m as f64).ln();
        let vget = |a: u32| -> &[f32] { &flat[a as usize * dim..(a as usize + 1) * dim] };
        // Precompute levels (deterministic per node hash → order-independent shape).
        let levels: Vec<usize> = (0..n)
            .map(|i| {
                let seed = ids[i].wrapping_add((i as u64).wrapping_mul(6364136223846793005));
                (-random_unit(seed).ln() * level_mult) as usize
            })
            .collect();
        // Pre-allocate every node's (empty) layers behind a per-node lock.
        let nodes: Vec<RwLock<Vec<Vec<u32>>>> =
            (0..n).map(|i| RwLock::new(vec![Vec::new(); levels[i] + 1])).collect();
        let entry = RwLock::new((0u32, levels[0]));

        (1..n as u32).into_par_iter().for_each(|i| {
            let qv = vget(i);
            let level = levels[i as usize];
            let (mut cur, top) = *entry.read().unwrap();
            let mut cur_d = D::eval(qv, vget(cur));
            // Phase 1: greedy descent (ef=1), reading neighbour lists under short locks.
            for l in ((level + 1)..=top).rev() {
                loop {
                    let nbs = nodes[cur as usize].read().unwrap().get(l).cloned().unwrap_or_default();
                    let mut improved = false;
                    for nb in nbs {
                        let d = D::eval(qv, vget(nb));
                        if d < cur_d {
                            cur_d = d;
                            cur = nb;
                            improved = true;
                        }
                    }
                    if !improved {
                        break;
                    }
                }
            }
            // Phase 2: connect at each shared level.
            for l in (0..=level.min(top)).rev() {
                let cands = search_layer_dense_locked::<D>(&nodes, flat, dim, qv, cur, ef_construction, l);
                let m_l = if l == 0 { m_max0 } else { m };
                let chosen = select_neighbors_dense::<D>(&cands, m_l, flat, dim);
                // Merge my links (don't overwrite — a back-link may already be here).
                {
                    let mut me = nodes[i as usize].write().unwrap();
                    for &c in &chosen {
                        if !me[l].contains(&c) {
                            me[l].push(c);
                        }
                    }
                }
                // Back-links (+ prune the neighbour's list if over capacity).
                for &nb in &chosen {
                    let mm = if l == 0 { m_max0 } else { m };
                    let mut gnb = nodes[nb as usize].write().unwrap();
                    if l < gnb.len() {
                        if !gnb[l].contains(&i) {
                            gnb[l].push(i);
                        }
                        if gnb[l].len() > mm {
                            let nbv = &flat[nb as usize * dim..(nb as usize + 1) * dim];
                            let mut scored: Vec<(f32, u32)> = gnb[l]
                                .iter()
                                .map(|&x| (D::eval(nbv, &flat[x as usize * dim..(x as usize + 1) * dim]), x))
                                .collect();
                            scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
                            scored.truncate(mm);
                            gnb[l] = scored.into_iter().map(|(_, x)| x).collect();
                        }
                    }
                }
                if let Some(c) = cands.first() {
                    cur = c.1;
                }
            }
            if level > top {
                let mut e = entry.write().unwrap();
                if level > e.1 {
                    *e = (i, level);
                }
            }
        });

        // Convert dense → public HashMap-keyed graph.
        let mut g = Self::new(m);
        let (ep, eplvl) = *entry.read().unwrap();
        g.entry_point = Some((ids[ep as usize], eplvl));
        for i in 0..n {
            let layers = nodes[i].read().unwrap();
            let mapped: Vec<Vec<u64>> = layers
                .iter()
                .map(|layer| layer.iter().map(|&d| ids[d as usize]).collect())
                .collect();
            g.nodes.insert(ids[i], mapped);
        }
        g
    }

    /// Search for the `k` approximate nearest neighbours to `query`.
    ///
    /// - `ef`: exploration factor (must be ≥ k; try `ef = k * 3` for good recall)
    /// - Returns node IDs sorted ascending by distance (closest first).
    ///
    /// Falls back gracefully: if the graph is empty the result is empty.
    pub fn search<D: Distance, V: VectorAccess>(
        &self,
        query: &[f32],
        vectors: &V,
        k: usize,
        ef: usize,
    ) -> Vec<u64> {
        let (mut ep_id, ep_level) = match self.entry_point {
            Some(ep) => ep,
            None => return vec![],
        };

        // Greedy descent through upper layers (ef=1 → move to nearest at each hop).
        for level in (1..=ep_level).rev() {
            let cands = search_layer::<D, V>(&self.nodes, query, ep_id, 1, level, vectors);
            if let Some(best) = cands.into_iter().min_by(|a, b| {
                a.dist.partial_cmp(&b.dist).unwrap_or(Ordering::Equal)
            }) {
                ep_id = best.id;
            }
        }

        // Beam search at layer 0.
        let ef_actual = ef.max(k);
        let mut results = search_layer::<D, V>(&self.nodes, query, ep_id, ef_actual, 0, vectors);
        results.sort_by(|a, b| a.dist.partial_cmp(&b.dist).unwrap_or(Ordering::Equal));
        results.truncate(k);
        results.into_iter().map(|c| c.id).collect()
    }

    /// **Disk-first int8 traversal.** Same greedy-descent + beam search as
    /// [`search`](Self::search), but distances are the integer L2 over u8 codes
    /// held in RAM ([`QuantAccess`]) — no f32, no disk reads on the hot path.
    ///
    /// Returns the top `k` candidate ids ranked by *approximate* (quantized)
    /// distance. Callers re-rank these against full-precision f32 from disk.
    /// `q_code` is the query quantized with the field's calibration.
    pub fn search_quant<Q: QuantAccess>(
        &self,
        q_code: &[u8],
        codes: &Q,
        k: usize,
        ef: usize,
    ) -> Vec<u64> {
        let (mut ep_id, ep_level) = match self.entry_point {
            Some(ep) => ep,
            None => return vec![],
        };

        // Greedy descent through upper layers (ef=1).
        for level in (1..=ep_level).rev() {
            let cands = search_layer_quant(&self.nodes, q_code, ep_id, 1, level, codes);
            if let Some(best) = cands.into_iter().min_by(|a, b| {
                a.dist.partial_cmp(&b.dist).unwrap_or(Ordering::Equal)
            }) {
                ep_id = best.id;
            }
        }

        // Beam search at layer 0.
        let ef_actual = ef.max(k);
        let mut results = search_layer_quant(&self.nodes, q_code, ep_id, ef_actual, 0, codes);
        results.sort_by(|a, b| a.dist.partial_cmp(&b.dist).unwrap_or(Ordering::Equal));
        results.truncate(k);
        results.into_iter().map(|c| c.id).collect()
    }

    /// Number of nodes in the graph.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Returns `true` if the graph contains no nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Returns the node ID of the current entry point, if any.
    pub fn entry_point_id(&self) -> Option<u64> {
        self.entry_point.map(|(id, _)| id)
    }

    /// Raw node adjacency (id → per-level neighbour ids) — for building a
    /// compact CSR representation. Not for hot-path use.
    pub(crate) fn raw_nodes(&self) -> &HashMap<u64, Vec<Vec<u64>>> {
        &self.nodes
    }

    /// Raw entry point (id, level) — for compaction.
    pub(crate) fn raw_entry(&self) -> Option<(u64, usize)> {
        self.entry_point
    }

    /// Approximate RAM held by the graph, in bytes — for memory profiling.
    /// Counts the node HashMap (keys + bucket overhead) and every per-node
    /// `Vec<Vec<u64>>` (outer + inner Vec headers + neighbour-id capacity).
    pub fn mem_bytes(&self) -> usize {
        // HashMap: ~1.1× load factor; entry = 8-byte key + Vec<Vec<u64>> header (24).
        let mut total = self.nodes.capacity() * (8 + 24 + 8 /*control byte + slack*/);
        for layers in self.nodes.values() {
            total += layers.capacity() * 24; // inner Vec<u64> headers
            for lvl in layers {
                total += lvl.capacity() * 8; // neighbour ids
            }
        }
        total
    }

    /// Insert a single node into an existing HNSW graph in O(log n) time.
    ///
    /// This is the incremental counterpart to [`build()`](Self::build).
    /// Use it when adding vectors one at a time to avoid O(n log n) full
    /// graph rebuilds.
    ///
    /// # Parameters
    ///
    /// - `node_id`: the `u64` hash of the node's slug. Must already have a
    ///   corresponding entry in `vectors`.
    /// - `vectors`: the full vector store for this field (all nodes, not just
    ///   the new one). The graph uses these to compute distances during
    ///   neighbour search.
    /// - `ef_construction`: beam width (larger = better recall, slower build;
    ///   200 is a good default).
    ///
    /// # No-op conditions
    ///
    /// If `node_id` has no entry in `vectors`, the call is silently ignored.
    /// If the node already exists in the graph, it will be reinserted (the
    /// old entry's layers are overwritten).
    pub fn insert<D: Distance, V: VectorAccess>(
        &mut self,
        node_id: u64,
        vectors: &V,
        ef_construction: usize,
    ) {
        self.insert_node::<D, V>(node_id, vectors, ef_construction);
    }

    /// Remove a node from the HNSW graph.
    ///
    /// Unlinks `node_id` from all neighbour lists across all layers and
    /// removes its own adjacency data. If the removed node was the graph's
    /// entry point, a new entry point is chosen (the node with the most
    /// layers).
    ///
    /// This is a **lazy** tombstone-free removal — neighbour connections
    /// that passed through the removed node are not re-wired. For large
    /// graphs this has negligible impact on recall. For small graphs
    /// (< 100 nodes) or high deletion rates, consider a periodic
    /// [`build()`](Self::build) to restore optimal connectivity.
    ///
    /// No-op if `node_id` is not in the graph.
    pub fn remove(&mut self, node_id: u64) {
        if let Some(layers) = self.nodes.remove(&node_id) {
            // Remove node_id from all its neighbors' adjacency lists
            for layer in &layers {
                for &nb_id in layer {
                    if let Some(nb_layers) = self.nodes.get_mut(&nb_id) {
                        for nb_layer in nb_layers.iter_mut() {
                            nb_layer.retain(|&id| id != node_id);
                        }
                    }
                }
            }
        }
        // Fix entry point if we just removed it
        if self.entry_point.map(|(id, _)| id) == Some(node_id) {
            self.entry_point = self.nodes.iter()
                .max_by_key(|(_, layers)| layers.len())
                .map(|(&id, layers)| (id, layers.len().saturating_sub(1)));
        }
    }

    // ── Construction internals ────────────────────────────────────────────────

    fn insert_node<D: Distance, V: VectorAccess>(
        &mut self,
        node_id: u64,
        vectors: &V,
        ef_construction: usize,
    ) {
        let query = match vectors.get(node_id) {
            Some(v) => v,
            None => return,
        };

        let max_level = self.pick_level(node_id);

        // First node: set as entry point with empty layers, done.
        if self.entry_point.is_none() {
            let layers = (0..=max_level).map(|_| Vec::new()).collect();
            self.nodes.insert(node_id, layers);
            self.entry_point = Some((node_id, max_level));
            return;
        }

        let (ep_id, ep_level) = self.entry_point.unwrap();

        // Pre-insert node with empty layers so search can see it (it has no
        // neighbours yet, so it won't be traversed back to).
        {
            let layers = (0..=max_level).map(|_| Vec::new()).collect();
            self.nodes.insert(node_id, layers);
        }

        // ── Phase 1: greedy descent from ep_level to max_level+1 (ef=1) ──────
        let mut curr_ep = ep_id;
        for level in (max_level + 1..=ep_level).rev() {
            let cands =
                search_layer::<D, V>(&self.nodes, query, curr_ep, 1, level, vectors);
            if let Some(best) = cands.into_iter().min_by(|a, b| {
                a.dist.partial_cmp(&b.dist).unwrap_or(Ordering::Equal)
            }) {
                curr_ep = best.id;
            }
        }

        // ── Phase 2: connect at each shared level ─────────────────────────────
        for level in (0..=max_level.min(ep_level)).rev() {
            let cands = search_layer::<D, V>(
                &self.nodes,
                query,
                curr_ep,
                ef_construction,
                level,
                vectors,
            );

            // Best candidate becomes entry for the next (lower) level.
            if let Some(best) = cands.iter().min_by(|a, b| {
                a.dist.partial_cmp(&b.dist).unwrap_or(Ordering::Equal)
            }) {
                curr_ep = best.id;
            }

            let m_max = self.m_max(level);
            let neighbors = select_neighbors_heuristic::<D, V>(&cands, m_max, vectors);

            // Write the new node's neighbour list at this level.
            if let Some(node_layers) = self.nodes.get_mut(&node_id) {
                if level < node_layers.len() {
                    node_layers[level] = neighbors.clone();
                }
            }

            // Bidirectional wiring.
            for &nb_id in &neighbors {
                // We need a clone of the current neighbour list to avoid
                // simultaneous mutable+immutable borrow of self.nodes.
                let needs_pruning = {
                    if let Some(nb_layers) = self.nodes.get_mut(&nb_id) {
                        if level < nb_layers.len() {
                            nb_layers[level].push(node_id);
                            nb_layers[level].len() > m_max
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                };

                if needs_pruning {
                    // Clone the vec, compute pruned list, write back.
                    let current: Vec<u64> = self
                        .nodes
                        .get(&nb_id)
                        .and_then(|ls| ls.get(level))
                        .cloned()
                        .unwrap_or_default();
                    let nb_vec: Vec<f32> = vectors.get(nb_id).map(|s| s.to_vec()).unwrap_or_default();
                    let pruned = prune_neighbors::<D, V>(&nb_vec, &current, m_max, vectors);
                    if let Some(nb_layers) = self.nodes.get_mut(&nb_id) {
                        if level < nb_layers.len() {
                            nb_layers[level] = pruned;
                        }
                    }
                }
            }
        }

        // ── Phase 3: promote entry point if new node reached a higher level ───
        if max_level > ep_level {
            self.entry_point = Some((node_id, max_level));
        }
    }
}

// ── Iterable vectors (needed for build) ──────────────────────────────────────

/// Extension trait for vector stores that support iteration.
///
/// Required by [`HnswGraph::build()`] which needs to enumerate all vectors.
/// Separated from [`VectorAccess`] because mmap-backed stores can implement
/// `get()` cheaply but `iter()` requires scanning the offset index.
pub trait IterableVectors {
    /// Iterate over all `(node_id, vector)` pairs.
    fn iter_vectors(&self) -> Box<dyn Iterator<Item = (u64, &[f32])> + '_>;
}

impl IterableVectors for HashMap<u64, Vec<f32>> {
    fn iter_vectors(&self) -> Box<dyn Iterator<Item = (u64, &[f32])> + '_> {
        Box::new(self.iter().map(|(&id, v)| (id, v.as_slice())))
    }
}

/// Contiguous, cache-friendly snapshot of a field's vectors for fast index BUILD.
///
/// The persistent store is a `HashMap<u64, Vec<f32>>` — one scattered heap
/// allocation per vector. Building an HNSW does *billions* of random distance
/// evaluations, so each read is a pointer-chase to a random heap Vec → a cache
/// miss (this is why sekejap's build scaled super-linearly). We first copy every
/// vector into ONE flat buffer and index it densely, turning every distance read
/// into a contiguous slice. Same idea as hnswlib's dense `Vec<NodeId>` store and
/// Qdrant's global-to-local id table.
pub struct DenseVectors {
    data: Vec<f32>,          // n * dim, contiguous
    dim: usize,
    ids: Vec<u64>,           // dense index → node hash
    idx: HashMap<u64, u32>,  // node hash → dense index
}

impl DenseVectors {
    /// Copy every vector from `src` into one contiguous buffer.
    pub fn snapshot<V: IterableVectors>(src: &V) -> Self {
        let mut data = Vec::new();
        let mut ids = Vec::new();
        let mut idx = HashMap::new();
        let mut dim = 0usize;
        for (id, v) in src.iter_vectors() {
            if dim == 0 { dim = v.len(); }
            idx.insert(id, ids.len() as u32);
            ids.push(id);
            data.extend_from_slice(v);
        }
        Self { data, dim, ids, idx }
    }
    /// Flat buffer + dense→hash id map (for the parallel builder).
    pub fn parts(&self) -> (&[f32], usize, &[u64]) { (&self.data, self.dim, &self.ids) }
}

impl VectorAccess for DenseVectors {
    fn get(&self, id: u64) -> Option<&[f32]> {
        let i = *self.idx.get(&id)? as usize;
        Some(&self.data[i * self.dim..(i + 1) * self.dim])
    }
    fn len(&self) -> usize { self.ids.len() }
}

impl IterableVectors for DenseVectors {
    fn iter_vectors(&self) -> Box<dyn Iterator<Item = (u64, &[f32])> + '_> {
        let dim = self.dim;
        Box::new(self.ids.iter().enumerate().map(move |(i, &id)| (id, &self.data[i * dim..(i + 1) * dim])))
    }
}

// ── Module-level search helpers ───────────────────────────────────────────────

/// Beam search restricted to one layer of the graph.
///
/// Returns all explored candidates sorted ascending by distance.
fn search_layer<D: Distance, V: VectorAccess>(
    nodes: &HashMap<u64, Vec<Vec<u64>>>,
    query: &[f32],
    entry_point: u64,
    ef: usize,
    layer: usize,
    vectors: &V,
) -> Vec<MinCand> {
    let d0 = match vectors.get(entry_point) {
        Some(v) => D::eval(query, v),
        None => return vec![],
    };

    let mut visited: HashSet<u64> = HashSet::new();
    visited.insert(entry_point);

    // Min-heap: process closest candidate first.
    let mut to_visit: BinaryHeap<MinCand> = BinaryHeap::new();
    to_visit.push(MinCand { id: entry_point, dist: d0 });

    // Max-heap: keep best ef results (evict farthest when over capacity).
    let mut results: BinaryHeap<MaxCand> = BinaryHeap::new();
    results.push(MaxCand { id: entry_point, dist: d0 });

    while let Some(MinCand { id, dist: c_dist }) = to_visit.pop() {
        let worst = results.peek().map(|r| r.dist).unwrap_or(f32::INFINITY);
        if c_dist > worst && results.len() >= ef {
            break;
        }

        let neighbours = nodes
            .get(&id)
            .and_then(|ls| ls.get(layer))
            .map(|ns| ns.as_slice())
            .unwrap_or(&[]);

        for &nb in neighbours {
            if visited.contains(&nb) {
                continue;
            }
            visited.insert(nb);

            let d = match vectors.get(nb) {
                Some(v) => D::eval(query, v),
                None => continue,
            };

            let worst = results.peek().map(|r| r.dist).unwrap_or(f32::INFINITY);
            if d < worst || results.len() < ef {
                to_visit.push(MinCand { id: nb, dist: d });
                results.push(MaxCand { id: nb, dist: d });
                if results.len() > ef {
                    results.pop(); // evict farthest
                }
            }
        }
    }

    // Convert to Vec sorted ascending by distance.
    let mut out: Vec<MinCand> = results
        .into_iter()
        .map(|mc| MinCand { id: mc.id, dist: mc.dist })
        .collect();
    out.sort_by(|a, b| a.dist.partial_cmp(&b.dist).unwrap_or(Ordering::Equal));
    out
}

/// Integer-distance twin of [`search_layer`] for the disk-first int8 path.
///
/// Identical beam search, but distance = [`l2_u8`] over u8 codes read from RAM.
/// The integer sum (≤ dim·255² ≈ 8.3M for 128-d) is < 2²⁴, so casting to `f32`
/// for the heap ordering is exact and lets us reuse `MinCand`/`MaxCand`.
fn search_layer_quant<Q: QuantAccess>(
    nodes: &HashMap<u64, Vec<Vec<u64>>>,
    q_code: &[u8],
    entry_point: u64,
    ef: usize,
    layer: usize,
    codes: &Q,
) -> Vec<MinCand> {
    let d0 = match codes.code(entry_point) {
        Some(c) => l2_u8(q_code, c) as f32,
        None => return vec![],
    };

    let mut visited: HashSet<u64> = HashSet::new();
    visited.insert(entry_point);

    let mut to_visit: BinaryHeap<MinCand> = BinaryHeap::new();
    to_visit.push(MinCand { id: entry_point, dist: d0 });
    let mut results: BinaryHeap<MaxCand> = BinaryHeap::new();
    results.push(MaxCand { id: entry_point, dist: d0 });

    while let Some(MinCand { id, dist: c_dist }) = to_visit.pop() {
        let worst = results.peek().map(|r| r.dist).unwrap_or(f32::INFINITY);
        if c_dist > worst && results.len() >= ef {
            break;
        }

        let neighbours = nodes
            .get(&id)
            .and_then(|ls| ls.get(layer))
            .map(|ns| ns.as_slice())
            .unwrap_or(&[]);

        for &nb in neighbours {
            if !visited.insert(nb) {
                continue;
            }
            let d = match codes.code(nb) {
                Some(c) => l2_u8(q_code, c) as f32,
                None => continue,
            };
            let worst = results.peek().map(|r| r.dist).unwrap_or(f32::INFINITY);
            if d < worst || results.len() < ef {
                to_visit.push(MinCand { id: nb, dist: d });
                results.push(MaxCand { id: nb, dist: d });
                if results.len() > ef {
                    results.pop();
                }
            }
        }
    }

    let mut out: Vec<MinCand> = results
        .into_iter()
        .map(|mc| MinCand { id: mc.id, dist: mc.dist })
        .collect();
    out.sort_by(|a, b| a.dist.partial_cmp(&b.dist).unwrap_or(Ordering::Equal));
    out
}

/// Select up to `m` diverse neighbours using the paper's simple heuristic.
///
/// Accepts a candidate whose closest already-selected neighbour is farther from
/// it than the query is.  Fills remaining slots from discarded candidates.
fn select_neighbors_heuristic<D: Distance, V: VectorAccess>(
    candidates: &[MinCand],
    m: usize,
    vectors: &V,
) -> Vec<u64> {
    if candidates.len() <= m {
        return candidates.iter().map(|c| c.id).collect();
    }

    // Candidates are already sorted ascending by dist to query.
    let mut result: Vec<u64> = Vec::with_capacity(m);
    let mut discarded: Vec<&MinCand> = Vec::new();

    'outer: for candidate in candidates {
        if result.len() >= m {
            break;
        }
        let cv = match vectors.get(candidate.id) {
            Some(v) => v,
            None => continue,
        };
        // Accept only if no already-chosen neighbour is closer to this candidate
        // than the query itself.
        for &sel_id in &result {
            if let Some(sv) = vectors.get(sel_id) {
                if D::eval(cv, sv) < candidate.dist {
                    discarded.push(candidate);
                    continue 'outer;
                }
            }
        }
        result.push(candidate.id);
    }

    // Fill remaining slots from discarded (preserve count = min(candidates, m)).
    for c in discarded {
        if result.len() >= m {
            break;
        }
        result.push(c.id);
    }

    result
}

// ── Dense build helpers (contiguous data + dense ids, no HashMap) ──────────────

/// Beam search over one layer of the DENSE graph. `nodes[id][layer]` holds
/// neighbour dense ids; vectors are read as `flat[id*dim..]`. Returns candidates
/// sorted ascending by distance as `(dist, dense_id)`.
fn search_layer_dense<D: Distance>(
    nodes: &[Vec<Vec<u32>>],
    flat: &[f32],
    dim: usize,
    query: &[f32],
    ep: u32,
    ef: usize,
    layer: usize,
) -> Vec<(f32, u32)> {
    let vget = |a: u32| -> &[f32] { &flat[a as usize * dim..(a as usize + 1) * dim] };
    let d0 = D::eval(query, vget(ep));
    let mut visited: HashSet<u32> = HashSet::new();
    visited.insert(ep);
    let mut to_visit: BinaryHeap<MinCand> = BinaryHeap::new();
    to_visit.push(MinCand { id: ep as u64, dist: d0 });
    let mut results: BinaryHeap<MaxCand> = BinaryHeap::new();
    results.push(MaxCand { id: ep as u64, dist: d0 });

    while let Some(MinCand { id, dist: c_dist }) = to_visit.pop() {
        let worst = results.peek().map(|r| r.dist).unwrap_or(f32::INFINITY);
        if c_dist > worst && results.len() >= ef {
            break;
        }
        if let Some(nbs) = nodes[id as usize].get(layer) {
            for &nb in nbs {
                if !visited.insert(nb) {
                    continue;
                }
                let d = D::eval(query, vget(nb));
                let worst = results.peek().map(|r| r.dist).unwrap_or(f32::INFINITY);
                if d < worst || results.len() < ef {
                    to_visit.push(MinCand { id: nb as u64, dist: d });
                    results.push(MaxCand { id: nb as u64, dist: d });
                    if results.len() > ef {
                        results.pop();
                    }
                }
            }
        }
    }
    let mut out: Vec<(f32, u32)> = results.into_iter().map(|mc| (mc.dist, mc.id as u32)).collect();
    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
    out
}

/// Neighbour-selection heuristic (dense): keep a candidate only if no already-chosen
/// neighbour is closer to it than the query is. `cands` sorted ascending by dist.
fn select_neighbors_dense<D: Distance>(cands: &[(f32, u32)], m: usize, flat: &[f32], dim: usize) -> Vec<u32> {
    let vget = |a: u32| -> &[f32] { &flat[a as usize * dim..(a as usize + 1) * dim] };
    if cands.len() <= m {
        return cands.iter().map(|c| c.1).collect();
    }
    let mut result: Vec<u32> = Vec::with_capacity(m);
    let mut discarded: Vec<u32> = Vec::new();
    'outer: for &(cd, cid) in cands {
        if result.len() >= m {
            break;
        }
        let cv = vget(cid);
        for &sid in &result {
            if D::eval(cv, vget(sid)) < cd {
                discarded.push(cid);
                continue 'outer;
            }
        }
        result.push(cid);
    }
    for cid in discarded {
        if result.len() >= m {
            break;
        }
        result.push(cid);
    }
    result
}

/// Locked variant of [`search_layer_dense`] for the parallel builder: neighbour
/// lists are read under a short per-node `RwLock` (cloned out so distance evals
/// never hold the lock).
fn search_layer_dense_locked<D: Distance>(
    nodes: &[std::sync::RwLock<Vec<Vec<u32>>>],
    flat: &[f32],
    dim: usize,
    query: &[f32],
    ep: u32,
    ef: usize,
    layer: usize,
) -> Vec<(f32, u32)> {
    let vget = |a: u32| -> &[f32] { &flat[a as usize * dim..(a as usize + 1) * dim] };
    let d0 = D::eval(query, vget(ep));
    let mut visited: HashSet<u32> = HashSet::new();
    visited.insert(ep);
    let mut to_visit: BinaryHeap<MinCand> = BinaryHeap::new();
    to_visit.push(MinCand { id: ep as u64, dist: d0 });
    let mut results: BinaryHeap<MaxCand> = BinaryHeap::new();
    results.push(MaxCand { id: ep as u64, dist: d0 });

    while let Some(MinCand { id, dist: c_dist }) = to_visit.pop() {
        let worst = results.peek().map(|r| r.dist).unwrap_or(f32::INFINITY);
        if c_dist > worst && results.len() >= ef {
            break;
        }
        let nbs = nodes[id as usize].read().unwrap().get(layer).cloned().unwrap_or_default();
        for nb in nbs {
            if !visited.insert(nb) {
                continue;
            }
            let d = D::eval(query, vget(nb));
            let worst = results.peek().map(|r| r.dist).unwrap_or(f32::INFINITY);
            if d < worst || results.len() < ef {
                to_visit.push(MinCand { id: nb as u64, dist: d });
                results.push(MaxCand { id: nb as u64, dist: d });
                if results.len() > ef {
                    results.pop();
                }
            }
        }
    }
    let mut out: Vec<(f32, u32)> = results.into_iter().map(|mc| (mc.dist, mc.id as u32)).collect();
    out.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
    out
}

/// Re-select neighbours for an existing node after its list grew too large.
fn prune_neighbors<D: Distance, V: VectorAccess>(
    query: &[f32],
    current: &[u64],
    m: usize,
    vectors: &V,
) -> Vec<u64> {
    let mut candidates: Vec<MinCand> = current
        .iter()
        .filter_map(|&id| {
            vectors
                .get(id)
                .map(|v| MinCand { id, dist: D::eval(query, v) })
        })
        .collect();
    candidates.sort_by(|a, b| a.dist.partial_cmp(&b.dist).unwrap_or(Ordering::Equal));
    select_neighbors_heuristic::<D, V>(&candidates, m, vectors)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vector::CosineDistance;

    fn make_vecs(n: usize, dim: usize) -> HashMap<u64, Vec<f32>> {
        let mut map = HashMap::new();
        for i in 0..n {
            // Simple deterministic vectors: mostly zeros with one hot-ish component.
            let mut v = vec![0.0f32; dim];
            v[i % dim] = 1.0;
            v[(i + 1) % dim] = 0.3;
            map.insert(i as u64, v);
        }
        map
    }

    #[test]
    fn build_and_search_basic() {
        let vecs = make_vecs(20, 8);
        let graph = HnswGraph::build::<CosineDistance, _>(&vecs, 4, 40);
        assert_eq!(graph.len(), 20);

        // Query identical to node 0's vector → the top result must have
        // cosine distance ≈ 0 (several nodes may share the same vector due
        // to the `i % dim` construction).
        let query = vecs[&0].clone();
        let results = graph.search::<CosineDistance, _>(&query, &vecs, 3, 10);
        assert!(!results.is_empty());
        let top_dist = CosineDistance::eval(&query, &vecs[&results[0]]);
        assert!(
            top_dist < 1e-5,
            "top result should be at distance ~0, got {top_dist}"
        );
    }

    #[test]
    fn search_returns_at_most_k() {
        let vecs = make_vecs(50, 16);
        let graph = HnswGraph::build::<CosineDistance, _>(&vecs, 8, 100);
        let query = vecs[&0].clone();
        let results = graph.search::<CosineDistance, _>(&query, &vecs, 5, 20);
        assert!(results.len() <= 5);
    }

    #[test]
    fn empty_graph_search_is_empty() {
        let graph = HnswGraph::new(8);
        let vecs: HashMap<u64, Vec<f32>> = HashMap::new();
        let query = vec![1.0f32, 0.0, 0.0];
        let results = graph.search::<CosineDistance, _>(&query, &vecs, 5, 10);
        assert!(results.is_empty());
    }

    #[test]
    fn single_node_graph() {
        let mut vecs = HashMap::new();
        vecs.insert(42u64, vec![1.0f32, 0.0, 0.0]);
        let graph = HnswGraph::build::<CosineDistance, _>(&vecs, 4, 20);
        let results = graph.search::<CosineDistance, _>(&[1.0, 0.0, 0.0], &vecs, 1, 5);
        assert_eq!(results, vec![42u64]);
    }

    #[test]
    fn recall_at_10_reasonable() {
        // Build 200 random-ish vectors in 32 dims.
        let n = 200usize;
        let dim = 32usize;
        let mut vecs: HashMap<u64, Vec<f32>> = HashMap::new();
        for i in 0..n {
            let v: Vec<f32> = (0..dim)
                .map(|j| {
                    // Deterministic pseudo-random value via xorshift
                    let seed = (i as u64).wrapping_mul(6364136223846793005)
                        ^ (j as u64).wrapping_mul(1442695040888963407);
                    let x = random_unit(seed) as f32;
                    x * 2.0 - 1.0
                })
                .collect();
            vecs.insert(i as u64, v);
        }

        let graph = HnswGraph::build::<CosineDistance, _>(&vecs, 16, 200);
        let query = vecs[&0].clone();
        let k = 10;

        // Brute-force ground truth.
        let mut brute: Vec<(u64, f32)> = vecs
            .iter()
            .map(|(&id, v)| (id, CosineDistance::eval(&query, v)))
            .collect();
        brute.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));
        let ground_truth: HashSet<u64> = brute.iter().take(k).map(|(id, _)| *id).collect();

        let hnsw_results: HashSet<u64> =
            graph.search::<CosineDistance, _>(&query, &vecs, k, k * 3).into_iter().collect();

        let hits = ground_truth.intersection(&hnsw_results).count();
        // Expect at least 70% recall (typically >90% with m=16, ef=30).
        assert!(
            hits >= 7,
            "recall@10 too low: {hits}/10 correct (HNSW: {:?}, truth: {:?})",
            hnsw_results,
            ground_truth
        );
    }
}
