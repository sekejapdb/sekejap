//! ENDPOINT SETS: one key per DISTINCT entity that has at least one edge of a
//! given (context, edge type, direction), in its own keyspace (tag `0x7E`)
//! behind the additive feature bit `ENDPOINT_FEATURE = 0x4000`.
//!
//! The oracle is held in the test process: a `BTreeSet` of
//! `(context, type, direction, entity)` DERIVED HERE from the edges this test
//! wrote, never read back from the engine's own second reading. Every
//! assertion below compares what `Database::edge_endpoints` answers, and what
//! the `0x7E` keyspace holds, against that set.
//!
//! `docs/core/GRAPH_CONTRACT.md` §4.1-§4.3 (the edge keyspace the set is
//! derived from), `docs/core/FORMAT_V2.md` (the tag registry and the additive
//! feature bit), `docs/core/FOUNDATION_TEST_STANDARD.md` L1, L3, L5, L8.
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{
        verification::{verify_indexed_source, VerificationLimits},
        CollectionId, Database, Direction, DropMode, EdgeTypeId, EntityId, Error, GraphContextId,
        NewEdge, QueryBudget, ENDPOINT_FEATURE, SUPPORTED_LOGICAL_FEATURES,
    },
    internal::{admit_logical_features, logical_features},
    pagewal::PageWalStore,
    Kind,
};
use serde_json::json;
use std::{
    collections::BTreeSet,
    path::Path,
};

const ENDPOINT_TAG: u8 = 0x7E;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// `PHASE2_INDEX_FORMAT.md`: `0x80 + width`, then minimal unsigned big-endian.
/// Spelled here so the test reads the keyspace with its own decoder.
fn ordered(n: u64) -> Vec<u8> {
    let b = n.to_be_bytes();
    let start = b.iter().position(|x| *x != 0).unwrap_or(7);
    let mut k = vec![0x80 + (8 - start) as u8];
    k.extend_from_slice(&b[start..]);
    k
}

fn read_ordered(key: &[u8], at: &mut usize) -> u64 {
    let width = usize::from(key[*at]) - 0x80;
    *at += 1;
    let mut out = 0u64;
    for b in &key[*at..*at + width] {
        out = (out << 8) | u64::from(*b);
    }
    *at += width;
    out
}

/// One entry of the oracle and of the keyspace: the four fields the key
/// carries, in the order it carries them.
type Endpoint = (u64, u64, u8, u32, u64);

fn endpoint(context: GraphContextId, edge_type: EdgeTypeId, dir: u8, id: EntityId) -> Endpoint {
    (
        context.0,
        edge_type.0,
        dir,
        id.collection.0,
        id.sequence,
    )
}

/// The key this test expects the engine to write for one endpoint:
/// `tag || context || type || direction || collection || sequence`.
fn endpoint_key(e: Endpoint) -> Vec<u8> {
    let mut k = vec![ENDPOINT_TAG];
    k.extend(ordered(e.0));
    k.extend(ordered(e.1));
    k.push(e.2);
    k.extend(ordered(u64::from(e.3)));
    k.extend(ordered(e.4));
    k
}

/// Every `0x7E` key on disk, decoded by this test rather than by the engine.
fn endpoint_keyspace(path: &Path) -> BTreeSet<Endpoint> {
    let raw = PageWalStore::open_snapshot(path, 1 << 20).unwrap();
    let mut out = BTreeSet::new();
    for row in raw.range(&[ENDPOINT_TAG]).unwrap() {
        let (key, value) = row.unwrap();
        if key.first() != Some(&ENDPOINT_TAG) {
            break;
        }
        assert!(value.is_empty(), "an endpoint key carries no value");
        let mut at = 1;
        let context = read_ordered(&key, &mut at);
        let edge_type = read_ordered(&key, &mut at);
        let dir = key[at];
        at += 1;
        let collection = u32::try_from(read_ordered(&key, &mut at)).unwrap();
        let sequence = read_ordered(&key, &mut at);
        assert_eq!(at, key.len(), "an endpoint key has exactly five fields");
        assert!(dir <= 1, "direction is 0 (outgoing) or 1 (incoming)");
        out.insert((context, edge_type, dir, collection, sequence));
    }
    out
}

