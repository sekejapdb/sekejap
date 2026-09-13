//! The graph: a key discipline over the phase-1 store. No structure of its own.
//!
//! One hop = one descent + a sequential read of the degree (GRAPH.md). Levers:
//! dense sequential ids so co-inserted nodes share leaves; sorted-frontier BFS
//! so a wave is one ordered sweep instead of a descent per node; props in the
//! leaf so hybrid filters cost no extra I/O.
//!
//! Laws: streaming iterators (no Vec ∝ store); hop cost ∝ degree; the visited
//! set of a BFS ∝ the reachable set -- inherent to "never revisit", the one
//! named exception, bounded by the caller's depth.

use crate::keys;
use crate::store::Store;
use crate::Result;

/// Catalog field holding the id allocator's high-water mark.
const CAT_NEXT_ID: u64 = 1;
/// Catalog slot subdivided per vector field: `catalog_field(CAT_VEC, field)`
/// holds that field's [`VecMeta`].
const CAT_VEC: u64 = 2;
/// Sparse set of vector ids written at-or-below a field's nav watermark.
/// Ordinary increasing-id ingest writes no row here. Catalog placement is
/// deliberate: records after TAG_VCODE turn fingerprint appends into splits.
pub(crate) const CAT_NAV_PENDING: u64 = 3;

/// Everything one vector field needs to be read back correctly, in one row
/// (`keys::catalog_field(CAT_VEC, field)`). Nothing here is derivable from
/// the vectors:
///
/// - `dim` is the field's width; a row of another width is corruption at
///   birth, refused rather than discovered as a garbage distance later.
/// - `seed` and `bits` ARE the fingerprint recipe -- (dim, seed, bits) is
///   the whole of it, so any process, at any later date, re-derives the
///   encoder that wrote the codes rather than the one the code's defaults
///   happen to produce that day.
/// - `max` is the highest id that ever carried a vector for this field.
///   `nearest_par` partitions [1, max]; `next_id` is wrong for it when
///   vectors are set on caller-managed ids with no nodes (caught by the
///   parallel-vs-serial equality test: partitions covered almost nothing
///   and the test diverged).
/// - `medoid` and `watermark` are the nav tier's start point and fast resume
///   point. 0 means "no graph yet", which is a usable sentinel because the
///   id allocator reserves 0 and hands out nothing below 1. The watermark
///   rides each fold insert; ids written behind it are recorded separately
///   in `CAT_NAV_PENDING`, so a crash resumes from facts rather than ordering.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VecMeta {
    pub dim: u64,
    pub seed: u64,
    pub bits: u64,
    pub max: u64,
    pub medoid: u64,
    pub watermark: u64,
}

impl VecMeta {
    /// Six big-endian words. Fixed width: a short row is a row this build
    /// cannot read, and reading it half-way would hand out a recipe that
    /// decodes every code in the field wrongly.
    pub const LEN: usize = 48;

    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(Self::LEN);
        for w in [self.dim, self.seed, self.bits, self.max, self.medoid, self.watermark] {
            v.extend_from_slice(&w.to_be_bytes());
        }
        v
    }

    pub fn decode(b: &[u8]) -> Option<VecMeta> {
        if b.len() != Self::LEN { return None; }
        let w = |i: usize| u64::from_be_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        Some(VecMeta { dim: w(0), seed: w(1), bits: w(2), max: w(3), medoid: w(4), watermark: w(5) })
    }
}

pub struct Graph {
    store: Store,
    next_id: u64,
    /// False when the durable allocator row is missing from a non-empty node
    /// keyspace or has any invalid encoding. Reads remain available, but no
    /// mutation may invent a replacement high-water mark.
    allocator_verified: bool,
    /// One entry per vector field, read whole at open. Bounded by the
    /// number of DECLARED vector fields, never by the vectors in them.
    vec_meta: std::collections::HashMap<u64, VecMeta>,
    /// Fields whose `max` moved since the last commit. Deferring that one
    /// word is why a vector write is two row writes and not three.
    vec_dirty: std::collections::HashSet<u64>,
    /// fold_nav checkpoints every this-many inserts (WAL bound; tests tune it)
    pub(crate) nav_fold_every: u64,
    /// Prepared encoders, one per field, built on first use. The sign
    /// vectors and level table are fixed per (dim, seed, bits), so they are
    /// computed once per field and never per vector.
    encoders: std::cell::RefCell<std::collections::HashMap<u64, std::sync::Arc<crate::vecquant::Encoder>>>,
    /// Mirror `0x04` keys so in_edges is a seek, not a scan of every edge.
    /// SACRIFICE (Law 4): one extra key per edge (+57% storage measured for
    /// marker edges in a prior build). Off = in_edges unsupported.
    pub redge: bool,
}

impl Graph {
    pub fn new(store: Store) -> Result<Graph> {
        let counter = store.get(&keys::catalog(CAT_NEXT_ID))?;
        let (next_id, allocator_verified) = match counter {
            Some(v) if v.len() == 8 => {
                let next = u64::from_be_bytes(v.try_into().unwrap());
                (next, next >= 1)
            }
            Some(_) => (0, false),
            None => {
                // Starting at one is derived only from a verified empty node
                // keyspace. If any node exists, absence is damage and the
                // allocator stays disabled rather than reusing a live id.
                let mut rows = store.scan(&[keys::TAG_NODE])?;
                let empty = match rows.next() {
                    None => true,
                    Some(Ok((key, _))) => key.first() != Some(&keys::TAG_NODE),
                    Some(Err(e)) => return Err(e),
                };
                if empty { (1, true) } else { (0, false) }
            }
        };
        // The whole vector catalogue in one prefix scan: a handful of rows,
        // one per declared field.
        let mut vec_meta = std::collections::HashMap::new();
        let vpre = keys::catalog_field_prefix(CAT_VEC);
        store.scan(&vpre)?.for_each_ref(|k, v| {
            if !k.starts_with(&vpre) { return false; }
            if k.len() == 17 {
                if let Some(m) = VecMeta::decode(v) { vec_meta.insert(keys::u64_at(k, 9), m); }
            }
            true
        })?;
        Ok(Graph { store, next_id, allocator_verified, vec_meta, vec_dirty: Default::default(),
                   nav_fold_every: crate::nav::NAV_FOLD_CHECKPOINT_EVERY,
                   encoders: Default::default(), redge: true })
    }

    pub fn store(&mut self) -> &mut Store { &mut self.store }
    pub fn store_ref(&self) -> &Store { &self.store }

    /// Persist the allocator then commit. The counter rides the same commit as
    /// the data it numbers, so a replayed crash can never hand out an id twice.
    pub fn commit(&mut self) -> Result<()> {
        if !self.allocator_verified {
            return Err(crate::Error::Corrupt {
                page_no: 0,
                why: "id allocator counter is missing or malformed",
            });
        }
        self.store.put(&keys::catalog(CAT_NEXT_ID), &self.next_id.to_be_bytes())?;
        for field in std::mem::take(&mut self.vec_dirty) {
            if let Some(m) = self.vec_meta.get(&field).copied() {
                self.store.put(&keys::catalog_field(CAT_VEC, field), &m.encode())?;
            }
        }
        self.store.commit()
    }

    pub fn checkpoint(&mut self) -> Result<()> { self.store.checkpoint() }

    /// Tune how often fold_nav checkpoints (WAL disk bound; see nav.rs).
    pub fn set_nav_fold_interval(&mut self, every: u64) { self.nav_fold_every = every.max(1); }

    /// Dense sequential id: the id policy IS the clustering policy (GRAPH.md
    /// lever 1). `ext` is the caller's uuid/slug, resolved later by
    /// [`resolve`]; stored WITH the id so a hash collision is detected, not
    /// silently wrong.
    pub fn add_node(&mut self, ext: Option<&[u8]>, label: u64, props: &[u8]) -> Result<u64> {
        if !self.allocator_verified {
            return Err(crate::Error::Corrupt {
                page_no: 0,
                why: "id allocator counter is missing or malformed",
            });
        }
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or(crate::Error::Corrupt {
            page_no: 0,
            why: "id allocator counter is exhausted",
        })?;
        let mut v = Vec::with_capacity(8 + props.len());
        v.extend_from_slice(&label.to_be_bytes());
        v.extend_from_slice(props);
        self.store.put(&keys::node(id), &v)?;
        self.store.put(&keys::label(label, id), &[])?;
        if let Some(e) = ext {
            let mut ev = Vec::with_capacity(8 + e.len());
            ev.extend_from_slice(&id.to_be_bytes());
            ev.extend_from_slice(e);
            self.store.put(&keys::extkey(keys::ext_hash(e)), &ev)?;
        }
        Ok(id)
    }

