//! Phase 0 — dense-id, offset-addressable, StreamVByte-delta topology format.
//!
//! This module is **self-contained**: it turns an in-memory graph (nodes + edges)
//! into a set of pointer-free byte buffers, and reads them back. It is NOT yet wired
//! into `CoreDB` — the engine still loads topology into RAM. The point of Phase 0 is
//! to lock the on-disk shape so that Phase 1 (mmap) and Phase 2 (S3) are read-path
//! flips, not migrations. See `docs/internals/topology-format-v2.md`.
//!
//! ## Files (each starts with a 16-byte `[magic 8][version u32][flags u32]` header)
//! - `nodes.bin` — `count u64` + fixed 24-byte node records, addressed by dense id
//!   (`record(k) = data_start + k*24`). Location is pure arithmetic, no index.
//! - `adj_fwd.bin` / `adj_rev.bin` — CSR: `count`, `block_region_start`, an
//!   `offsets[count+1] u64` array (absolute byte offset of each node's edge block),
//!   then variable-length per-node blocks.
//! - `idx.bin` — sorted `(hash u64, dense_id u64)` for `hash → dense_id` name
//!   resolution (binary search; touched only at query roots).
//! - `dict.bin` — collection-name and edge-type-name dictionaries (`id → string`).
//!
//! ## Per-node edge block
//! `[count varint][neighbor deltas: StreamVByte][type_ids u32×n][strengths f32×n][meta u32×n]`
//! Neighbor ids are sorted, delta-encoded, then StreamVByte-packed with 2-bit length
//! classes {1,2,4,8} bytes — compact (~1–2 B/neighbor), decode-4-at-a-time, and
//! *unbounded* (a delta widens as needed, so there is no `u32` node ceiling).

// Phase 0: the format is built and round-trip tested, but not yet called from the
// engine (that happens in Phase 1 — `compact()` writes it, `open()` mmaps it). Until
// then the public surface is exercised only by this module's own tests.
#![allow(dead_code)]

const MAGIC_NODES: [u8; 8] = *b"SKNODE\0\0";
const MAGIC_ADJF: [u8; 8] = *b"SKADJF\0\0";
const MAGIC_ADJR: [u8; 8] = *b"SKADJR\0\0";
const MAGIC_IDX: [u8; 8] = *b"SKIDX\0\0\0";
const MAGIC_SLUG: [u8; 8] = *b"SKSLUG\0\0";
const MAGIC_DICT: [u8; 8] = *b"SKDICT\0\0";

/// Topology format version (independent of the snapshot version).
const TOPO_VERSION: u32 = 1;
const HEADER_LEN: usize = 16;
const NODE_RECSIZE: usize = 24;
/// Sentinel for "no collection" / "no edge metadata".
pub const NO_ID: u32 = u32::MAX;

// ── Input (what the builder consumes) ─────────────────────────────────────────

pub struct TopoNode {
    pub hash: u64,
    pub slug: String,
    pub collection: String,
    pub payload_offset: u64,
    pub payload_len: u32,
}

pub struct TopoEdge {
    pub from_hash: u64,
    pub to_hash: u64,
    pub edge_type: String,
    pub strength: f32,
}

/// The five topology files as owned byte buffers (Phase 0). In Phase 1 the reader
/// takes mmap slices of exactly these bytes.
pub struct TopologyBlob {
    pub nodes: Vec<u8>,
    pub fwd: Vec<u8>,
    pub rev: Vec<u8>,
    pub idx: Vec<u8>,
    pub slugs: Vec<u8>,
    pub dict: Vec<u8>,
}

// ── Little-endian read helpers ────────────────────────────────────────────────

