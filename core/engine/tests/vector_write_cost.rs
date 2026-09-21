//! What one bulk write into a collection with LIVE vector indexes costs, and
//! that it still answers the right rows.
//!
//! The engine change these tests hold down is on the write path, not the read
//! path: `maintain_indexes` used to re-read the whole index registry and every
//! index descriptor for EVERY row written, and the two vector families each
//! probed the store for an entry that a freshly allocated entity id cannot
//! have. Neither read changes a byte on disk, so the way to test their removal
//! is to show that (a) the answer is still the brute-force answer, (b) the
//! verifier still calls the file clean, and (c) the L2 counters -- the page-WAL
//! frames and bytes one batch writes -- are UNCHANGED, because work that was
//! never written cannot have been removed from what is written.
//!
//! The oracle is brute force held in this process: the test keeps the 1,000
//! vectors it wrote in a `Vec` and computes the exact top-10 itself, in the
//! same f64 lane order the engine accumulates in.
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{
        verification::{verify_indexed_source, VerificationLimits},
        CollectionId, CollectionOptions, Database, EntityId, IndexId, QuantizedVectorCandidates,
        VectorCandidates, VectorMetric,
    },
    Kind,
};
use serde_json::json;
use std::path::Path;

const DIM: usize = 32;
const ROWS: usize = 1_000;
const QUERIES: usize = 20;
const K: usize = 10;

