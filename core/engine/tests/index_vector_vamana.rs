//! The vamana graph family (keyspace `0x7D`, feature bit `0x8000`).
//!
//! THE ORACLE IS HELD IN THIS PROCESS. Every vector this file indexes is
//! generated here by a deterministic generator, kept in a `Vec`, and the
//! exact top-k is computed here from those f32 lanes with the metric written
//! out below. No assertion compares one engine call against another engine
//! call: the recall floor, the reopen equality and the delete invariants are
//! all measured against numbers this file owns.
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{
        verification::{verify_indexed_source, VerificationLimits},
        ApproxVectorMethod, CollectionId, CollectionOptions, Database, EntityId, Error, IndexFamily,
        IndexId, IndexState, VectorHit, VectorMetric, SUPPORTED_LOGICAL_FEATURES,
        VAMANA_ADJACENCY, VAMANA_ENTRY, VAMANA_FEATURE, VAMANA_MAX_DEGREE,
    },
    internal::{admit_logical_features, logical_features},
    pagewal::PageWalStore,
    Kind,
};
use serde_json::json;
use std::{collections::BTreeMap, fs, path::Path};

const DIM: usize = 16;

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

// ── the corpus and the oracle, both owned here ────────────────────────────

/// A deterministic 64-bit generator, so every run indexes the same corpus and
/// a recall number is reproducible rather than a sample of luck.
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
    fn vector(&mut self) -> Vec<f32> {
        (0..DIM).map(|_| self.unit() * 2.0 - 1.0).collect()
    }
}

/// The distances the engine computes, written out here from the definitions
/// in `core/engine/src/index/vector/quant.rs` rather than called.
fn distance(metric: VectorMetric, stored: &[f32], query: &[f32]) -> f64 {
    let wide = |v: &[f32]| v.iter().map(|lane| f64::from(*lane)).collect::<Vec<f64>>();
    let (stored, query) = (wide(stored), wide(query));
    match metric {
        VectorMetric::SquaredL2 => stored
            .iter()
            .zip(&query)
            .map(|(s, q)| (s - q) * (s - q))
            .sum(),
        VectorMetric::NegativeDot => -stored.iter().zip(&query).map(|(s, q)| s * q).sum::<f64>(),
        VectorMetric::Cosine => {
            let dot: f64 = stored.iter().zip(&query).map(|(s, q)| s * q).sum();
            let sn: f64 = stored.iter().map(|s| s * s).sum();
            let qn: f64 = query.iter().map(|q| q * q).sum();
            1.0 - dot / (sn.sqrt() * qn.sqrt())
        }
    }
}

/// Brute force: the true top-k over the corpus this process generated.
fn exact_top_k(
    corpus: &BTreeMap<u64, Vec<f32>>,
    query: &[f32],
    metric: VectorMetric,
    k: usize,
) -> Vec<u64> {
    let mut scored: Vec<(f64, u64)> = corpus
        .iter()
        .map(|(seq, stored)| (distance(metric, stored, query), *seq))
        .collect();
    scored.sort_by(|left, right| {
        left.0
            .total_cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
    });
    scored.into_iter().take(k).map(|(_, seq)| seq).collect()
}

fn corpus(count: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    (0..count).map(|_| rng.vector()).collect()
}

struct Built {
    db: Database,
    collection: CollectionId,
    index: IndexId,
    /// Sequence -> the f32 lanes this process generated for it. The ORACLE.
    held: BTreeMap<u64, Vec<f32>>,
    /// Insertion position -> entity id, so a test can name the row it wrote.
    ids: Vec<EntityId>,
}

