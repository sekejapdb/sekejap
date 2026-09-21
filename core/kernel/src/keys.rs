//! The keyspaces. Extensibility = new tags, never new structures.
//!
//! Big-endian everywhere: byte order IS numeric order, so "everything of X" is a
//! range scan from a prefix. Tag first, so spaces never interleave.
//!
//! 0x00 catalog | 0x01 node(id) -> payload | 0x03 edge(src,type,dst) -> props
//! 0x04 redge(dst,type,src) mirror | 0x05 vec(field,id) -> embedding (phase 2)

pub const TAG_CATALOG: u8 = 0x00;
pub const TAG_NODE: u8 = 0x01;
pub const TAG_EDGE: u8 = 0x03;
pub const TAG_REDGE: u8 = 0x04;
/// Embedding rows (2e): 0x05 | field | id -> f32-LE coordinates. FIELD
/// FIRST, like every other index family here: each vector field owns a
/// disjoint, prefix-scannable range, so two embedding columns of two
/// different widths never meet in one scan.
pub const TAG_VEC: u8 = 0x05;
pub const TAG_LABEL: u8 = 0x02;
pub const TAG_EXT: u8 = 0x06;
/// ctx != 0 edges live in their own, wider keyspace. The base graph (ctx = 0)
/// keeps 25-byte keys: measured, the flat +8B/key grew a 5M-node file by
/// 640 MB and doubled 3-hop latency through OS-cache pressure alone -- a price
/// paid by workloads that never use perspectives. Split tags, each side pays
/// its own way. Sort order per space is unchanged (separate tags).
pub const TAG_CEDGE: u8 = 0x08;
pub const TAG_CREDGE: u8 = 0x09;
/// Property index: 0x0A | prop_id | value | node_id, empty value. 25B.
/// The VALUE is an order-preserving 8-byte encoding, so every property query
/// is a range scan: equality = one (prop,value) prefix, range = (prop,lo)..hi,
/// top-k DESC = a descending-encoded prop scanned forward with LIMIT.
pub const TAG_PROP: u8 = 0x0A;
/// Vector fingerprint (2g): 0x0B | field | id -> [norm f32-LE][packed code].
/// Its own keyspace so `nearest` scans codes ONLY -- never the 6KB vectors.
/// Codes of different fields are different WIDTHS (the width follows the
/// field's dimension), so the field must be in the key: a scan that mixed
/// them would decode one field's bytes with another field's recipe.
pub const TAG_VCODE: u8 = 0x0B;
/// Full-text postings (2h). Two shapes share the tag, split by segment id:
///   HEAD (seg 0, mutable):   0x0C | field | 0u32 | term | 0x00 | docid  -> tf varint
///   FOLDED (seg>0, immut.):  0x0C | field | seg  | term               -> packed postings
/// The head is row-per-posting so every write is BLIND (appending to a
/// packed value would be read-modify-write -- the e1 BM25 wound); folds
/// pack head rows into value-per-term segments. 0x00 separates term from docid in head keys: tokenizer terms are
/// alphanumeric UTF-8 (never 0x00), and the separator MUST sort before
/// every text byte -- with 0xFF, longer terms sorted ahead of their own
/// prefixes ("handle" before "hand") and the fuzzy walk's seek skipped
/// real terms; the oracle test caught it.
pub const TAG_TEXT: u8 = 0x0C;
/// Per-(field, doc) token count -- BM25's |d|. Point lookups at scoring
/// time only (candidates are few), so no packed norm blocks are needed.
pub const TAG_TEXTNORM: u8 = 0x0D;
/// Per-(field, seg) metadata: doc_count + total_tokens (BM25 denominators)
/// and the segment's alive-bitmap (the deletion discipline: one value).
pub const TAG_TEXTMETA: u8 = 0x0E;
/// Spatial cell postings (2i): 0x0F | field | level u8 | hilbert u64 | id
/// -> [bbox f32x4 outward-rounded]. Hilbert order makes a neighbourhood's
/// postings contiguous on disk; the bbox filters without payload reads;
/// a degenerate bbox IS the point (exact tier free for point data).
pub const TAG_SPAT: u8 = 0x0F;
/// Geometry rows (2i): 0x10 | field | id -> binary-encoded Geom. The
/// kernel speaks TYPED geometry only; GeoJSON parsing is an API-layer
/// concern (the core stays pure -- no JSON dependency below the SQL line).
pub const TAG_GEOM: u8 = 0x10;
/// Vector navigation rows (2k): 0x11 | field | id -> [norm f32][2-bit code]
/// [n u16][neighbor u64...]. Code co-located with links (FACT-02): one row
/// read per visited node during a beam walk -- ranking data arrives with
/// the topology, no second lookup. One small-world graph per field: links
/// carry bare ids, so a shared keyspace would wire two fields' vectors
/// into one another's neighbourhoods.
pub const TAG_NAV: u8 = 0x11;
/// SQL-layer metadata (phase 3): collection-name interning, schemas,
/// edge-type names. The kernel never reads these rows; the tag is minted
/// here so the keyspace vocabulary stays in ONE place (D4).
pub const TAG_SQLMETA: u8 = 0x12;
/// SQL field (secondary) indexes: (coll, field, order-preserving value
/// bytes, id) -> []. The VALUE ENCODING is the SQL layer's business; the
/// kernel only promises byte-ordered iteration (D4: an index is rows).
pub const TAG_FIELDIDX: u8 = 0x13;
/// SQL SEARCH-index slot maps (phase 3c): dense u32 slots <-> node hashes,
/// per search index. Kind 0: slot -> hash. Kind 1: hash -> slot.
/// Kind 2: next-slot counter.
pub const TAG_SEARCHSLOT: u8 = 0x14;