fn cfg() -> Config {
    Config {
        budget_bytes: 4 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// A deterministic unit vector generator, so a failure is reproducible and no
/// test depends on a random seed the next run will not have.
fn vectors(count: usize, seed: u64) -> Vec<Vec<f32>> {
    let mut state = seed | 1;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let mut lanes = Vec::with_capacity(DIM);
        let mut norm = 0.0f64;
        for _ in 0..DIM {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let lane = f64::from((state >> 40) as u32 as u16) / 32_768.0 - 1.0;
            norm += lane * lane;
            lanes.push(lane as f32);
        }
        let norm = norm.sqrt().max(1e-9);
        for lane in lanes.iter_mut() {
            *lane = (f64::from(*lane) / norm) as f32;
        }
        out.push(lanes);
    }
    out
}

/// Cosine distance, brute force, in the engine's own lane order and width.
fn cosine(stored: &[f32], query: &[f32]) -> Option<f64> {
    let mut dot = 0.0f64;
    let mut stored_norm = 0.0f64;
    let mut query_norm = 0.0f64;
    for (&s, &q) in stored.iter().zip(query) {
        let (s, q) = (f64::from(s), f64::from(q));
        dot += s * q;
        stored_norm += s * s;
        query_norm += q * q;
    }
    if stored_norm == 0.0 {
        return None;
    }
    let distance = 1.0 - dot / (stored_norm.sqrt() * query_norm.sqrt());
    Some(if distance == 0.0 { 0.0 } else { distance })
}

/// The top-`K` entity ids for one query, computed from the vectors this test
/// is holding: ascending distance, entity id breaking a tie, which is the
/// order `QueryOrder::Distance` and `VectorHit` both use.
fn brute_force(rows: &[Vec<f32>], collection: CollectionId, query: &[f32]) -> Vec<EntityId> {
    let mut scored: Vec<(f64, EntityId)> = rows
        .iter()
        .enumerate()
        .filter_map(|(at, stored)| {
            cosine(stored, query).map(|distance| {
                (
                    distance,
                    EntityId {
                        collection,
                        sequence: at as u64 + 1,
                    },
                )
            })
        })
        .collect();
    scored.sort_by(|left, right| {
        left.0
            .total_cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
    });
    scored.truncate(K);
    scored.into_iter().map(|(_, id)| id).collect()
}

struct Written {
    collection: CollectionId,
    exact: Option<IndexId>,
    quantized: Option<IndexId>,
    /// Page-WAL frames and bytes the ONE batch appended: the L2 counters.
    frames: u64,
    bytes: u64,
}

/// Create a collection, make the two vector indexes LIVE on it while it is
/// still empty (so they are READY before any row arrives -- not a late build),
/// then write `rows` as ONE batch under a bulk scope whose close commits.
fn write_batch(path: &Path, rows: &[Vec<f32>], indexed: bool) -> (Database, Written) {
    let mut db = Database::create(path, cfg()).unwrap();
    let collection = db
        .create_collection_declared(
            "vec",
            vec![
                ("key".into(), Kind::Text),
                ("emb".into(), Kind::Vector(DIM)),
            ],
            vec![],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    let (exact, quantized) = if indexed {
        let exact = db.create_exact_vector_index(collection, "vec_exact", "emb").unwrap();
        db.commit().unwrap();
        assert!(db.build_index_step(exact, 256).unwrap(), "an empty index is READY in one step");
        db.commit().unwrap();
        let quantized = db
            .create_quantized_vector_index(collection, "vec_ann", "emb")
            .unwrap();
        db.commit().unwrap();
        assert!(db.build_index_step(quantized, 256).unwrap());
        db.commit().unwrap();
        (Some(exact), Some(quantized))
    } else {
        (None, None)
    };
    let before = db.io_counters().unwrap();
    db.begin_bulk().unwrap();
    for (at, lanes) in rows.iter().enumerate() {
        let key = format!("k{at:06}");
        let id = db
            .put(collection, &key, &json!({"key": key, "emb": lanes}))
            .unwrap();
        assert_eq!(
            id.sequence,
            at as u64 + 1,
            "the oracle indexes rows by sequence - 1, so the put order is the row order"
        );
    }
    assert!(db.end_bulk().unwrap(), "the outermost close commits");
    let after = db.io_counters().unwrap();
    (
        db,
        Written {
            collection,
            exact,
            quantized,
            frames: after.wal_frames_appended - before.wal_frames_appended,
            bytes: after.wal_bytes_written - before.wal_bytes_written,
        },
    )
}

#[test]
fn a_bulk_write_into_live_vector_indexes_answers_the_brute_force_top_ten() {
    let temp = tempfile::tempdir().unwrap();
    let rows = vectors(ROWS, 20_260_921);
    let (db, written) = write_batch(&temp.path().join("live"), &rows, true);
    let queries = vectors(QUERIES, 7);

    for query in &queries {
        let expected = brute_force(&rows, written.collection, query);
        let exact = db
            .query_exact_vector(
                written.exact.unwrap(),
                query,
                VectorMetric::Cosine,
                K,
                VectorCandidates::All,
                usize::MAX,
                || false,
            )
            .unwrap();
        assert_eq!(
            exact.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            expected,
            "the exact index did not return the brute-force top ten"
        );
        for (hit, id) in exact.iter().zip(&expected) {
            let at = (id.sequence - 1) as usize;
            assert_eq!(
                hit.distance.total_cmp(&cosine(&rows[at], query).unwrap()),
                std::cmp::Ordering::Equal,
                "the exact distance is not the brute-force distance"
            );
        }
        // The quantized companion reranks its shortlist from the same
        // authoritative sidecars, so at an `ef` this wide it owes the same
        // ten rows the exact index owes.
        let approximate = db
            .query_quantized_vector(
                written.quantized.unwrap(),
                query,
                VectorMetric::Cosine,
                K,
                ROWS,
                QuantizedVectorCandidates::All,
                usize::MAX,
                || false,
            )
            .unwrap();
        assert_eq!(
            approximate.hits.iter().map(|hit| hit.id).collect::<Vec<_>>(),
            expected,
            "the quantized index at ef = the corpus did not return the brute-force top ten"
        );
    }
    drop(db);

    let report = verify_indexed_source(
        temp.path().join("live"),
        VerificationLimits::default(),
        |issue| panic!("verification issue after a live-index bulk write: {issue:?}"),
    )
    .unwrap();
    assert!(
        report.complete && report.clean,
        "verify_indexed_source is not clean after a live-index bulk write: {report:?}"
    );
}

#[test]
fn the_index_maintenance_of_a_bulk_write_writes_exactly_what_it_wrote_before() {
    // L2. The write path stopped READING the index registry per row and
    // stopped probing for an entry a fresh entity id cannot have. Neither is
    // a write, so the frames and bytes one batch appends must be the numbers
    // they always were -- and they must still be STRICTLY more than the same
    // batch with no index, or the maintenance is not happening at all.
    let temp = tempfile::tempdir().unwrap();
    let rows = vectors(ROWS, 5);
    let (indexed_db, indexed) = write_batch(&temp.path().join("indexed"), &rows, true);
    let (bare_db, bare) = write_batch(&temp.path().join("bare"), &rows, false);
    drop(indexed_db);
    drop(bare_db);

    // Frozen on 2026-09-21 from the engine this test ships with: 1,000 rows
    // of a 32-lane vector, one batch, one commit. A change to either number
    // is a change to what a live-index vector write puts on disk and has to
    // be argued for, not absorbed.
    assert_eq!(
        (indexed.frames, indexed.bytes),
        (77, 323_232),
        "the live-index batch's page-WAL frames and bytes moved"
    );
    assert_eq!(
        (bare.frames, bare.bytes),
        (54, 227_920),
        "the no-index batch's page-WAL frames and bytes moved"
    );
    assert!(
        indexed.frames > bare.frames && indexed.bytes > bare.bytes,
        "index maintenance wrote nothing: {indexed:?} vs {bare:?}",
        indexed = (indexed.frames, indexed.bytes),
        bare = (bare.frames, bare.bytes),
    );
}

#[test]
fn a_replacing_write_still_reads_the_entry_it_may_have_to_leave_alone() {
    // The probe removed on the insert path is NOT removed on the update path:
    // a row written over an existing one can already have a locator and a
    // compact entry, and an unchanged vector must leave both exactly where
    // they are while a changed one must replace both. This is the case the
    // `fresh` argument refuses to claim, and the oracle is the answer: both
    // indexes still have to return the brute-force nearest row afterwards.
    let temp = tempfile::tempdir().unwrap();
    let mut rows = vectors(8, 11);
    let (mut db, written) = write_batch(&temp.path().join("update"), &rows, true);
    let collection = written.collection;
    let exact = written.exact.unwrap();
    let quantized = written.quantized.unwrap();

    let nearest = |db: &Database, query: &[f32]| -> (EntityId, EntityId) {
        let e = db
            .query_exact_vector(
                exact,
                query,
                VectorMetric::Cosine,
                1,
                VectorCandidates::All,
                usize::MAX,
                || false,
            )
            .unwrap();
        let a = db
            .query_quantized_vector(
                quantized,
                query,
                VectorMetric::Cosine,
                1,
                8,
                QuantizedVectorCandidates::All,
                usize::MAX,
                || false,
            )
            .unwrap();
        (e[0].id, a.hits[0].id)
    };

    // Rewriting a row with the SAME vector must not disturb either family.
    db.put(
        collection,
        "k000001",
        &json!({"key": "k000001", "emb": rows[1]}),
    )
    .unwrap();
    db.commit().unwrap();
    let expected = brute_force(&rows, collection, &rows[1])[0];
    assert_eq!(nearest(&db, &rows[1]), (expected, expected));

    // Rewriting it with a DIFFERENT vector must replace both entries, and the
    // oracle moves with it: the test's own copy of the corpus is updated too.
    let replacement = vectors(1, 999);
    rows[1] = replacement[0].clone();
    db.put(
        collection,
        "k000001",
        &json!({"key": "k000001", "emb": rows[1]}),
    )
    .unwrap();
    db.commit().unwrap();
    let expected = brute_force(&rows, collection, &rows[1])[0];
    assert_eq!(
        expected,
        EntityId {
            collection,
            sequence: 2
        },
        "the replaced row should be nearest to its own new vector"
    );
    assert_eq!(nearest(&db, &rows[1]), (expected, expected));
    // And the row that used to hold the old vector is no longer claimed by it.
    let expected = brute_force(&rows, collection, &replacement[0])[0];
    assert_eq!(nearest(&db, &replacement[0]), (expected, expected));
    drop(db);

    let report = verify_indexed_source(
        temp.path().join("update"),
        VerificationLimits::default(),
        |issue| panic!("verification issue after a replacing write: {issue:?}"),
    )
    .unwrap();
    assert!(report.complete && report.clean, "{report:?}");
}
