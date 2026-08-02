//! Edge storage backend.
//!
//! `EdgeStore` manages graph adjacency lists (forward + reverse) and edge
//! metadata.  Two modes:
//!
//! - **Fat** — edge metadata (`Option<Value>`) lives in RAM alongside the
//!   topology.  Same as the original sekejap representation.  Used for
//!   in-memory databases and when maximum edge-meta read speed is needed.
//!
//! - **Compact** — only the topology (other, type) lives in RAM;
//!   edge metadata is stored in an append-only `edge_meta.bin` file read via
//!   mmap.  Cuts RAM ~2.7× per edge (64 → 24 bytes) and moves bulky JSON
//!   metadata to disk.
//!
//! The public API is identical for both modes — callers iterate `&[Edge]`
//! slices and call `edge_meta()` when (rarely) needed.

use std::collections::HashMap;
use std::io;
use std::path::Path;

use serde_json::Value;

/// Compact edge stored in adjacency lists. 20 bytes on 64-bit.
///
/// An edge is a naked connector: who it points to and what type it is. Nothing
/// else — a relation is not a body. Any weight/attribute is opt-in, and rides
/// beside the edge in a fast-lane column or the JSON bag, never inside it.
///
/// Used by both Fat and Compact modes — the only difference is where
/// metadata lives (RAM vs disk), pointed to by `meta_id`.
#[derive(Clone)]
pub(crate) struct Edge {
    pub other: u64,
    pub edge_type: u64,
    /// Index into the meta store.  `u32::MAX` = no metadata.
    meta_id: u32,
}

const NO_META: u32 = u32::MAX;

/// High bit of `meta_id` marks a paged-topology base metadata reference
/// (an index into `edgemeta.bin`) rather than an index into this store's
/// `MetaStore`. `NO_META` (all ones) is checked first, so it never collides.
const BASE_META_BIT: u32 = 0x8000_0000;

impl Edge {
    /// Construct an edge for the paged-topology base. `base_meta_ref` is the
    /// `edgemeta.bin` index (`u32::MAX` = no metadata).
    pub(crate) fn from_base(other: u64, edge_type: u64, base_meta_ref: u32) -> Self {
        let meta_id = if base_meta_ref == u32::MAX {
            NO_META
        } else {
            base_meta_ref | BASE_META_BIT
        };
        Self { other, edge_type, meta_id }
    }

    /// The edge's attribute slot (its index into the resident columns + JSON
    /// store), or `None` if it has no resident attributes (no meta, or a base
    /// edge). The hot path uses this to index a resolved column directly.
    pub(crate) fn attr_slot(&self) -> Option<u32> {
        if self.meta_id == NO_META || self.base_meta_ref().is_some() {
            None
        } else {
            Some(self.meta_id)
        }
    }

    /// If this edge's metadata lives in the paged base (`edgemeta.bin`), return
    /// the base index. `None` = no metadata or resident-store metadata.
    pub(crate) fn base_meta_ref(&self) -> Option<u32> {
        if self.meta_id != NO_META && self.meta_id & BASE_META_BIT != 0 {
            Some(self.meta_id & !BASE_META_BIT)
        } else {
            None
        }
    }
}

/// Runtime edge-storage mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EdgeMode {
    /// Full edge metadata in RAM (original behaviour).
    Fat,
    /// Compact topology in RAM, metadata on disk via mmap.
    Compact,
}

