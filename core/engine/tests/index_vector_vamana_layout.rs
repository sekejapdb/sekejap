//! The vamana graph's RECORD LAYOUT: what is in the `0x7D` node keyspace,
//! what is in the `0x7F` adjacency keyspace, and what ONE insert dirties.
//!
//! THE CLAIM THIS FILE EXISTS FOR. A node's head is `6 + 8 + dim` bytes, so
//! at 4,096 lanes it is 4,110 bytes -- larger than a 4,096-byte page. While
//! the head and the adjacency shared one record, appending a back edge to a
//! neighbour rewrote that neighbour's codes and the overflow chain holding
//! them, `R = 48` times per insert, and the page-WAL refused the transaction
//! by its managed-byte allowance however small the batch was made. Split,
//! an edge append rewrites one adjacency record of at most 1,156 bytes.
//!
//! So the assertions here are about SIZE and about BYTES DIRTIED, not about
//! answers: that a node record is exactly one head, that an adjacency record
//! fits one page, that the two keyspaces hold the same sequences, and that
//! ONE insert's WAL footprint barely moves when the dimension is multiplied
//! by 256. A future change that folds the two back together fails HERE
//! rather than in a benchmark.
//!
//! THE ORACLE IS HELD IN THIS PROCESS: every vector is generated here and
//! the exact top-k is computed here from those f32 lanes.
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{
        verification::{verify_indexed_source, VerificationLimits},
        CollectionId, CollectionOptions, Database, IndexId, VectorMetric, VAMANA_ADJACENCY,
        VAMANA_ENTRY, VAMANA_MAX_DEGREE,
    },
    pagewal::PageWalStore,
    Kind,
};
use serde_json::json;
use std::{collections::BTreeMap, path::Path};

/// The dimension whose codes exceed one page. 4,096 lanes is 4,110 bytes of
/// head against a 4,096-byte page, and it is the case the benchmark broke on.
const WIDE: usize = 4_096;
/// The dimension whose whole node fits well inside a page, so both sides of
/// the page boundary are exercised by the same assertions.
const NARROW: usize = 16;
/// Bytes per stored edge, and the ceiling one adjacency record can reach.
const EDGE: usize = 12;
const PAGE: usize = 4_096;

fn cfg() -> Config {
    Config {
        budget_bytes: 32 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 11) as f32 / (1u64 << 53) as f32
    }
    fn vector(&mut self, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| self.unit() * 2.0 - 1.0).collect()
    }
}

/// Clustered rows: 64 centres with a little noise, the shape an embedding
/// corpus has.
fn corpus(rows: usize, dim: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    let centres: Vec<Vec<f32>> = (0..64).map(|_| rng.vector(dim)).collect();
    (0..rows)
        .map(|_| {
            let centre = &centres[(rng.next() % 64) as usize];
            centre
                .iter()
                .map(|lane| lane + (rng.unit() - 0.5) * 0.2)
                .collect()
        })
        .collect()
}

fn squared_l2(stored: &[f32], query: &[f32]) -> f64 {
    stored
        .iter()
        .zip(query)
        .map(|(s, q)| (f64::from(*s) - f64::from(*q)) * (f64::from(*s) - f64::from(*q)))
        .sum()
}

/// Brute force, in this process, over the lanes this process generated.
fn exact_top_k(held: &BTreeMap<u64, Vec<f32>>, query: &[f32], k: usize) -> Vec<u64> {
    let mut scored: Vec<(f64, u64)> = held
        .iter()
        .map(|(seq, stored)| (squared_l2(stored, query), *seq))
        .collect();
    scored.sort_by(|l, r| l.0.total_cmp(&r.0).then_with(|| l.1.cmp(&r.1)));
    scored.into_iter().take(k).map(|(_, seq)| seq).collect()
}

struct Built {
    db: Database,
    collection: CollectionId,
    index: IndexId,
    held: BTreeMap<u64, Vec<f32>>,
}

