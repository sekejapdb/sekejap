//! # Topology — the graph (nodes + edges) as memory-mappable files
//!
//! "Topology" is the shape of the graph: which nodes exist and which edges connect
//! them. This module turns that in-memory graph into a set of flat, pointer-free
//! byte buffers on disk, and reads them back — so in paged mode the graph can be
//! served straight from an mmap (`MappedTopology`, held by `CoreDB.topo_base`)
//! with the write-since-open overlay merged on top.
//!
//! Two ideas make it mappable with no parsing:
//! - **Dense ids.** Nodes are numbered 0, 1, 2, … so node `k`'s fixed-size record
//!   is at `data_start + k * record_size` — pure arithmetic, no lookup table.
//! - **CSR adjacency** (Compressed Sparse Row). Instead of a list-per-node
//!   (pointers everywhere), all edge blocks are packed back-to-back, and an
//!   `offsets[]` array says where each node's block starts. That's the standard
//!   compact way to store a graph's neighbours in one contiguous buffer.
//!
//! Edge blocks use StreamVByte delta encoding (small, SIMD-friendly integers).
//! Each file starts with a `[magic 8][version u32][flags u32]` header. See
//! `docs/developer/notes/topology-format.md` for the byte-level details.
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
//! `[count varint][neighbor deltas: StreamVByte][type_ids u32×n][meta u32×n]`
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
const MAGIC_COLL: [u8; 8] = *b"SKCOLL\0\0";
const MAGIC_EMET: [u8; 8] = *b"SKEMET\0\0";
const MAGIC_SPAT: [u8; 8] = *b"SKSPAT\0\0";

/// Topology format version (independent of the snapshot version).
/// v1 = 24 B node records (no hash — derived from the slug on read).
/// v2 = 32 B node records with the slug hash inline, giving O(1) `id → hash`
///      (needed to serve the hash-keyed engine API from mmap without re-hashing
///      slug strings on every edge).
const TOPO_VERSION: u32 = 2;
const HEADER_LEN: usize = 16;
const NODE_RECSIZE_V1: usize = 24;
const NODE_RECSIZE_V2: usize = 32;
/// Sentinel for "no collection" / "no edge metadata".
pub const NO_ID: u32 = u32::MAX;

#[inline]
fn node_recsize(version: u32) -> usize {
    if version >= 2 { NODE_RECSIZE_V2 } else { NODE_RECSIZE_V1 }
}

// ── Input (what the builder consumes) ─────────────────────────────────────────

pub struct TopoNode<'a> {
    pub hash: u64,
    /// Borrowed, not owned. Building this list used to clone the slug and the
    /// collection name for every node — two million String allocations on a
    /// million-node compaction, of data the caller already holds.
    pub slug: &'a str,
    pub collection: &'a str,
    pub payload_offset: u64,
    pub payload_len: u32,
    /// Spatial metadata, if the node has geometry: 6 f64s =
    /// [centroid_lat, centroid_lon, bbox_min_lat, bbox_min_lon, bbox_max_lat, bbox_max_lon].
    /// Stored in the `spatial.bin` side-table; `NodeRecord.spatial_ref` points at it.
    ///
    /// Boxed because it is almost always `None`: inline, the six f64s cost 56 bytes
    /// on every node in the graph whether it has geometry or not — tens of megabytes
    /// of zeroes on a large store. Boxed it is 8 bytes for a node without geometry
    /// and one allocation for a node with it.
    pub spatial: Option<Box<[f64; 6]>>,
}

pub struct TopoEdge<'a> {
    pub from_hash: u64,
    pub to_hash: u64,
    /// Borrowed when the type has a registered name — which is the normal case and
    /// there are only a handful of distinct names — owned only for the hex fallback.
    pub edge_type: std::borrow::Cow<'a, str>,
    /// Raw JSON edge metadata, if any (stored sparsely in `edgemeta.bin`).
    pub meta: Option<String>,
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
    /// `spatial.bin` — sparse side-table of 48-byte spatial records
    /// (`NodeRecord.spatial_ref` → 6×f64), so paged mode and recovery get spatial
    /// metadata without parsing payloads.
    pub spat: Vec<u8>,
    /// `edgemeta.bin` — sparse edge-metadata blobs (`meta_ref` → JSON bytes).
    pub emeta: Vec<u8>,
    /// `collections.bin` — per-collection posting lists of member dense ids
    /// (sorted, delta+StreamVByte encoded like adjacency), so collection scans in
    /// paged mode don't require a full `nodes.bin` sweep.
    pub colls: Vec<u8>,
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