pub(crate) struct EdgeStore {
    /// Forward adjacency: from_hash → outgoing edges.
    fwd: HashMap<u64, Vec<Edge>>,
    /// Reverse adjacency: to_hash → incoming edges.
    rev: HashMap<u64, Vec<Edge>>,
    /// edge_type_hash → human-readable name.
    type_names: HashMap<u64, String>,
    /// Metadata backend (the JSON bag — the slow lane).
    meta: MetaStore,
    /// The FAST LANE: columnar primitive edge attributes, keyed by
    /// `(edge_type_hash, attr_name)`, each a dense `Vec<f64>` indexed by the
    /// edge's attribute slot (its `meta_id`). Read = one arithmetic array index,
    /// no parse — a direct-indexed weight lane, opt-in, for any
    /// user-named primitive column. `NaN` = unset for that edge.
    columns: HashMap<(u64, String), EdgeColumn>,
    /// Identity set for KEYED edges: `(from_hash, edge_type, key_hash)` where
    /// `key_hash = sk_hash(_key)`. An edge that carries a `_key` attribute is
    /// deduped on this triple — re-asserting the same keyed edge is idempotent
    /// (no parallel-edge stacking). Rebuilt from the edges on reopen (WAL replay
    /// funnels through the same insert path), so it needs no separate persistence.
    /// Maps the identity to the edge's attribute slot (`meta_id`) so re-insert can
    /// UPSERT — overwrite that edge's attributes in place (last-wins).
    keyed: HashMap<(u64, u64, u64), u32>,
}

/// One fast-lane column. `vals` is dense over the attribute-slot space; `is_bool`
/// records whether values were booleans (so reads return `true/false`, not `1.0`).
pub(crate) struct EdgeColumn {
    vals: Vec<f64>,
    is_bool: bool,
}

impl EdgeColumn {
    fn new(is_bool: bool) -> Self {
        Self { vals: Vec::new(), is_bool }
    }
    /// Set this edge's value at its slot, growing with NaN (unset) as needed.
    fn set(&mut self, slot: u32, v: f64) {
        let slot = slot as usize;
        if slot >= self.vals.len() {
            self.vals.resize(slot + 1, f64::NAN);
        }
        self.vals[slot] = v;
    }
    /// Read the value at a slot as JSON, or `None` if unset/out-of-range.
    /// Direct array index — the fast-lane read (no lookup, no parse).
    pub(crate) fn at(&self, slot: u32) -> Option<Value> {
        let v = *self.vals.get(slot as usize)?;
        if v.is_nan() {
            return None;
        }
        if self.is_bool {
            Some(Value::Bool(v != 0.0))
        } else if v.fract() == 0.0 && v.abs() < 9.007_199_254_740_992e15 {
            // Whole numbers round-trip as JSON integers so consumers using
            // `.as_i64()` (year, count, kWh) see an int, not `30.0`. Bounded to
            // the exact-integer f64 range to avoid precision surprises.
            Some(Value::Number((v as i64).into()))
        } else {
            serde_json::Number::from_f64(v).map(Value::Number)
        }
    }
}

/// A primitive edge-attribute value, routed to the fast lane by its type.
pub(crate) enum ColVal {
    Num(f64),
    Bool(bool),
}

/// Value equality with numeric coercion (JSON int `2` == literal `2.0`), so an
/// edge attribute stored via the columnar lane matches a parsed SQL literal.
fn val_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

enum MetaStore {
    /// Metadata in RAM — `meta_id` indexes into `metas`.
    Ram {
        metas: Vec<Value>,
    },
    /// Metadata on disk — `meta_id` indexes into `offsets`, which point into
    /// `edge_meta.bin` via mmap.
    #[cfg(unix)]
    Disk {
        /// (byte_offset, byte_len) per meta entry.
        offsets: Vec<(u32, u16)>,
        file: std::fs::File,
        total_len: u64,
        mmap: Option<super::mmap::MmapView>,
    },
}

impl EdgeStore {
    // ── Constructors ─────────────────────────────────────────────────────

    /// Create an empty Fat (in-RAM) edge store.
    pub fn new_fat() -> Self {
        Self {
            fwd: HashMap::new(),
            rev: HashMap::new(),
            type_names: HashMap::new(),
            meta: MetaStore::Ram { metas: Vec::new() },
            columns: HashMap::new(),
            keyed: HashMap::new(),
        }
    }