/// The edges this test has written, as the test remembers them. The oracle is
/// derived from this map and from nothing the engine says.
#[derive(Default)]
struct Edges {
    live: BTreeSet<(u64, u64, u32, u64, u32, u64)>,
}

impl Edges {
    fn link(&mut self, context: GraphContextId, t: EdgeTypeId, s: EntityId, d: EntityId) {
        self.live.insert((
            context.0,
            t.0,
            s.collection.0,
            s.sequence,
            d.collection.0,
            d.sequence,
        ));
    }
    fn unlink(&mut self, context: GraphContextId, t: EdgeTypeId, s: EntityId, d: EntityId) {
        self.live.remove(&(
            context.0,
            t.0,
            s.collection.0,
            s.sequence,
            d.collection.0,
            d.sequence,
        ));
    }
    /// Every edge incident on `id`, in either direction, forgotten.
    fn forget_entity(&mut self, id: EntityId) {
        self.live.retain(|e| {
            (e.2, e.3) != (id.collection.0, id.sequence)
                && (e.4, e.5) != (id.collection.0, id.sequence)
        });
    }
    fn forget_collection(&mut self, c: CollectionId) {
        self.live.retain(|e| e.2 != c.0 && e.4 != c.0);
    }
    /// THE ORACLE: one entry per distinct (context, type, direction, entity)
    /// that has at least one live edge, computed by brute force here.
    fn oracle(&self) -> BTreeSet<Endpoint> {
        let mut out = BTreeSet::new();
        for (context, t, sc, ss, dc, ds) in &self.live {
            out.insert((*context, *t, 0u8, *sc, *ss));
            out.insert((*context, *t, 1u8, *dc, *ds));
        }
        out
    }
    /// The oracle's answer to `edge_endpoints`: the sequences of one
    /// collection, ascending.
    fn ids(
        &self,
        collection: CollectionId,
        context: GraphContextId,
        t: EdgeTypeId,
        dir: u8,
    ) -> Vec<u64> {
        let mut out: Vec<u64> = self
            .oracle()
            .into_iter()
            .filter(|e| e.0 == context.0 && e.1 == t.0 && e.2 == dir && e.3 == collection.0)
            .map(|e| e.4)
            .collect();
        out.sort_unstable();
        out
    }
}

fn answered(db: &Database, c: CollectionId, t: EdgeTypeId, dir: Direction) -> Vec<u64> {
    db.edge_endpoints(
        c,
        GraphContextId::BASE,
        t,
        dir,
        usize::MAX,
        usize::MAX,
        QueryBudget::unlimited(),
        || false,
    )
    .unwrap()
    .into_iter()
    .map(|id| id.sequence)
    .collect()
}

/// A `place` collection with `rows` rows, and a `related` edge type.
fn fixture(path: &Path, rows: usize) -> (Database, CollectionId, Vec<EntityId>) {
    let mut db = Database::create(path, cfg()).unwrap();
    let place = db
        .create_collection(
            "place",
            vec![("n".into(), Kind::Int)],
            Default::default(),
        )
        .unwrap();
    db.enable_graph().unwrap();
    let mut ids = Vec::new();
    for n in 0..rows {
        ids.push(db.put(place, &format!("p{n}"), &json!({ "n": n })).unwrap());
    }
    db.commit().unwrap();
    (db, place, ids)
}

/// Every assertion this file makes about a database that is up to date: the
/// keyspace equals the oracle, and the read path answers from it.
fn agrees(db: &Database, path: &Path, edges: &Edges, c: CollectionId, t: EdgeTypeId) {
    assert_eq!(
        endpoint_keyspace(path),
        edges.oracle(),
        "the 0x7E keyspace disagrees with the set derived from the edges"
    );
    assert_eq!(
        answered(db, c, t, Direction::Outgoing),
        edges.ids(c, GraphContextId::BASE, t, 0),
        "edge_endpoints(Outgoing) disagrees with the oracle"
    );
    assert_eq!(
        answered(db, c, t, Direction::Incoming),
        edges.ids(c, GraphContextId::BASE, t, 1),
        "edge_endpoints(Incoming) disagrees with the oracle"
    );
}

