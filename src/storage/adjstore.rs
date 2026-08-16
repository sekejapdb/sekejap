//! Adjacency that accepts an edge in place, addressed by slug hash.
//!
//! # The structure this replaces
//!
//! Adjacency lives in `adj_fwd.bin` / `adj_rev.bin` as CSR — compressed sparse row.
//! One array holds every node's edge block back to back, and a second array of
//! offsets says where each node's block starts. It is the standard read-optimised
//! layout and it is genuinely good at what it does: a node's edges are one
//! contiguous run, found by two array reads.
//!
//! It also cannot absorb a single new edge. Giving node 5 one more neighbour means
//! its block grows, so every block after it shifts, so every offset after it
//! changes. There is no insert; there is only *rebuild*. That is why adding edges
//! accumulates in RAM until compaction folds the whole graph back — and why the
//! fold costs time proportional to the graph rather than to the change.
//!
//! # Why the neighbour is a hash here and not a dense id
//!
//! CSR stores a neighbour as a *dense id*: the node's rank in `nodes.bin`. Ids are
//! small and sorted within a block, so they delta-encode.
//!
//! Except they do not, in this database. `write_topology_files` sorts nodes by hash
//! before numbering them, so a dense id is a node's rank *by hash* — and a hash is
//! chosen precisely to destroy ordering. Neighbours adjacent in the graph get
//! unrelated ids. Measured in `examples/topo_bytes.rs`: the same 200 000-node graph
//! built with neighbours at `i+1` and with neighbours scattered at random produces
//! `adj_fwd.bin` files 0.006% apart. The compression is dead weight by construction.
//!
//! Worse, the dense id has to be converted back. Every caller of this data speaks
//! hashes — `fwd_edges(hash)` in, `other_hash` out — so the CSR reader performs a
//! random read into `nodes.bin` for *each neighbour it reports*, purely to undo the
//! numbering. On a 48-million-node store `nodes.bin` is 1.5 GB, so that is a likely
//! page fault per edge traversed.
//!
//! Storing the hash directly costs 20 bytes an edge against roughly 13.6, and buys:
//! insert in place, no numbering to keep consistent, and no per-neighbour lookup.
//!
//! # The layout
//!
//! One [`PagedStore`] — a B+tree from key to record id, over slotted pages with a
//! free list. The key is the owner's slug hash; the record is that node's whole
//! edge list:
//!
//! ```text
//!   key: owner_hash            record: [ neighbour u64 | type u64 | meta u32 ] × n
//!                                        └──────────── 20 bytes ────────────┘
//! ```
//!
//! Reading a node's edges is one tree descent plus one record read, and the record
//! is the answer — no second structure, no per-neighbour lookup. Adding an edge
//! rewrites that one node's list, which is bounded by its degree rather than by the
//! size of the graph. **That is the property the whole direction exists for**, and
//! `adding_an_edge_costs_the_same_at_every_size` is where it is checked.
//!
//! # What is given up
//!
//! A high-degree node's list is rewritten in full on every edge added to it, so
//! building a node of degree *d* costs O(d²) rather than O(d). For ordinary graph
//! degrees that is invisible — see `a_high_degree_node_stays_usable` for where it
//! stops being invisible. Bulk loading should group a node's edges and add them
//! together; `add_many` exists for exactly that and turns the d² back into d.

use super::pagedstore::PagedStore;
use std::io;
use std::path::Path;

/// One stored edge: who it points at, what type it is, and where its attributes
/// live. 20 bytes, fixed — the same three fields CSR keeps, without the numbering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdjEdge {
    /// `sk_hash(slug)` of the node at the other end.
    pub other: u64,
    /// `sk_hash` of the edge type label. A hash rather than an interned id because
    /// every caller already speaks hashes, and because interning needs a dictionary
    /// that itself has to be maintained in place.
    pub edge_type: u64,
    /// Index into the edge-metadata store, or `NO_META` when the edge is naked.
    pub meta_ref: u32,
}

pub(crate) const NO_META: u32 = u32::MAX;

