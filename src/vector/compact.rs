//! Compact, slot-indexed disk-first vector index — the low-RAM representation.
//!
//! Replaces the two fat RAM structures of the naive disk-first path — the
//! `HashMap<u64, Vec<Vec<u64>>>` HNSW graph (~336 B/node) and the int8 store's
//! `id → slot` HashMap — with dense, contiguous arrays keyed by **slot** (0..n):
//!
//! - **int8 codes**: one flat `Vec<u8>`, slot-major (`slot*dim`), direct indexing.
//! - **layer-0 graph**: CSR — `l0_off: Vec<u32>` (n+1) + `l0_neigh: Vec<u32>` (slots).
//! - **upper layers** (only ~1/M nodes have them): a small sparse map.
//! - **slot → id**: a single `Vec<u64>`; there is NO id→slot map, because the whole
//!   hot loop runs in slot space. Ids are recovered only for the final f32 re-rank.
//!
//! Result: the traversal hot path is pure array indexing (no hashing, no pointer
//! chasing), and RAM per node drops from ~478 B (fat graph + int8+map) to
//! ~`dim + 8 + 4 + avg_deg*4` B (e.g. ~270 B at dim=128, deg≈32).

use std::collections::HashMap;

use crate::vector::access::QuantAccess;
use crate::vector::hnsw::HnswGraph;
use crate::vector::quant::{l2_u8, QuantizedField, ScalarQuantizer};

/// A frozen, slot-indexed disk-first index: int8 codes + CSR graph in RAM,
/// full-precision f32 on disk (re-rank reads it by id).
pub struct CompactDiskIndex {
    dim: usize,
    quantizer: ScalarQuantizer,
    /// int8 codes, slot-major: `codes[slot*dim .. (slot+1)*dim]`.
    codes: Vec<u8>,
    /// slot → node id (for re-rank + results). No reverse map: hot loop is slot-only.
    slot_to_id: Vec<u64>,
    /// Layer-0 CSR neighbour offsets (len = n+1).
    l0_off: Vec<u32>,
    /// Layer-0 neighbour slots, concatenated.
    l0_neigh: Vec<u32>,
    /// Upper-layer adjacency for the few nodes that have it: slot → levels≥1 → slots.
    upper: HashMap<u32, Vec<Box<[u32]>>>,
    entry_slot: u32,
    entry_level: usize,
}

impl CompactDiskIndex {
    /// Build from a constructed HNSW graph + the field's int8 store. Consumes the
    /// codes into slot order; the caller drops the fat `HnswGraph`/`QuantizedField`
    /// afterwards, leaving only this compact form resident.
    pub fn from_hnsw(graph: &HnswGraph, qf: &QuantizedField, dim: usize) -> Self {
        let nodes = graph.raw_nodes();
        let n = nodes.len();

        // Assign slots in a deterministic id order (sorted → stable, reproducible).
        let mut ids: Vec<u64> = nodes.keys().copied().collect();
        ids.sort_unstable();
        let mut id_to_slot: HashMap<u64, u32> = HashMap::with_capacity(n);
        for (slot, &id) in ids.iter().enumerate() {
            id_to_slot.insert(id, slot as u32);
        }

        let mut codes = vec![0u8; n * dim];
        let mut slot_to_id = vec![0u64; n];
        let mut l0_off = Vec::with_capacity(n + 1);
        let mut l0_neigh = Vec::new();
        let mut upper: HashMap<u32, Vec<Box<[u32]>>> = HashMap::new();

        l0_off.push(0u32);
        for (slot, &id) in ids.iter().enumerate() {
            slot_to_id[slot] = id;
            if let Some(c) = qf.code(id) {
                codes[slot * dim..(slot + 1) * dim].copy_from_slice(c);
            }
            let layers = &nodes[&id];
            // Layer 0.
            if let Some(l0) = layers.first() {
                for &nb in l0 {
                    if let Some(&s) = id_to_slot.get(&nb) {
                        l0_neigh.push(s);
                    }
                }
            }
            l0_off.push(l0_neigh.len() as u32);
            // Upper layers (≥1), if any.
            if layers.len() > 1 {
                let ups: Vec<Box<[u32]>> = layers[1..]
                    .iter()
                    .map(|lvl| {
                        lvl.iter()
                            .filter_map(|nb| id_to_slot.get(nb).copied())
                            .collect::<Vec<u32>>()
                            .into_boxed_slice()
                    })
                    .collect();
                upper.insert(slot as u32, ups);
            }
        }

        let (entry_slot, entry_level) = match graph.raw_entry() {
            Some((id, lvl)) => (*id_to_slot.get(&id).unwrap_or(&0), lvl),
            None => (0, 0),
        };

        l0_neigh.shrink_to_fit();
        Self {
            dim,
            quantizer: qf.quantizer.clone(),
            codes,
            slot_to_id,
            l0_off,
            l0_neigh,
            upper,
            entry_slot,
            entry_level,
        }
    }