#[test]
fn a_link_and_a_link_many_write_one_key_per_distinct_endpoint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, place, ids) = fixture(&path, 12);
    let mut edges = Edges::default();

    // `link` interns the type and writes the first edge; the feature bit
    // rides that first key.
    assert!(
        !db.endpoint_sets_present(),
        "a file with no edge yet declares nothing"
    );
    db.link(ids[0], "related", ids[1], "", &json!({})).unwrap();
    db.commit().unwrap();
    let t = db.edge_type("related").unwrap().unwrap();
    edges.link(GraphContextId::BASE, t, ids[0], ids[1]);
    assert!(db.endpoint_sets_present(), "the bit rides the first key");
    assert_eq!(
        logical_features(&db) & ENDPOINT_FEATURE,
        ENDPOINT_FEATURE,
        "the file must declare what it wrote"
    );
    agrees(&db, &path, &edges, place, t);

    // A second edge out of the SAME source adds no key: the set is one key
    // per distinct entity, not per edge.
    let before = endpoint_keyspace(&path).len();
    db.link(ids[0], "related", ids[2], "", &json!({})).unwrap();
    db.commit().unwrap();
    edges.link(GraphContextId::BASE, t, ids[0], ids[2]);
    assert_eq!(
        endpoint_keyspace(&path).len(),
        before + 1,
        "only the new destination is new"
    );
    agrees(&db, &path, &edges, place, t);

    // `link_many`, with repeats inside the batch and an entity that is both a
    // source and a destination.
    let batch: Vec<NewEdge> = [(3, 4), (3, 5), (4, 5), (5, 3), (3, 4)]
        .iter()
        .map(|(s, d)| NewEdge {
            source: ids[*s],
            destination: ids[*d],
            properties: json!({}),
        })
        .collect();
    db.link_many(GraphContextId::BASE, t, &batch).unwrap();
    db.commit().unwrap();
    for (s, d) in [(3, 4), (3, 5), (4, 5), (5, 3)] {
        edges.link(GraphContextId::BASE, t, ids[s], ids[d]);
    }
    agrees(&db, &path, &edges, place, t);

    // Re-writing an edge that already exists writes no key and changes
    // nothing: the set is idempotent under a repeated put.
    let before = endpoint_keyspace(&path);
    db.link(ids[3], "related", ids[4], "", &json!({"w": 1}))
        .unwrap();
    db.commit().unwrap();
    assert_eq!(endpoint_keyspace(&path), before);
    agrees(&db, &path, &edges, place, t);
}

#[test]
fn a_second_context_and_a_second_type_are_separate_sets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, place, ids) = fixture(&path, 8);

    db.link(ids[0], "related", ids[1], "", &json!({})).unwrap();
    db.link(ids[0], "cites", ids[2], "", &json!({})).unwrap();
    db.link(ids[0], "related", ids[3], "other", &json!({}))
        .unwrap();
    db.commit().unwrap();

    let related = db.edge_type("related").unwrap().unwrap();
    let cites = db.edge_type("cites").unwrap().unwrap();
    let other = db.graph_context("other").unwrap().unwrap();

    let mut oracle = BTreeSet::new();
    oracle.insert(endpoint(GraphContextId::BASE, related, 0, ids[0]));
    oracle.insert(endpoint(GraphContextId::BASE, related, 1, ids[1]));
    oracle.insert(endpoint(GraphContextId::BASE, cites, 0, ids[0]));
    oracle.insert(endpoint(GraphContextId::BASE, cites, 1, ids[2]));
    oracle.insert(endpoint(other, related, 0, ids[0]));
    oracle.insert(endpoint(other, related, 1, ids[3]));
    assert_eq!(endpoint_keyspace(&path), oracle);

    // The base graph's `related` set names one source; the other context's
    // names the same entity under its own key, and neither leaks into the
    // other's answer.
    assert_eq!(
        answered(&db, place, related, Direction::Incoming),
        vec![ids[1].sequence]
    );
    assert_eq!(
        db.edge_endpoints(
            place,
            other,
            related,
            Direction::Incoming,
            usize::MAX,
            usize::MAX,
            QueryBudget::unlimited(),
            || false,
        )
        .unwrap()
        .into_iter()
        .map(|id| id.sequence)
        .collect::<Vec<_>>(),
        vec![ids[3].sequence]
    );
}