    /// External uuid/slug -> id. One point read, paid at query entry, never
    /// per hop.
    pub fn resolve(&self, ext: &[u8]) -> Result<Option<u64>> {
        Ok(match self.store.get(&keys::extkey(keys::ext_hash(ext)))? {
            Some(v) if v.len() >= 8 && &v[8..] == ext => {
                Some(u64::from_be_bytes(v[..8].try_into().unwrap()))
            }
            _ => None,
        })
    }

    pub fn get_node(&self, id: u64) -> Result<Option<(u64, Vec<u8>)>> {
        let Some(v) = self.store.get(&keys::node(id))? else { return Ok(None); };
        let label = v.get(..8).ok_or(crate::Error::Corrupt {
            page_no: 0,
            why: "node row is shorter than its eight-byte label",
        })?;
        Ok(Some((u64::from_be_bytes(label.try_into().unwrap()), v[8..].to_vec())))
    }

    /// `ctx` = perspective / named graph; 0 = the base graph. Edge identity is
    /// the full key, so re-asserting within a ctx overwrites (set semantics)
    /// and two ctxs never collide.
    pub fn add_edge(&mut self, ctx: u64, src: u64, ty: u64, dst: u64, props: &[u8]) -> Result<()> {
        self.store.put(&keys::edge(ctx, src, ty, dst), props)?;
        if self.redge {
            self.store.put(&keys::redge(ctx, dst, ty, src), &[])?;
        }
        Ok(())
    }