#[inline]
fn rd_u32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
#[inline]
fn rd_u64(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}
#[inline]
fn rd_f32(b: &[u8], o: usize) -> f32 {
    f32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn write_header(out: &mut Vec<u8>, magic: &[u8; 8], flags: u32) {
    out.extend_from_slice(magic);
    out.extend_from_slice(&TOPO_VERSION.to_le_bytes());
    out.extend_from_slice(&flags.to_le_bytes());
}

fn check_header(b: &[u8], magic: &[u8; 8]) -> Result<(), String> {
    if b.len() < HEADER_LEN || &b[0..8] != magic {
        return Err("topology: bad or missing magic".into());
    }
    let ver = rd_u32(b, 8);
    if ver > TOPO_VERSION {
        return Err(format!("topology: version {ver} newer than supported {TOPO_VERSION}"));
    }
    Ok(())
}

// ── LEB128 varint (for the per-block edge count) ──────────────────────────────

fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

fn read_varint(b: &[u8], mut pos: usize) -> (u64, usize) {
    let mut v = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = b[pos];
        pos += 1;
        v |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    (v, pos)
}

// ── StreamVByte with {1,2,4,8}-byte length classes (u64-capable, unbounded) ────

#[inline]
fn len_class(v: u64) -> u8 {
    if v < (1 << 8) {
        0
    } else if v < (1 << 16) {
        1
    } else if v < (1u64 << 32) {
        2
    } else {
        3
    }
}
#[inline]
fn class_bytes(c: u8) -> usize {
    match c {
        0 => 1,
        1 => 2,
        2 => 4,
        _ => 8,
    }
}

/// Encode `values` into a 2-bit-per-value control stream + a data stream.
fn svb_encode(values: &[u64], control: &mut Vec<u8>, data: &mut Vec<u8>) {
    let ctrl_len = (values.len() + 3) / 4;
    let ctrl_start = control.len();
    control.resize(ctrl_start + ctrl_len, 0);
    for (i, &v) in values.iter().enumerate() {
        let c = len_class(v);
        control[ctrl_start + i / 4] |= c << ((i % 4) * 2);
        let nb = class_bytes(c);
        data.extend_from_slice(&v.to_le_bytes()[..nb]);
    }
}

/// Decode `count` values from `control` (starting at bit 0) and `data`. Returns the
/// values and the number of `data` bytes consumed.
fn svb_decode(control: &[u8], data: &[u8], count: usize) -> (Vec<u64>, usize) {
    let mut out = Vec::with_capacity(count);
    let mut pos = 0usize;
    for i in 0..count {
        let c = (control[i / 4] >> ((i % 4) * 2)) & 0b11;
        let nb = class_bytes(c);
        let mut buf = [0u8; 8];
        buf[..nb].copy_from_slice(&data[pos..pos + nb]);
        out.push(u64::from_le_bytes(buf));
        pos += nb;
    }
    (out, pos)
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// Intern strings to dense ids in first-appearance order.
#[derive(Default)]
struct Interner {
    map: std::collections::HashMap<String, u32>,
    list: Vec<String>,
}
impl Interner {
    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let id = self.list.len() as u32;
        self.list.push(s.to_string());
        self.map.insert(s.to_string(), id);
        id
    }
}

/// Serialize one node's edge block: `[count][SVB neighbor deltas][types][strengths][metas]`.
fn encode_block(sorted: &[(u64, u32, f32)]) -> Vec<u8> {
    let n = sorted.len();
    let mut out = Vec::new();
    write_varint(&mut out, n as u64);

    // Neighbor ids → deltas → StreamVByte.
    let mut deltas = Vec::with_capacity(n);
    let mut prev = 0u64;
    for &(nid, _, _) in sorted {
        deltas.push(nid - prev);
        prev = nid;
    }
    let mut control = Vec::new();
    let mut data = Vec::new();
    svb_encode(&deltas, &mut control, &mut data);
    out.extend_from_slice(&control);
    out.extend_from_slice(&data);

    // Parallel attribute arrays.
    for &(_, tid, _) in sorted {
        out.extend_from_slice(&tid.to_le_bytes());
    }
    for &(_, _, s) in sorted {
        out.extend_from_slice(&s.to_le_bytes());
    }
    for _ in 0..n {
        out.extend_from_slice(&NO_ID.to_le_bytes()); // meta_ref (Phase 0: none)
    }
    out
}

fn serialize_csr(magic: &[u8; 8], per_node: &[Vec<(u64, u32, f32)>]) -> Vec<u8> {
    let n = per_node.len();
    // Encode each block, remembering its length.
    let blocks: Vec<Vec<u8>> = per_node.iter().map(|e| encode_block(e)).collect();

    // header(16) + count(8) + block_region_start(8) + offsets((n+1)*8) + blocks
    let offsets_start = HEADER_LEN + 8 + 8;
    let block_region_start = offsets_start + (n + 1) * 8;

    let mut out = Vec::new();
    write_header(&mut out, magic, 0);
    out.extend_from_slice(&(n as u64).to_le_bytes());
    out.extend_from_slice(&(block_region_start as u64).to_le_bytes());

    // Offsets = absolute byte offset of each block; offsets[n] = end.
    let mut cursor = block_region_start as u64;
    for b in &blocks {
        out.extend_from_slice(&cursor.to_le_bytes());
        cursor += b.len() as u64;
    }
    out.extend_from_slice(&cursor.to_le_bytes()); // sentinel end

    for b in &blocks {
        out.extend_from_slice(b);
    }
    out
}

/// Build the five topology files from an in-memory graph. Dense ids are assigned in
/// `nodes` order (0..n). Edges whose endpoints aren't in `nodes` are skipped.
pub fn build(nodes: &[TopoNode], edges: &[TopoEdge]) -> TopologyBlob {
    let n = nodes.len();

    // hash → dense id
    let mut hash_to_id: std::collections::HashMap<u64, u64> =
        std::collections::HashMap::with_capacity(n);
    for (i, node) in nodes.iter().enumerate() {
        hash_to_id.insert(node.hash, i as u64);
    }

    let mut colls = Interner::default();
    let mut types = Interner::default();

    // nodes.bin
    let mut nodes_buf = Vec::new();
    write_header(&mut nodes_buf, &MAGIC_NODES, 0);
    nodes_buf.extend_from_slice(&(n as u64).to_le_bytes());
    for node in nodes {
        let coll_id = if node.collection.is_empty() {
            NO_ID
        } else {
            colls.intern(&node.collection)
        };
        nodes_buf.extend_from_slice(&node.payload_offset.to_le_bytes()); // 8
        nodes_buf.extend_from_slice(&node.payload_len.to_le_bytes()); // 4
        nodes_buf.extend_from_slice(&coll_id.to_le_bytes()); // 4
        nodes_buf.extend_from_slice(&NO_ID.to_le_bytes()); // spatial_ref 4
        nodes_buf.extend_from_slice(&0u16.to_le_bytes()); // flags 2
        nodes_buf.extend_from_slice(&[0u8, 0u8]); // pad → 24
    }

    // Adjacency (fwd by from, rev by to), neighbors sorted.
    let mut fwd: Vec<Vec<(u64, u32, f32)>> = vec![Vec::new(); n];
    let mut rev: Vec<Vec<(u64, u32, f32)>> = vec![Vec::new(); n];
    for e in edges {
        let (Some(&fid), Some(&tid)) =
            (hash_to_id.get(&e.from_hash), hash_to_id.get(&e.to_hash))
        else {
            continue; // dangling edge — skip
        };
        let type_id = types.intern(&e.edge_type);
        fwd[fid as usize].push((tid, type_id, e.strength));
        rev[tid as usize].push((fid, type_id, e.strength));
    }
    for v in fwd.iter_mut().chain(rev.iter_mut()) {
        v.sort_by_key(|&(nid, _, _)| nid);
    }

    let fwd_buf = serialize_csr(&MAGIC_ADJF, &fwd);
    let rev_buf = serialize_csr(&MAGIC_ADJR, &rev);

    // idx.bin — sorted (hash, dense_id)
    let mut entries: Vec<(u64, u64)> =
        nodes.iter().enumerate().map(|(i, node)| (node.hash, i as u64)).collect();
    entries.sort_by_key(|&(h, _)| h);
    let mut idx_buf = Vec::new();
    write_header(&mut idx_buf, &MAGIC_IDX, 0);
    idx_buf.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    for (h, id) in &entries {
        idx_buf.extend_from_slice(&h.to_le_bytes());
        idx_buf.extend_from_slice(&id.to_le_bytes());
    }

    // slugs.bin — dense_id → slug string (reverse of idx.bin).
    // Layout: header, count, offsets[(n+1)] into the blob, then the UTF-8 blob.
    let mut slugs_buf = Vec::new();
    write_header(&mut slugs_buf, &MAGIC_SLUG, 0);
    slugs_buf.extend_from_slice(&(n as u64).to_le_bytes());
    let blob_start = (HEADER_LEN + 8 + (n + 1) * 8) as u64;
    let mut cursor = blob_start;
    for node in nodes {
        slugs_buf.extend_from_slice(&cursor.to_le_bytes());
        cursor += node.slug.len() as u64;
    }
    slugs_buf.extend_from_slice(&cursor.to_le_bytes()); // sentinel end
    for node in nodes {
        slugs_buf.extend_from_slice(node.slug.as_bytes());
    }

    // dict.bin — collections then edge types
    let mut dict_buf = Vec::new();
    write_header(&mut dict_buf, &MAGIC_DICT, 0);
    write_string_table(&mut dict_buf, &colls.list);
    write_string_table(&mut dict_buf, &types.list);

    TopologyBlob {
        nodes: nodes_buf,
        fwd: fwd_buf,
        rev: rev_buf,
        idx: idx_buf,
        slugs: slugs_buf,
        dict: dict_buf,
    }
}

fn write_string_table(out: &mut Vec<u8>, list: &[String]) {
    out.extend_from_slice(&(list.len() as u32).to_le_bytes());
    for s in list {
        let bytes = s.as_bytes();
        out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
        out.extend_from_slice(bytes);
    }
}

fn read_string_table(b: &[u8], mut pos: usize) -> (Vec<String>, usize) {
    let count = rd_u32(b, pos) as usize;
    pos += 4;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let len = u16::from_le_bytes([b[pos], b[pos + 1]]) as usize;
        pos += 2;
        out.push(String::from_utf8_lossy(&b[pos..pos + len]).into_owned());
        pos += len;
    }
    (out, pos)
}