#[test]
fn an_unlink_removes_a_key_only_when_the_entity_s_last_edge_of_that_direction_goes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, place, ids) = fixture(&path, 8);
    let mut edges = Edges::default();

    for d in [1, 2, 3] {
        db.link(ids[0], "related", ids[d], "", &json!({})).unwrap();
    }
    db.commit().unwrap();
    let t = db.edge_type("related").unwrap().unwrap();
    for d in [1, 2, 3] {
        edges.link(GraphContextId::BASE, t, ids[0], ids[d]);
    }
    agrees(&db, &path, &edges, place, t);

    // Two of the three go. The source still has an edge, so its OUTGOING key
    // stays; each destination loses its last incoming edge, so its key goes.
    for d in [1, 2] {
        assert!(db.unlink(ids[0], "related", ids[d], "").unwrap());
        db.commit().unwrap();
        edges.unlink(GraphContextId::BASE, t, ids[0], ids[d]);
        agrees(&db, &path, &edges, place, t);
    }
    assert!(endpoint_keyspace(&path).contains(&endpoint(GraphContextId::BASE, t, 0, ids[0])));

    // The last one goes and the source's key goes with it.
    assert!(db.unlink(ids[0], "related", ids[3], "").unwrap());
    db.commit().unwrap();
    edges.unlink(GraphContextId::BASE, t, ids[0], ids[3]);
    agrees(&db, &path, &edges, place, t);
    assert!(endpoint_keyspace(&path).is_empty(), "no edge, no key");

    // An unlink of an edge that is not there removes nothing.
    db.link(ids[4], "related", ids[5], "", &json!({})).unwrap();
    db.commit().unwrap();
    edges.link(GraphContextId::BASE, t, ids[4], ids[5]);
    assert!(!db.unlink(ids[4], "related", ids[6], "").unwrap());
    db.commit().unwrap();
    agrees(&db, &path, &edges, place, t);
}

#[test]
fn a_cascade_delete_takes_the_entity_s_keys_and_every_neighbour_s_last_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, place, ids) = fixture(&path, 8);
    let mut edges = Edges::default();

    // 0 -> 1, 0 -> 2, 3 -> 0, 3 -> 1. Deleting 0 must remove 0's two keys,
    // 2's incoming key (its last), but NOT 1's (it still has 3 -> 1) and NOT
    // 3's outgoing key (it still has 3 -> 1).
    for (s, d) in [(0, 1), (0, 2), (3, 0), (3, 1)] {
        db.link(ids[s], "related", ids[d], "", &json!({})).unwrap();
    }
    db.commit().unwrap();
    let t = db.edge_type("related").unwrap().unwrap();
    for (s, d) in [(0, 1), (0, 2), (3, 0), (3, 1)] {
        edges.link(GraphContextId::BASE, t, ids[s], ids[d]);
    }
    agrees(&db, &path, &edges, place, t);

    assert!(db.delete(place, "p0").unwrap());
    db.commit().unwrap();
    edges.forget_entity(ids[0]);
    agrees(&db, &path, &edges, place, t);
    let live = endpoint_keyspace(&path);
    assert!(live.contains(&endpoint(GraphContextId::BASE, t, 1, ids[1])));
    assert!(live.contains(&endpoint(GraphContextId::BASE, t, 0, ids[3])));
    assert!(!live.contains(&endpoint(GraphContextId::BASE, t, 1, ids[2])));
    assert!(!live.contains(&endpoint(GraphContextId::BASE, t, 0, ids[0])));
    assert!(!live.contains(&endpoint(GraphContextId::BASE, t, 1, ids[0])));
}

#[test]
fn a_dropped_collection_leaves_no_endpoint_key_behind() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, place, ids) = fixture(&path, 6);
    let other = db
        .create_collection("thing", vec![("n".into(), Kind::Int)], Default::default())
        .unwrap();
    let mut others = Vec::new();
    for n in 0..3 {
        others.push(db.put(other, &format!("t{n}"), &json!({ "n": n })).unwrap());
    }
    db.commit().unwrap();
    let mut edges = Edges::default();

    for (s, d) in [(0, 1), (1, 2), (2, 0)] {
        db.link(ids[s], "related", ids[d], "", &json!({})).unwrap();
    }
    // One edge each way between the two collections, so the drop has to
    // maintain the SURVIVING collection's keys too.
    db.link(ids[3], "related", others[0], "", &json!({}))
        .unwrap();
    db.link(others[1], "related", ids[4], "", &json!({}))
        .unwrap();
    db.commit().unwrap();
    let t = db.edge_type("related").unwrap().unwrap();
    for (s, d) in [(0, 1), (1, 2), (2, 0)] {
        edges.link(GraphContextId::BASE, t, ids[s], ids[d]);
    }
    edges.link(GraphContextId::BASE, t, ids[3], others[0]);
    edges.link(GraphContextId::BASE, t, others[1], ids[4]);
    agrees(&db, &path, &edges, place, t);

    db.begin_drop_collection_mode(place, DropMode::Cascade)
        .unwrap();
    loop {
        if db.drop_collection_step(place, 4).unwrap().done {
            break;
        }
    }
    db.commit().unwrap();
    edges.forget_collection(place);
    assert_eq!(
        endpoint_keyspace(&path),
        edges.oracle(),
        "a drop left endpoint keys of the collection it removed"
    );
    assert!(
        endpoint_keyspace(&path).is_empty(),
        "every edge named a dropped row, so every key is gone"
    );
}