/// Catalogue subspace for the SQL covering `_key` order index.
///
/// This deliberately is NOT a new top-level tag. A tag above the append-heavy
/// data spaces interrupts their right-edge growth and measurably lowers leaf
/// occupancy. `TAG_CATALOG` is 0x00, ahead of every data row, so these records
/// cannot split the vector/node growth point. The fixed identity following the
/// slot is `(collection, field)`; a field name alone is never globally unique.
pub const CAT_KEY_ORDER: u64 = 4;
/// Disposable SQL aggregate summaries. Kept before append-heavy data tags.
/// Recovery must discard these: a salvaged source can differ from its summary.
pub const CAT_FIELD_AGGREGATE: u64 = 5;

pub fn field_aggregate_key(coll: u64, field: u64) -> Vec<u8> {
    k(TAG_CATALOG, &[CAT_FIELD_AGGREGATE, coll, field])
}

pub fn is_field_aggregate_key(key: &[u8]) -> bool {
    key.len() == 25 && key[0] == TAG_CATALOG
        && key[1..9] == CAT_FIELD_AGGREGATE.to_be_bytes()
}

fn k(tag: u8, parts: &[u64]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + parts.len() * 8);
    v.push(tag);
    for p in parts { v.extend_from_slice(&p.to_be_bytes()); }
    v
}

pub fn node(id: u64) -> Vec<u8> { k(TAG_NODE, &[id]) }
// ctx first (GRAPH contract): RCA traverses within one perspective, so each
// perspective's edges cluster physically, and a whole perspective's KG is one
// contiguous range. ctx=0 is the base graph.
pub fn edge(ctx: u64, src: u64, ty: u64, dst: u64) -> Vec<u8> {
    if ctx == 0 { k(TAG_EDGE, &[src, ty, dst]) } else { k(TAG_CEDGE, &[ctx, src, ty, dst]) }
}
pub fn redge(ctx: u64, dst: u64, ty: u64, src: u64) -> Vec<u8> {
    if ctx == 0 { k(TAG_REDGE, &[dst, ty, src]) } else { k(TAG_CREDGE, &[ctx, dst, ty, src]) }
}
pub fn edge_prefix(ctx: u64, src: u64) -> Vec<u8> {
    if ctx == 0 { k(TAG_EDGE, &[src]) } else { k(TAG_CEDGE, &[ctx, src]) }
}
pub fn ctx_prefix(ctx: u64) -> Vec<u8> {
    if ctx == 0 { vec![TAG_EDGE] } else { k(TAG_CEDGE, &[ctx]) }
}
pub fn vec_key(field: u64, id: u64) -> Vec<u8> { k(TAG_VEC, &[field, id]) }
pub fn vec_prefix(field: u64) -> Vec<u8> { k(TAG_VEC, &[field]) }
pub fn vcode_key(field: u64, id: u64) -> Vec<u8> { k(TAG_VCODE, &[field, id]) }
pub fn vcode_prefix(field: u64) -> Vec<u8> { k(TAG_VCODE, &[field]) }
pub fn nav_prefix(field: u64) -> Vec<u8> { k(TAG_NAV, &[field]) }