/// Insert the rows, then drive the late build at ONE batch size and with NO
/// retry: a refusal is the test failing, which is the point.
fn build(path: &Path, dim: usize, vectors: &[Vec<f32>], batch: usize) -> Built {
    // DEFAULT resource limits: no policy is installed, so the only bound is
    // the page-WAL's own fixed 16 MiB managed-byte allowance.
    let mut db = Database::create(path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "points",
            vec![("embedding".into(), Kind::Vector(dim))],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut held = BTreeMap::new();
    for (at, vector) in vectors.iter().enumerate() {
        let id = db
            .put(collection, &format!("k{at}"), &json!({"embedding": vector}))
            .unwrap();
        held.insert(id.sequence, vector.clone());
        if at % 128 == 127 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let index = db
        .create_vamana_index(collection, "embedding_vamana", "embedding")
        .unwrap();
    db.commit().unwrap();
    while !db.build_index_step(index, batch).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    Built {
        db,
        collection,
        index,
        held,
    }
}

fn ordered(number: u64) -> Vec<u8> {
    let bytes = number.to_be_bytes();
    let start = bytes.iter().position(|byte| *byte != 0).unwrap_or(7);
    let mut out = vec![0x80 + (8 - start) as u8];
    out.extend_from_slice(&bytes[start..]);
    out
}

fn read_ordered(bytes: &[u8]) -> u64 {
    let width = usize::from(bytes[0] - 0x80);
    let mut out = 0u64;
    for byte in &bytes[1..1 + width] {
        out = (out << 8) | u64::from(*byte);
    }
    out
}

/// Every record of one keyspace of one index, by sequence, read from the raw
/// store rather than through any engine walk.
fn keyspace(path: &Path, tag: u8, index: IndexId) -> BTreeMap<u64, Vec<u8>> {
    let raw = PageWalStore::open(path, false, 1 << 20).unwrap();
    let mut prefix = vec![tag];
    prefix.extend(ordered(index.0));
    let mut records = BTreeMap::new();
    for row in raw.range(&prefix).unwrap() {
        let (key, value) = row.unwrap();
        if !key.starts_with(&prefix) {
            break;
        }
        records.insert(read_ordered(&key[prefix.len()..]), value);
    }
    records
}

/// One live insert on top of a READY graph, and the WAL bytes its
/// transaction appended. This is the number the page-WAL's managed-byte
/// allowance is spent against, measured rather than estimated.
fn insert_wal_bytes(built: &mut Built, key: &str, vector: &[f32]) -> u64 {
    let before = built.db.io_counters().unwrap().wal_bytes_written;
    built
        .db
        .put(built.collection, key, &json!({"embedding": vector}))
        .unwrap();
    built.db.commit().unwrap();
    built.db.io_counters().unwrap().wal_bytes_written - before
}

fn clean(path: &Path) {
    let report = verify_indexed_source(path, VerificationLimits::default(), |issue| {
        panic!("verification issue: {issue:?}");
    })
    .unwrap();
    assert!(report.complete && report.clean, "{report:?}");
}

/// The layout itself, at BOTH sides of the page boundary: a node record is
/// exactly one head and carries no edges, an adjacency record fits one page
/// whatever the dimension, and the two keyspaces hold the same sequences.
fn layout_holds(path: &Path, index: IndexId, dim: usize, nodes: usize) {
    let mut heads = keyspace(path, VAMANA_ENTRY, index);
    let lists = keyspace(path, VAMANA_ADJACENCY, index);
    let header = heads.remove(&0).expect("graph header record");
    assert_eq!(header.len(), 30, "graph header length");
    assert_eq!(heads.len(), nodes, "node records");
    assert_eq!(lists.len(), nodes, "adjacency records");
    assert!(
        heads.keys().eq(lists.keys()),
        "the node and adjacency keyspaces hold different sequences"
    );
    let head_len = 6 + 8 + dim;
    for (seq, value) in &heads {
        assert_eq!(
            value.len(),
            head_len,
            "node record {seq} is {} bytes, not the {head_len}-byte head alone",
            value.len()
        );
    }
    let ceiling = 4 + VAMANA_MAX_DEGREE * EDGE;
    for (seq, value) in &lists {
        let own = usize::from(u16::from_be_bytes(value[..2].try_into().unwrap()));
        let back = usize::from(u16::from_be_bytes(value[2..4].try_into().unwrap()));
        assert!(own + back <= VAMANA_MAX_DEGREE, "degree at {seq}");
        assert_eq!(value.len(), 4 + (own + back) * EDGE, "adjacency length");
        assert!(
            value.len() <= ceiling && ceiling < PAGE,
            "an adjacency record must fit one page: {} bytes at {seq}",
            value.len()
        );
    }
}

// ── the tests ─────────────────────────────────────────────────────────────

/// The layout, at a dimension whose codes are SMALLER than a page and at one
/// whose codes are LARGER than a page. Both are built the same way, both are
/// verified, and both answer the brute-force top-k this process computed.
#[test]
fn a_node_record_is_one_head_and_its_edges_live_in_a_record_of_their_own() {
    for (dim, rows) in [(NARROW, 800usize), (WIDE, 400usize)] {
        assert_eq!(dim > PAGE - 14, dim == WIDE, "the two cases must straddle a page");
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db");
        let vectors = corpus(rows, dim, 0x1a2b_3c4d_5e6f_7081 ^ dim as u64);
        let built = build(&path, dim, &vectors, 256);
        // The graph answers, so the layout change did not cost the index its
        // job: the top-1 of a row that is in the corpus is that row.
        for at in (0..rows).step_by(rows / 8) {
            let hits = built
                .db
                .query_vamana_vector(
                    built.index,
                    &vectors[at],
                    VectorMetric::SquaredL2,
                    1,
                    120,
                    usize::MAX,
                    || false,
                )
                .unwrap()
                .hits;
            assert_eq!(hits.len(), 1, "dim {dim}: no answer at row {at}");
            let truth = exact_top_k(&built.held, &vectors[at], 1);
            assert_eq!(hits[0].id.sequence, truth[0], "dim {dim}: wrong top-1 at {at}");
        }
        let index = built.index;
        let nodes = built.held.len();
        drop(built);
        layout_holds(&path, index, dim, nodes);
        clean(&path);
    }
}

/// THE COST CLAIM, and the regression this file exists to catch.
///
/// One insert's transaction dirties the new node's head ONCE and then at
/// most `R` adjacency records of a few hundred bytes each. So its WAL
/// footprint is dominated by the DEGREE and barely by the dimension: raising
/// the dimension by 256x must not raise it by anything like that. While the
/// head and the adjacency shared one record, the same measurement at 4,096
/// lanes was 2,780,624 bytes against 265,216 after the split, and every
/// build transaction was refused by the 16 MiB allowance.
#[test]
fn one_insert_dirties_the_edges_and_not_the_codes_however_wide_the_vector_is() {
    let mut measured = Vec::new();
    for (dim, rows) in [(NARROW, 800usize), (WIDE, 400usize)] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db");
        let vectors = corpus(rows + 1, dim, 0x2b3c_4d5e_6f70_8192 ^ dim as u64);
        let mut built = build(&path, dim, &vectors[..rows], 256);
        let bytes = insert_wal_bytes(&mut built, "one-more", &vectors[rows]);
        // Well inside the page-WAL's fixed 16 MiB allowance, so a batch of
        // rows fits one transaction and the build needs no retry loop.
        assert!(
            bytes < (16 << 20) / 8,
            "dim {dim}: one insert appended {bytes} bytes of WAL, an eighth of the allowance"
        );
        let index = built.index;
        let nodes = built.held.len() + 1;
        drop(built);
        layout_holds(&path, index, dim, nodes);
        clean(&path);
        measured.push((dim, bytes));
    }
    let (narrow_dim, narrow) = measured[0];
    let (wide_dim, wide) = measured[1];
    assert_eq!((narrow_dim, wide_dim), (NARROW, WIDE));
    // 256 times the lanes, and at most 4 times the bytes dirtied. A layout
    // that rewrote a whole node record per edge would put this near 256.
    assert!(
        wide <= narrow * 4,
        "one insert cost {wide} bytes at {WIDE} lanes against {narrow} at {NARROW}: \
         the cost is tracking the dimension, so an edge is rewriting the codes again"
    );
}

/// RECALL IS WHAT IT WAS, to the last of 500 answers.
///
/// The split moved bytes and touched no decision: the same corpus under the
/// same seeds walks the same entry point, prunes the same candidates and
/// builds the same graph, so the recall is the recall the single-record
/// layout got. The three numbers below were MEASURED on the engine before
/// the split, over this corpus at these search lists, and are pinned exactly
/// -- `recall@10` over 25 queries is a multiple of 1/250, so an exact
/// comparison is the right one and a tolerance would hide a changed graph.
///
/// This is also what pins the WRITE ORDER inside `link_node`. The entry
/// point is refreshed from a SAMPLE of the graph, and the sample is taken
/// before the node being linked is written; writing the new head earlier
/// would put that node in its own sample and move the entry point. Recall
/// at `ef = 10` was 0.940 when it did.
#[test]
fn recall_is_unchanged_by_moving_the_adjacency_into_its_own_record() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let rows = 2_000;
    let vectors = corpus(rows, NARROW, 0x5eed_1234_9abc_def1);
    let built = build(&path, NARROW, &vectors, 64);
    let queries = corpus(25, NARROW, 0x0fed_cba9_8765_4321);
    // (search list, recall@10 measured before the adjacency moved)
    for (ef, before) in [(10usize, 0.844f64), (40, 1.0), (120, 1.0)] {
        let mut hit = 0usize;
        for query in &queries {
            let truth = exact_top_k(&built.held, query, 10);
            let got = built
                .db
                .query_vamana_vector(
                    built.index,
                    query,
                    VectorMetric::SquaredL2,
                    10,
                    ef,
                    usize::MAX,
                    || false,
                )
                .unwrap()
                .hits;
            assert_eq!(got.len(), 10, "ef {ef}: short answer");
            hit += got
                .iter()
                .filter(|h| truth.contains(&h.id.sequence))
                .count();
        }
        let recall = hit as f64 / (queries.len() * 10) as f64;
        assert_eq!(
            (recall * 1_000.0).round() as i64,
            (before * 1_000.0).round() as i64,
            "ef {ef}: recall@10 is {recall:.3} and was {before:.3} before the split, \
             so the layout change altered the graph"
        );
    }
    let index = built.index;
    let nodes = built.held.len();
    drop(built);
    layout_holds(&path, index, NARROW, nodes);
    clean(&path);
}