// ── Reader (a view over the byte buffers — mmap-ready) ─────────────────────────

pub struct NodeRec {
    pub payload_offset: u64,
    pub payload_len: u32,
    pub collection_id: u32,
}

pub struct EdgeRec {
    pub neighbor: u64, // dense id
    pub edge_type_id: u32,
    pub strength: f32,
}

pub struct TopologyView<'a> {
    nodes: &'a [u8],
    fwd: &'a [u8],
    rev: &'a [u8],
    idx: &'a [u8],
    slugs: &'a [u8],
    collections: Vec<String>,
    edge_types: Vec<String>,
    node_count: usize,
}

impl<'a> TopologyView<'a> {
    pub fn new(blob: &'a TopologyBlob) -> Result<Self, String> {
        check_header(&blob.nodes, &MAGIC_NODES)?;
        check_header(&blob.fwd, &MAGIC_ADJF)?;
        check_header(&blob.rev, &MAGIC_ADJR)?;
        check_header(&blob.idx, &MAGIC_IDX)?;
        check_header(&blob.slugs, &MAGIC_SLUG)?;
        check_header(&blob.dict, &MAGIC_DICT)?;

        let (collections, pos) = read_string_table(&blob.dict, HEADER_LEN);
        let (edge_types, _) = read_string_table(&blob.dict, pos);
        let node_count = rd_u64(&blob.nodes, HEADER_LEN) as usize;

        Ok(Self {
            nodes: &blob.nodes,
            fwd: &blob.fwd,
            rev: &blob.rev,
            idx: &blob.idx,
            slugs: &blob.slugs,
            collections,
            edge_types,
            node_count,
        })
    }