    /// Out-edges, streamed: one seek then sequential. `ty` narrows the RANGE
    /// (ty sits between src and dst in the key), it does not filter.
    pub fn out_edges(&self, ctx: u64, src: u64, ty: Option<u64>) -> Result<EdgeIter<'_>> {
        let prefix = match ty {
            Some(t) => keys::edge_type_prefix(ctx, src, t),
            None => keys::edge_prefix(ctx, src),
        };
        Ok(EdgeIter { inner: self.store.scan(&prefix)?, prefix, rev: false, base: ctx == 0 })
    }

    pub fn in_edges(&self, ctx: u64, dst: u64, ty: Option<u64>) -> Result<EdgeIter<'_>> {
        assert!(self.redge, "in_edges requires the redge keyspace");
        let prefix = match ty {
            Some(t) => keys::redge_type_prefix(ctx, dst, t),
            None => keys::redge_prefix(ctx, dst),
        };
        Ok(EdgeIter { inner: self.store.scan(&prefix)?, prefix, rev: true, base: ctx == 0 })
    }

    /// Every node of a label, streamed off the `0x02` range.
    pub fn nodes_with_label(&self, l: u64) -> Result<LabelIter<'_>> {
        let prefix = keys::label_prefix(l);
        Ok(LabelIter { inner: self.store.scan(&prefix)?, prefix })
    }

    /// Index one property value for a node. 0x0A | prop | value | id.
    ///
    /// The caller chooses WHICH properties to index and encodes values with the
    /// order-preserving encoders in `keys` -- this is CREATE INDEX as a write
    /// discipline rather than a schema. Blind write, like everything else.
    pub fn set_prop(&mut self, prop: u64, value: u64, id: u64) -> Result<()> {
        self.store.put(&keys::prop(prop, value, id), &[])
    }

    /// Change an indexed value. The caller supplies the OLD encoding, so the
    /// stale entry is retracted without a read -- supplying it wrongly leaves a
    /// stale index entry pointing at this node (Law 4: the price of keeping
    /// writes blind; a scan re-checking the record would mask it, and 2b keeps
    /// index entries authoritative instead).
    pub fn update_prop(&mut self, prop: u64, old: u64, new: u64, id: u64) -> Result<()> {
        self.store.delete(&keys::prop(prop, old, id))?;
        self.store.put(&keys::prop(prop, new, id), &[])
    }

    /// All ids whose `prop` value lies in [lo, hi], streamed in value order.
    pub fn prop_range(&self, prop: u64, lo: u64, hi: u64) -> Result<PropIter<'_>> {
        let from = keys::prop_value_prefix(prop, lo);
        Ok(PropIter { inner: self.store.scan(&from)?, prop, hi })
    }

    /// Equality on an indexed value: the (prop, value) prefix.
    pub fn prop_eq(&self, prop: u64, value: u64) -> Result<PropIter<'_>> {
        self.prop_range(prop, value, value)
    }

    /// Count a label's members without allocating per entry.
    pub fn count_label(&self, l: u64) -> Result<usize> {
        let prefix = keys::label_prefix(l);
        let mut n = 0;
        self.store.scan(&prefix)?.for_each_ref(|k, _| {
            if k.len() == 17 && k.starts_with(&prefix) { n += 1; true } else { false }
        })?;
        Ok(n)
    }

    /// Count ids whose `prop` value lies in [lo, hi], zero allocations.
    pub fn count_prop_range(&self, prop: u64, lo: u64, hi: u64) -> Result<usize> {
        let from = keys::prop_value_prefix(prop, lo);
        let mut n = 0;
        self.store.scan(&from)?.for_each_ref(|k, _| {
            if k.len() == 25 && k[0] == keys::TAG_PROP && keys::u64_at(&k, 1) == prop
                && keys::u64_at(&k, 9) <= hi { n += 1; true } else { false }
        })?;
        Ok(n)
    }

    // ---- vectors (2e) ------------------------------------------------------

    /// The rotation seed a field of width `dim` gets at its first vector.
    /// Mixed from the dim so it is deterministic without being one constant
    /// everywhere (a rotation bug identical across stores would otherwise be
    /// invisible to cross-store comparison).
    pub fn vec_seed(dim: u64) -> u64 {
        0xC0FF_EE00_2600_u64 ^ dim.wrapping_mul(0x9E37_79B9_7F4A_7C15)
    }

    /// This field's recipe, or `None` until its first vector arrives.
    pub fn vec_meta(&self, field: u64) -> Option<VecMeta> { self.vec_meta.get(&field).copied() }

    /// Install a precomputed vector recipe/high-water row. Bulk import uses
    /// the same metadata path as live writes after generating dock rows with
    /// [`vec_dock_rows`](Self::vec_dock_rows).
    pub fn set_vec_meta(&mut self, field: u64, m: VecMeta) -> Result<()> {
        self.vec_meta.insert(field, m);
        self.vec_dirty.remove(&field);
        self.store.put(&keys::catalog_field(CAT_VEC, field), &m.encode())
    }

    /// Drop the derived Vamana graph while preserving the vectors and their
    /// scan-tier fingerprints. `DROP INDEX` removes an access path, not the
    /// values stored in the table; a later build starts again at watermark 0.
    pub fn clear_nav(&mut self, field: u64) -> Result<()> {
        self.store.delete_prefix(&keys::nav_prefix(field))?;
        self.store.delete_prefix(&keys::catalog_field(CAT_NAV_PENDING, field))?;
        if let Some(mut meta) = self.vec_meta.get(&field).copied() {
            meta.medoid = 0;
            meta.watermark = 0;
            self.set_vec_meta(field, meta)?;
        }
        Ok(())
    }

    /// Store a node's embedding under `field`. The FIRST vector written to a
    /// field fixes THAT FIELD's dimension and recipe; every later write to it
    /// must match. A 1536-dim row landing in a 768-dim field is corruption at
    /// birth, refused here rather than discovered as a garbage distance
    /// later -- and a 768-dim field is no longer the store's business, so a
    /// second field of a different width is not a conflict at all.
    pub fn set_vec(&mut self, field: u64, id: u64, v: &[f32]) -> Result<()> {
        let dim = v.len() as u64;
        let meta = match self.vec_meta.get(&field).copied() {
            Some(m) if m.dim != dim => return Err(crate::Error::TooLarge), // dim mismatch
            Some(m) => m,
            None => {
                let m = VecMeta { dim, seed: Self::vec_seed(dim),
                                  bits: crate::vecquant::DEFAULT_BITS as u64,
                                  ..Default::default() };
                self.set_vec_meta(field, m)?;
                m
            }
        };
        let mut bytes = Vec::with_capacity(v.len() * 4);
        for x in v {
            bytes.extend_from_slice(&x.to_le_bytes());
        }
        self.store.put(&keys::vec_key(field, id), &bytes)?;
        // The fingerprint row (2g): ~dim/2 bytes beside a dim*4-byte vector.
        // Written blind on the same path -- the similarity index is simply
        // always current; there is no build step to run or forget.
        let enc = self.encoder(field).expect("recipe was just established");
        let (norm, code) = enc.encode(v);
        let mut row = Vec::with_capacity(4 + code.len());
        row.extend_from_slice(&norm.to_le_bytes());
        row.extend_from_slice(&code);
        if id > meta.max {
            self.vec_meta.entry(field).and_modify(|m| m.max = id);
            self.vec_dirty.insert(field);
        }
        self.store.put(&keys::vcode_key(field, id), &row)?;
        // The watermark remains the zero-overhead fast path for ordinary
        // increasing ids. Anything behind it needs a durable work marker:
        // it is both query-visible before a fold and consumed by that fold.
        if id <= meta.watermark {
            self.store.put(&keys::catalog_field_item(CAT_NAV_PENDING, field, id), &[])?;
        }
        Ok(())
    }

    /// Remove a vector AND its fingerprint. Both deletes ride the same
    /// commit: replay after a crash removes both or neither.
    pub fn delete_vec(&mut self, field: u64, id: u64) -> Result<bool> {
        let had = self.store.delete(&keys::vec_key(field, id))?;
        self.store.delete(&keys::vcode_key(field, id))?;
        self.store.delete(&keys::catalog_field_item(CAT_NAV_PENDING, field, id))?;
        Ok(had)
    }

    /// The dock helper (2g.2): the two rows a bulk load must emit per
    /// vector, so no caller can create unsearchable vectors by forgetting
    /// the fingerprint. Feed the flattened pairs to `Store::bulk_load`.
    /// `enc` must be the encoder of `field`'s recipe -- see
    /// [`Graph::vec_meta_row`], which emits the row that records it.
    pub fn vec_dock_rows(enc: &crate::vecquant::Encoder, field: u64, id: u64, v: &[f32])
        -> [(Vec<u8>, Vec<u8>); 2]
    {
        let mut bytes = Vec::with_capacity(v.len() * 4);
        for x in v { bytes.extend_from_slice(&x.to_le_bytes()); }
        let (norm, code) = enc.encode(v);
        let mut row = Vec::with_capacity(4 + code.len());
        row.extend_from_slice(&norm.to_le_bytes());
        row.extend_from_slice(&code);
        [(keys::vec_key(field, id), bytes), (keys::vcode_key(field, id), row)]
    }

    /// The recipe row a bulk load must emit beside its dock rows. Without
    /// it the codes exist and nothing can read them.
    pub fn vec_meta_row(field: u64, m: VecMeta) -> (Vec<u8>, Vec<u8>) {
        (keys::catalog_field(CAT_VEC, field), m.encode())
    }

    pub fn get_vec(&self, field: u64, id: u64) -> Result<Option<Vec<f32>>> {
        self.store.get(&keys::vec_key(field, id))?.map(decode_f32s).transpose()
    }

    /// Distance to one stored vector. The scan callback borrows the record's
    /// bytes from the pinned leaf (or the iterator's one overflow buffer), and
    /// distance decodes f32 lanes directly from that slice (D23).
    pub fn vec_distance(&self, field: u64, id: u64, query: &[f32], metric: Metric)
        -> Result<Option<f32>>
    {
        let key = keys::vec_key(field, id);
        let mut found = None;
        let mut malformed = false;
        self.store.scan(&key)?.for_each_ref(|candidate, bytes| {
            if candidate != key.as_slice() { return false; }
            if bytes.len() != query.len() * 4 {
                malformed = true;
            } else {
                found = Some(metric.distance_bytes(bytes, query));
            }
            false
        })?;
        if malformed { return Err(crate::Error::TooLarge); }
        Ok(found)
    }

    /// A field's width, 0 when it holds no vectors yet.
    pub fn vec_dim(&self, field: u64) -> u64 { self.vec_meta.get(&field).map_or(0, |m| m.dim) }
    pub fn vec_bits_pub(&self, field: u64) -> usize {
        self.vec_meta.get(&field).map_or(crate::vecquant::DEFAULT_BITS, |m| m.bits as usize)
    }
    /// The prepared encoder for `field`, built once and shared. `None`
    /// when the field has no recipe: there is nothing to encode against,
    /// and guessing one would produce codes nothing can decode.
    pub(crate) fn encoder(&self, field: u64) -> Option<std::sync::Arc<crate::vecquant::Encoder>> {
        if let Some(e) = self.encoders.borrow().get(&field) { return Some(e.clone()); }
        let m = self.vec_meta.get(&field).copied()?;
        let e = std::sync::Arc::new(
            crate::vecquant::Encoder::new(m.dim as usize, m.seed, m.bits as usize));
        self.encoders.borrow_mut().insert(field, e.clone());
        Some(e)
    }

    /// Rank `candidates` by distance to `query`, best `k` first.
    ///
    /// This IS the vector story (D20): traversal/labels/props FIND candidates,
    /// this ranks them. Heap holds k entries, never the candidate count; each
    /// candidate costs one point read. A missing vector skips the candidate
    /// rather than failing the query -- RCA nodes without embeddings are
    /// normal, not errors.
    pub fn rescore(
        &self,
        field: u64,
        candidates: &[u64],
        query: &[f32],
        metric: Metric,
        k: usize,
    ) -> Result<Vec<(u64, f32)>> {
        if k == 0 { return Ok(Vec::new()); }
        let mut heap: std::collections::BinaryHeap<Scored> = std::collections::BinaryHeap::new();
        for &id in candidates {
            let Some(d) = self.vec_distance(field, id, query, metric)? else { continue };
            offer_score(&mut heap, Scored { d, id }, k);
        }
        let mut out: Vec<(u64, f32)> = heap.into_iter().map(|s| (s.id, s.d)).collect();
        out.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        Ok(out)
    }

    /// Exact whole-field rescore with a `k`-bounded heap. Vector rows are one
    /// contiguous prefix, so this is one sequential cursor and retains neither
    /// the vectors nor the candidate set (D23, Law 1).
    pub fn rescore_all(&self, field: u64, query: &[f32], metric: Metric, k: usize)
        -> Result<Vec<(u64, f32)>>
    {
        if k == 0 { return Ok(Vec::new()); }
        let prefix = keys::vec_prefix(field);
        let query_norm = metric.query_norm(query);
        let mut heap: std::collections::BinaryHeap<Scored> = std::collections::BinaryHeap::new();
        let mut malformed = false;
        self.store.scan(&prefix)?.for_each_ref(|key, bytes| {
            if !key.starts_with(&prefix) { return false; }
            if key.len() != 17 || bytes.len() != query.len() * 4 {
                malformed = true;
                return false;
            }
            let id = keys::u64_at(key, 9);
            let d = metric.distance_bytes_prepared(bytes, query, query_norm);
            offer_score(&mut heap, Scored { d, id }, k);
            true
        })?;
        if malformed { return Err(crate::Error::TooLarge); }
        let mut out: Vec<(u64, f32)> = heap.into_iter().map(|s| (s.id, s.d)).collect();
        out.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        Ok(out)
    }

    /// Rescore an id-sorted candidate slice with one sequential vector cursor.
    /// The caller's candidate slice is the query's existing working set; this
    /// method adds only the `k`-entry heap and one cursor (D23).
    pub fn rescore_sorted(&self, field: u64, candidates: &[u64], query: &[f32],
                          metric: Metric, k: usize) -> Result<Vec<(u64, f32)>> {
        if k == 0 || candidates.is_empty() { return Ok(Vec::new()); }
        debug_assert!(candidates.windows(2).all(|pair| pair[0] <= pair[1]));
        let prefix = keys::vec_prefix(field);
        let query_norm = metric.query_norm(query);
        let mut at = 0usize;
        let mut heap: std::collections::BinaryHeap<Scored> = std::collections::BinaryHeap::new();
        let mut malformed = false;
        self.store.scan(&prefix)?.for_each_ref(|key, bytes| {
            if !key.starts_with(&prefix) || at == candidates.len() { return false; }
            if key.len() != 17 { malformed = true; return false; }
            let id = keys::u64_at(key, 9);
            while at < candidates.len() && candidates[at] < id { at += 1; }
            if at == candidates.len() { return false; }
            if candidates[at] != id { return true; }
            while at + 1 < candidates.len() && candidates[at + 1] == id { at += 1; }
            at += 1;
            if bytes.len() != query.len() * 4 { malformed = true; return false; }
            offer_score(&mut heap, Scored {
                d: metric.distance_bytes_prepared(bytes, query, query_norm), id
            }, k);
            true
        })?;
        if malformed { return Err(crate::Error::TooLarge); }
        let mut out: Vec<(u64, f32)> = heap.into_iter().map(|score| (score.id, score.d)).collect();
        out.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        Ok(out)
    }

    /// Score an id-sorted slice in one cursor, for expressions that genuinely
    /// need one score per output candidate rather than top-k.
    pub fn distances_sorted(&self, field: u64, candidates: &[u64], query: &[f32], metric: Metric)
        -> Result<Vec<(u64, f32)>>
    {
        if candidates.is_empty() { return Ok(Vec::new()); }
        debug_assert!(candidates.windows(2).all(|pair| pair[0] <= pair[1]));
        let prefix = keys::vec_prefix(field);
        let query_norm = metric.query_norm(query);
        let mut at = 0usize;
        let mut out = Vec::with_capacity(candidates.len());
        let mut malformed = false;
        self.store.scan(&prefix)?.for_each_ref(|key, bytes| {
            if !key.starts_with(&prefix) || at == candidates.len() { return false; }
            if key.len() != 17 { malformed = true; return false; }
            let id = keys::u64_at(key, 9);
            while at < candidates.len() && candidates[at] < id { at += 1; }
            if at == candidates.len() { return false; }
            if candidates[at] != id { return true; }
            if bytes.len() != query.len() * 4 { malformed = true; return false; }
            out.push((id, metric.distance_bytes_prepared(bytes, query, query_norm)));
            while at < candidates.len() && candidates[at] == id { at += 1; }
            true
        })?;
        if malformed { return Err(crate::Error::TooLarge); }
        Ok(out)
    }

    /// Vector-first search (2g): the nearest `k` ids to `q` across the WHOLE
    /// store, no prior candidate set. Two stages: (1) scan the fingerprint
    /// keyspace, scoring every code against the pre-rotated query -- a
    /// bounded heap keeps only `k * oversample` candidates; (2) exact
    /// rescore (D23) of those candidates against the full f32 rows. The
    /// approximation can therefore only ever MISS a true neighbour, never
    /// misrank one it found; recall is measured, not assumed. RAM: the heap
    /// and one rotated query -- never the store (Law 1).
    pub fn nearest(&self, field: u64, q: &[f32], k: usize, metric: Metric, oversample: usize)
        -> Result<Vec<(u64, f32)>>
    {
        let Some(meta) = self.vec_meta.get(&field).copied() else {
            return Err(crate::Error::TooLarge);
        };
        if meta.dim == 0 || q.len() as u64 != meta.dim {
            return Err(crate::Error::TooLarge);
        }
        let enc = self.encoder(field).expect("a field with a recipe has an encoder");
        let bits = meta.bits as usize;
        let expected_code_len = 4usize
            .checked_add(crate::vecquant::code_len(crate::vecquant::pad_dim(meta.dim as usize), bits))
            .ok_or(crate::Error::Corrupt {
                page_no: 0,
                why: "vector code row length overflows",
            })?;
        let aq = enc.affine_query(q);
        // L1's scan stage ranks by the L2 estimate (rotation preserves L2,
        // not L1, so no honest code-space L1 exists); the looser proxy gets
        // a 4x wider candidate pool, and the exact rescore restores the
        // metric. Measured: recall 0.775 -> above floor at the same k.
        let proxy_boost = if matches!(metric, Metric::L1) { 4 } else { 1 };
        let cap = k.saturating_mul(oversample).saturating_mul(proxy_boost).max(k);

        let mut heap: std::collections::BinaryHeap<Scored> = std::collections::BinaryHeap::new();
        // ONE field's codes: the prefix is where the scan starts AND where
        // it stops, so a neighbouring field's codes -- a different width,
        // a different recipe -- are never fed to this decoder.
        let prefix = crate::keys::vcode_prefix(field);
        let it = self.store.scan(&prefix)?;
        let mut malformed = false;
        it.for_each_ref(|key, val| {
            // fixed-width identity check, not a slice compare: this runs once
            // per stored code
            if key.len() != 17 {
                malformed = key.starts_with(&prefix);
                return false;
            }
            if key[0] != crate::keys::TAG_VCODE || crate::keys::u64_at(key, 1) != field {
                return false; // left the field's range
            }
            if val.len() != expected_code_len {
                malformed = true;
                return false;
            }
            let id = crate::keys::u64_at(key, 9);
            let norm = f32::from_le_bytes(val[0..4].try_into().unwrap());
            let dot = if bits == 2 {
                crate::vecquant::dot_est_affine2(norm, &val[4..], &aq)
            } else {
                crate::vecquant::dot_est_affine(norm, &val[4..], &aq)
            };
            // One estimated dot serves every metric; constant terms that do
            // not change the ORDER (|q|^2 for L2, |q| for cosine) are dropped.
            // L1 has no affine decomposition from these codes; its scan
            // stage ranks by the L2 estimate (same neighbourhoods on dense
            // data -- the recall gate measures the truth of that) and the
            // rescore stage applies EXACT L1. Named approximation (L4).
            let d = match metric {
                Metric::L2 | Metric::L1 => norm * norm - 2.0 * dot,
                Metric::Dot => -dot,
                Metric::Cosine => if norm > 0.0 { -dot / norm } else { 0.0 },
            };
            heap.push(Scored { d, id });
            if heap.len() > cap { heap.pop(); }
            true
        })?;
        if malformed {
            return Err(crate::Error::Corrupt {
                page_no: 0,
                why: "vector code row has an invalid length",
            });
        }

        let cands: Vec<u64> = heap.into_iter().map(|s| s.id).collect();
        self.rescore(field, &cands, q, metric, k)
    }

    /// `nearest`, fanned across `threads` snapshot readers (2g.2): the code
    /// keyspace is split into contiguous id ranges (ids are dense, D13);
    /// each thread opens its OWN read-only snapshot (Law 6 machinery -- no
    /// locks, no shared pool) and scans its slice into a bounded heap; the
    /// merged survivors are exact-rescored here. Visibility: the last
    /// PUBLISHED generation (snapshot semantics), where single-threaded
    /// `nearest` sees the writer's own uncheckpointed tail too.
    pub fn nearest_par(&self, field: u64, q: &[f32], k: usize, metric: Metric, oversample: usize,
                       threads: usize) -> Result<Vec<(u64, f32)>>
    {
        if threads <= 1 { return self.nearest(field, q, k, metric, oversample); }
        let mut ps = self.parallel_searcher(field, threads)?;
        ps.nearest(q, k, metric, oversample)
    }

    /// Build a reusable parallel searcher over the CURRENT published
    /// generation. Readers are opened once and reused across queries --
    /// opening per query re-pays pool warmup every time (measured: slower
    /// than serial). Rebuild after a checkpoint if freshness matters.
    pub fn parallel_searcher(&self, field: u64, threads: usize) -> Result<ParallelSearcher> {
        let hi_id = self.vec_meta.get(&field).map_or(0, |m| m.max).max(self.next_id).max(1);
        let chunk = hi_id.div_ceil(threads as u64).max(1);
        let mut readers = Vec::new();
        for t in 0..threads as u64 {
            let (lo, hi) = (1 + t * chunk, (1 + (t + 1) * chunk).min(hi_id + 1));
            if lo >= hi { break; }
            let s = crate::store::Store::open_snapshot(self.store.dir(), crate::store::Config::default())?;
            readers.push((Graph::new(s)?, lo, hi));
        }
        Ok(ParallelSearcher { field, readers })
    }

    fn geo_rows(field: u64, id: u64, g: &crate::spatial::Geom)
        -> Result<(Vec<Vec<u8>>, Vec<u8>, Vec<u8>)>
    {
        use crate::spatial as sp;
        let Some((xmin, xmax, ymin, ymax)) = g.bbox() else {
            return Err(crate::Error::TooLarge); // empty geometry refused
        };
        // PostGIS refuses out-of-range geography coordinates; so do we --
        // BEFORE anything reaches the WAL. (Vincenty fed a 145-degree
        // "latitude" returned 0.0 through its coincident-point guard: an
        // invalid write must never be able to place a geometry everywhere.)
        if !(xmin >= -180.0 && xmax <= 180.0 && ymin >= -90.0 && ymax <= 90.0)
            || !xmin.is_finite() || !xmax.is_finite()
            || !ymin.is_finite() || !ymax.is_finite() {
            return Err(crate::Error::TooLarge);
        }
        let bx = sp::BoxF::from_f64(xmin, xmax, ymin, ymax);
        let (level, cells) = match sp::cover_cells(xmin, xmax, ymin, ymax,
                                                   sp::LEVEL_FINE, sp::MAX_CELLS) {
            Some(c) => (sp::LEVEL_FINE, c),
            None => match sp::cover_cells(xmin, xmax, ymin, ymax,
                                          sp::LEVEL_COARSE, sp::MAX_CELLS) {
                Some(c) => (sp::LEVEL_COARSE, c),
                // continent-scale: one world-bucket posting (see LEVEL_WORLD)
                None => (sp::LEVEL_WORLD, vec![(0, 0)]),
            }
        };
        let new_keys: Vec<Vec<u8>> = cells.into_iter().map(|(cx, cy)| {
            let h = sp::cell_hilbert(cx, cy, level);
            keys::spat_key(field, level, h, id)
        }).collect();
        Ok((new_keys, bx.encode().to_vec(), g.encode()))
    }

    /// Initial-build fast path. The caller has established that this field has
    /// no trusted old index, so there is nothing to read, verify, or delete:
    /// these are ordinary blind kernel writes (D10) and retain one geometry.
    pub fn insert_geo_new(&mut self, field: u64, id: u64, g: &crate::spatial::Geom) -> Result<()> {
        let (new_keys, encoded_box, encoded_geom) = Self::geo_rows(field, id, g)?;
        for key in &new_keys { self.store.put(key, &encoded_box)?; }
        self.store.put(&keys::geom_key(field, id), &encoded_geom)?;
        Ok(())
    }

    /// SQL rows already own the exact GeoJSON payload. Its spatial index only
    /// needs Hilbert postings + outward bbox; duplicating the geometry would
    /// add a second write and a second durable copy for no read-path benefit.
    pub fn insert_geo_postings_new(&mut self, field: u64, id: u64,
                                   g: &crate::spatial::Geom) -> Result<()> {
        let (new_keys, encoded_box, _) = Self::geo_rows(field, id, g)?;
        for key in &new_keys { self.store.put(key, &encoded_box)?; }
        Ok(())
    }

    /// Pure build-side lowering for a SQL spatial index. CREATE INDEX can
    /// sort and pack these rows without invoking the live one-key-at-a-time
    /// maintenance path; values are the same outward-rounded boxes queried by
    /// `geo_candidates`.
    pub fn geo_posting_rows(field: u64, id: u64, g: &crate::spatial::Geom)
        -> Result<Vec<(Vec<u8>, Vec<u8>)>>
    {
        let (keys, bbox, _) = Self::geo_rows(field, id, g)?;
        Ok(keys.into_iter().map(|key| (key, bbox.clone())).collect())
    }

    fn possible_geo_keys(field: u64, id: u64, g: &crate::spatial::Geom) -> Vec<Vec<u8>> {
        use crate::spatial as sp;
        let mut old_keys = Vec::new();
        if let Some((xmin, xmax, ymin, ymax)) = g.bbox() {
            for level in [sp::LEVEL_FINE, sp::LEVEL_COARSE] {
                if let Some(cells) = sp::cover_cells(xmin, xmax, ymin, ymax, level, sp::MAX_CELLS) {
                    old_keys.extend(cells.into_iter().map(|(cx, cy)| {
                        keys::spat_key(field, level, sp::cell_hilbert(cx, cy, level), id)
                    }));
                }
            }
            old_keys.push(keys::spat_key(field, sp::LEVEL_WORLD, 0, id));
        }
        old_keys
    }

    /// Maintain a posting-only SQL spatial index. The old geometry comes from
    /// the row-change record, so no private copy or read-before-write is needed.
    pub fn replace_geo_postings(&mut self, field: u64, id: u64,
                                old: Option<&crate::spatial::Geom>,
                                new: Option<&crate::spatial::Geom>) -> Result<()> {
        let (new_keys, encoded_box) = match new {
            Some(g) => {
                let (keys, bbox, _) = Self::geo_rows(field, id, g)?;
                for key in &keys { self.store.put(key, &bbox)?; }
                (keys, Some(bbox))
            }
            None => (Vec::new(), None),
        };
        if old.is_some() {
            if let Some(encoded_box) = encoded_box.as_deref() {
                for key in &new_keys {
                    if self.store_ref().get(key)?.as_deref() != Some(encoded_box) {
                        return Err(crate::Error::Corrupt {
                            page_no: 0, why: "new spatial posting did not verify",
                        });
                    }
                }
            }
        }
        if let Some(old) = old {
            for key in Self::possible_geo_keys(field, id, old) {
                if !new_keys.contains(&key) { self.store.delete(&key)?; }
            }
        }
        Ok(())
    }

    /// Index or replace a geometry for `(field, id)` (2i, D31). Replacement
    /// writes and independently verifies the new rows before stale old
    /// postings are removed (Law 3).
    pub fn set_geo(&mut self, field: u64, id: u64, g: &crate::spatial::Geom) -> Result<()> {
        use crate::spatial as sp;
        let old = self.store_ref().get(&keys::geom_key(field, id))?
            .and_then(|bytes| sp::Geom::decode(&bytes));
        let (new_keys, encoded_box, encoded_geom) = Self::geo_rows(field, id, g)?;
        // New first. A failed build can leave harmless extra candidates, but
        // it cannot erase the old geometry or its distinct postings.
        for key in &new_keys { self.store.put(key, &encoded_box)?; }
        self.store.put(&keys::geom_key(field, id), &encoded_geom)?;
        // Only after the new geometry and every new posting read back exactly
        // may stale old placements be dropped (Law 3).
        if let Some(old) = old {
            for key in &new_keys {
                if self.store_ref().get(key)?.as_deref() != Some(encoded_box.as_slice()) {
                    return Err(crate::Error::Corrupt { page_no: 0, why: "new spatial posting did not verify" });
                }
            }
            if self.store_ref().get(&keys::geom_key(field, id))?.as_deref()
                != Some(encoded_geom.as_slice())
            {
                return Err(crate::Error::Corrupt { page_no: 0, why: "new geometry row did not verify" });
            }
            for key in Self::possible_geo_keys(field, id, &old) {
                if !new_keys.contains(&key) { self.store.delete(&key)?; }
            }
        }
        Ok(())
    }

    /// Remove debris from an index build that never published its registry.
    /// No query can trust this field while unpublished, so this drops no old
    /// usable state; a subsequent build starts from an unambiguous empty keyspace.
    pub fn clear_unpublished_geo(&mut self, field: u64) -> Result<()> {
        use crate::spatial as sp;
        for level in [sp::LEVEL_FINE, sp::LEVEL_COARSE, sp::LEVEL_WORLD] {
            self.store.delete_prefix(&keys::spat_prefix(field, level))?;
        }
        self.store.delete_prefix(&keys::geom_prefix(field))?;
        Ok(())
    }

    /// Remove a geometry and its postings; needs nothing from the caller
    /// (the stored geometry row supplies the old cells).
    pub fn delete_geo(&mut self, field: u64, id: u64) -> Result<bool> {
        use crate::spatial as sp;
        let Some(raw) = self.store_ref().get(&keys::geom_key(field, id))? else {
            return Ok(false);
        };
        if let Some(g) = sp::Geom::decode(&raw) {
            if let Some((xmin, xmax, ymin, ymax)) = g.bbox() {
                for level in [sp::LEVEL_FINE, sp::LEVEL_COARSE] {
                    if let Some(cells) = sp::cover_cells(xmin, xmax, ymin, ymax, level, sp::MAX_CELLS) {
                        for (cx, cy) in cells {
                            let h = sp::cell_hilbert(cx, cy, level);
                            self.store.delete(&keys::spat_key(field, level, h, id))?;
                        }
                    }
                }
                self.store.delete(&keys::spat_key(field, sp::LEVEL_WORLD, 0, id))?;
            }
        }
        self.store.delete(&keys::geom_key(field, id))?;
        Ok(true)
    }

    pub fn get_geo(&self, field: u64, id: u64) -> Result<Option<crate::spatial::Geom>> {
        Ok(self.store_ref().get(&keys::geom_key(field, id))?
            .and_then(|b| crate::spatial::Geom::decode(&b)))
    }

    /// Candidate (id, bbox) pairs whose postings intersect the query box:
    /// cover the box with Hilbert ranges at BOTH levels, scan each range
    /// (contiguous keys -- the Hilbert payoff), filter by the in-value
    /// bbox, dedupe ids (multi-cell geometries post more than once).
    fn geo_candidates(&self, field: u64, xmin: f64, xmax: f64, ymin: f64, ymax: f64)
        -> Result<Vec<(u64, crate::spatial::BoxF)>>
    {
        let diag = std::env::var_os("GEO_DIAG").is_some();
        let t0 = std::time::Instant::now();
        use crate::spatial as sp;
        let qbox = sp::BoxF::from_f64(xmin, xmax, ymin, ymax);
        let cover_started = std::time::Instant::now();
        let covers: Vec<(u8, usize, Vec<(u64, u64)>)> =
            [sp::LEVEL_FINE, sp::LEVEL_COARSE].into_iter().map(|level| {
                let (x0, y0) = sp::cell_of(xmin, ymin, level);
                let (x1, y1) = sp::cell_of(xmax, ymax, level);
                let cells = (x1 - x0 + 1) as usize * (y1 - y0 + 1) as usize;
                (level, cells, sp::cover_ranges(xmin, xmax, ymin, ymax, level, 256))
            }).collect();
        let cover_elapsed = cover_started.elapsed();
        let pages_before = self.store_ref().pool_stats();
        let gather_started = std::time::Instant::now();
        let mut out: Vec<(u64, sp::BoxF)> = Vec::new();
        let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
        let mut world_rows = 0u64;
        let mut level_rows = Vec::with_capacity(covers.len());
        // the world bucket first: one tiny scan, continent-scale members only
        {
            let prefix = keys::spat_prefix(field, sp::LEVEL_WORLD);
            let it = self.store_ref().scan(&prefix)?;
            it.for_each_ref(|key, val| {
                if !key.starts_with(&prefix) || key.len() != prefix.len() + 16 {
                    return false;
                }
                world_rows += 1;
                let id = u64::from_be_bytes(key[prefix.len() + 8..].try_into().unwrap());
                if let Some(bx) = sp::BoxF::decode(val) {
                    if bx.intersects(&qbox) && seen.insert(id) {
                        out.push((id, bx));
                    }
                }
                true
            })?;
        }
        for (level, cells, ranges) in covers {
            let prefix = keys::spat_prefix(field, level);
            let range_count = ranges.len();
            let mut posting_rows = 0u64;
            for (lo, hi) in ranges {
                let mut from = prefix.clone();
                from.extend_from_slice(&lo.to_be_bytes());
                let it = self.store_ref().scan(&from)?;
                it.for_each_ref(|key, val| {
                    if !key.starts_with(&prefix) || key.len() != prefix.len() + 16 {
                        return false;
                    }
                    let h = u64::from_be_bytes(key[prefix.len()..prefix.len() + 8].try_into().unwrap());
                    if h > hi { return false; }
                    posting_rows += 1;
                    let id = u64::from_be_bytes(key[prefix.len() + 8..].try_into().unwrap());
                    if let Some(bx) = sp::BoxF::decode(val) {
                        if bx.intersects(&qbox) && seen.insert(id) {
                            out.push((id, bx));
                        }
                    }
                    true
                })?;
            }
            level_rows.push((level, cells, range_count, posting_rows));
        }
        if diag {
            let pages_after = self.store_ref().pool_stats();
            let logical = pages_after.hits.saturating_add(pages_after.misses)
                .saturating_sub(pages_before.hits.saturating_add(pages_before.misses));
            let misses = pages_after.misses.saturating_sub(pages_before.misses);
            eprintln!(
                "GEO_DIAG cover_ms={:.3} gather_ms={:.3} total_ms={:.3} world_rows={} candidates={} pool_logical={} pool_misses={} levels={:?}",
                cover_elapsed.as_secs_f64() * 1000.0,
                gather_started.elapsed().as_secs_f64() * 1000.0,
                t0.elapsed().as_secs_f64() * 1000.0,
                world_rows, out.len(), logical, misses, level_rows,
            );
        }
        Ok(out)
    }

    /// Exact geodesic distance in METRES from (lat, lon) to the geometry
    /// of `(field, id)` -- the ST_Distance atom. Point geometries are
    /// exact Vincenty; polygons are 0 when the point is inside, else the
    /// vertex-minimum (the e1 deviation, named in the contract); lines and
    /// multis are vertex-minimum.
    pub fn st_distance(&self, field: u64, id: u64, lat: f64, lon: f64) -> Result<Option<f64>> {
        use crate::{geomath as gm, spatial::Geom};
        let Some(g) = self.get_geo(field, id)? else { return Ok(None) };
        let d = match &g {
            Geom::Point(x, y) => gm::geodesic_distance_m(lat, lon, *y, *x),
            _ => {
                let inside = g.rings_latlon().iter()
                    .any(|r| gm::point_in_polygon(lat, lon, r));
                if inside { 0.0 } else {
                    let mut min = f64::MAX;
                    let mut visit = |x: f64, y: f64| {
                        let d = gm::geodesic_distance_m(lat, lon, y, x);
                        if d < min { min = d; }
                    };
                    match &g {
                        Geom::LineString(c) | Geom::MultiPoint(c) =>
                            c.iter().for_each(|p| visit(p[0], p[1])),
                        Geom::Polygon(rs) | Geom::MultiLineString(rs) =>
                            rs.iter().flatten().for_each(|p| visit(p[0], p[1])),
                        Geom::MultiPolygon(ps) =>
                            ps.iter().flatten().flatten().for_each(|p| visit(p[0], p[1])),
                        Geom::Point(..) => unreachable!(),
                    }
                    min
                }
            }
        };
        Ok(Some(d))
    }

    /// ST_DWithin + ordering: ids within `meters` of (lat, lon), nearest
    /// first, at most `k`. Point candidates answer from the posting alone
    /// (degenerate bbox = the point -- zero payload reads); others read
    /// their geometry once.
    pub fn within_radius(&self, field: u64, lat: f64, lon: f64, meters: f64, k: usize)
        -> Result<Vec<(u64, f64)>>
    {
        use crate::geomath as gm;
        // conservative degree window (e1's expansion), poles clamped
        let dlat = meters / 110_574.0;
        let dlon = meters / (111_320.0 * lat.to_radians().cos().abs().max(0.01));
        let cands = self.geo_candidates(field, lon - dlon, lon + dlon,
                                        lat - dlat, lat + dlat)?;
        let mut hits: Vec<(u64, f64)> = Vec::new();
        // haversine first: within 0.6% of the boundary Vincenty decides;
        // everywhere else the cheap sphere formula is decisive (their
        // divergence is < 0.56% on WGS84). 26x of the 1M radius query was
        // exact math on cover-overshoot candidates before this band.
        let band = meters * 6e-3;
        for (id, bx) in cands {
            let d = match bx.as_point() {
                Some((x, y)) => {
                    let h = gm::haversine_km(lat, lon, y, x) * 1000.0;
                    if h > meters + band { continue; }
                    if h < meters - band { h }
                    else { gm::geodesic_distance_m(lat, lon, y, x) }
                }
                None => match self.st_distance(field, id, lat, lon)? {
                    Some(d) => d,
                    None => continue,
                },
            };
            if d <= meters { hits.push((id, d)); }
        }
        hits.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        hits.truncate(k);
        Ok(hits)
    }

    /// Ids whose geometry bbox intersects the (lon/lat) box. Exact per the
    /// PostGIS && operator semantics: a BOX test, deliberately (their
    /// recheck=false posture); geometry-exact predicates layer above.
    pub fn in_bbox(&self, field: u64, xmin: f64, xmax: f64, ymin: f64, ymax: f64)
        -> Result<Vec<u64>>
    {
        let mut ids: Vec<u64> = self.geo_candidates(field, xmin, xmax, ymin, ymax)?
            .into_iter().map(|(id, _)| id).collect();
        ids.sort_unstable();
        Ok(ids)
    }

    /// SQL's exact radius tier needs the outward bbox carried by each posting.
    /// Keep that metadata beside the id instead of discarding it and then
    /// reopening the posting (or the JSON row) merely to recover the same box.
    pub fn in_bbox_with_boxes(&self, field: u64, xmin: f64, xmax: f64,
                              ymin: f64, ymax: f64)
        -> Result<Vec<(u64, crate::spatial::BoxF)>>
    {
        self.geo_candidates(field, xmin, xmax, ymin, ymax)
    }

    /// Stream each logical bbox candidate once. Multi-cell geometries choose
    /// the lowest query-covered posting that actually exists; checking at most
    /// the write-time MAX_CELLS alternatives avoids a candidate-sized dedup set.
    pub fn for_each_bbox_candidate(
        &self,
        field: u64,
        xmin: f64,
        xmax: f64,
        ymin: f64,
        ymax: f64,
        mut visit: impl FnMut(u64, crate::spatial::BoxF) -> Result<bool>,
    ) -> Result<()> {
        use crate::spatial as sp;
        let diag = std::env::var_os("GEO_DIAG").is_some();
        let started = std::time::Instant::now();
        let cover_started = std::time::Instant::now();
        let covers: Vec<(u8, usize, Vec<(u64, u64)>)> =
            [sp::LEVEL_FINE, sp::LEVEL_COARSE].into_iter().map(|level| {
                let (x0, y0) = sp::cell_of(xmin, ymin, level);
                let (x1, y1) = sp::cell_of(xmax, ymax, level);
                let cells = (x1 - x0 + 1) as usize * (y1 - y0 + 1) as usize;
                (level, cells, sp::cover_ranges(xmin, xmax, ymin, ymax, level, 256))
            }).collect();
        let cover_elapsed = cover_started.elapsed();
        let qbox = sp::BoxF::from_f64(xmin, xmax, ymin, ymax);
        let pages_before = self.store_ref().pool_stats();
        let mut keep_going = true;
        let mut failure = None;
        let mut callback_elapsed = std::time::Duration::ZERO;
        let mut candidates = 0u64;
        let mut world_rows = 0u64;
        let mut level_rows = Vec::with_capacity(covers.len());
        let world = keys::spat_prefix(field, sp::LEVEL_WORLD);
        self.store_ref().scan(&world)?.for_each_ref(|key, value| {
            if !keep_going || failure.is_some() { return false }
            if !key.starts_with(&world) || key.len() != world.len() + 16 { return false }
            world_rows += 1;
            let id = u64::from_be_bytes(key[world.len() + 8..].try_into().unwrap());
            if let Some(bx) = sp::BoxF::decode(value).filter(|bx| bx.intersects(&qbox)) {
                candidates += 1;
                let callback_started = std::time::Instant::now();
                match visit(id, bx) {
                    Ok(keep) => keep_going = keep,
                    Err(error) => failure = Some(error),
                }
                callback_elapsed += callback_started.elapsed();
            }
            keep_going && failure.is_none()
        })?;
        if let Some(error) = failure.take() { return Err(error) }
        if !keep_going { return Ok(()) }

        for (level, cells, ranges) in covers {
            let prefix = keys::spat_prefix(field, level);
            let range_count = ranges.len();
            let mut posting_rows = 0u64;
            for &(lo, hi) in &ranges {
                let mut from = prefix.clone();
                from.extend_from_slice(&lo.to_be_bytes());
                self.store_ref().scan(&from)?.for_each_ref(|key, value| {
                    if !keep_going || failure.is_some() { return false }
                    if !key.starts_with(&prefix) || key.len() != prefix.len() + 16 {
                        return false;
                    }
                    let h = u64::from_be_bytes(
                        key[prefix.len()..prefix.len() + 8].try_into().unwrap());
                    if h > hi { return false }
                    posting_rows += 1;
                    let id = u64::from_be_bytes(key[prefix.len() + 8..].try_into().unwrap());
                    let Some(bx) = sp::BoxF::decode(value).filter(|bx| bx.intersects(&qbox))
                    else { return true };

                    // Outward rounding can add boundary cells, but it cannot
                    // remove any cell that was written. Probe only lower cells
                    // which this query also scans; if one exists, that posting
                    // owns the callback and this duplicate is skipped.
                    let cells = sp::cover_cells(
                        bx.xmin as f64, bx.xmax as f64,
                        bx.ymin as f64, bx.ymax as f64,
                        level, sp::MAX_CELLS * 4);
                    if let Some(cells) = cells {
                        for (cx, cy) in cells {
                            let other = sp::cell_hilbert(cx, cy, level);
                            if other >= h || !ranges.iter().any(|&(a, b)| a <= other && other <= b) {
                                continue;
                            }
                            match self.store_ref().get(&keys::spat_key(field, level, other, id)) {
                                Ok(Some(_)) => return true,
                                Ok(None) => {}
                                Err(error) => { failure = Some(error); return false }
                            }
                        }
                    }
                    candidates += 1;
                    let callback_started = std::time::Instant::now();
                    match visit(id, bx) {
                        Ok(keep) => keep_going = keep,
                        Err(error) => failure = Some(error),
                    }
                    callback_elapsed += callback_started.elapsed();
                    keep_going && failure.is_none()
                })?;
                if let Some(error) = failure.take() { return Err(error) }
                if !keep_going { return Ok(()) }
            }
            level_rows.push((level, cells, range_count, posting_rows));
        }
        if diag {
            let after = self.store_ref().pool_stats();
            let total = started.elapsed();
            let gather = total.saturating_sub(cover_elapsed).saturating_sub(callback_elapsed);
            eprintln!(
                "GEO_DIAG streaming=1 cover_ms={:.3} gather_ms={:.3} exact_callback_ms={:.3} total_ms={:.3} world_rows={} candidates={} pool_logical={} pool_misses={} levels={:?}",
                cover_elapsed.as_secs_f64() * 1000.0,
                gather.as_secs_f64() * 1000.0,
                callback_elapsed.as_secs_f64() * 1000.0,
                total.as_secs_f64() * 1000.0,
                world_rows, candidates,
                after.hits.saturating_add(after.misses)
                    .saturating_sub(pages_before.hits.saturating_add(pages_before.misses)),
                after.misses.saturating_sub(pages_before.misses), level_rows,
            );
        }
        Ok(())
    }

    /// k nearest geometries to (lat, lon): expanding-radius search --
    /// start at one fine cell's span, double until k found (or the world
    /// is covered), then exact-rank. Same narrow-then-exact shape as
    /// vectors; cost bounded by the ring that satisfies k.
    pub fn knn_geo(&self, field: u64, lat: f64, lon: f64, k: usize)
        -> Result<Vec<(u64, f64)>>
    {
        let mut radius = 700.0; // ~ one fine cell at the equator
        loop {
            let hits = self.within_radius(field, lat, lon, radius, k)?;
            if hits.len() >= k || radius > 21_000_000.0 {
                return Ok(hits);
            }
            radius *= 2.0;
        }
    }

    /// Ids of polygons containing the point -- ST_Contains(geom, point).
    pub fn contains_point(&self, field: u64, lat: f64, lon: f64) -> Result<Vec<u64>> {
        use crate::geomath as gm;
        let eps = 1e-9;
        let cands = self.geo_candidates(field, lon - eps, lon + eps, lat - eps, lat + eps)?;
        let mut out = Vec::new();
        for (id, _) in cands {
            let Some(g) = self.get_geo(field, id)? else { continue };
            if g.rings_latlon().iter().any(|r| gm::point_in_polygon(lat, lon, r)) {
                out.push(id);
            }
        }
        out.sort_unstable();
        Ok(out)
    }

    /// Sorted-frontier BFS (GRAPH.md lever 2): each wave is sorted, so its
    /// edge-range lookups arrive in key order and walk the tree as one ordered
    /// sweep -- frontier nodes sharing a leaf pin it once.
    ///
    /// Returns nodes in the order first reached. SACRIFICE (Law 4): within a
    /// wave that order is key order, not insertion order; and `seen` grows
    /// with the reachable set -- inherent to never revisiting.
    /// The contract's "perspective subgraph": every edge of one ctx, one
    /// contiguous range, cost ∝ that KG and never ∝ the store.
    pub fn perspective(&self, ctx: u64) -> Result<EdgeIter<'_>> {
        let prefix = keys::ctx_prefix(ctx);
        Ok(EdgeIter { inner: self.store.scan(&prefix)?, prefix, rev: false, base: ctx == 0 })
    }

    pub fn bfs(&self, ctx: u64, from: u64, ty: Option<u64>, depth: usize) -> Result<Vec<u64>> {
        let mut seen = std::collections::HashSet::new();
        let mut order = Vec::new();
        let mut frontier = vec![from];
        seen.insert(from);
        for _ in 0..depth {
            frontier.sort_unstable();
            let mut next = Vec::new();
            for src in frontier.drain(..) {
                for e in self.out_edges(ctx, src, ty)? {
                    let (_, _, dst, _) = e?;
                    if seen.insert(dst) {
                        order.push(dst);
                        next.push(dst);
                    }
                }
            }
            if next.is_empty() { break; }
            frontier = next;
        }
        Ok(order)
    }
}