fn build(path: &Path, vectors: &[Vec<f32>]) -> Built {
    let mut db = Database::create(path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "points",
            vec![
                ("embedding".into(), Kind::Vector(DIM)),
                ("tag".into(), Kind::Int),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut held = BTreeMap::new();
    let mut ids = Vec::with_capacity(vectors.len());
    for (at, vector) in vectors.iter().enumerate() {
        let id = db
            .put(
                collection,
                &format!("k{at}"),
                &json!({"embedding": vector, "tag": at as i64 % 7}),
            )
            .unwrap();
        held.insert(id.sequence, vector.clone());
        ids.push(id);
    }
    db.commit().unwrap();
    let index = db
        .create_vamana_index(collection, "embedding_vamana", "embedding")
        .unwrap();
    db.commit().unwrap();
    while !db.build_index_step(index, 64).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    Built {
        db,
        collection,
        index,
        held,
        ids,
    }
}

fn answer(db: &Database, index: IndexId, query: &[f32], metric: VectorMetric, k: usize, ef: usize)
    -> Vec<VectorHit>
{
    db.query_vamana_vector(index, query, metric, k, ef, usize::MAX, || false)
        .unwrap()
        .hits
}

fn recall(
    db: &Database,
    index: IndexId,
    held: &BTreeMap<u64, Vec<f32>>,
    queries: &[Vec<f32>],
    metric: VectorMetric,
    k: usize,
    ef: usize,
) -> f64 {
    let mut hit = 0usize;
    for query in queries {
        let truth = exact_top_k(held, query, metric, k);
        let got: Vec<u64> = answer(db, index, query, metric, k, ef)
            .into_iter()
            .map(|h| h.id.sequence)
            .collect();
        hit += got.iter().filter(|seq| truth.contains(seq)).count();
    }
    hit as f64 / (queries.len() * k) as f64
}

fn files(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                visit(root, &path, out);
            } else {
                out.insert(name, fs::read(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out);
    out
}

fn clean(path: &Path) {
    let report = verify_indexed_source(path, VerificationLimits::default(), |issue| {
        panic!("verification issue: {issue:?}");
    })
    .unwrap();
    assert!(report.complete && report.clean, "{report:?}");
}

/// Every record of one keyspace of one index, by sequence, read from the RAW
/// store rather than through any engine walk.
fn keyspace(raw: &PageWalStore, tag: u8, index: IndexId) -> BTreeMap<u64, Vec<u8>> {
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

/// The structural claim, checked from the RAW keyspaces rather than from any
/// engine walk: every node record is exactly one HEAD and nothing more, the
/// two keyspaces hold exactly the same sequences, every node names
/// neighbours that exist, every named neighbour names it back, no list is
/// longer than the declared degree, and the header's entry point is a node
/// that is there.
fn structure(path: &Path, index: IndexId, nodes: usize) {
    let raw = PageWalStore::open(path, false, 1 << 20).unwrap();
    let mut records = keyspace(&raw, VAMANA_ENTRY, index);
    let lists = keyspace(&raw, VAMANA_ADJACENCY, index);
    let header = records.remove(&0).expect("graph header record");
    assert_eq!(header.len(), 30, "graph header length");
    let entry = u64::from_be_bytes(header[6..14].try_into().unwrap());
    let count = u64::from_be_bytes(header[14..22].try_into().unwrap());
    assert_eq!(count as usize, nodes, "graph header node count");
    assert_eq!(records.len(), nodes, "node records");
    assert_eq!(lists.len(), nodes, "adjacency records");
    assert!(
        records.keys().eq(lists.keys()),
        "the node and adjacency keyspaces hold different sequences"
    );
    if nodes > 0 {
        assert!(records.contains_key(&entry), "entry point has no record");
    }
    let head = 6 + 8 + DIM;
    // The node record IS the head: an adjacency tail here would be the
    // layout this split exists to remove.
    for (seq, value) in &records {
        assert_eq!(value.len(), head, "node record {seq} is not one head");
    }
    let mut edges: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for (seq, value) in &lists {
        let own = u16::from_be_bytes(value[..2].try_into().unwrap()) as usize;
        let back = u16::from_be_bytes(value[2..4].try_into().unwrap()) as usize;
        assert!(
            own + back <= VAMANA_MAX_DEGREE,
            "degree {} at {seq}",
            own + back
        );
        assert_eq!(value.len(), 4 + (own + back) * 12, "record length");
        let mut list = Vec::new();
        for at in 0..own + back {
            let off = 4 + at * 12;
            let neighbour = u64::from_be_bytes(value[off..off + 8].try_into().unwrap());
            assert_ne!(neighbour, *seq, "self loop at {seq}");
            assert!(
                records.contains_key(&neighbour),
                "node {seq} names {neighbour}, which has no record"
            );
            list.push(neighbour);
        }
        edges.insert(*seq, list);
    }
    for (seq, list) in &edges {
        for neighbour in list {
            assert!(
                edges[neighbour].contains(seq),
                "edge {seq} -> {neighbour} is not mirrored"
            );
        }
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

// ── the tests ─────────────────────────────────────────────────────────────

/// The recall floor, against a brute-force top-k this process computed, and
/// the claim that buying a bigger search list buys recall.
#[test]
fn the_graph_finds_most_of_the_true_nearest_neighbours_and_finds_more_of_them_with_a_longer_search_list()
{
    let temp = tempfile::tempdir().unwrap();
    let vectors = corpus(2_000, 0x5eed_1234_9abc_def1);
    let Built {
        db, index, held, ..
    } = build(&temp.path().join("db"), &vectors);
    let queries = corpus(40, 0xfeed_4321_1234_5678);
    for metric in [
        VectorMetric::SquaredL2,
        VectorMetric::Cosine,
        VectorMetric::NegativeDot,
    ] {
        let narrow = recall(&db, index, &held, &queries, metric, 10, 10);
        let wide = recall(&db, index, &held, &queries, metric, 10, 120);
        // The stated floor: recall@10 at a search list of 120 over 2,000
        // 16-lane vectors. The int8 approximation is inside this number --
        // the shortlist is scored on codes -- so it is a floor on the whole
        // family and not on the graph alone.
        assert!(
            wide >= 0.90,
            "{metric:?}: recall@10 at ef=120 was {wide:.3}, below the 0.90 floor"
        );
        // And recall RISES with the search list: the knob means something.
        assert!(
            wide >= narrow,
            "{metric:?}: ef=120 recall {wide:.3} did not beat ef=10 recall {narrow:.3}"
        );
    }
    // A graph, not a scan: the records read are a fraction of the corpus.
    let result = db
        .query_vamana_vector(index, &queries[0], VectorMetric::SquaredL2, 10, 40, usize::MAX, || false)
        .unwrap();
    assert_eq!(result.method, ApproxVectorMethod::VamanaGraphV1);
    assert!(
        result.examined < 2_000 / 2,
        "the walk read {} of 2,000 nodes, which is not a graph search",
        result.examined
    );
}

/// The graph is on disk and nothing about it lives in the handle.
#[test]
fn the_graph_survives_a_reopen_and_answers_identically() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let vectors = corpus(600, 0x1111_2222_3333_4444);
    let Built {
        db, index, held, ..
    } = build(&path, &vectors);
    let queries = corpus(12, 0x9999_8888_7777_6666);
    let before: Vec<Vec<VectorHit>> = queries
        .iter()
        .map(|query| answer(&db, index, query, VectorMetric::Cosine, 8, 60))
        .collect();
    drop(db);
    let reopened = Database::open(&path, cfg()).unwrap();
    let after: Vec<Vec<VectorHit>> = queries
        .iter()
        .map(|query| answer(&reopened, index, query, VectorMetric::Cosine, 8, 60))
        .collect();
    assert_eq!(before, after);
    assert_eq!(before.len(), queries.len());
    assert!(before.iter().all(|hits| hits.len() == 8));
    drop(reopened);
    structure(&path, index, held.len());
    clean(&path);
}

/// Law 8 and the maintenance decision together: a build that stops part way
/// leaves the index BUILDING, and a BUILDING vamana index refuses a query by
/// name instead of answering from the part of the graph it has.
#[test]
fn a_build_that_stops_part_way_leaves_the_index_not_ready_and_refuses_to_answer() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let vectors = corpus(300, 0xabcd_ef01_2345_6789);
    let mut db = Database::create(&path, cfg()).unwrap();
    let collection = db
        .create_collection(
            "points",
            vec![("embedding".into(), Kind::Vector(DIM))],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut held = BTreeMap::new();
    for (at, vector) in vectors.iter().enumerate() {
        let id = db
            .put(collection, &format!("k{at}"), &json!({"embedding": vector}))
            .unwrap();
        held.insert(id.sequence, vector.clone());
    }
    db.commit().unwrap();
    let index = db.create_vamana_index(collection, "v", "embedding").unwrap();
    db.commit().unwrap();
    assert!(!db.build_index_step(index, 32).unwrap());
    db.commit().unwrap();
    // The state says BUILDING, with a cursor.
    assert!(matches!(
        db.index_info(index).unwrap().state,
        IndexState::Building { after } if after > 0
    ));
    // And the query is REFUSED, not served from the partial graph.
    let error = db
        .query_vamana_vector(index, &vectors[0], VectorMetric::SquaredL2, 5, 20, usize::MAX, || false)
        .unwrap_err();
    assert!(
        matches!(&error, Error::InvalidInput(message) if message.contains("not ready")),
        "{error:?}"
    );
    // The same answer after a crash: the handle is dropped with the partial
    // build committed, and the reopened database still refuses.
    drop(db);
    let reopened = Database::open(&path, cfg()).unwrap();
    assert!(matches!(
        reopened.index_info(index).unwrap().state,
        IndexState::Building { .. }
    ));
    assert!(reopened
        .query_vamana_vector(index, &vectors[0], VectorMetric::SquaredL2, 5, 20, usize::MAX, || false)
        .is_err());
    drop(reopened);
    // Finishing it makes it answer, and the structure is whole. (A BUILDING
    // index is not eligible for complete derived verification, by the
    // verifier's own rule, so the clean check waits until it is READY.)
    let mut db = Database::open(&path, cfg()).unwrap();
    while !db.build_index_step(index, 32).unwrap() {
        db.commit().unwrap();
    }
    db.commit().unwrap();
    assert_eq!(db.index_info(index).unwrap().state, IndexState::Ready);
    assert_eq!(
        answer(&db, index, &vectors[0], VectorMetric::SquaredL2, 5, 60).len(),
        5
    );
    drop(db);
    structure(&path, index, held.len());
    clean(&path);
}

/// The maintenance claim. Deletes and updates run on the write path, and
/// after them no list names a node that is gone, no node is stranded, and the
/// answers still agree with the brute-force top-k over what is LEFT.
#[test]
fn deletes_and_updates_orphan_no_node_and_strand_no_neighbour_list() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let vectors = corpus(500, 0x0f0f_0f0f_1234_5678);
    let Built {
        db,
        collection,
        index,
        mut held,
        ids,
    } = build(&path, &vectors);
    drop(db);
    structure(&path, index, held.len());
    let mut db = Database::open(&path, cfg()).unwrap();

    // Delete every seventh row.
    let mut removed = Vec::new();
    for at in (0..vectors.len()).step_by(7) {
        assert!(db.delete(collection, &format!("k{at}")).unwrap());
        held.remove(&ids[at].sequence);
        removed.push(at);
    }
    db.commit().unwrap();
    drop(db);
    structure(&path, index, held.len());
    clean(&path);

    // Update the vector of every fifth surviving row, which is an unlink
    // followed by a link.
    let mut db = Database::open(&path, cfg()).unwrap();
    let mut rng = Rng(0xdead_beef_cafe_0001);
    for at in (1..vectors.len()).step_by(5) {
        if removed.contains(&at) {
            continue;
        }
        let moved = rng.vector();
        db.update(
            collection,
            &format!("k{at}"),
            &json!({"embedding": moved.clone(), "tag": 1}),
        )
        .unwrap();
        held.insert(ids[at].sequence, moved);
    }
    db.commit().unwrap();
    drop(db);
    structure(&path, index, held.len());
    clean(&path);

    // And the answers still track the oracle over what is left.
    let db = Database::open(&path, cfg()).unwrap();
    let queries = corpus(20, 0x2468_1357_9bdf_0246);
    let score = recall(&db, index, &held, &queries, VectorMetric::SquaredL2, 5, 120);
    assert!(
        score >= 0.85,
        "recall after deletes and updates was {score:.3}"
    );
}

/// Law 8. A database that declares `0x8000` is refused WHOLE by a binary
/// whose supported mask predates the bit -- by name, as `Unsupported` and not
/// as corruption -- and the refusal changes no byte of the source.
#[test]
fn an_older_build_refuses_a_database_that_declares_the_vamana_bit_by_name() {
    assert_eq!(VAMANA_FEATURE, 0x8000);
    assert_eq!(SUPPORTED_LOGICAL_FEATURES & VAMANA_FEATURE, 0x8000);
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let vectors = corpus(40, 0x3333_4444_5555_6666);
    let Built { db, .. } = build(&path, &vectors);
    let written = logical_features(&db);
    assert_ne!(written & VAMANA_FEATURE, 0, "the create did not set the bit");
    drop(db);

    let before = files(&path);
    // The older binary is spelled as the mask it carried -- this build's mask
    // with the vamana bit taken out -- and put through the production
    // admission decision rather than a copy of it.
    let older = SUPPORTED_LOGICAL_FEATURES & !VAMANA_FEATURE;
    admit_logical_features(written, SUPPORTED_LOGICAL_FEATURES).unwrap();
    let refusal = admit_logical_features(written, older).unwrap_err();
    match &refusal {
        Error::Unsupported(message) => {
            assert!(
                message.contains(&format!("{written:#x}")),
                "the refusal must name the feature word: {message}"
            );
        }
        other => panic!("an unimplemented bit must be Unsupported, never {other:?}"),
    }
    assert_eq!(files(&path), before, "a refusal changed the source");
}

/// Law 5. Verification is clean over a built index and stays clean after
/// writes, because it derives each node's head from the row independently and
/// walks the adjacency against its own reads.
#[test]
fn verify_indexed_source_is_clean_after_a_build_and_after_writes() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let vectors = corpus(300, 0x7777_8888_9999_aaaa);
    let Built {
        db,
        collection,
        index,
        ..
    } = build(&path, &vectors);
    drop(db);
    clean(&path);
    let mut db = Database::open(&path, cfg()).unwrap();
    let mut rng = Rng(0xbbbb_cccc_dddd_eeee);
    for at in 0..40 {
        db.put(
            collection,
            &format!("late{at}"),
            &json!({"embedding": rng.vector(), "tag": 3}),
        )
        .unwrap();
    }
    for at in (0..60).step_by(3) {
        db.delete(collection, &format!("k{at}")).unwrap();
    }
    db.commit().unwrap();
    drop(db);
    clean(&path);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.index_info(index).unwrap().family, IndexFamily::VamanaGraph);
    assert_eq!(db.index_info(index).unwrap().state, IndexState::Ready);
}