const EDGE_BYTES: usize = 20;

fn encode(edges: &[AdjEdge], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(edges.len() * EDGE_BYTES);
    for e in edges {
        out.extend_from_slice(&e.other.to_le_bytes());
        out.extend_from_slice(&e.edge_type.to_le_bytes());
        out.extend_from_slice(&e.meta_ref.to_le_bytes());
    }
}

/// Decode a stored list. A trailing partial edge is dropped rather than trusted:
/// this is disk data, and a short read must not become a panic.
fn decode(bytes: &[u8]) -> Vec<AdjEdge> {
    let n = bytes.len() / EDGE_BYTES;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let o = i * EDGE_BYTES;
        out.push(AdjEdge {
            other: u64::from_le_bytes(bytes[o..o + 8].try_into().unwrap()),
            edge_type: u64::from_le_bytes(bytes[o + 8..o + 16].try_into().unwrap()),
            meta_ref: u32::from_le_bytes(bytes[o + 16..o + 20].try_into().unwrap()),
        });
    }
    out
}

pub(crate) struct AdjStore {
    store: PagedStore,
    /// Reused across writes so adding an edge does not allocate.
    scratch: Vec<u8>,
}

impl AdjStore {
    /// Open or create an adjacency under `dir`, with files named `<name>.rec` and
    /// `<name>.idx`. Two of these make a graph: one forward, one reverse.
    pub(crate) fn open(dir: &Path, name: &str, page_size: usize) -> io::Result<Self> {
        Ok(Self {
            store: PagedStore::open_named(dir, name, page_size)?,
            scratch: Vec::new(),
        })
    }

    /// Nodes that have at least one edge. Not the node count — a node with no
    /// edges has no entry here at all, which is why the offsets array CSR needs
    /// per node has no equivalent.
    pub(crate) fn owner_count(&self) -> u64 { self.store.len() }

    pub(crate) fn page_counts(&self) -> (u64, u64) { self.store.page_counts() }

    pub(crate) fn sync(&mut self) -> io::Result<()> { self.store.sync() }

    /// Every edge leaving `owner`, or `None` if it has none.
    ///
    /// One tree descent and one record read. The neighbour hashes are in the record
    /// already, so nothing further is looked up — which is the difference from CSR,
    /// where each neighbour costs a random read into `nodes.bin` to recover its
    /// hash from its dense id.
    pub(crate) fn edges(&self, owner: u64) -> io::Result<Option<Vec<AdjEdge>>> {
        Ok(self.store.get(owner as u128)?.map(|b| decode(&b)))
    }

    /// Add one edge. Duplicates are allowed: two nodes may be connected twice by
    /// different types, and the same type twice is the caller's business.
    pub(crate) fn add(&mut self, owner: u64, edge: AdjEdge) -> io::Result<()> {
        self.add_many(owner, std::slice::from_ref(&edge))
    }

    /// Add several edges to one owner in a single rewrite.
    ///
    /// The list is stored as one record, so adding edges one at a time rewrites it
    /// once per edge — O(d²) to build a node of degree d. Loading a graph should
    /// come through here instead, which makes it O(d).
    pub(crate) fn add_many(&mut self, owner: u64, edges: &[AdjEdge]) -> io::Result<()> {
        if edges.is_empty() { return Ok(()) }
        let mut list = match self.store.get(owner as u128)? {
            Some(b) => decode(&b),
            None => Vec::with_capacity(edges.len()),
        };
        list.extend_from_slice(edges);
        let mut scratch = std::mem::take(&mut self.scratch);
        encode(&list, &mut scratch);
        let r = self.store.put(owner as u128, &scratch);
        self.scratch = scratch;
        r
    }