/// Long-lived fan-out searcher (2g.2): T pinned snapshot readers, each
/// owning one contiguous id slice of the fingerprint keyspace. Send the
/// same query to all slices, merge survivors, exact-rescore on reader 0.
pub struct ParallelSearcher {
    /// The one field these readers scan. Fixed at construction: the slices
    /// are id ranges WITHIN a field's code range, and a searcher that
    /// changed fields between queries would be scanning someone else's.
    field: u64,
    readers: Vec<(Graph, u64, u64)>,
}

impl ParallelSearcher {
    /// `&mut self`: each spawned thread takes EXCLUSIVE `&mut` access to its
    /// own reader -- Graph is deliberately not Sync (single-threaded cells),
    /// and this is the honest way to say "one reader per thread".
    pub fn nearest(&mut self, q: &[f32], k: usize, metric: Metric, oversample: usize)
        -> Result<Vec<(u64, f32)>>
    {
        let field = self.field;
        let Some((g0, _, _)) = self.readers.first() else { return Ok(Vec::new()) };
        let enc_bits = g0.vec_bits_pub(field);
        let Some(enc) = g0.encoder(field) else { return Ok(Vec::new()) };
        let expected_code_len = 4usize
            .checked_add(crate::vecquant::code_len(
                crate::vecquant::pad_dim(g0.vec_dim(field) as usize), enc_bits))
            .ok_or(crate::Error::Corrupt {
                page_no: 0,
                why: "vector code row length overflows",
            })?;
        let prefix = crate::keys::vcode_prefix(field);
        let aq = enc.affine_query(q);
        // L1's scan stage ranks by the L2 estimate (rotation preserves L2,
        // not L1, so no honest code-space L1 exists); the looser proxy gets
        // a 4x wider candidate pool, and the exact rescore restores the
        // metric. Measured: recall 0.775 -> above floor at the same k.
        let proxy_boost = if matches!(metric, Metric::L1) { 4 } else { 1 };
        let cap = k.saturating_mul(oversample).saturating_mul(proxy_boost).max(k);
        let cands: Vec<u64> = std::thread::scope(|sc| -> Result<Vec<u64>> {
            let mut handles = Vec::new();
            for (g, lo, hi) in &mut self.readers {
                let (aq, prefix, lo, hi) = (&aq, &prefix, *lo, *hi);
                let g: &mut Graph = g;
                handles.push(sc.spawn(move || -> Result<Vec<(f32, u64)>> {
                    let mut heap: std::collections::BinaryHeap<Scored> = std::collections::BinaryHeap::new();
                    let mut malformed = false;
                    let from = crate::keys::vcode_key(field, lo);
                    let it = g.store.scan(&from)?;
                    it.for_each_ref(|key, val| {
                        if key.len() != 17 {
                            malformed = key.starts_with(prefix);
                            return false;
                        }
                        if key[0] != crate::keys::TAG_VCODE || crate::keys::u64_at(key, 1) != field {
                            return false;
                        }
                        if val.len() != expected_code_len {
                            malformed = true;
                            return false;
                        }
                        let id = crate::keys::u64_at(key, 9);
                        if id >= hi { return false; }
                        let norm = f32::from_le_bytes(val[0..4].try_into().unwrap());
                        let dot = if enc_bits == 2 {
                            crate::vecquant::dot_est_affine2(norm, &val[4..], aq)
                        } else {
                            crate::vecquant::dot_est_affine(norm, &val[4..], aq)
                        };
                        let d = match metric {
                            Metric::L2 | Metric::L1 => norm * norm - 2.0 * dot,
                            Metric::Dot => -dot,
                            Metric::Cosine => if norm > 0.0 { -dot / norm } else { 0.0 },
                        };
                        heap.push(Scored { d, id });
                        if heap.len() > cap { heap.pop(); }
                        true
                    })?;
                    if malformed {
                        return Err(crate::Error::Corrupt {
                            page_no: 0,
                            why: "vector code row has an invalid length",
                        });
                    }
                    Ok(heap.into_iter().map(|s| (s.d, s.id)).collect())
                }));
            }
            let mut all: Vec<(f32, u64)> = Vec::new();
            for h in handles { all.extend(h.join().unwrap()?); }
            all.sort_by(|a, b| a.0.total_cmp(&b.0));
            all.truncate(cap);
            Ok(all.into_iter().map(|(_, id)| id).collect())
        })?;
        self.readers[0].0.rescore(field, &cands, q, metric, k)
    }
}