/// Serialize one node's edge block: `[count][SVB neighbor deltas][types][metas]`.
/// Tuple = `(neighbor_id, edge_type_id, meta_ref)`; `meta_ref` indexes
/// `edgemeta.bin` (`NO_ID` = no metadata). Edges are naked — no weight in the
/// topology; any weight is an opt-in JSON/column attribute in `edgemeta.bin`.
fn encode_block(sorted: &[(u64, u32, u32)]) -> Vec<u8> {
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
    for &(_, _, m) in sorted {
        out.extend_from_slice(&m.to_le_bytes());
    }
    out
}

/// `rows` must be sorted by `(owner_id, neighbour_id)` — CSR order.
///
/// Encodes into one shared blob rather than a `Vec<u8>` per node: the previous
/// version allocated a vector for every node in the graph, twice over, purely to
/// concatenate them a moment later.
fn serialize_csr(magic: &[u8; 8], rows: &[(u64, u64, u32, u32)], n: usize) -> Vec<u8> {
    let mut blob: Vec<u8> = Vec::new();
    let mut spans: Vec<(usize, usize)> = Vec::with_capacity(n);
    let mut scratch: Vec<(u64, u32, u32)> = Vec::new();
    let mut cur = 0usize;
    for owner in 0..n as u64 {
        scratch.clear();
        while cur < rows.len() && rows[cur].0 == owner {
            let (_, nid, t, m) = rows[cur];
            scratch.push((nid, t, m));
            cur += 1;
        }
        let start = blob.len();
        blob.extend_from_slice(&encode_block(&scratch));
        spans.push((start, blob.len() - start));
    }

    // header(16) + count(8) + block_region_start(8) + offsets((n+1)*8) + blocks
    let offsets_start = HEADER_LEN + 8 + 8;
    let block_region_start = offsets_start + (n + 1) * 8;

    let mut out = Vec::new();
    write_header(&mut out, magic, 0);
    out.extend_from_slice(&(n as u64).to_le_bytes());
    out.extend_from_slice(&(block_region_start as u64).to_le_bytes());

    // Offsets = absolute byte offset of each block; offsets[n] = end.
    let mut cursor = block_region_start as u64;
    for &(_, len) in &spans {
        out.extend_from_slice(&cursor.to_le_bytes());
        cursor += len as u64;
    }
    out.extend_from_slice(&cursor.to_le_bytes()); // sentinel end
    out.extend_from_slice(&blob);
    out
}