    /// Drop the first edge from `owner` to `other`, optionally of a given type.
    ///
    /// Returns whether one was found. When the owner's last edge goes the record is
    /// deleted outright, so its space returns to the free list and the owner stops
    /// appearing in `owner_count` — a node that loses all its edges costs nothing,
    /// which a CSR offsets array cannot express.
    pub(crate) fn remove(&mut self, owner: u64, other: u64, edge_type: Option<u64>)
        -> io::Result<bool>
    {
        let Some(bytes) = self.store.get(owner as u128)? else { return Ok(false) };
        let mut list = decode(&bytes);
        let Some(at) = list.iter().position(|e| {
            e.other == other && edge_type.is_none_or(|t| e.edge_type == t)
        }) else { return Ok(false) };
        list.remove(at);
        if list.is_empty() {
            self.store.delete(owner as u128)?;
        } else {
            let mut scratch = std::mem::take(&mut self.scratch);
            encode(&list, &mut scratch);
            let r = self.store.put(owner as u128, &scratch);
            self.scratch = scratch;
            r?;
        }
        Ok(true)
    }

    /// Drop every edge belonging to `owner`. This is what deleting a node does to
    /// its own side of the graph.
    pub(crate) fn remove_owner(&mut self, owner: u64) -> io::Result<bool> {
        self.store.delete(owner as u128)
    }

    /// Drop every edge pointing *at* `other`, wherever it comes from.
    ///
    /// Deleting a node has to do this on the opposite direction's store, and there
    /// is no index from neighbour back to owner — so it is a scan. It is here
    /// rather than left to the caller because the alternative is worse: leaving
    /// dangling edges and filtering them out on every read, which is what "nothing
    /// that can be wrong about what exists may delete" is meant to prevent.
    ///
    /// The cost is named rather than hidden: O(edges) in time. It is *not* O(edges)
    /// in memory — the scan streams, and the only thing collected is the list of
    /// owners that actually point at the victim, which is its in-degree. Collecting
    /// every owner first would cost 24 bytes each with nothing bounding it.
    ///
    /// A delete-heavy workload should hold the opposite direction and call
    /// [`remove`](Self::remove) per known owner instead of scanning.
    pub(crate) fn remove_all_to(&mut self, other: u64) -> io::Result<usize> {
        let mut owners: Vec<u64> = Vec::new();
        let mut err = None;
        self.store.for_each_key(|key, _| {
            match self.store.get(key) {
                Ok(Some(bytes)) => {
                    if decode(&bytes).iter().any(|e| e.other == other) {
                        owners.push(key as u64);
                    }
                    true
                }
                Ok(None) => true,
                Err(e) => { err = Some(e); false }
            }
        })?;
        if let Some(e) = err { return Err(e) }

        let mut dropped = 0usize;
        for owner in owners {
            while self.remove(owner, other, None)? { dropped += 1 }
        }
        Ok(dropped)
    }

    /// Every owner that has edges, with its list, one at a time.
    ///
    /// Streams for the same reason as [`remove_all_to`](Self::remove_all_to):
    /// a graph's worth of edge lists does not fit in memory, and the point of the
    /// design is that it never has to. `f` returning `false` stops the walk.
    pub(crate) fn for_each_owner(&self, mut f: impl FnMut(u64, Vec<AdjEdge>) -> bool)
        -> io::Result<()>
    {
        let mut err = None;
        self.store.for_each_key(|key, _| {
            match self.store.get(key) {
                Ok(Some(bytes)) => f(key as u64, decode(&bytes)),
                Ok(None) => true,
                Err(e) => { err = Some(e); false }
            }
        })?;
        match err { Some(e) => Err(e), None => Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::pagestore::DEFAULT_PAGE_SIZE;
    use std::collections::HashMap;
    use std::time::Instant;

    fn store(dir: &tempfile::TempDir) -> AdjStore {
        AdjStore::open(dir.path(), "adj_fwd", DEFAULT_PAGE_SIZE).unwrap()
    }
    fn h(i: u64) -> u64 { i.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5851_F42D_4C95_7F2D }
    fn e(other: u64, t: u64) -> AdjEdge {
        AdjEdge { other: h(other), edge_type: t, meta_ref: NO_META }
    }

    #[test]
    fn edges_round_trip() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        for i in 0..2_000u64 {
            for j in 0..4u64 { s.add(h(i), e(i * 7 + j, 100 + j)).unwrap() }
        }
        assert_eq!(s.owner_count(), 2_000);
        for i in 0..2_000u64 {
            let got = s.edges(h(i)).unwrap().expect("owner lost its edges");
            assert_eq!(got.len(), 4, "owner {i}");
            for (j, edge) in got.iter().enumerate() {
                assert_eq!(*edge, e(i * 7 + j as u64, 100 + j as u64), "owner {i} edge {j}");
            }
        }
        assert_eq!(s.edges(h(999_999)).unwrap(), None, "a node with no edges has no record");
    }

    /// Order is the order edges were added. Callers filter by type and compare
    /// neighbour sets, so nothing depends on sort order — but it must not be
    /// *arbitrary*, or two runs of the same load disagree.
    #[test]
    fn insertion_order_is_preserved() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let order = [9u64, 3, 7, 1, 8, 2];
        for &n in &order { s.add(h(0), e(n, 1)).unwrap() }
        let got: Vec<u64> = s.edges(h(0)).unwrap().unwrap().iter().map(|e| e.other).collect();
        assert_eq!(got, order.iter().map(|&n| h(n)).collect::<Vec<_>>());
    }