/// (src, ty, other, props) per edge. For `rev` (redge) keys, `other` is the
/// true source and props are on the forward key only.
pub struct EdgeIter<'p> {
    inner: crate::btree::RangeIter<'p>,
    prefix: Vec<u8>,
    rev: bool,
    /// Base-graph keys are 25B (no ctx field); perspective keys are 33B.
    base: bool,
}

impl Iterator for EdgeIter<'_> {
    type Item = Result<(u64, u64, u64, Vec<u8>)>;
    fn next(&mut self) -> Option<Self::Item> {
        let (k, v) = match self.inner.next()? {
            Ok(kv) => kv,
            Err(e) => return Some(Err(e)),
        };
        let want = if self.base { 25 } else { 33 };
        if !k.starts_with(&self.prefix) || k.len() != want {
            return None;
        }
        let o = if self.base { 1 } else { 9 };
        let (a, ty, b) = (keys::u64_at(&k, o), keys::u64_at(&k, o + 8), keys::u64_at(&k, o + 16));
        // edge: a=src b=dst. redge: a=dst b=src -- normalise to (src,ty,dst).
        Some(Ok(if self.rev { (b, ty, a, v) } else { (a, ty, b, v) }))
    }
}

pub struct LabelIter<'p> {
    inner: crate::btree::RangeIter<'p>,
    prefix: Vec<u8>,
}