pub fn text_head_key(field: u64, term: &[u8], docid: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + 8 + 4 + term.len() + 1 + 8);
    v.push(TAG_TEXT);
    v.extend_from_slice(&field.to_be_bytes());
    v.extend_from_slice(&0u32.to_be_bytes());
    v.extend_from_slice(term);
    v.push(0x00);
    v.extend_from_slice(&docid.to_be_bytes());
    v
}
/// One document-membership row in the mutable text head.  Empty terms do not
/// exist, so the zero byte after segment 0 is an unambiguous namespace for
/// streaming/counting the documents owned by the head.
pub fn text_head_doc_key(field: u64, docid: u64) -> Vec<u8> {
    text_head_key(field, b"", docid)
}
pub fn text_seg_key(field: u64, seg: u32, term: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + 8 + 4 + term.len());
    v.push(TAG_TEXT);
    v.extend_from_slice(&field.to_be_bytes());
    v.extend_from_slice(&seg.to_be_bytes());
    v.extend_from_slice(term);
    v
}
/// Prefix and key for a bounded immutable posting block.  Token bytes never
/// contain zero, so `[term|0x00|first_doc]` remains prefix-seekable by term.
pub fn text_seg_block_prefix(field: u64, seg: u32, term: &[u8]) -> Vec<u8> {
    let mut v = text_seg_key(field, seg, term);
    v.push(0);
    v
}
pub fn text_seg_block_key(field: u64, seg: u32, term: &[u8], first_doc: u64) -> Vec<u8> {
    let mut v = text_seg_block_prefix(field, seg, term);
    v.extend_from_slice(&first_doc.to_be_bytes());
    v
}
/// Per-document membership/length row for an immutable segment.  Ownership
/// metadata belongs under TEXTMETA so folded posting-row shape stays stable.
pub fn text_seg_doc_prefix(field: u64, seg: u32) -> Vec<u8> {
    let mut v = text_meta_key(field, seg);
    v.push(0);
    v
}
pub fn text_seg_doc_key(field: u64, seg: u32, docid: u64) -> Vec<u8> {
    let mut v = text_seg_doc_prefix(field, seg);
    v.extend_from_slice(&docid.to_be_bytes());
    v
}
pub fn text_norm_key(field: u64, docid: u64) -> Vec<u8> { k(TAG_TEXTNORM, &[field, docid]) }
pub fn text_prefix(field: u64) -> Vec<u8> { k(TAG_TEXT, &[field]) }
pub fn text_norm_prefix(field: u64) -> Vec<u8> { k(TAG_TEXTNORM, &[field]) }
pub fn text_meta_prefix(field: u64) -> Vec<u8> { k(TAG_TEXTMETA, &[field]) }
pub fn spat_key(field: u64, level: u8, cell: u64, id: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + 8 + 1 + 8 + 8);
    v.push(TAG_SPAT);
    v.extend_from_slice(&field.to_be_bytes());
    v.push(level);
    v.extend_from_slice(&cell.to_be_bytes());
    v.extend_from_slice(&id.to_be_bytes());
    v
}
pub fn geom_key(field: u64, id: u64) -> Vec<u8> { k(TAG_GEOM, &[field, id]) }
pub fn geom_prefix(field: u64) -> Vec<u8> { k(TAG_GEOM, &[field]) }
pub fn nav_key(field: u64, id: u64) -> Vec<u8> { k(TAG_NAV, &[field, id]) }
/// (kind, hash) -> bytes. kind: 0 = collection name, 1 = table schema,
/// 2 = edge-type name, 3 = index definition, 4 = edge-insertion counter
/// (hash 0; the SQL layer's monotonic edge seq), 6 = vector field name.
/// (coll_hash, field_hash, value_bytes, id). id is the fixed 8-byte
/// suffix -- positional, never searched, so value bytes are unrestricted.
pub fn fieldidx_key(coll: u64, field: u64, value: &[u8], id: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(17 + value.len() + 8);
    v.push(TAG_FIELDIDX);
    v.extend_from_slice(&coll.to_be_bytes());
    v.extend_from_slice(&field.to_be_bytes());
    v.extend_from_slice(value);
    v.extend_from_slice(&id.to_be_bytes());
    v
}
pub fn fieldidx_prefix(coll: u64, field: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(17);
    v.push(TAG_FIELDIDX);
    v.extend_from_slice(&coll.to_be_bytes());
    v.extend_from_slice(&field.to_be_bytes());
    v
}

pub fn searchslot_key(idx: u64, kind: u8, id: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(18);
    v.push(TAG_SEARCHSLOT);
    v.extend_from_slice(&idx.to_be_bytes());
    v.push(kind);
    v.extend_from_slice(&id.to_be_bytes());
    v
}