    /// Two nodes connected twice by different types are two edges, not one.
    #[test]
    fn parallel_edges_of_different_types_both_survive() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        s.add(h(1), e(2, 10)).unwrap();
        s.add(h(1), e(2, 20)).unwrap();
        assert_eq!(s.edges(h(1)).unwrap().unwrap().len(), 2);

        assert!(s.remove(h(1), h(2), Some(10)).unwrap());
        let left = s.edges(h(1)).unwrap().unwrap();
        assert_eq!(left.len(), 1, "removing one type took both");
        assert_eq!(left[0].edge_type, 20, "removing a type took the wrong one");

        assert!(!s.remove(h(1), h(2), Some(10)).unwrap(), "removing twice reported success");
        assert!(s.remove(h(1), h(2), None).unwrap());
        assert_eq!(s.edges(h(1)).unwrap(), None, "an emptied owner still has a record");
        assert_eq!(s.owner_count(), 0);
    }

    /// Against a `HashMap` that cannot be wrong, over a mixed workload.
    #[test]
    fn a_mixed_workload_matches_an_oracle() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let mut oracle: HashMap<u64, Vec<AdjEdge>> = HashMap::new();

        for i in 0..8_000u64 {
            let owner = h(i % 900);
            let edge = e(i % 700, i % 5);
            s.add(owner, edge).unwrap();
            oracle.entry(owner).or_default().push(edge);

            if i % 3 == 0 {
                let victim = h((i * 13) % 900);
                let target = h((i * 29) % 700);
                let got = s.remove(victim, target, None).unwrap();
                let want = oracle.get_mut(&victim)
                    .and_then(|l| l.iter().position(|x| x.other == target).map(|at| { l.remove(at); }))
                    .is_some();
                assert_eq!(got, want, "remove disagreed at step {i}");
                if oracle.get(&victim).is_some_and(|l| l.is_empty()) { oracle.remove(&victim); }
            }
            if i % 500 == 0 {
                let victim = h((i * 7) % 900);
                s.remove_owner(victim).unwrap();
                oracle.remove(&victim);
            }
        }

        assert_eq!(s.owner_count() as usize, oracle.len(), "owner count drifted");
        for (owner, want) in &oracle {
            assert_eq!(s.edges(*owner).unwrap().as_ref(), Some(want), "owner {owner:x}");
        }
        for owner in [h(901), h(5_000), 0] {
            if !oracle.contains_key(&owner) {
                assert_eq!(s.edges(owner).unwrap(), None, "phantom edges for {owner:x}");
            }
        }
    }

    #[test]
    fn edges_survive_a_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        {
            let mut s = store(&dir);
            for i in 0..3_000u64 { s.add(h(i), e(i + 1, 7)).unwrap() }
            s.remove_owner(h(5)).unwrap();
            s.sync().unwrap();
        }
        let s = store(&dir);
        assert_eq!(s.owner_count(), 2_999, "owner count did not survive");
        assert_eq!(s.edges(h(9)).unwrap().unwrap()[0], e(10, 7));
        assert_eq!(s.edges(h(5)).unwrap(), None, "a removed owner came back");
    }

    /// Removing a node has to remove the edges pointing at it too, and the store
    /// has no index in that direction — so this scans. The test pins the behaviour,
    /// not the cost.
    #[test]
    fn edges_pointing_at_a_node_can_all_be_dropped() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        for i in 0..500u64 {
            s.add(h(i), e(i + 1, 1)).unwrap();
            if i % 5 == 0 { s.add(h(i), e(999, 2)).unwrap() }
        }
        let dropped = s.remove_all_to(h(999)).unwrap();
        assert_eq!(dropped, 100, "dropped {dropped} of the 100 edges pointing at the victim");
        for i in 0..500u64 {
            let list = s.edges(h(i)).unwrap().unwrap_or_default();
            assert!(list.iter().all(|e| e.other != h(999)), "owner {i} still points at the victim");
            assert_eq!(list.len(), 1, "owner {i} lost an unrelated edge");
        }
    }

    /// **The measurement the direction exists for.**
    ///
    /// CSR cannot take a new edge at all: one more neighbour on one node shifts
    /// every block after it, so edges accumulate in RAM until a fold rewrites the
    /// whole graph — cost proportional to the graph, not to the change.
    ///
    /// Here an edge touches one node's record. The assertion is about the *shape*
    /// of the cost: a constant factor is a tuning problem, growth with the store is
    /// a violated principle.
    #[test]
    fn adding_an_edge_costs_the_same_at_every_size() {
        let batch = 5_000u64;
        let mut timings = Vec::new();

        for &preload in &[20_000u64, 100_000, 400_000] {
            let dir = tempfile::TempDir::new().unwrap();
            let mut s = store(&dir);
            for i in 0..preload { s.add(h(i), e(i + 1, 1)).unwrap() }

            let t = Instant::now();
            for i in preload..preload + batch { s.add(h(i), e(i + 1, 1)).unwrap() }
            timings.push((preload, t.elapsed().as_secs_f64() * 1e6 / batch as f64));
        }

        for (preload, us) in &timings {
            println!("  {preload:>7} edges already stored → {us:.2} us per edge added");
        }
        let (smallest, largest) = (timings[0].1, timings[timings.len() - 1].1);
        assert!(
            largest < smallest * 3.0,
            "an edge costs {largest:.2} us on a 400k-edge graph against {smallest:.2} us \
             on a 20k one — the cost is tracking the size of the graph rather than the \
             size of the change, which is the failure this design exists to remove",
        );
    }

    /// The other named sacrifice: disk.
    ///
    /// CSR spends 13.6 bytes an edge per direction — 4 for the type id, 4 for the
    /// metadata ref, about 2.9 for a delta-coded neighbour, and 8 per *node* for
    /// the offsets array. Here an edge is a flat 20 bytes and there is no offsets
    /// array, but records sit in pages that are not perfectly full and the index
    /// costs an entry per owner.
    ///
    /// The estimate the design was chosen on was ~1.2x. **The real number is 2.3x**,
    /// and this test is where that was found. The gap is not in the records — 300k
    /// edges at 20 bytes fill 1564 pages, which is 1.07x the bytes they contain, so
    /// slotted pages are packing near perfectly. It is in the index: 518 pages for
    /// 50 000 owners is 42 bytes each, because a B+tree entry is a 16-byte key plus
    /// an 8-byte value and random keys leave leaves about 57% full.
    ///
    /// Two levers are known, both measured from the numbers above:
    ///
    /// - **The key is twice as wide as it needs to be.** The tree takes `u128` to
    ///   carry composite keys elsewhere; an owner hash is a `u64`. A narrow key
    ///   would make an entry 16 bytes rather than 24 — worth ~2.3 bytes an edge.
    /// - **The index should not exist.** Reaching a node's edges already requires
    ///   looking the node up, so its adjacency record id belongs *in the node
    ///   record*. That deletes all 7 bytes an edge this index costs and takes the
    ///   ratio to about 1.75x, and it is free once nodes move onto records.
    ///
    /// The assertion sits just above the measured number, so it fails on a
    /// regression rather than restating an intention.
    #[test]
    fn the_disk_cost_against_csr_is_what_it_was_chosen_on() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let (nodes, degree) = (50_000u64, 6u64);
        for i in 0..nodes {
            let batch: Vec<AdjEdge> = (0..degree).map(|j| e(i * 7 + j, j)).collect();
            s.add_many(h(i), &batch).unwrap();
        }
        s.sync().unwrap();

        let edges = (nodes * degree) as f64;
        let (rec_pages, idx_pages) = s.page_counts();
        let bytes = (rec_pages + idx_pages) * DEFAULT_PAGE_SIZE as u64;
        let per_edge = bytes as f64 / edges;
        // CSR, one direction, as measured in examples/topo_bytes.rs: 4 + 4 for the
        // fixed columns, 2.87 for the neighbour delta, 8/degree for the offsets.
        let csr = 4.0 + 4.0 + 2.87 + 8.0 / degree as f64;

        let rec_bytes = rec_pages * DEFAULT_PAGE_SIZE as u64;
        let idx_bytes = idx_pages * DEFAULT_PAGE_SIZE as u64;
        println!("  {nodes} owners x degree {degree}");
        println!("    records {rec_pages:>5} pages = {:.2} bytes/edge \
                  ({:.2}x the {EDGE_BYTES} bytes an edge holds)",
                 rec_bytes as f64 / edges, rec_bytes as f64 / edges / EDGE_BYTES as f64);
        println!("    index   {idx_pages:>5} pages = {:.2} bytes/edge \
                  ({:.0} bytes per owner)",
                 idx_bytes as f64 / edges, idx_bytes as f64 / nodes as f64);
        println!("    total   {per_edge:.2} bytes/edge against {csr:.2} for CSR \
                  → {:.2}x", per_edge / csr);
        assert!(per_edge / csr < 2.5,
                "{per_edge:.2} bytes an edge against CSR's {csr:.2} is {:.2}x, worse \
                 than the 2.3x measured when this layout was accepted",
                per_edge / csr);
        assert!(rec_bytes as f64 / edges / EDGE_BYTES as f64 > 0.95,
                "records are packing at {:.2}x the bytes they hold, which is under \
                 one — the arithmetic above is wrong somewhere",
                rec_bytes as f64 / edges / EDGE_BYTES as f64);
    }

    /// The named sacrifice, measured rather than asserted: one edge at a time
    /// rewrites the owner's whole list, so a high-degree node costs O(d²).
    /// `add_many` is the way out, and this checks it actually is one.
    #[test]
    fn a_high_degree_node_stays_usable() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut s = store(&dir);
        let d = 20_000u64;

        let t = Instant::now();
        for i in 0..d { s.add(h(1), e(i, 1)).unwrap() }
        let one_at_a_time = t.elapsed();

        let batched: Vec<AdjEdge> = (0..d).map(|i| e(i, 1)).collect();
        let t = Instant::now();
        s.add_many(h(2), &batched).unwrap();
        let in_one_go = t.elapsed();

        println!("  degree {d}: {:.0} ms one at a time, {:.1} ms via add_many",
                 one_at_a_time.as_secs_f64() * 1e3, in_one_go.as_secs_f64() * 1e3);
        assert_eq!(s.edges(h(1)).unwrap().unwrap().len() as u64, d);
        assert_eq!(s.edges(h(2)).unwrap().unwrap().len() as u64, d);
        assert!(in_one_go < one_at_a_time,
                "add_many ({in_one_go:?}) did not beat one-at-a-time ({one_at_a_time:?}), \
                 so the documented way out of the O(d^2) cost is not one");
    }
}