/// Build the five topology files from an in-memory graph. Dense ids are assigned in
/// `nodes` order (0..n). Edges whose endpoints aren't in `nodes` are skipped.
pub fn build(nodes: &[TopoNode<'_>], edges: &[TopoEdge<'_>]) -> TopologyBlob {
    let n = nodes.len();

    // hash → dense id
    let mut hash_to_id: std::collections::HashMap<u64, u64> =
        std::collections::HashMap::with_capacity(n);
    for (i, node) in nodes.iter().enumerate() {
        hash_to_id.insert(node.hash, i as u64);
    }

    let mut colls = Interner::default();
    let mut types = Interner::default();

    // nodes.bin — v2 records (32 B): hash inline for O(1) id → hash.
    // spatial.bin — sparse 48-byte records; spatial_ref indexes into it.
    let mut nodes_buf = Vec::new();
    let mut spat_buf = Vec::new();
    write_header(&mut nodes_buf, &MAGIC_NODES, 0);
    write_header(&mut spat_buf, &MAGIC_SPAT, 0);
    let mut spat_count: u64 = 0;
    spat_buf.extend_from_slice(&spat_count.to_le_bytes()); // patched below
    nodes_buf.extend_from_slice(&(n as u64).to_le_bytes());
    for node in nodes {
        let coll_id = if node.collection.is_empty() {
            NO_ID
        } else {
            colls.intern(&node.collection)
        };
        let spatial_ref = match &node.spatial {
            Some(vals) => {
                let r = spat_count as u32;
                for v in vals.iter() {
                    spat_buf.extend_from_slice(&v.to_le_bytes());
                }
                spat_count += 1;
                r
            }
            None => NO_ID,
        };
        nodes_buf.extend_from_slice(&node.hash.to_le_bytes()); // 8
        nodes_buf.extend_from_slice(&node.payload_offset.to_le_bytes()); // 8
        nodes_buf.extend_from_slice(&node.payload_len.to_le_bytes()); // 4
        nodes_buf.extend_from_slice(&coll_id.to_le_bytes()); // 4
        nodes_buf.extend_from_slice(&spatial_ref.to_le_bytes()); // 4
        nodes_buf.extend_from_slice(&0u16.to_le_bytes()); // flags 2
        nodes_buf.extend_from_slice(&[0u8, 0u8]); // pad → 32
    }
    spat_buf[HEADER_LEN..HEADER_LEN + 8].copy_from_slice(&spat_count.to_le_bytes());

    // Adjacency (fwd by from, rev by to), neighbors sorted. Edge metadata is
    // interned into edgemeta.bin; both directions share one meta_ref.
    // Flat `(owner_id, neighbour_id, type, meta)` rows, one allocation per
    // direction, sorted once at the end. This used to be `vec![Vec::new(); n]`
    // twice — two million vector headers plus a small allocation every time an
    // edge was pushed, which on a million-node graph is where both the time and
    // the fragmentation came from.
    let mut fwd: Vec<(u64, u64, u32, u32)> = Vec::with_capacity(edges.len());
    let mut rev: Vec<(u64, u64, u32, u32)> = Vec::with_capacity(edges.len());
    let mut meta_blobs: Vec<&str> = Vec::new();
    for e in edges {
        let (Some(&fid), Some(&tid)) =
            (hash_to_id.get(&e.from_hash), hash_to_id.get(&e.to_hash))
        else {
            continue; // dangling edge — skip
        };
        let type_id = types.intern(e.edge_type.as_ref());
        let meta_ref = match &e.meta {
            Some(m) => {
                let r = meta_blobs.len() as u32;
                meta_blobs.push(m.as_str());
                r
            }
            None => NO_ID,
        };
        fwd.push((fid, tid, type_id, meta_ref));
        rev.push((tid, fid, type_id, meta_ref));
    }
    // Sorted by owner then neighbour, which is exactly CSR order — so the blocks
    // can be walked straight out of these without regrouping.
    fwd.sort_unstable_by_key(|&(owner, nid, _, _)| (owner, nid));
    rev.sort_unstable_by_key(|&(owner, nid, _, _)| (owner, nid));

    // edgemeta.bin — sparse blobs: header, count, offsets[(m+1)], JSON bytes.
    let mut emeta_buf = Vec::new();
    write_header(&mut emeta_buf, &MAGIC_EMET, 0);
    let m = meta_blobs.len();
    emeta_buf.extend_from_slice(&(m as u64).to_le_bytes());
    let mut cursor = (HEADER_LEN + 8 + (m + 1) * 8) as u64;
    for b in &meta_blobs {
        emeta_buf.extend_from_slice(&cursor.to_le_bytes());
        cursor += b.len() as u64;
    }
    emeta_buf.extend_from_slice(&cursor.to_le_bytes());
    for b in &meta_blobs {
        emeta_buf.extend_from_slice(b.as_bytes());
    }

    let fwd_buf = serialize_csr(&MAGIC_ADJF, &fwd, n);
    let rev_buf = serialize_csr(&MAGIC_ADJR, &rev, n);

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

    // collections.bin — per-collection posting lists of member dense ids.
    // Members are ascending (nodes iterated in dense-id order) → delta + SVB.
    let mut coll_members: Vec<Vec<u64>> = vec![Vec::new(); colls.list.len()];
    for (i, node) in nodes.iter().enumerate() {
        if !node.collection.is_empty() {
            if let Some(&cid) = colls.map.get(node.collection) {
                coll_members[cid as usize].push(i as u64);
            }
        }
    }
    let coll_blocks: Vec<Vec<u8>> = coll_members
        .iter()
        .map(|members| {
            let mut block = Vec::new();
            write_varint(&mut block, members.len() as u64);
            let mut deltas = Vec::with_capacity(members.len());
            let mut prev = 0u64;
            for &m in members {
                deltas.push(m - prev);
                prev = m;
            }
            let mut control = Vec::new();
            let mut data = Vec::new();
            svb_encode(&deltas, &mut control, &mut data);
            block.extend_from_slice(&control);
            block.extend_from_slice(&data);
            block
        })
        .collect();
    let mut colls_buf = Vec::new();
    write_header(&mut colls_buf, &MAGIC_COLL, 0);
    let ncolls = coll_blocks.len();
    colls_buf.extend_from_slice(&(ncolls as u64).to_le_bytes());
    let mut cursor = (HEADER_LEN + 8 + (ncolls + 1) * 8) as u64;
    for b in &coll_blocks {
        colls_buf.extend_from_slice(&cursor.to_le_bytes());
        cursor += b.len() as u64;
    }
    colls_buf.extend_from_slice(&cursor.to_le_bytes());
    for b in &coll_blocks {
        colls_buf.extend_from_slice(b);
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
        spat: spat_buf,
        emeta: emeta_buf,
        colls: colls_buf,
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
    /// `sk_hash(slug)` — inline since v2 (v1 readers derive it from the slug).
    pub hash: u64,
    pub payload_offset: u64,
    pub payload_len: u32,
    pub collection_id: u32,
    /// Index into `spatial.bin` (`NO_ID` = no geometry).
    pub spatial_ref: u32,
}

pub struct EdgeRec {
    pub neighbor: u64, // dense id
    pub edge_type_id: u32,
    /// Index into `edgemeta.bin` (`NO_ID` = no metadata).
    pub meta_ref: u32,
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
    /// Format version of `nodes.bin` (decides the record size / hash presence).
    version: u32,
}

impl<'a> TopologyView<'a> {
    pub fn new(blob: &'a TopologyBlob) -> Result<Self, String> {
        Self::from_slices(&blob.nodes, &blob.fwd, &blob.rev, &blob.idx, &blob.slugs, &blob.dict)
    }

    /// Build a view over raw slices — the mmap path hands in mapped byte ranges.
    pub fn from_slices(
        nodes: &'a [u8],
        fwd: &'a [u8],
        rev: &'a [u8],
        idx: &'a [u8],
        slugs: &'a [u8],
        dict: &'a [u8],
    ) -> Result<Self, String> {
        check_header(nodes, &MAGIC_NODES)?;
        check_header(fwd, &MAGIC_ADJF)?;
        check_header(rev, &MAGIC_ADJR)?;
        check_header(idx, &MAGIC_IDX)?;
        check_header(slugs, &MAGIC_SLUG)?;
        check_header(dict, &MAGIC_DICT)?;

        let (collections, pos) = read_string_table(dict, HEADER_LEN);
        let (edge_types, _) = read_string_table(dict, pos);
        let node_count = rd_u64(nodes, HEADER_LEN) as usize;
        let version = rd_u32(nodes, 8);

        Ok(Self {
            nodes,
            fwd,
            rev,
            idx,
            slugs,
            collections,
            edge_types,
            node_count,
            version,
        })
    }

    /// O(1) `dense id → slug hash`. v2 reads it from the record; v1 derives it
    /// from the slug string.
    pub fn hash_of(&self, id: u64) -> Option<u64> {
        if self.version >= 2 {
            let k = id as usize;
            if k >= self.node_count {
                return None;
            }
            Some(rd_u64(self.nodes, HEADER_LEN + 8 + k * NODE_RECSIZE_V2))
        } else {
            self.slug(id).map(crate::sk_hash)
        }
    }

    /// `dense id → slug` — the reverse of [`resolve`](Self::resolve). Touched only
    /// when building results or disambiguating collisions, never during hops.
    pub fn slug(&self, id: u64) -> Option<&'a str> {
        slug_in(self.slugs, self.node_count, id)
    }

    pub fn node_count(&self) -> usize {
        self.node_count
    }

    /// `hash → dense id` via binary search over `idx.bin`. Touched only at query roots.
    pub fn resolve(&self, hash: u64) -> Option<u64> {
        resolve_in(self.idx, hash, None)
    }

    /// Fixed-size node record via arithmetic: `record(k) = data_start + k*recsize`.
    pub fn node_record(&self, id: u64) -> Option<NodeRec> {
        let k = id as usize;
        if k >= self.node_count {
            return None;
        }
        let o = HEADER_LEN + 8 + k * node_recsize(self.version);
        if self.version >= 2 {
            Some(NodeRec {
                hash: rd_u64(self.nodes, o),
                payload_offset: rd_u64(self.nodes, o + 8),
                payload_len: rd_u32(self.nodes, o + 16),
                collection_id: rd_u32(self.nodes, o + 20),
                spatial_ref: rd_u32(self.nodes, o + 24),
            })
        } else {
            Some(NodeRec {
                hash: self.slug(id).map(crate::sk_hash).unwrap_or(0),
                payload_offset: rd_u64(self.nodes, o),
                payload_len: rd_u32(self.nodes, o + 8),
                collection_id: rd_u32(self.nodes, o + 12),
                spatial_ref: rd_u32(self.nodes, o + 16),
            })
        }
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
        let metas_at = types_at + count * 4;

        (0..count)
            .map(|i| EdgeRec {
                neighbor: neighbors[i],
                edge_type_id: rd_u32(block, types_at + i * 4),
                meta_ref: rd_u32(block, metas_at + i * 4),
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

// ── Shared low-level readers (used by TopologyView and MappedTopology) ─────────

/// Binary search `idx.bin` for `hash`. With a sparse index (every
/// `SPARSE_STRIDE`-th hash, resident in RAM), the search is first narrowed to one
/// stride-sized window so a cold lookup touches ~1 page instead of ~log2(n).
fn resolve_in(idx: &[u8], hash: u64, sparse: Option<&[u64]>) -> Option<u64> {
    let count = rd_u64(idx, HEADER_LEN) as usize;
    let base = HEADER_LEN + 8;
    let (mut lo, mut hi) = match sparse {
        Some(s) if !s.is_empty() => {
            // First window whose leading hash is > target starts after our window.
            let w = s.partition_point(|&h| h <= hash);
            let lo = w.saturating_sub(1) * SPARSE_STRIDE;
            let hi = (w * SPARSE_STRIDE).min(count);
            (lo, hi)
        }
        _ => (0usize, count),
    };
    while lo < hi {
        let mid = (lo + hi) / 2;
        let h = rd_u64(idx, base + mid * 16);
        if h < hash {
            lo = mid + 1;
        } else if h > hash {
            hi = mid;
        } else {
            return Some(rd_u64(idx, base + mid * 16 + 8));
        }
    }
    None
}

fn slug_in(slugs: &[u8], node_count: usize, id: u64) -> Option<&str> {
    let k = id as usize;
    if k >= node_count {
        return None;
    }
    let offsets = HEADER_LEN + 8;
    let start = rd_u64(slugs, offsets + k * 8) as usize;
    let end = rd_u64(slugs, offsets + (k + 1) * 8) as usize;
    std::str::from_utf8(&slugs[start..end]).ok()
}

/// Entries per sparse-index bucket for `idx.bin` (16 B/entry → 4 KB pages hold 256;
/// 256 keeps each narrowed window within ~1 page).
const SPARSE_STRIDE: usize = 256;

/// Read one 48-byte spatial record out of raw `spatial.bin` bytes.
/// Returns the 6 f64s or `None` on `NO_ID`/OOB/absent file.
pub(crate) fn spatial_at(spat: &[u8], spatial_ref: u32) -> Option<[f64; 6]> {
    if spatial_ref == NO_ID || spat.len() < HEADER_LEN + 8 {
        return None;
    }
    let count = rd_u64(spat, HEADER_LEN) as usize;
    let k = spatial_ref as usize;
    if k >= count {
        return None;
    }
    let o = HEADER_LEN + 8 + k * 48;
    let mut vals = [0f64; 6];
    for (i, v) in vals.iter_mut().enumerate() {
        *v = f64::from_le_bytes(spat[o + i * 8..o + i * 8 + 8].try_into().ok()?);
    }
    Some(vals)
}

/// Read one edge-metadata blob out of raw `edgemeta.bin` bytes (free-standing so
/// the recovery path can use it over an owned buffer). `None` on absence/OOB.
pub(crate) fn emeta_bytes_at(emeta: &[u8], meta_ref: u32) -> Option<&[u8]> {
    if meta_ref == NO_ID || emeta.len() < HEADER_LEN + 8 {
        return None;
    }
    let count = rd_u64(emeta, HEADER_LEN) as usize;
    let k = meta_ref as usize;
    if k >= count {
        return None;
    }
    let offsets = HEADER_LEN + 8;
    let start = rd_u64(emeta, offsets + k * 8) as usize;
    let end = rd_u64(emeta, offsets + (k + 1) * 8) as usize;
    emeta.get(start..end)
}

// ── MappedTopology — the Phase 1 store: files served via mmap ─────────────────

/// One topology file, mmap'd when possible (unix), owned bytes otherwise. The OS
/// page cache then holds hot pages and evicts cold ones — RAM adapts automatically.
enum Backing {
    #[cfg(unix)]
    Map {
        /// Kept open for the lifetime of the mapping.
        _file: std::fs::File,
        map: super::mmap::MmapView,
    },
    Owned(Vec<u8>),
}

impl Backing {
    fn open(path: &std::path::Path) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            let file = std::fs::File::open(path)?;
            let len = file.metadata()?.len() as usize;
            if let Some(map) = super::mmap::MmapView::try_new(&file, len) {
                return Ok(Backing::Map { _file: file, map });
            }
        }
        Ok(Backing::Owned(std::fs::read(path)?))
    }

    fn bytes(&self) -> &[u8] {
        match self {
            #[cfg(unix)]
            Backing::Map { map, .. } => map.slice(0, map.len()).unwrap_or(&[]),
            Backing::Owned(v) => v.as_slice(),
        }
    }
}

/// An edge as the hash-keyed engine sees it: neighbor + edge-type as slug hashes.
pub struct MappedEdge {
    pub other_hash: u64,
    pub edge_type_hash: u64,
    /// Index into `edgemeta.bin` (`NO_ID` = none).
    pub meta_ref: u32,
}

/// mmap-backed topology store with a **hash-keyed** API mirroring the engine's
/// (`fwd_edges(hash)` / `rev_edges(hash)` / `node lookup`), so it can slot behind
/// the existing executor. Per call: one `idx.bin` resolve (sparse-narrowed binary
/// search), then pure offset arithmetic + StreamVByte decode.
pub struct MappedTopology {
    nodes: Backing,
    fwd: Backing,
    rev: Backing,
    idx: Backing,
    slugs: Backing,
    node_count: usize,
    version: u32,
    collections: Vec<String>,
    edge_types: Vec<String>,
    /// `edge_type_id → sk_hash(name)` — precomputed so edges convert in O(1).
    type_hashes: Vec<u64>,
    /// Every `SPARSE_STRIDE`-th hash from `idx.bin`, resident (~16 B per 256 nodes).
    sparse: Vec<u64>,
    /// `collections.bin` — per-collection member posting lists.
    colls: Backing,
    /// `sk_hash(collection name) → collection_id` (tiny; one entry per collection).
    coll_hash_to_id: std::collections::HashMap<u64, u32>,
    /// `edgemeta.bin` — sparse edge-metadata JSON blobs.
    emeta: Backing,
    /// `spatial.bin` — sparse 48-byte spatial records.
    spat: Backing,
}

impl MappedTopology {
    pub fn open(dir: &std::path::Path) -> std::io::Result<Self> {
        let nodes = Backing::open(&dir.join("nodes.bin"))?;
        let fwd = Backing::open(&dir.join("adj_fwd.bin"))?;
        let rev = Backing::open(&dir.join("adj_rev.bin"))?;
        let idx = Backing::open(&dir.join("idx.bin"))?;
        let slugs = Backing::open(&dir.join("slugs.bin"))?;
        let dict = std::fs::read(dir.join("dict.bin"))?; // tiny — parsed once, not kept

        // Validate headers + parse dictionaries via the existing view logic.
        let view = TopologyView::from_slices(
            nodes.bytes(),
            fwd.bytes(),
            rev.bytes(),
            idx.bytes(),
            slugs.bytes(),
            &dict,
        )
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let node_count = view.node_count();
        let version = view.version;
        let collections = view.collections.clone();
        let edge_types = view.edge_types.clone();
        let type_hashes = edge_types.iter().map(|t| crate::sk_hash(t)).collect();
        let colls = Backing::open(&dir.join("collections.bin"))?;
        check_header(colls.bytes(), &MAGIC_COLL)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        // edgemeta.bin: tolerate absence (dirs compacted by earlier builds) —
        // synthesize an empty, valid blob so meta lookups just return None.
        let emeta = match Backing::open(&dir.join("edgemeta.bin")) {
            Ok(b) => {
                check_header(b.bytes(), &MAGIC_EMET)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                b
            }
            Err(_) => {
                let mut empty = Vec::new();
                write_header(&mut empty, &MAGIC_EMET, 0);
                empty.extend_from_slice(&0u64.to_le_bytes()); // count
                empty.extend_from_slice(&((HEADER_LEN + 16) as u64).to_le_bytes()); // sentinel
                Backing::Owned(empty)
            }
        };
        let coll_hash_to_id = collections
            .iter()
            .enumerate()
            .map(|(i, name)| (crate::sk_hash(name), i as u32))
            .collect();

        // Resident sparse index over idx.bin (every SPARSE_STRIDE-th hash).
        let idx_bytes = idx.bytes();
        let count = rd_u64(idx_bytes, HEADER_LEN) as usize;
        let base = HEADER_LEN + 8;
        let sparse: Vec<u64> = (0..count)
            .step_by(SPARSE_STRIDE)
            .map(|i| rd_u64(idx_bytes, base + i * 16))
            .collect();

        // spatial.bin: tolerate absence (older dirs) — empty blob → no spatial.
        let spat = match Backing::open(&dir.join("spatial.bin")) {
            Ok(b) => {
                check_header(b.bytes(), &MAGIC_SPAT)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
                b
            }
            Err(_) => {
                let mut empty = Vec::new();
                write_header(&mut empty, &MAGIC_SPAT, 0);
                empty.extend_from_slice(&0u64.to_le_bytes());
                Backing::Owned(empty)
            }
        };

        Ok(Self {
            nodes,
            fwd,
            rev,
            idx,
            slugs,
            node_count,
            version,
            collections,
            edge_types,
            type_hashes,
            sparse,
            colls,
            coll_hash_to_id,
            emeta,
            spat,
        })
    }

    pub fn node_count(&self) -> usize {
        self.node_count
    }

    /// Member node *hashes* of the collection with this name hash. Decodes the
    /// posting list (delta+SVB dense ids) and maps each to its slug hash in O(1).
    pub fn members_by_coll_hash(&self, coll_hash: u64) -> Option<Vec<u64>> {
        let cid = *self.coll_hash_to_id.get(&coll_hash)? as usize;
        let b = self.colls.bytes();
        let ncolls = rd_u64(b, HEADER_LEN) as usize;
        if cid >= ncolls {
            return None;
        }
        let offsets = HEADER_LEN + 8;
        let start = rd_u64(b, offsets + cid * 8) as usize;
        let end = rd_u64(b, offsets + (cid + 1) * 8) as usize;
        let block = &b[start..end];
        let (count, mut pos) = read_varint(block, 0);
        let count = count as usize;
        let ctrl_len = (count + 3) / 4;
        let control = &block[pos..pos + ctrl_len];
        pos += ctrl_len;
        let (deltas, _) = svb_decode(control, &block[pos..], count);
        let mut acc = 0u64;
        let mut out = Vec::with_capacity(count);
        for d in deltas {
            acc += d;
            if let Some(h) = self.hash_of(acc) {
                out.push(h);
            }
        }
        Some(out)
    }

        /// Raw JSON bytes of an edge-metadata blob (`meta_ref` from a [`MappedEdge`]).
    pub fn edge_meta_bytes(&self, meta_ref: u32) -> Option<&[u8]> {
        emeta_bytes_at(self.emeta.bytes(), meta_ref)
    }

    /// Spatial record for a node (6 f64s), or `None` if it has no geometry.
    pub fn spatial(&self, id: u64) -> Option<[f64; 6]> {
        let rec = self.node_record(id)?;
        spatial_at(self.spat.bytes(), rec.spatial_ref)
    }

    /// All node hashes in the store (iterates `nodes.bin` records — O(n), used by
    /// `SELECT … FROM ALL` style scans).
    pub fn all_hashes(&self) -> Vec<u64> {
        (0..self.node_count as u64).filter_map(|id| self.hash_of(id)).collect()
    }

    /// `sk_hash(collection name) → name` (for collection_name lookups in paged mode).
    /// Every collection name recorded in the base. Needed so callers that enumerate
    /// collections do not miss ones that exist only in the mmap'd base.
    pub fn collection_names(&self) -> &[String] {
        &self.collections
    }

    pub fn collection_name_by_hash(&self, coll_hash: u64) -> Option<&str> {
        self.coll_hash_to_id
            .get(&coll_hash)
            .and_then(|&id| self.collections.get(id as usize))
            .map(|s| s.as_str())
    }

    /// `slug hash → dense id` (sparse-narrowed binary search over the mmap'd idx).
    pub fn resolve(&self, hash: u64) -> Option<u64> {
        resolve_in(self.idx.bytes(), hash, Some(&self.sparse))
    }

    pub fn slug_of(&self, id: u64) -> Option<&str> {
        slug_in(self.slugs.bytes(), self.node_count, id)
    }

    pub fn node_record(&self, id: u64) -> Option<NodeRec> {
        let k = id as usize;
        if k >= self.node_count {
            return None;
        }
        let b = self.nodes.bytes();
        let o = HEADER_LEN + 8 + k * node_recsize(self.version);
        if self.version >= 2 {
            Some(NodeRec {
                hash: rd_u64(b, o),
                payload_offset: rd_u64(b, o + 8),
                payload_len: rd_u32(b, o + 16),
                collection_id: rd_u32(b, o + 20),
                spatial_ref: rd_u32(b, o + 24),
            })
        } else {
            Some(NodeRec {
                hash: self.slug_of(id).map(crate::sk_hash).unwrap_or(0),
                payload_offset: rd_u64(b, o),
                payload_len: rd_u32(b, o + 8),
                collection_id: rd_u32(b, o + 12),
                spatial_ref: rd_u32(b, o + 16),
            })
        }
    }

    pub fn collection_name(&self, id: u32) -> Option<&str> {
        if id == NO_ID {
            None
        } else {
            self.collections.get(id as usize).map(|s| s.as_str())
        }
    }

    /// Outgoing edges of the node with this slug hash, converted to the engine's
    /// hash-keyed shape. `None` = node unknown.
    pub fn fwd_by_hash(&self, hash: u64) -> Option<Vec<MappedEdge>> {
        self.edges_by_hash(hash, /*fwd=*/ true)
    }
    /// Every edge type this base knows, as `(hash, name)`.
    ///
    /// The names are persisted here but the live edge store only learns them by
    /// observing a `link()`. On a paged reopen no links are made, so without this
    /// every edge served from the base would come back with no type name at all —
    /// which is what made graph introspection look empty.
    pub fn edge_type_table(&self) -> Vec<(u64, String)> {
        self.type_hashes.iter().copied().zip(self.edge_types.iter().cloned()).collect()
    }

    /// Total forward edges, without decoding any of them.
    ///
    /// Each CSR block begins with a varint count, so the total is one varint read
    /// per node — no delta decoding, no prefix sums, no allocation. Used by the
    /// post-compaction verification, which has to be cheap enough to run on every
    /// compaction or it will not be run at all.
    pub fn edge_count(&self) -> usize {
        let csr = self.fwd.bytes();
        let offsets_start = HEADER_LEN + 8 + 8;
        let need = offsets_start + (self.node_count + 1) * 8;
        if csr.len() < need {
            return 0;
        }
        let mut total = 0usize;
        for k in 0..self.node_count {
            let start = rd_u64(csr, offsets_start + k * 8) as usize;
            let end = rd_u64(csr, offsets_start + (k + 1) * 8) as usize;
            if end <= start || end > csr.len() {
                continue;
            }
            let (count, _) = read_varint(&csr[start..end], 0);
            total += count as usize;
        }
        total
    }

    pub fn rev_by_hash(&self, hash: u64) -> Option<Vec<MappedEdge>> {
        self.edges_by_hash(hash, /*fwd=*/ false)
    }

    fn edges_by_hash(&self, hash: u64, fwd: bool) -> Option<Vec<MappedEdge>> {
        let id = self.resolve(hash)?;
        let csr = if fwd { self.fwd.bytes() } else { self.rev.bytes() };
        let recs = TopologyView::edges_of(csr, id, self.node_count);
        Some(
            recs.into_iter()
                .filter_map(|e| {
                    let other_hash = self.hash_of(e.neighbor)?;
                    let edge_type_hash =
                        self.type_hashes.get(e.edge_type_id as usize).copied()?;
                    Some(MappedEdge {
                        other_hash,
                        edge_type_hash,
                        meta_ref: e.meta_ref,
                    })
                })
                .collect(),
        )
    }

    /// O(1) `dense id → slug hash` (v2 records carry it inline).
    pub fn hash_of(&self, id: u64) -> Option<u64> {
        let k = id as usize;
        if k >= self.node_count {
            return None;
        }
        if self.version >= 2 {
            Some(rd_u64(self.nodes.bytes(), HEADER_LEN + 8 + k * NODE_RECSIZE_V2))
        } else {
            self.slug_of(id).map(crate::sk_hash)
        }
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
            TopoNode { hash: h("t/chloe"),  slug: "t/chloe",  collection: "tourist", payload_offset: 0,   payload_len: 10, spatial: None },
            TopoNode { hash: h("p/uluwatu"),slug: "p/uluwatu",collection: "place",   payload_offset: 10,  payload_len: 20, spatial: None },
            TopoNode { hash: h("p/ubud"),   slug: "p/ubud",   collection: "place",   payload_offset: 30,  payload_len: 15, spatial: None },
            TopoNode { hash: h("a/south"),  slug: "a/south",  collection: "area",    payload_offset: 45,  payload_len: 5, spatial: None  },
        ];
        let edges = vec![
            TopoEdge { from_hash: h("t/chloe"),   to_hash: h("p/uluwatu"), edge_type: "visited".into(), meta: None },
            TopoEdge { from_hash: h("t/chloe"),   to_hash: h("p/ubud"),    edge_type: "visited".into(), meta: None },
            TopoEdge { from_hash: h("p/uluwatu"), to_hash: h("a/south"),   edge_type: "in_area".into(), meta: None },
        ];

        let blob = build(&nodes, &edges);
        let view = TopologyView::new(&blob).unwrap();
        assert_eq!(view.node_count(), 4);

        // Name resolution round-trips to the dense id assigned in `nodes` order,
        // and slugs.bin gives the reverse mapping back.
        for (i, node) in nodes.iter().enumerate() {
            assert_eq!(view.resolve(node.hash), Some(i as u64), "resolve {}", node.slug);
            assert_eq!(view.slug(i as u64), Some(node.slug), "slug of id {i}");
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