#[test]
fn a_rollback_discards_the_keys_of_the_edges_it_discards() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, place, ids) = fixture(&path, 6);
    let mut edges = Edges::default();

    db.link(ids[0], "related", ids[1], "", &json!({})).unwrap();
    db.commit().unwrap();
    let t = db.edge_type("related").unwrap().unwrap();
    edges.link(GraphContextId::BASE, t, ids[0], ids[1]);
    agrees(&db, &path, &edges, place, t);

    // Written and rolled back: neither the edge nor its two keys survive.
    db.link(ids[2], "related", ids[3], "", &json!({})).unwrap();
    db.rollback().unwrap();
    agrees(&db, &path, &edges, place, t);

    // And a rolled-back REMOVAL leaves the key it was about to take.
    db.unlink(ids[0], "related", ids[1], "").unwrap();
    db.rollback().unwrap();
    agrees(&db, &path, &edges, place, t);
}

#[test]
fn the_set_answers_the_same_question_after_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, place, ids) = fixture(&path, 10);
    let mut edges = Edges::default();
    for (s, d) in [(0, 1), (0, 2), (1, 2), (3, 4), (4, 3)] {
        db.link(ids[s], "related", ids[d], "", &json!({})).unwrap();
    }
    db.commit().unwrap();
    let t = db.edge_type("related").unwrap().unwrap();
    for (s, d) in [(0, 1), (0, 2), (1, 2), (3, 4), (4, 3)] {
        edges.link(GraphContextId::BASE, t, ids[s], ids[d]);
    }
    drop(db);

    let db = Database::open(&path, cfg()).unwrap();
    assert!(db.endpoint_sets_present());
    agrees(&db, &path, &edges, place, t);
    drop(db);
    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        panic!("a good endpoint keyspace reported {issue:?}");
    })
    .unwrap();
    assert!(report.complete && report.clean);
}