pub fn searchslot_prefix(idx: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(9);
    v.push(TAG_SEARCHSLOT);
    v.extend_from_slice(&idx.to_be_bytes());
    v
}

pub fn sqlmeta_key(kind: u8, hash: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(10);
    v.push(TAG_SQLMETA); v.push(kind); v.extend_from_slice(&hash.to_be_bytes()); v
}
pub fn spat_prefix(field: u64, level: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + 8 + 1);
    v.push(TAG_SPAT);
    v.extend_from_slice(&field.to_be_bytes());
    v.push(level);
    v
}

pub fn text_meta_key(field: u64, seg: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(1 + 8 + 4);
    v.push(TAG_TEXTMETA);
    v.extend_from_slice(&field.to_be_bytes());
    v.extend_from_slice(&seg.to_be_bytes());
    v
}
/// Reserved metadata coordinates. Segment ids are allocated below these two
/// values; keeping them under the existing metadata tag avoids minting a tag
/// above append-heavy data keyspaces (the measured occupancy trap in D30).
pub const TEXT_TERM_STATS_SEG: u32 = u32::MAX - 1;
pub const TEXT_FIELD_META_SEG: u32 = u32::MAX;
pub fn text_field_meta_key(field: u64) -> Vec<u8> {
    text_meta_key(field, TEXT_FIELD_META_SEG)
}
pub fn text_term_id_key(field: u64, term_id: u64) -> Vec<u8> {
    let mut v = text_meta_key(field, TEXT_TERM_STATS_SEG);
    v.push(0);
    v.extend_from_slice(&term_id.to_be_bytes());
    v
}
/// Lexically ordered live-term directory used by prefix and typo expansion.
/// Counts remain in the hashed point-lookup rows above; this second view keeps
/// expansion from walking posting blocks or the number of immutable batches.
pub fn text_term_lex_prefix(field: u64) -> Vec<u8> {
    let mut v = text_meta_key(field, TEXT_TERM_STATS_SEG);
    v.push(1);
    v
}
pub fn text_term_lex_key(field: u64, term: &[u8]) -> Vec<u8> {
    let mut v = text_term_lex_prefix(field);
    v.extend_from_slice(term);
    v
}
pub fn label(l: u64, id: u64) -> Vec<u8> { k(TAG_LABEL, &[l, id]) }
pub fn label_prefix(l: u64) -> Vec<u8> { k(TAG_LABEL, &[l]) }
pub fn edge_type_prefix(ctx: u64, src: u64, ty: u64) -> Vec<u8> {
    if ctx == 0 { k(TAG_EDGE, &[src, ty]) } else { k(TAG_CEDGE, &[ctx, src, ty]) }
}
pub fn redge_prefix(ctx: u64, dst: u64) -> Vec<u8> {
    if ctx == 0 { k(TAG_REDGE, &[dst]) } else { k(TAG_CREDGE, &[ctx, dst]) }
}
pub fn redge_type_prefix(ctx: u64, dst: u64, ty: u64) -> Vec<u8> {
    if ctx == 0 { k(TAG_REDGE, &[dst, ty]) } else { k(TAG_CREDGE, &[ctx, dst, ty]) }
}
pub fn extkey(h: u64) -> Vec<u8> { k(TAG_EXT, &[h]) }
pub fn catalog(field: u64) -> Vec<u8> { k(TAG_CATALOG, &[field]) }
/// A catalog slot subdivided PER FIELD: 0x00 | slot | field, 17 bytes.
/// It cannot collide with the 9-byte global slots -- different lengths are
/// different keys -- which is why an arbitrary field hash is safe here
/// while `catalog(field_hash)` would not be: that would land on whichever
/// global slot the hash happened to equal.
///
/// It has to be the CATALOG and not a fresh tag. Every keyspace shares one
/// tree, and the leaf splitter only closes a left page full when the
/// growth point is the RIGHTMOST leaf. A per-field vector record under a
/// tag above 0x0B put permanent rows past the fingerprints, so appending a
/// fingerprint stopped being an append: leaves settled at 4 entries where
/// they had held 7, the file grew 5% and every scan read the extra pages.
/// The catalog sorts FIRST, ahead of every data keyspace, so it disturbs
/// no space's growth point.
pub fn catalog_field(slot: u64, field: u64) -> Vec<u8> { k(TAG_CATALOG, &[slot, field]) }
pub fn catalog_field_prefix(slot: u64) -> Vec<u8> { k(TAG_CATALOG, &[slot]) }
/// A per-item record within a catalog field: 0x00 | slot | field | item.
/// Use this for sparse bookkeeping that must sort ahead of every data
/// keyspace; putting it under a later tag changes right-edge append splits.
pub fn catalog_field_item(slot: u64, field: u64, item: u64) -> Vec<u8> {
    k(TAG_CATALOG, &[slot, field, item])
}