/// DROP INDEX reclaims BOTH keyspaces.
///
/// The family owns two ranges now, and a bounded drop step fills its batch
/// from them in order and is only done when both are empty
/// (`core/engine/src/collections/catalog.rs::drop_index_step`). A drop that
/// reclaimed the heads and left the adjacency behind would leave a keyspace
/// no descriptor names, which is what this catches.
#[test]
fn dropping_the_index_reclaims_the_adjacency_keyspace_as_well_as_the_heads() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let vectors = corpus(400, NARROW, 0x55e6_f708_192a_3b4c);
    let built = build(&path, NARROW, &vectors, 256);
    let index = built.index;
    let nodes = built.held.len();
    drop(built);
    // Populated before the drop, so what follows is about a drop and not
    // about an index that was never built.
    layout_holds(&path, index, NARROW, nodes);

    let mut db = Database::open(&path, cfg()).unwrap();
    db.begin_drop_index(index).unwrap();
    db.commit().unwrap();
    // A batch of one, so the step has to cross from one keyspace into the
    // other rather than emptying both in a single pass.
    while !db.drop_index_step(index, 1).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    assert!(
        db.list_indexes(db.collection("points").unwrap().unwrap())
            .unwrap()
            .is_empty(),
        "the descriptor outlived the drop"
    );
    drop(db);
    assert!(
        keyspace(&path, VAMANA_ENTRY, index).is_empty(),
        "the node keyspace survived the drop"
    );
    assert!(
        keyspace(&path, VAMANA_ADJACENCY, index).is_empty(),
        "the adjacency keyspace survived the drop"
    );
    clean(&path);
}