/// A database whose edges PREDATE the bit keeps today's walk, and one bounded,
/// resumable pass builds the set it was missing.
#[test]
fn a_backfill_of_a_database_written_before_the_bit_equals_the_derived_set() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, place, ids) = fixture(&path, 40);
    let mut edges = Edges::default();
    for s in 0..30usize {
        for d in [(s + 1) % 40, (s + 7) % 40, (s + 13) % 40] {
            db.link(ids[s], "related", ids[d], "", &json!({})).unwrap();
        }
    }
    db.commit().unwrap();
    let t = db.edge_type("related").unwrap().unwrap();
    for s in 0..30usize {
        for d in [(s + 1) % 40, (s + 7) % 40, (s + 13) % 40] {
            edges.link(GraphContextId::BASE, t, ids[s], ids[d]);
        }
    }
    agrees(&db, &path, &edges, place, t);
    drop(db);

    // Make the file look like one an older binary wrote: the keyspace empty
    // and the bit clear. Everything else about it is untouched.
    strip_endpoint_sets(&path);
    assert!(endpoint_keyspace(&path).is_empty());

    let mut db = Database::open(&path, cfg()).unwrap();
    assert!(!db.endpoint_sets_present(), "the stripped file declares nothing");
    // The read path still answers, by the walk it has always taken.
    assert_eq!(
        answered(&db, place, t, Direction::Outgoing),
        edges.ids(place, GraphContextId::BASE, t, 0)
    );
    // A write on such a file maintains nothing: the set would be incomplete,
    // and an incomplete set believed is a wrong answer.
    db.link(ids[35], "related", ids[36], "", &json!({})).unwrap();
    db.commit().unwrap();
    edges.link(GraphContextId::BASE, t, ids[35], ids[36]);
    assert!(!db.endpoint_sets_present());
    assert!(endpoint_keyspace(&path).is_empty());
    assert_eq!(
        answered(&db, place, t, Direction::Outgoing),
        edges.ids(place, GraphContextId::BASE, t, 0)
    );

    // BOUNDED and RESUMABLE: a budget of 7 edges per call, so the pass takes
    // many calls, and every one of them commits what it wrote.
    let mut calls = 0u32;
    loop {
        let progress = db.backfill_endpoint_sets(7).unwrap();
        calls += 1;
        assert!(progress.edges_seen <= 7, "a step exceeded its budget");
        if progress.done {
            break;
        }
        assert!(calls < 10_000, "the backfill did not terminate");
    }
    assert!(calls > 3, "a 91-edge graph at 7 edges a step took {calls} steps");
    db.commit().unwrap();
    assert!(db.endpoint_sets_present(), "the backfill did not set the bit");
    agrees(&db, &path, &edges, place, t);

    // Idempotent: a second pass over a finished set writes the same keys.
    let before = endpoint_keyspace(&path);
    loop {
        if db.backfill_endpoint_sets(7).unwrap().done {
            break;
        }
    }
    db.commit().unwrap();
    assert_eq!(endpoint_keyspace(&path), before);

    // And from here the ordinary write path maintains it again.
    db.link(ids[37], "related", ids[38], "", &json!({})).unwrap();
    db.commit().unwrap();
    edges.link(GraphContextId::BASE, t, ids[37], ids[38]);
    agrees(&db, &path, &edges, place, t);
    drop(db);

    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        panic!("a backfilled endpoint keyspace reported {issue:?}");
    })
    .unwrap();
    assert!(report.complete && report.clean);
}

/// Law 8: an intact file that declares the bit is refused by a binary that
/// predates it as `Unsupported`, before a record is read -- never as damage.
#[test]
fn an_endpoint_set_file_is_unsupported_to_a_binary_that_predates_the_bit() {
    assert_eq!(ENDPOINT_FEATURE, 0x4000);
    assert_eq!(
        SUPPORTED_LOGICAL_FEATURES & ENDPOINT_FEATURE,
        ENDPOINT_FEATURE,
        "the mask this build publishes must contain the bit it writes"
    );
    let older = SUPPORTED_LOGICAL_FEATURES & !ENDPOINT_FEATURE;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, _, ids) = fixture(&path, 4);
    db.link(ids[0], "related", ids[1], "", &json!({})).unwrap();
    db.commit().unwrap();
    let written = logical_features(&db);
    drop(db);
    assert_eq!(written & ENDPOINT_FEATURE, ENDPOINT_FEATURE);

    // This build opens the file it writes.
    admit_logical_features(written, SUPPORTED_LOGICAL_FEATURES).unwrap();
    Database::open(&path, cfg()).unwrap();

    // The binary that predates the bit refuses it WHOLE, and as Unsupported.
    let refused = admit_logical_features(written, older).unwrap_err();
    assert!(
        matches!(refused, Error::Unsupported(ref m) if m.contains(&format!("{written:#x}"))),
        "an intact newer file must be Unsupported and name its feature word: {refused:?}"
    );

    // And the bit cannot be cleared to smuggle the keyspace past a reader
    // that would not understand it.
    clear_endpoint_bit(&path);
    assert!(matches!(
        Database::open(&path, cfg()),
        Err(Error::Corrupt(_))
    ));
}