/// `(catalog, key-order slot, collection, field)` prefix.
pub fn key_order_prefix(collection: u64, field: u64) -> Vec<u8> {
    k(TAG_CATALOG, &[CAT_KEY_ORDER, collection, field])
}

/// One live-row entry. NUL is escaped as `00 ff`, then `00 00` terminates the
/// UTF-8 key before the row hash tie-breaker. This is order preserving (a key
/// sorts before every longer key it prefixes), supports embedded NUL, and makes
/// duplicate logical keys distinct physical rows rather than an overwritten
/// value. The key bytes remain covering and need no payload read.
pub fn key_order_key(collection: u64, field: u64, key: &[u8], id: u64) -> Vec<u8> {
    let mut out = key_order_prefix(collection, field);
    for &byte in key {
        if byte == 0 { out.extend_from_slice(&[0, u8::MAX]); }
        else { out.push(byte); }
    }
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&id.to_be_bytes());
    out
}

/// FNV-1a 64. Only contract: same bytes -> same hash. Collision = wrong id
/// returned by resolve(); the extkey VALUE stores the full external key so a
/// collision is DETECTED (compare) rather than silently wrong.
pub fn ext_hash(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for x in b { h ^= *x as u64; h = h.wrapping_mul(0x1000_0000_01b3); }
    h
}

pub fn prop(prop_id: u64, value: u64, id: u64) -> Vec<u8> { k(TAG_PROP, &[prop_id, value, id]) }
pub fn prop_prefix(prop_id: u64) -> Vec<u8> { k(TAG_PROP, &[prop_id]) }
pub fn prop_value_prefix(prop_id: u64, value: u64) -> Vec<u8> { k(TAG_PROP, &[prop_id, value]) }

/// Order-preserving encodings: byte order of the 8-byte result == the natural
/// order of the value, which is the whole trick that turns queries into ranges.
/// i64: flip the sign bit. f64 (IEEE 754): negative -> flip ALL bits,
/// non-negative -> flip the sign bit; total order matches numeric order
/// (NaN sorts above +inf; callers who care filter NaN before indexing).
pub fn enc_i64(v: i64) -> u64 { (v as u64) ^ (1 << 63) }
pub fn enc_f64(v: f64) -> u64 {
    let b = v.to_bits();
    if b >> 63 == 1 { !b } else { b ^ (1 << 63) }
}
/// Descending variants: an ascending scan over these yields DESC order, which
/// is how top-k works on a forward-only iterator.
pub fn enc_f64_desc(v: f64) -> u64 { !enc_f64(v) }
pub fn enc_i64_desc(v: i64) -> u64 { !enc_i64(v) }

/// Decode helpers for scans.
pub fn u64_at(k: &[u8], off: usize) -> u64 {
    let mut w = [0u8; 8]; w.copy_from_slice(&k[off..off+8]); u64::from_be_bytes(w)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_order_is_numeric_order_and_tags_never_interleave() {
        let mut ks: Vec<Vec<u8>> = (0..64u64).map(|i| edge(0, i * 7919, 1, i)).collect();
        let want = ks.clone();
        ks.sort();
        assert_eq!(ks, want, "big-endian keys must already be sorted");
        assert!(node(u64::MAX) < edge(0, 0, 0, 0), "node space must sort before edge space");
        assert!(edge(0, u64::MAX, u64::MAX, u64::MAX) < redge(0, 0, 0, 0));
        assert!(redge(0, u64::MAX, 0, 0) < edge(1, 0, 0, 0), "ctx spaces sort after base");
        assert!(edge(1, u64::MAX, 0, 0) < redge(1, 0, 0, 0));
    }
    #[test]
    fn adjacency_is_a_contiguous_prefix() {
        let pre = edge_prefix(0, 42);
        for d in 0..16u64 {
            assert!(edge(0, 42, 7, d).starts_with(&pre));
            assert!(!edge(0, 43, 7, d).starts_with(&pre));
            assert!(!edge(1, 42, 7, d).starts_with(&pre), "another ctx leaked in");
        }
        // a whole perspective is one contiguous prefix
        let kg = ctx_prefix(5);
        assert!(edge(5, 0, 0, 0).starts_with(&kg) && edge(5, u64::MAX, 1, 1).starts_with(&kg));
        assert!(!edge(6, 0, 0, 0).starts_with(&kg));
    }
}
