//! # The compact vector index — arrays instead of hash maps
//!
//! The straightforward HNSW-on-disk layout uses hash maps keyed by node id (a
//! `HashMap<u64, ...>` for the graph, another for id→slot), which are pointer-
//! heavy and RAM-hungry. This file replaces them with dense, contiguous ARRAYS
//! indexed by a small **slot** number (0, 1, 2, …): the int8 codes become one
//! flat `Vec<u8>`, the graph becomes flat neighbour arrays. Contiguous arrays are
//! tiny, cache-friendly, and — crucially — trivially memory-mappable, which is
//! what lets the vector index be served from disk with little resident RAM.
//!
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

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use crate::storage::mmap::MmapView;
use crate::vector::access::QuantAccess;
use crate::vector::hnsw::HnswGraph;
use crate::vector::quant::{l2_u8, QuantizedField, ScalarQuantizer};

/// Byte array backing: heap-resident `Vec<u8>` or a range of an mmap'd sidecar.
enum Bytes8 {
    Owned(Vec<u8>),
    Mapped { view: Arc<MmapView>, off: usize, len: usize },
}
impl Bytes8 {
    #[inline]
    fn slice(&self, o: usize, n: usize) -> &[u8] {
        match self {
            Bytes8::Owned(v) => &v[o..o + n],
            Bytes8::Mapped { view, off, .. } => view.slice(off + o, n).unwrap_or(&[]),
        }
    }
    fn byte_len(&self) -> usize {
        match self { Bytes8::Owned(v) => v.len(), Bytes8::Mapped { len, .. } => *len }
    }
}

/// `u32` array backing. Resident returns a borrowed slice (zero cost); mapped
/// decodes on demand (per-element `get`, or a small owned `Cow` for a range —
/// the neighbour lists are ~degree-sized, so the per-node decode is cheap).
enum ArrU32 {
    Owned(Vec<u32>),
    Mapped { view: Arc<MmapView>, off: usize, count: usize },
}
impl ArrU32 {
    #[inline]
    fn get(&self, i: usize) -> u32 {
        match self {
            ArrU32::Owned(v) => v[i],
            ArrU32::Mapped { view, off, .. } => {
                let s = view.slice(off + i * 4, 4).unwrap_or(&[0; 4]);
                u32::from_le_bytes([s[0], s[1], s[2], s[3]])
            }
        }
    }
    #[inline]
    fn range(&self, a: usize, b: usize) -> Cow<'_, [u32]> {
        match self {
            ArrU32::Owned(v) => Cow::Borrowed(&v[a..b]),
            ArrU32::Mapped { view, off, .. } => {
                let bytes = view.slice(off + a * 4, (b - a) * 4).unwrap_or(&[]);
                Cow::Owned(bytes.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
            }
        }
    }
    fn count(&self) -> usize {
        match self { ArrU32::Owned(v) => v.len(), ArrU32::Mapped { count, .. } => *count }
    }
}

/// `u64` array backing (slot → id).
enum ArrU64 {
    Owned(Vec<u64>),
    Mapped { view: Arc<MmapView>, off: usize, count: usize },
}
impl ArrU64 {
    #[inline]
    fn get(&self, i: usize) -> u64 {
        match self {
            ArrU64::Owned(v) => v[i],
            ArrU64::Mapped { view, off, .. } => {
                let s = view.slice(off + i * 8, 8).unwrap_or(&[0; 8]);
                u64::from_le_bytes(s.try_into().unwrap_or([0; 8]))
            }
        }
    }
    fn count(&self) -> usize {
        match self { ArrU64::Owned(v) => v.len(), ArrU64::Mapped { count, .. } => *count }
    }
}