    #[inline]
    fn code(&self, slot: u32) -> &[u8] {
        let o = slot as usize * self.dim;
        &self.codes[o..o + self.dim]
    }

    /// Neighbours of `slot` at `layer` (0 = CSR, ≥1 = sparse upper map).
    #[inline]
    fn neighbours(&self, slot: u32, layer: usize) -> &[u32] {
        if layer == 0 {
            let a = self.l0_off[slot as usize] as usize;
            let b = self.l0_off[slot as usize + 1] as usize;
            &self.l0_neigh[a..b]
        } else {
            self.upper
                .get(&slot)
                .and_then(|ls| ls.get(layer - 1))
                .map(|b| &b[..])
                .unwrap_or(&[])
        }
    }

    /// Map a raw query vector to codes with this field's calibration.
    #[inline]
    pub fn quantize_query(&self, q: &[f32]) -> Vec<u8> {
        self.quantizer.quantize(q)
    }

    pub fn slot_to_id(&self, slot: u32) -> u64 {
        self.slot_to_id[slot as usize]
    }

    pub fn len(&self) -> usize {
        self.slot_to_id.len()
    }
    pub fn is_empty(&self) -> bool {
        self.slot_to_id.is_empty()
    }

    /// Approximate resident RAM, in bytes — for profiling.
    pub fn mem_bytes(&self) -> usize {
        self.codes.capacity()
            + self.slot_to_id.capacity() * 8
            + self.l0_off.capacity() * 4
            + self.l0_neigh.capacity() * 4
            + self.upper.capacity() * (4 + 24)
            + self.upper.values().flatten().map(|b| b.len() * 4).sum::<usize>()
    }

    /// int8 traversal (greedy descent + layer-0 beam), all in slot space.
    /// Returns the top `k` **node ids** by approximate distance for f32 re-rank.
    pub fn search(&self, q_code: &[u8], k: usize, ef: usize) -> Vec<u64> {
        if self.slot_to_id.is_empty() {
            return vec![];
        }
        let dist = |slot: u32| l2_u8(q_code, self.code(slot));

        let mut ep = self.entry_slot;
        let mut ep_d = dist(ep);

        // Greedy descent through upper layers.
        for level in (1..=self.entry_level).rev() {
            let mut improved = true;
            while improved {
                improved = false;
                for &nb in self.neighbours(ep, level) {
                    let d = dist(nb);
                    if d < ep_d {
                        ep_d = d;
                        ep = nb;
                        improved = true;
                    }
                }
            }
        }

        // Layer-0 beam search.
        let ef = ef.max(k);
        let mut visited: HashMap<u32, ()> = HashMap::with_capacity(ef * 8);
        visited.insert(ep, ());
        // candidate min-heap by dist (use Vec + sort-free via BinaryHeap of Reverse)
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        let mut cand: BinaryHeap<(Reverse<u32>, u32)> = BinaryHeap::new(); // (dist, slot) min
        let mut res: BinaryHeap<(u32, u32)> = BinaryHeap::new(); // (dist, slot) max
        cand.push((Reverse(ep_d), ep));
        res.push((ep_d, ep));

        while let Some((Reverse(cd), cs)) = cand.pop() {
            let worst = res.peek().map(|&(d, _)| d).unwrap_or(u32::MAX);
            if cd > worst && res.len() >= ef {
                break;
            }
            for &nb in self.neighbours(cs, 0) {
                if visited.insert(nb, ()).is_some() {
                    continue;
                }
                let d = dist(nb);
                let worst = res.peek().map(|&(dd, _)| dd).unwrap_or(u32::MAX);
                if d < worst || res.len() < ef {
                    cand.push((Reverse(d), nb));
                    res.push((d, nb));
                    if res.len() > ef {
                        res.pop();
                    }
                }
            }
        }

        let mut out: Vec<(u32, u32)> = res.into_vec();
        out.sort_unstable_by_key(|&(d, _)| d);
        out.truncate(k);
        out.into_iter().map(|(_, s)| self.slot_to_id[s as usize]).collect()
    }
}