    /// `dense id → slug` — the reverse of [`resolve`](Self::resolve). Touched only
    /// when building results or disambiguating collisions, never during hops.
    pub fn slug(&self, id: u64) -> Option<&'a str> {
        let k = id as usize;
        if k >= self.node_count {
            return None;
        }
        let offsets = HEADER_LEN + 8;
        let start = rd_u64(self.slugs, offsets + k * 8) as usize;
        let end = rd_u64(self.slugs, offsets + (k + 1) * 8) as usize;
        std::str::from_utf8(&self.slugs[start..end]).ok()
    }

    pub fn node_count(&self) -> usize {
        self.node_count
    }

    /// `hash → dense id` via binary search over `idx.bin`. Touched only at query roots.
    pub fn resolve(&self, hash: u64) -> Option<u64> {
        let count = rd_u64(self.idx, HEADER_LEN) as usize;
        let base = HEADER_LEN + 8;
        let (mut lo, mut hi) = (0usize, count);
        while lo < hi {
            let mid = (lo + hi) / 2;
            let h = rd_u64(self.idx, base + mid * 16);
            if h < hash {
                lo = mid + 1;
            } else if h > hash {
                hi = mid;
            } else {
                return Some(rd_u64(self.idx, base + mid * 16 + 8));
            }
        }
        None
    }

    /// Fixed-size node record via arithmetic: `record(k) = data_start + k*24`.
    pub fn node_record(&self, id: u64) -> Option<NodeRec> {
        let k = id as usize;
        if k >= self.node_count {
            return None;
        }
        let o = HEADER_LEN + 8 + k * NODE_RECSIZE;
        Some(NodeRec {
            payload_offset: rd_u64(self.nodes, o),
            payload_len: rd_u32(self.nodes, o + 8),
            collection_id: rd_u32(self.nodes, o + 12),
        })
    }

    pub fn fwd_edges(&self, id: u64) -> Vec<EdgeRec> {
        Self::edges_of(self.fwd, id, self.node_count)
    }
    pub fn rev_edges(&self, id: u64) -> Vec<EdgeRec> {
        Self::edges_of(self.rev, id, self.node_count)
    }

    fn edges_of(csr: &[u8], id: u64, node_count: usize) -> Vec<EdgeRec> {
        let k = id as usize;
        if k >= node_count {
            return Vec::new();
        }
        let offsets_start = HEADER_LEN + 8 + 8;
        let start = rd_u64(csr, offsets_start + k * 8) as usize;
        let end = rd_u64(csr, offsets_start + (k + 1) * 8) as usize;
        let block = &csr[start..end];

        let (count, mut pos) = read_varint(block, 0);
        let count = count as usize;
        if count == 0 {
            return Vec::new();
        }
        let ctrl_len = (count + 3) / 4;
        let control = &block[pos..pos + ctrl_len];
        pos += ctrl_len;
        let (deltas, used) = svb_decode(control, &block[pos..], count);
        pos += used;

        // prefix-sum deltas → absolute neighbor ids
        let mut neighbors = Vec::with_capacity(count);
        let mut acc = 0u64;
        for d in deltas {
            acc += d;
            neighbors.push(acc);
        }

        let types_at = pos;
        let strengths_at = types_at + count * 4;
        // meta array follows strengths (unused in Phase 0)

        (0..count)
            .map(|i| EdgeRec {
                neighbor: neighbors[i],
                edge_type_id: rd_u32(block, types_at + i * 4),
                strength: rd_f32(block, strengths_at + i * 4),
            })
            .collect()
    }

    pub fn collection_name(&self, id: u32) -> Option<&str> {
        if id == NO_ID {
            None
        } else {
            self.collections.get(id as usize).map(|s| s.as_str())
        }
    }
    pub fn edge_type_name(&self, id: u32) -> Option<&str> {
        self.edge_types.get(id as usize).map(|s| s.as_str())
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> u64 {
        crate::sk_hash(s)
    }

    #[test]
    fn streamvbyte_roundtrip_all_length_classes() {
        let vals: Vec<u64> = vec![0, 1, 255, 256, 65535, 65536, 4_000_000_000, 1u64 << 40];
        let mut ctrl = Vec::new();
        let mut data = Vec::new();
        svb_encode(&vals, &mut ctrl, &mut data);
        let (got, used) = svb_decode(&ctrl, &data, vals.len());
        assert_eq!(got, vals);
        assert_eq!(used, data.len());
    }

    #[test]
    fn topology_roundtrip_nodes_edges_names() {
        // Bali-themed mini graph: tourists → places, places → area.
        let nodes = vec![
            TopoNode { hash: h("t/chloe"),  slug: "t/chloe".into(),  collection: "tourist".into(), payload_offset: 0,   payload_len: 10 },
            TopoNode { hash: h("p/uluwatu"),slug: "p/uluwatu".into(),collection: "place".into(),   payload_offset: 10,  payload_len: 20 },
            TopoNode { hash: h("p/ubud"),   slug: "p/ubud".into(),   collection: "place".into(),   payload_offset: 30,  payload_len: 15 },
            TopoNode { hash: h("a/south"),  slug: "a/south".into(),  collection: "area".into(),    payload_offset: 45,  payload_len: 5  },
        ];
        let edges = vec![
            TopoEdge { from_hash: h("t/chloe"),   to_hash: h("p/uluwatu"), edge_type: "visited".into(), strength: 1.0 },
            TopoEdge { from_hash: h("t/chloe"),   to_hash: h("p/ubud"),    edge_type: "visited".into(), strength: 0.5 },
            TopoEdge { from_hash: h("p/uluwatu"), to_hash: h("a/south"),   edge_type: "in_area".into(), strength: 1.0 },
        ];

        let blob = build(&nodes, &edges);
        let view = TopologyView::new(&blob).unwrap();
        assert_eq!(view.node_count(), 4);

        // Name resolution round-trips to the dense id assigned in `nodes` order,
        // and slugs.bin gives the reverse mapping back.
        for (i, node) in nodes.iter().enumerate() {
            assert_eq!(view.resolve(node.hash), Some(i as u64), "resolve {}", node.slug);
            assert_eq!(view.slug(i as u64), Some(node.slug.as_str()), "slug of id {i}");
        }
        assert_eq!(view.resolve(h("does/not/exist")), None);
        assert_eq!(view.slug(99), None);

        // Node records carry payload offset/len + collection.
        let chloe = view.resolve(h("t/chloe")).unwrap();
        let rec = view.node_record(chloe).unwrap();
        assert_eq!((rec.payload_offset, rec.payload_len), (0, 10));
        assert_eq!(view.collection_name(rec.collection_id), Some("tourist"));

        // Forward edges from Chloe: two "visited" places, sorted by neighbor id.
        let out = view.fwd_edges(chloe);
        assert_eq!(out.len(), 2);
        let ulu = view.resolve(h("p/uluwatu")).unwrap();
        let ubud = view.resolve(h("p/ubud")).unwrap();
        let neigh: std::collections::HashSet<u64> = out.iter().map(|e| e.neighbor).collect();
        assert_eq!(neigh, [ulu, ubud].into_iter().collect());
        for e in &out {
            assert_eq!(view.edge_type_name(e.edge_type_id), Some("visited"));
        }

        // Reverse edges into a/south: one edge, from uluwatu, type "in_area".
        let south = view.resolve(h("a/south")).unwrap();
        let rin = view.rev_edges(south);
        assert_eq!(rin.len(), 1);
        assert_eq!(rin[0].neighbor, ulu);
        assert_eq!(view.edge_type_name(rin[0].edge_type_id), Some("in_area"));

        // A node with no outgoing edges.
        assert!(view.fwd_edges(south).is_empty());
    }

    #[test]
    fn topology_empty_graph() {
        let blob = build(&[], &[]);
        let view = TopologyView::new(&blob).unwrap();
        assert_eq!(view.node_count(), 0);
        assert_eq!(view.resolve(h("x")), None);
    }

    #[test]
    fn topology_headers_present_and_versioned() {
        let blob = build(&[], &[]);
        assert_eq!(&blob.nodes[0..8], b"SKNODE\0\0");
        assert_eq!(&blob.fwd[0..8], b"SKADJF\0\0");
        assert_eq!(rd_u32(&blob.nodes, 8), TOPO_VERSION);
    }
}
