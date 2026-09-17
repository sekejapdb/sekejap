//! A rollback must forget EVERY tree's append hint, not just the primary
//! tree's.
//!
//! `PageWalStore` keeps one append hint for tree 1 and an eight-slot table of
//! hints for every other tree a handle has touched -- the per-index trees. A
//! rollback rewinds the file to its last committed extent, so any hint left
//! over from the discarded transaction can name a page that no longer exists.
//!
//! That is not merely a stale guess that costs one descent. The fast path
//! READS the hinted leaf in order to re-check its shape, so the read happens
//! first and fails outright: `Io(UnexpectedEof: failed to fill whole buffer)`.
//! The next perfectly ordinary write after a rollback is what surfaces it.
//!
//! The property under test is therefore: after a rollback, a handle is still
//! usable, and what it goes on to write is correct.

use e4_prototype::{
    collections::{
        set_create_index_trees,
        verification::{verify_indexed_source, VerificationLimits},
        CollectionOptions, Database, IndexState, ScalarPredicate,
    },
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn person(i: u64) -> serde_json::Value {
    json!({ "age": (i * 37 % 100_003) as i64 })
}

/// A row whose index key is strictly ascending, so the index tree's append
/// hint follows the RIGHTMOST leaf -- which is the page a growing tree has
/// just allocated. That is the hint that a rollback can leave pointing past
/// the end of the file; a scattered key never parks the hint there.
fn ascending(i: u64) -> serde_json::Value {
    json!({ "age": i as i64 })
}

#[test]
fn a_rollback_forgets_every_trees_append_hint() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let previous = set_create_index_trees(true);
    let mut db = Database::create(&path, cfg()).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![("age".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    for i in 0..500u64 {
        db.put(people, &format!("p{i}"), &person(i)).unwrap();
    }
    db.commit().unwrap();

    let age = db
        .create_scalar_index(people, "age_idx", "age", false)
        .unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(age, 256).unwrap();
    assert_eq!(db.index_info(age).unwrap().state, IndexState::Ready);
    let committed = db.index_tree(age).unwrap().expect("the index owns a tree");
    assert_ne!(committed.1, 0);

    // A transaction big enough to grow the index's own tree and extend the
    // file, leaving that tree's append hint on a freshly allocated page --
    // and then discarded, exactly as a crash would discard it.
    for i in 1_000_000..1_012_000u64 {
        db.put(people, &format!("q{i}"), &ascending(i)).unwrap();
    }
    db.rollback().unwrap();
    assert_eq!(
        db.index_tree(age).unwrap().unwrap(),
        committed,
        "the rollback did not restore the last committed root"
    );

    // THE PROPERTY. Ordinary use of the same handle in a new transaction.
    // Both reads and writes reach a non-primary tree through the same hint,
    // so before the fix one of them reads the leaf the discarded transaction
    // left behind and fails with `Io(UnexpectedEof)` -- a read past the end
    // of the data file, for a page that only ever existed as a WAL frame the
    // rollback truncated away.
    for i in 2_000_000..2_000_400u64 {
        db.put(people, &format!("r{i}"), &ascending(i)).unwrap();
    }
    db.commit().unwrap();

    // And what it wrote is correct: the rolled-back rows are absent, the new
    // ones are present, and the index answers for both.
    assert!(db.get(people, "q1000000").unwrap().is_none());
    assert!(db.get(people, "r2000000").unwrap().is_some());
    for i in [2_000_000u64, 2_000_137, 2_000_399] {
        let hits = db
            .query_scalar(age, ScalarPredicate::Eq(ascending(i)["age"].clone()), 64)
            .unwrap();
        assert!(
            !hits.is_empty(),
            "the index lost the row written after a rollback"
        );
    }
    drop(db);

    let db = Database::open(&path, cfg()).unwrap();
    assert!(db.get(people, "r2000399").unwrap().is_some());
    drop(db);
    let report = verify_indexed_source(&path, VerificationLimits::default(), |_| {}).unwrap();
    assert!(
        report.complete && report.clean,
        "the database written after a rollback does not verify clean"
    );
    set_create_index_trees(previous);
}

/// The same property for the PRIMARY tree's per-keyspace hints (K1).
///
/// `PageWalStore::rollback` cleared the whole-tree hint and the eight per-tree
/// slots. The per-keyspace hints are a third table and they carry more than a
/// page number: each one also caches the FENCES its arming descent walked. A
/// survivor of a discarded transaction is therefore two kinds of wrong at once
/// -- it can name a page that only ever existed as a WAL frame the rollback
/// truncated away (an `Io(UnexpectedEof)` on the read that re-checks it), and
/// it can describe an interval of a tree shape that no longer exists.
///
/// Relationship rows are what make this reachable from the primary tree: their
/// forward keys ascend behind the row and mapping keyspaces, which is exactly
/// the run the per-keyspace hint exists to serve.
#[test]
fn a_rollback_forgets_the_per_keyspace_hints_of_the_primary_tree() {
    use e4_prototype::collections::{EntityId, GraphContextId};

    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let people = db
        .create_collection(
            "people",
            vec![("age".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.enable_graph().unwrap();
    let knows = db.create_edge_type("knows").unwrap();
    db.commit().unwrap();
    for i in 0..4_000u64 {
        db.put(people, &format!("p{i:06}"), &person(i)).unwrap();
    }
    db.commit().unwrap();
    db.checkpoint().unwrap();
    let entity = |i: u64| EntityId {
        collection: people,
        sequence: i,
    };

    // A discarded transaction whose forward-edge keys ascend, so the hint for
    // the 0x71 keyspace parks on a leaf this transaction allocated -- and then
    // the file is rewound past it.
    for i in 1..3_000u64 {
        db.put_edge(GraphContextId::BASE, entity(i), knows, entity(i + 1), &json!({}))
            .unwrap();
    }
    db.rollback().unwrap();

    // THE PROPERTY. The same handle, a new transaction, and an ascending run
    // that starts INSIDE the interval the discarded transaction's hint claims
    // -- the first edge here is the one a survivor would answer for. A
    // survivor either reads past the end of the data file or appends a row
    // below its own fence.
    for i in 2_900..3_400u64 {
        db.put_edge(GraphContextId::BASE, entity(i), knows, entity(i + 2), &json!({}))
            .unwrap();
    }
    db.commit().unwrap();
    assert!(
        db.neighbors(e4_prototype::collections::NeighborRequest {
            entity: entity(1),
            direction: e4_prototype::collections::Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(knows),
            limit: 16,
        })
        .unwrap()
        .is_empty(),
        "an edge from the discarded transaction survived the rollback"
    );

    // And the graph answers for what was written, not for what was discarded.
    for i in [2_900u64, 3_111, 3_399] {
        let out = db
            .neighbors(e4_prototype::collections::NeighborRequest {
                entity: entity(i),
                direction: e4_prototype::collections::Direction::Outgoing,
                context: GraphContextId::BASE,
                edge_type: Some(knows),
                limit: 16,
            })
            .unwrap();
        let seen: Vec<u64> = out.iter().map(|e| e.key.destination.sequence).collect();
        assert_eq!(seen, vec![i + 2], "row {i} does not have exactly the edge written after the rollback");
    }
    drop(db);
    let report = verify_indexed_source(&path, VerificationLimits::default(), |_| {}).unwrap();
    assert!(
        report.complete && report.clean,
        "the database written after a rollback does not verify clean"
    );
}