/// A frozen, slot-indexed disk-first index: int8 codes + CSR graph in RAM,
/// full-precision f32 on disk (re-rank reads it by id).
pub struct CompactDiskIndex {
    dim: usize,
    quantizer: ScalarQuantizer,
    /// int8 codes, slot-major: `codes[slot*dim .. (slot+1)*dim]`.
    codes: Bytes8,
    /// slot → node id (for re-rank + results). No reverse map: hot loop is slot-only.
    slot_to_id: ArrU64,
    /// Layer-0 CSR neighbour offsets (len = n+1).
    l0_off: ArrU32,
    /// Layer-0 neighbour slots, concatenated.
    l0_neigh: ArrU32,
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
            codes: Bytes8::Owned(codes),
            slot_to_id: ArrU64::Owned(slot_to_id),
            l0_off: ArrU32::Owned(l0_off),
            l0_neigh: ArrU32::Owned(l0_neigh),
            upper,
            entry_slot,
            entry_level,
        }
    }

    #[inline]
    fn code(&self, slot: u32) -> &[u8] {
        self.codes.slice(slot as usize * self.dim, self.dim)
    }

    /// Neighbours of `slot` at `layer` (0 = CSR, ≥1 = sparse upper map). Borrowed
    /// (zero cost) in resident mode; a small owned decode in mmap mode.
    #[inline]
    fn neighbours(&self, slot: u32, layer: usize) -> Cow<'_, [u32]> {
        if layer == 0 {
            let a = self.l0_off.get(slot as usize) as usize;
            let b = self.l0_off.get(slot as usize + 1) as usize;
            self.l0_neigh.range(a, b)
        } else {
            self.upper
                .get(&slot)
                .and_then(|ls| ls.get(layer - 1))
                .map(|b| Cow::Borrowed(&b[..]))
                .unwrap_or(Cow::Borrowed(&[]))
        }
    }

    /// Map a raw query vector to codes with this field's calibration.
    #[inline]
    pub fn quantize_query(&self, q: &[f32]) -> Vec<u8> {
        self.quantizer.quantize(q)
    }

    pub fn slot_to_id(&self, slot: u32) -> u64 {
        self.slot_to_id.get(slot as usize)
    }

    pub fn len(&self) -> usize {
        self.slot_to_id.count()
    }
    pub fn is_empty(&self) -> bool {
        self.slot_to_id.count() == 0
    }

    /// True when the arrays are served from the mmap sidecar (paged, disk-first).
    pub fn is_disk_backed(&self) -> bool {
        matches!(self.codes, Bytes8::Mapped { .. })
    }

    /// Approximate resident RAM, in bytes — for profiling. mmap-backed arrays count
    /// as ~0 (they live in the page cache, not the heap).
    pub fn mem_bytes(&self) -> usize {
        let owned8 = |b: &Bytes8| match b { Bytes8::Owned(v) => v.capacity(), _ => 0 };
        let owned32 = |a: &ArrU32| match a { ArrU32::Owned(v) => v.capacity() * 4, _ => 0 };
        let owned64 = |a: &ArrU64| match a { ArrU64::Owned(v) => v.capacity() * 8, _ => 0 };
        owned8(&self.codes)
            + owned64(&self.slot_to_id)
            + owned32(&self.l0_off)
            + owned32(&self.l0_neigh)
            + self.upper.capacity() * (4 + 24)
            + self.upper.values().flatten().map(|b| b.len() * 4).sum::<usize>()
    }

    /// int8 traversal (greedy descent + layer-0 beam), all in slot space.
    /// Returns the top `k` **node ids** by approximate distance for f32 re-rank.
    pub fn search(&self, q_code: &[u8], k: usize, ef: usize) -> Vec<u64> {
        if self.is_empty() {
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
                for &nb in self.neighbours(ep, level).iter() {
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
            for &nb in self.neighbours(cs, 0).iter() {
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
        out.into_iter().map(|(_, s)| self.slot_to_id.get(s as usize)).collect()
    }

    /// Serialize one field's index (read via the backings, so it works whether the
    /// arrays are resident or already mmap-backed). Format `SKVEC01` per blob:
    ///   [u32 dim][f32 offset][f32 scale][u32 entry_slot][u32 entry_level]
    ///   [u64 n]  codes: n*dim | slot_to_id: n*u64 | l0_off: (n+1)*u32
    ///   [u64 l0_neigh_count]  l0_neigh: count*u32
    ///   [u32 upper_count]  per: [u32 slot][u8 num_levels] per level [u32 len][len*u32]
    pub(crate) fn write_binary<W: std::io::Write>(&self, w: &mut W) -> std::io::Result<()> {
        let n = self.len();
        w.write_all(&(self.dim as u32).to_le_bytes())?;
        w.write_all(&self.quantizer.offset.to_le_bytes())?;
        w.write_all(&self.quantizer.scale.to_le_bytes())?;
        w.write_all(&self.entry_slot.to_le_bytes())?;
        w.write_all(&(self.entry_level as u32).to_le_bytes())?;
        w.write_all(&(n as u64).to_le_bytes())?;
        // codes (all bytes)
        w.write_all(self.codes.slice(0, self.codes.byte_len()))?;
        // slot_to_id
        for s in 0..n { w.write_all(&self.slot_to_id.get(s).to_le_bytes())?; }
        // l0_off (n+1)
        for s in 0..=n { w.write_all(&self.l0_off.get(s).to_le_bytes())?; }
        // l0_neigh
        let neigh_count = self.l0_neigh.count();
        w.write_all(&(neigh_count as u64).to_le_bytes())?;
        for i in 0..neigh_count { w.write_all(&self.l0_neigh.get(i).to_le_bytes())?; }
        // upper (sparse)
        w.write_all(&(self.upper.len() as u32).to_le_bytes())?;
        for (&slot, levels) in &self.upper {
            w.write_all(&slot.to_le_bytes())?;
            w.write_all(&[levels.len() as u8])?;
            for lvl in levels {
                w.write_all(&(lvl.len() as u32).to_le_bytes())?;
                for &s in lvl.iter() { w.write_all(&s.to_le_bytes())?; }
            }
        }
        Ok(())
    }

    /// Open one field's index from an mmap'd container starting at `base`. The big
    /// arrays (codes, slot_to_id, l0_off, l0_neigh) are served from the map; only
    /// the small sparse `upper` map is read resident. Returns the index + bytes
    /// consumed so the container loop can advance to the next field.
    pub(crate) fn open_mapped(view: &Arc<MmapView>, base: usize) -> std::io::Result<(Self, usize)> {
        let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m);
        let total = view.len();
        let b = view.slice(base, total.saturating_sub(base)).ok_or_else(|| bad("vec blob range"))?;
        let need = |p: usize, n: usize| -> std::io::Result<()> {
            if p + n > b.len() { Err(bad("truncated vec blob")) } else { Ok(()) }
        };
        let rd_u32 = |p: usize| u32::from_le_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]);
        let rd_u64 = |p: usize| u64::from_le_bytes(b[p..p + 8].try_into().unwrap());
        let rd_f32 = |p: usize| f32::from_le_bytes([b[p], b[p + 1], b[p + 2], b[p + 3]]);

        need(0, 4 + 4 + 4 + 4 + 4 + 8)?;
        let dim = rd_u32(0) as usize;
        let quantizer = ScalarQuantizer { offset: rd_f32(4), scale: rd_f32(8) };
        let entry_slot = rd_u32(12);
        let entry_level = rd_u32(16) as usize;
        let n = rd_u64(20) as usize;
        let mut p = 28;

        let codes_off = base + p;
        p += n * dim;
        let slot_to_id_off = base + p;
        p += n * 8;
        let l0_off_off = base + p;
        p += (n + 1) * 4;
        need(p, 8)?;
        let neigh_count = rd_u64(p) as usize;
        p += 8;
        let l0_neigh_off = base + p;
        p += neigh_count * 4;

        // Upper (sparse) — read resident.
        need(p, 4)?;
        let upper_count = rd_u32(p) as usize;
        p += 4;
        let mut upper: HashMap<u32, Vec<Box<[u32]>>> = HashMap::with_capacity(upper_count);
        for _ in 0..upper_count {
            need(p, 5)?;
            let slot = rd_u32(p); p += 4;
            let num_levels = b[p] as usize; p += 1;
            let mut levels = Vec::with_capacity(num_levels);
            for _ in 0..num_levels {
                need(p, 4)?;
                let len = rd_u32(p) as usize; p += 4;
                need(p, len * 4)?;
                let lvl: Vec<u32> = (0..len).map(|i| rd_u32(p + i * 4)).collect();
                p += len * 4;
                levels.push(lvl.into_boxed_slice());
            }
            upper.insert(slot, levels);
        }

        let idx = Self {
            dim,
            quantizer,
            codes: Bytes8::Mapped { view: view.clone(), off: codes_off, len: n * dim },
            slot_to_id: ArrU64::Mapped { view: view.clone(), off: slot_to_id_off, count: n },
            l0_off: ArrU32::Mapped { view: view.clone(), off: l0_off_off, count: n + 1 },
            l0_neigh: ArrU32::Mapped { view: view.clone(), off: l0_neigh_off, count: neigh_count },
            upper,
            entry_slot,
            entry_level,
        };
        Ok((idx, p))
    }
}