impl Iterator for LabelIter<'_> {
    type Item = Result<u64>;
    fn next(&mut self) -> Option<Self::Item> {
        let (k, _) = match self.inner.next()? {
            Ok(kv) => kv,
            Err(e) => return Some(Err(e)),
        };
        if !k.starts_with(&self.prefix) || k.len() != 17 {
            return None;
        }
        Some(Ok(keys::u64_at(&k, 9)))
    }
}

/// (value, id) pairs from a property range, streamed. Stops past `hi` or the
/// prop's space -- the tag+prop prefix bounds it like every other scan.
pub struct PropIter<'p> {
    inner: crate::btree::RangeIter<'p>,
    prop: u64,
    hi: u64,
}

impl Iterator for PropIter<'_> {
    type Item = Result<(u64, u64)>;
    fn next(&mut self) -> Option<Self::Item> {
        let (k, _) = match self.inner.next()? {
            Ok(kv) => kv,
            Err(e) => return Some(Err(e)),
        };
        if k.len() != 25 || k[0] != keys::TAG_PROP || keys::u64_at(&k, 1) != self.prop {
            return None;
        }
        let value = keys::u64_at(&k, 9);
        if value > self.hi {
            return None;
        }
        Some(Ok((value, keys::u64_at(&k, 17))))
    }
}