    /// Create an empty Compact (disk-backed meta) edge store.
    #[cfg(unix)]
    pub fn new_compact(dir: &Path) -> io::Result<Self> {
        let path = dir.join("edge_meta.bin");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        Ok(Self {
            fwd: HashMap::new(),
            rev: HashMap::new(),
            type_names: HashMap::new(),
            meta: MetaStore::Disk {
                offsets: Vec::new(),
                file,
                total_len: 0,
                mmap: None,
            },
            columns: HashMap::new(),
            keyed: HashMap::new(),
        })
    }

    /// Open an existing Compact edge store (re-reads edge_meta.bin).
    #[cfg(unix)]
    pub fn open_compact(dir: &Path) -> io::Result<Self> {
        let path = dir.join("edge_meta.bin");
        if path.exists() {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)?;
            let file_len = file.metadata()?.len();
            let mmap = super::mmap::MmapView::try_new(&file, file_len as usize);
            Ok(Self {
                fwd: HashMap::new(),
                rev: HashMap::new(),
                type_names: HashMap::new(),
                meta: MetaStore::Disk {
                    offsets: Vec::new(),
                    file,
                    total_len: file_len,
                    mmap,
                },
                columns: HashMap::new(),
                keyed: HashMap::new(),
            })
        } else {
            Self::new_compact(dir)
        }
    }

    // ── Edge insertion ───────────────────────────────────────────────────

    /// Insert an edge without metadata.
    pub fn link(
        &mut self,
        from_hash: u64,
        to_hash: u64,
        edge_type: u64,
        edge_type_name: &str,
    ) {
        self.type_names
            .insert(edge_type, edge_type_name.to_string());
        let edge_fwd = Edge {
            other: to_hash,
            edge_type,
            meta_id: NO_META,
        };
        let edge_rev = Edge {
            other: from_hash,
            edge_type,
            meta_id: NO_META,
        };
        self.fwd.entry(from_hash).or_default().push(edge_fwd);
        self.rev.entry(to_hash).or_default().push(edge_rev);
    }

    /// Insert an edge with metadata.
    pub fn link_meta(
        &mut self,
        from_hash: u64,
        to_hash: u64,
        edge_type: u64,
        edge_type_name: &str,
        meta: Value,
    ) {
        self.type_names
            .insert(edge_type, edge_type_name.to_string());
        let mid = self.store_meta(meta);
        let edge_fwd = Edge {
            other: to_hash,
            edge_type,
            meta_id: mid,
        };
        let edge_rev = Edge {
            other: from_hash,
            edge_type,
            meta_id: mid,
        };
        self.fwd.entry(from_hash).or_default().push(edge_fwd);
        self.rev.entry(to_hash).or_default().push(edge_rev);
    }

    /// Insert an edge carrying fast-lane columns and/or a JSON bag. One attribute
    /// slot is shared by both: primitives ride the columns, the rest the JSON.
    pub fn link_with_attrs(
        &mut self,
        from_hash: u64,
        to_hash: u64,
        edge_type: u64,
        edge_type_name: &str,
        cols: &[(String, ColVal)],
        json: Option<Value>,
    ) {
        self.type_names.insert(edge_type, edge_type_name.to_string());
        // One slot in the meta store's slot space, shared by columns + json.
        let slot = self.store_meta(json.unwrap_or(Value::Null));
        for (name, val) in cols {
            let (v, is_bool) = match val {
                ColVal::Num(n) => (*n, false),
                ColVal::Bool(b) => (if *b { 1.0 } else { 0.0 }, true),
            };
            self.columns
                .entry((edge_type, name.clone()))
                .or_insert_with(|| EdgeColumn::new(is_bool))
                .set(slot, v);
        }
        let fwd = Edge { other: to_hash, edge_type, meta_id: slot };
        let rev = Edge { other: from_hash, edge_type, meta_id: slot };
        self.fwd.entry(from_hash).or_default().push(fwd);
        self.rev.entry(to_hash).or_default().push(rev);
    }

    /// Insert or UPSERT a KEYED edge, identity `(from, type, key)`. If no such edge
    /// exists it is created; if one exists its attributes are overwritten in place
    /// (last-wins) — so re-asserting the same keyed edge updates it rather than
    /// stacking a duplicate. Returns `true` if a new edge was created, `false` if an
    /// existing one was updated. `key_hash = sk_hash(_key)`.
    pub fn link_keyed(
        &mut self,
        from_hash: u64,
        to_hash: u64,
        edge_type: u64,
        edge_type_name: &str,
        key_hash: u64,
        cols: &[(String, ColVal)],
        json: Option<Value>,
    ) -> bool {
        self.type_names.insert(edge_type, edge_type_name.to_string());
        let id = (from_hash, edge_type, key_hash);
        if let Some(&slot) = self.keyed.get(&id) {
            // UPSERT: overwrite this edge's attributes at its existing slot.
            // Clear the type's columns at the slot first so attributes dropped from
            // the new value don't linger, then set the new ones.
            for ((etype, _name), col) in self.columns.iter_mut() {
                if *etype == edge_type {
                    col.set(slot, f64::NAN);
                }
            }
            self.set_cols(edge_type, slot, cols);
            self.set_meta(slot, json.unwrap_or(Value::Null));
            return false;
        }
        // NEW keyed edge: allocate a slot, store attrs, record the identity → slot.
        let slot = self.store_meta(json.unwrap_or(Value::Null));
        self.set_cols(edge_type, slot, cols);
        let fwd = Edge { other: to_hash, edge_type, meta_id: slot };
        let rev = Edge { other: from_hash, edge_type, meta_id: slot };
        self.fwd.entry(from_hash).or_default().push(fwd);
        self.rev.entry(to_hash).or_default().push(rev);
        self.keyed.insert(id, slot);
        true
    }

    /// Write fast-lane columns for one edge at `slot`.
    fn set_cols(&mut self, edge_type: u64, slot: u32, cols: &[(String, ColVal)]) {
        for (name, val) in cols {
            let (v, is_bool) = match val {
                ColVal::Num(n) => (*n, false),
                ColVal::Bool(b) => (if *b { 1.0 } else { 0.0 }, true),
            };
            self.columns
                .entry((edge_type, name.clone()))
                .or_insert_with(|| EdgeColumn::new(is_bool))
                .set(slot, v);
        }
    }

    /// Overwrite the JSON meta at an existing `slot` (upsert). RAM overwrites in
    /// place; Disk appends the new bytes and repoints the offset (reads fall back to
    /// `pread`, so the stale mmap is fine until the next remap/compaction).
    fn set_meta(&mut self, slot: u32, meta: Value) {
        match &mut self.meta {
            MetaStore::Ram { metas } => {
                if let Some(m) = metas.get_mut(slot as usize) {
                    *m = meta;
                }
            }
            #[cfg(unix)]
            MetaStore::Disk { offsets, file, total_len, .. } => {
                let json_bytes = serde_json::to_vec(&meta).unwrap_or_default();
                let offset = *total_len as u32;
                let len = json_bytes.len() as u16;
                use std::os::unix::fs::FileExt;
                file.write_all_at(&json_bytes, *total_len)
                    .expect("sekejap: edge meta disk write failed");
                *total_len += json_bytes.len() as u64;
                if let Some(e) = offsets.get_mut(slot as usize) {
                    *e = (offset, len);
                }
            }
        }
    }

    /// Resolve a fast-lane column ONCE for `(edge_type, attr)`. The hot path calls
    /// this once per query, then reads `col.at(edge.attr_slot())` per edge — a
    /// direct array index, no lookup, no parse. `None` = no such column.
    /// (Reserved for the hot-path read optimisation; the general path currently
    /// uses `edge_cols`.)
    #[allow(dead_code)]
    pub fn edge_column(&self, edge_type: u64, attr: &str) -> Option<&EdgeColumn> {
        self.columns.get(&(edge_type, attr.to_string()))
    }

    /// Convenience per-edge column read (does the resolve + index each call —
    /// use `edge_column()` + `EdgeColumn::at()` on the hot path instead).
    #[allow(dead_code)]
    pub fn edge_col(&self, edge: &Edge, attr: &str) -> Option<Value> {
        let slot = edge.attr_slot()?;
        self.edge_column(edge.edge_type, attr)?.at(slot)
    }

    /// All set fast-lane column values for this edge, as `(name, value)` pairs.
    /// Used to materialise an edge's attributes into a query row.
    pub fn edge_cols(&self, edge: &Edge) -> Vec<(String, Value)> {
        let Some(slot) = edge.attr_slot() else { return Vec::new() };
        self.columns
            .iter()
            .filter(|((et, _), _)| *et == edge.edge_type)
            .filter_map(|((_, name), col)| col.at(slot).map(|v| (name.clone(), v)))
            .collect()
    }

    /// Resolve ALL fast-lane columns for one edge type ONCE — `(name, &column)`.
    /// The hot path calls this a single time per query (not per edge), then reads
    /// `col.at(slot)` per edge: a direct array index, no HashMap scan, no clone.
    /// This is how the fast lane keeps its promise on aggregations.
    pub(crate) fn columns_for_type(&self, edge_type: u64) -> Vec<(&str, &EdgeColumn)> {
        self.columns
            .iter()
            .filter(|((et, _), _)| *et == edge_type)
            .map(|((_, name), col)| (name.as_str(), col))
            .collect()
    }

    /// Store metadata and return its id.
    fn store_meta(&mut self, meta: Value) -> u32 {
        match &mut self.meta {
            MetaStore::Ram { metas } => {
                let id = metas.len() as u32;
                metas.push(meta);
                id
            }
            #[cfg(unix)]
            MetaStore::Disk {
                offsets,
                file,
                total_len,
                ..
            } => {
                let json_bytes = serde_json::to_vec(&meta).unwrap_or_default();
                let offset = *total_len as u32;
                let len = json_bytes.len() as u16;
                use std::os::unix::fs::FileExt;
                file.write_all_at(&json_bytes, *total_len)
                    .expect("sekejap: edge meta disk write failed");
                *total_len += json_bytes.len() as u64;
                let id = offsets.len() as u32;
                offsets.push((offset, len));
                id
            }
        }
    }

    // ── Edge removal ─────────────────────────────────────────────────────

    /// Remove all edges of `edge_type` from `from_hash` to `to_hash`.
    pub fn unlink(
        &mut self,
        from_hash: u64,
        to_hash: u64,
        edge_type: u64,
    ) {
        if let Some(edges) = self.fwd.get_mut(&from_hash) {
            edges.retain(|e| !(e.other == to_hash && e.edge_type == edge_type));
        }
        if let Some(edges) = self.rev.get_mut(&to_hash) {
            edges.retain(|e| !(e.other == from_hash && e.edge_type == edge_type));
        }
        self.keyed.retain(|&(f, t, _k), _| !(f == from_hash && t == edge_type));
        // Dead meta entries are reclaimed by compact().
    }

    /// One attribute value for an edge — fast-lane column first, then the JSON bag.
    fn edge_attr(&self, e: &Edge, name: &str) -> Option<Value> {
        if let Some(slot) = e.attr_slot() {
            if let Some(col) = self.columns.get(&(e.edge_type, name.to_string())) {
                if let Some(v) = col.at(slot) {
                    return Some(v);
                }
            }
        }
        self.edge_meta(e).and_then(|m| m.get(name).cloned())
    }

    /// Delete only the edges `from → to` of `edge_type` whose attributes match ALL
    /// of `props` (equality). Empty `props` deletes them all (same as `unlink`).
    /// Returns the number of edges removed. Purges keyed identities for removed
    /// edges so a later re-insert of the same key creates cleanly.
    pub fn unlink_matching(
        &mut self,
        from_hash: u64,
        to_hash: u64,
        edge_type: u64,
        props: &[(String, Value)],
    ) -> usize {
        if props.is_empty() {
            let n = self.fwd.get(&from_hash).map_or(0, |es| {
                es.iter().filter(|e| e.other == to_hash && e.edge_type == edge_type).count()
            });
            if n > 0 {
                self.unlink(from_hash, to_hash, edge_type);
            }
            return n;
        }
        // Immutable pass: collect the attribute slots of matching edges.
        let mut rm: HashMap<u32, ()> = HashMap::new();
        if let Some(edges) = self.fwd.get(&from_hash) {
            for e in edges {
                if e.other == to_hash && e.edge_type == edge_type {
                    let all = props.iter().all(|(k, want)| {
                        self.edge_attr(e, k).map_or(false, |got| val_eq(&got, want))
                    });
                    if all {
                        if let Some(s) = e.attr_slot() {
                            rm.insert(s, ());
                        }
                    }
                }
            }
        }
        if rm.is_empty() {
            return 0;
        }
        let n = rm.len();
        if let Some(edges) = self.fwd.get_mut(&from_hash) {
            edges.retain(|e| !(e.other == to_hash && e.edge_type == edge_type
                && e.attr_slot().map_or(false, |s| rm.contains_key(&s))));
        }
        if let Some(edges) = self.rev.get_mut(&to_hash) {
            edges.retain(|e| !(e.other == from_hash && e.edge_type == edge_type
                && e.attr_slot().map_or(false, |s| rm.contains_key(&s))));
        }
        self.keyed.retain(|&(f, t, _k), slot| !(f == from_hash && t == edge_type && rm.contains_key(slot)));
        n
    }

    /// Set attributes on edges `from → to` of `edge_type` matching ALL of `pred`.
    /// `set_cols` are primitive fast-lane values; `set_json` are the remaining attrs
    /// merged into each matched edge's JSON bag (keys not in `set_json` are kept).
    /// Only edges that already have an attribute slot can be updated. Returns count.
    pub fn update_matching(
        &mut self,
        from_hash: u64,
        to_hash: u64,
        edge_type: u64,
        pred: &[(String, Value)],
        set_cols: &[(String, ColVal)],
        set_json: &serde_json::Map<String, Value>,
    ) -> usize {
        // Immutable pass: find the slots of matching edges.
        let mut slots: Vec<u32> = Vec::new();
        if let Some(edges) = self.fwd.get(&from_hash) {
            for e in edges {
                if e.other == to_hash && e.edge_type == edge_type {
                    let all = pred.iter().all(|(k, want)| {
                        self.edge_attr(e, k).map_or(false, |got| val_eq(&got, want))
                    });
                    if all {
                        if let Some(s) = e.attr_slot() {
                            slots.push(s);
                        }
                    }
                }
            }
        }
        // Mutable pass: overwrite the columns and merge the JSON bag at each slot.
        for &slot in &slots {
            self.set_cols(edge_type, slot, set_cols);
            if !set_json.is_empty() {
                let mut cur = match self.json_at(slot) {
                    Some(Value::Object(m)) => m,
                    _ => serde_json::Map::new(),
                };
                for (k, v) in set_json {
                    cur.insert(k.clone(), v.clone());
                }
                self.set_meta(slot, Value::Object(cur));
            }
        }
        slots.len()
    }

    /// Remove all edges involving `hash` (both directions).
    /// Returns the set of affected neighbour hashes for cascade cleanup.
    pub fn remove_node(&mut self, hash: u64) -> Vec<(u64, bool)> {
        let mut affected = Vec::new();

        // Remove forward edges: clean up reverse entries on targets.
        if let Some(fwd_edges) = self.fwd.remove(&hash) {
            for e in &fwd_edges {
                affected.push((e.other, true)); // true = was forward
                if let Some(rev) = self.rev.get_mut(&e.other) {
                    rev.retain(|r| r.other != hash);
                }
            }
        }
        // Remove reverse edges: clean up forward entries on sources.
        if let Some(rev_edges) = self.rev.remove(&hash) {
            for e in &rev_edges {
                affected.push((e.other, false)); // false = was reverse
                if let Some(fwd) = self.fwd.get_mut(&e.other) {
                    fwd.retain(|f| f.other != hash);
                }
            }
        }
        affected
    }

    // ── Edge reads ───────────────────────────────────────────────────────

    /// Outgoing edges from `hash`.
    #[inline]
    pub fn fwd_edges(&self, hash: u64) -> Option<&[Edge]> {
        self.fwd.get(&hash).map(|v| v.as_slice())
    }

    /// Incoming edges to `hash`.
    #[inline]
    pub fn rev_edges(&self, hash: u64) -> Option<&[Edge]> {
        self.rev.get(&hash).map(|v| v.as_slice())
    }

    /// Resolve metadata for an edge.  Returns `None` if the edge has no meta
    /// or if the meta could not be read.
    pub fn edge_meta(&self, edge: &Edge) -> Option<Value> {
        if edge.meta_id == NO_META || edge.base_meta_ref().is_some() {
            // Base-bit ids belong to the paged topology's edgemeta.bin —
            // resolved by CoreDB::edge_meta, never by this store.
            return None;
        }
        match &self.meta {
            MetaStore::Ram { metas } => {
                metas.get(edge.meta_id as usize).cloned()
            }
            #[cfg(unix)]
            MetaStore::Disk {
                offsets, mmap, file, ..
            } => {
                let &(offset, len) = offsets.get(edge.meta_id as usize)?;
                if len == 0 {
                    return None;
                }
                // Fast path: mmap. Fallback: pread — the mapping can be absent or
                // stale (shorter than the file) for metas appended since open;
                // without this fallback such metas silently read as None (which
                // previously made compact() drop them from snapshot + topology).
                if let Some(bytes) = mmap
                    .as_ref()
                    .and_then(|m| m.slice(offset as usize, len as usize))
                {
                    return serde_json::from_slice(bytes).ok();
                }
                use std::os::unix::fs::FileExt;
                let mut buf = vec![0u8; len as usize];
                file.read_exact_at(&mut buf, offset as u64).ok()?;
                serde_json::from_slice(&buf).ok()
            }
        }
    }

    /// Read the JSON meta bag at an attribute slot directly. Used when the caller
    /// already knows the EXACT edge's slot (from traversal), so there's no
    /// first-match ambiguity across parallel edges.
    pub(crate) fn json_at(&self, slot: u32) -> Option<Value> {
        if slot == NO_META {
            return None;
        }
        let synth = Edge { other: 0, edge_type: 0, meta_id: slot };
        self.edge_meta(&synth)
    }

    /// Resolve edge type hash to human-readable name.
    #[inline]
    pub fn type_name(&self, type_hash: u64) -> Option<&str> {
        self.type_names.get(&type_hash).map(|s| s.as_str())
    }

    // ── Iteration & stats ────────────────────────────────────────────────

    /// Total number of edges (forward direction only — each edge counted once).
    pub fn edge_count(&self) -> usize {
        self.fwd.values().map(|v| v.len()).sum()
    }

    /// Iterate all forward adjacency entries: (from_hash, &[Edge]).
    pub fn iter_fwd(&self) -> impl Iterator<Item = (&u64, &[Edge])> {
        self.fwd.iter().map(|(k, v)| (k, v.as_slice()))
    }

    // ── Compaction ───────────────────────────────────────────────────────

    /// Remap the metadata mmap to cover newly appended data.
    #[cfg(unix)]
    pub fn remap_meta(&mut self) {
        if let MetaStore::Disk {
            file,
            total_len,
            mmap,
            ..
        } = &mut self.meta
        {
            let len = *total_len as usize;
            if len == 0 {
                *mmap = None;
                return;
            }
            if let Some(ref m) = mmap {
                if m.len() >= len {
                    return;
                }
            }
            *mmap = super::mmap::MmapView::try_new(file, len);
        }
    }

}