#[test]
fn verification_names_a_missing_key_and_an_extra_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, _, ids) = fixture(&path, 6);
    for (s, d) in [(0, 1), (0, 2), (3, 4)] {
        db.link(ids[s], "related", ids[d], "", &json!({})).unwrap();
    }
    db.commit().unwrap();
    let t = db.edge_type("related").unwrap().unwrap();
    drop(db);

    // A key the edges do not justify.
    let extra = endpoint_key(endpoint(GraphContextId::BASE, t, 0, ids[5]));
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    raw.put(&extra, &[]).unwrap();
    raw.commit().unwrap();
    drop(raw);
    let mut seen = Vec::new();
    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        seen.push(issue.message.clone());
    })
    .unwrap();
    assert!(report.complete && !report.clean);
    assert!(
        seen.iter().any(|m| m.contains("endpoint key names an entity with no such edge")),
        "the extra key was not named: {seen:?}"
    );

    // And one the edges DO justify, taken away.
    let mut raw = PageWalStore::open(&path, false, 1 << 20).unwrap();
    raw.delete(&extra).unwrap();
    raw.delete(&endpoint_key(endpoint(GraphContextId::BASE, t, 1, ids[4])))
        .unwrap();
    raw.commit().unwrap();
    drop(raw);
    let mut seen = Vec::new();
    let report = verify_indexed_source(&path, VerificationLimits::default(), |issue| {
        seen.push(issue.message.clone());
    })
    .unwrap();
    assert!(report.complete && !report.clean);
    assert!(
        seen.iter().any(|m| m.contains("endpoint key missing")),
        "the missing key was not named: {seen:?}"
    );
}

/// The read path charges the caller's budget one `GraphVisited` per KEY, and
/// no `GraphEdges` at all: it never touches the edge keyspace.
#[test]
fn the_read_walk_charges_one_visit_per_key_and_no_edges() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let (mut db, place, ids) = fixture(&path, 10);
    for (s, d) in [(0, 1), (0, 2), (0, 3), (1, 2)] {
        db.link(ids[s], "related", ids[d], "", &json!({})).unwrap();
    }
    db.commit().unwrap();
    let t = db.edge_type("related").unwrap().unwrap();

    // Two distinct sources, four edges. A budget of two visits is enough.
    let mut budget = QueryBudget::unlimited();
    budget.graph_visited = 2;
    assert_eq!(
        db.edge_endpoints(
            place,
            GraphContextId::BASE,
            t,
            Direction::Outgoing,
            usize::MAX,
            usize::MAX,
            budget,
            || false,
        )
        .unwrap()
        .len(),
        2
    );
    // One is not.
    let mut budget = QueryBudget::unlimited();
    budget.graph_visited = 1;
    assert!(db
        .edge_endpoints(
            place,
            GraphContextId::BASE,
            t,
            Direction::Outgoing,
            usize::MAX,
            usize::MAX,
            budget,
            || false,
        )
        .is_err());
}

// ── making a file that predates the bit ───────────────────────────────────

fn reseal(packet: &mut [u8]) {
    let end = packet.len() - 4;
    let checksum = crc32c::crc32c(&packet[..end]).to_le_bytes();
    packet[end..].copy_from_slice(&checksum);
}

fn set_features(raw: &mut PageWalStore, features: u64) {
    for copy in 0..3u8 {
        let key = [0, 0, copy];
        let mut header = raw.get(&key).unwrap().unwrap();
        assert_eq!(&header[..8], b"E4COLL2\0");
        header[10 + 8..10 + 16].copy_from_slice(&features.to_be_bytes());
        reseal(&mut header);
        raw.put(&key, &header).unwrap();
    }
}

fn current_features(raw: &PageWalStore) -> u64 {
    let header = raw.get(&[0, 0, 0]).unwrap().unwrap();
    u64::from_be_bytes(header[10 + 8..10 + 16].try_into().unwrap())
}

/// The file an older binary would have written: the `0x7E` keyspace empty and
/// the bit clear, with every other byte of it left alone.
fn strip_endpoint_sets(path: &Path) {
    let mut raw = PageWalStore::open(path, false, 1 << 20).unwrap();
    let mut keys = Vec::new();
    for row in raw.range(&[ENDPOINT_TAG]).unwrap() {
        let (key, _) = row.unwrap();
        if key.first() != Some(&ENDPOINT_TAG) {
            break;
        }
        keys.push(key);
    }
    for key in keys {
        raw.delete(&key).unwrap();
    }
    let features = current_features(&raw) & !ENDPOINT_FEATURE;
    set_features(&mut raw, features);
    raw.commit().unwrap();
}

/// The bit cleared with the keyspace left in place -- the smuggling a reader
/// must refuse.
fn clear_endpoint_bit(path: &Path) {
    let mut raw = PageWalStore::open(path, false, 1 << 20).unwrap();
    let features = current_features(&raw) & !ENDPOINT_FEATURE;
    set_features(&mut raw, features);
    raw.commit().unwrap();
}