fn decode_f32s(bytes: Vec<u8>) -> Result<Vec<f32>> {
    if bytes.len() % 4 != 0 {
        return Err(crate::Error::Corrupt {
            page_no: 0,
            why: "stored f32 list has trailing bytes",
        });
    }
    Ok(bytes.chunks_exact(4)
        .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
        .collect())
}

/// Distance metrics. Lower is better for all three -- cosine and dot are
/// returned NEGATED so one ordering rule serves every metric.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Metric { L2, Cosine, Dot, L1 }

impl Metric {
    /// Distance between a STORED byte slice (f32-LE) and a query, no copy of
    /// the stored side: decoded lane by lane in the loop.
    pub fn distance_bytes(&self, stored: &[u8], q: &[f32]) -> f32 {
        self.distance_bytes_prepared(stored, q, self.query_norm(q))
    }

    #[inline]
    fn query_norm(&self, q: &[f32]) -> f32 {
        if *self != Metric::Cosine { return 0.0; }
        let mut norm2 = 0.0;
        for &v in q { norm2 += v * v; }
        norm2.sqrt()
    }

    #[inline]
    fn distance_bytes_prepared(&self, stored: &[u8], q: &[f32], query_norm: f32) -> f32 {
        if stored.len() != q.len() * 4 { return f32::INFINITY; }
        #[inline(always)]
        fn lane(bytes: &[u8], i: usize) -> f32 {
            // The caller above proved the complete lane is present. Unaligned
            // reads avoid constructing a temporary four-byte array per lane;
            // from_le keeps the on-disk format portable.
            let bits = unsafe {
                std::ptr::read_unaligned(bytes.as_ptr().add(i * 4).cast::<u32>())
            };
            f32::from_bits(u32::from_le(bits))
        }
        match self {
            Metric::L2 => {
                let mut sum = 0.0;
                for (i, &b) in q.iter().enumerate() {
                    let d = lane(stored, i) - b;
                    sum += d * d;
                }
                sum
            }
            Metric::L1 => {
                let mut sum = 0.0;
                for (i, &b) in q.iter().enumerate() { sum += (lane(stored, i) - b).abs(); }
                sum
            }
            Metric::Dot => {
                let mut dot = 0.0;
                for (i, &b) in q.iter().enumerate() { dot += lane(stored, i) * b; }
                -dot
            }
            Metric::Cosine => {
                let mut dot = 0.0f32;
                let mut na = 0.0f32;
                for (i, &b) in q.iter().enumerate() {
                    let a = lane(stored, i);
                    dot += a * b; na += a * a;
                }
                let denom = (na.sqrt() * query_norm).max(f32::MIN_POSITIVE);
                -(dot / denom)
            }
        }
    }
}

/// Max-heap by distance so `pop` evicts the WORST of the current top-k.
struct Scored { d: f32, id: u64 }
impl PartialEq for Scored { fn eq(&self, o: &Self) -> bool { self.d == o.d && self.id == o.id } }
impl Eq for Scored {}
impl Ord for Scored {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        self.d.total_cmp(&o.d).then(self.id.cmp(&o.id))
    }
}
impl PartialOrd for Scored { fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> { Some(self.cmp(o)) } }

/// Keep the best `k` scores without the old push-then-pop on every row.
/// Once full, non-competitive rows do no heap mutation at all.
#[inline]
fn offer_score(heap: &mut std::collections::BinaryHeap<Scored>, score: Scored, k: usize) {
    if heap.len() < k {
        heap.push(score);
    } else if score.cmp(heap.peek().expect("a full top-k heap is non-empty")).is_lt() {
        *heap.peek_mut().expect("a full top-k heap is non-empty") = score;
    }
}
