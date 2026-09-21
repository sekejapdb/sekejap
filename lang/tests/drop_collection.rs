//! `begin_drop_collection` / `drop_collection_step`: the bounded, resumable
//! removal of a collection, and `DROP TABLE` on top of it.
//!
//! The oracle of the big test is a brute-force one and it is worth naming,
//! because it is the whole statement a drop makes. Every persisted key and
//! value of the primary tree is dumped before the drop and after it, and an
//! INDEPENDENT classifier -- written here from the frozen key encodings, not
//! from the engine's own helpers -- decides for each key whether it belongs to
//! the collection being dropped. The assertion is then exact in both
//! directions: every key the classifier calls the collection's is gone, and
//! every other key is present with byte-identical bytes. A drop that removed
//! one record too few or one too many fails it.

use sekejap_lang::SqlDatabase;
use sekejap_core::{
    collections::{
        CollectionId, CollectionOptions, Database, DropMode, DropPhase, EntityId,
        MAX_DROP_BATCH,
    },
};
use sekejap_lang::{Param, SqlResult};
use sekejap_core::{
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::collections::BTreeMap;
use tempfile::TempDir;

fn config() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

// ── the frozen key encodings, written independently of the engine ─────────

/// `docs/core/COLLECTIONS.md`'s order-preserving integer: a width tag then the
/// significant big-endian bytes.
fn ordered(n: u64) -> Vec<u8> {
    let bytes = n.to_be_bytes();
    let start = bytes.iter().position(|b| *b != 0).unwrap_or(7);
    let mut out = vec![0x80 + (8 - start) as u8];
    out.extend_from_slice(&bytes[start..]);
    out
}

/// The same encoding, read back. `None` when the bytes are not one.
fn read_ordered(b: &[u8], at: &mut usize) -> Option<u64> {
    let width = usize::from(*b.get(*at)?).checked_sub(0x80)?;
    if !(1..=8).contains(&width) {
        return None;
    }
    *at += 1;
    let bytes = b.get(*at..*at + width)?;
    *at += width;
    let mut out = 0u64;
    for byte in bytes {
        out = (out << 8) | u64::from(*byte);
    }
    Some(out)
}

fn tagged(tag: u8, id: u32) -> Vec<u8> {
    let mut k = vec![tag];
    k.extend(ordered(u64::from(id)));
    k
}

/// Every index-entry tag of every family, from `docs/core/SOURCE_LAYOUT.md`'s
/// family table. All of them are `tag | ordered(index id) | ...`.
const INDEX_ENTRY_TAGS: [u8; 11] = [
    0x70, // scalar
    0x73, // exact vector locator
    0x74, // spatial point posting
    0x75, // text posting
    0x76, // text norm
    0x77, // text term statistics
    0x78, // text corpus statistics
    0x79, // quantized vector entry
    0x7a, // text segment
    0x7b, // text norm block
    0x7c, // geometry posting
];

/// The collection-scoped row keyspaces: mappings, rows, vector sidecars.
const ROW_TAGS: [u8; 3] = [0x20, 0x40, 0x60];
/// The two halves of the edge keyspace, keyed by the NEAR endpoint.
const EDGE_TAGS: [u8; 2] = [0x71, 0x72];

/// What the test knows about the collection it is about to drop, so it can
/// classify keys without asking the engine anything afterwards.
struct Owned {
    id: CollectionId,
    name: String,
    layout: u32,
    indexes: Vec<u64>,
}

impl Owned {
    /// True when `key` is a record only this collection owns. Written from
    /// the key encodings, not from the drop's own phase list.
    fn owns(&self, key: &[u8]) -> bool {
        let Some(&tag) = key.first() else {
            return false;
        };
        let cid = u64::from(self.id.0);
        // rows, mappings, sidecars
        if ROW_TAGS.contains(&tag) {
            return key.starts_with(&tagged(tag, self.id.0));
        }
        // An edge, under CASCADE, belongs to the collection when EITHER
        // endpoint is one of its rows: the near endpoint the key is sorted by
        // and the far one at its tail. `tag | coll | seq | context | type |
        // coll | seq`, all six integers in the frozen ordered encoding.
        if EDGE_TAGS.contains(&tag) {
            let mut at = 1;
            let mut ends = Vec::new();
            for field in 0..6 {
                let Some(value) = read_ordered(key, &mut at) else {
                    return false;
                };
                if field == 0 || field == 4 {
                    ends.push(value);
                }
            }
            return at == key.len() && ends.contains(&cid);
        }
        // index entries of one of its indexes
        if INDEX_ENTRY_TAGS.contains(&tag) {
            let mut at = 1;
            return read_ordered(key, &mut at).is_some_and(|id| self.indexes.contains(&id));
        }
        match tag {
            // the name mapping
            0x10 => key[1..] == *self.name.as_bytes(),
            // catalog and sequence replicas: [tag][copy][ordered id]
            1 | 2 => key.len() > 2 && key[1] < 3 && key[2..] == *ordered(cid),
            // layout replicas: [0][240][(layout * 3 + copy) as u64 BE]
            0 => {
                key.len() == 10
                    && key[1] == 240
                    && (0..3).any(|copy| {
                        key[2..] == (u64::from(self.layout) * 3 + copy).to_be_bytes()
                    })
            }
            // index descriptor replicas: [3][copy][ordered index id]
            3 => {
                let mut at = 2;
                key.len() > 2 && read_ordered(key, &mut at).is_some_and(|id| self.indexes.contains(&id))
            }
            // index registry: [4][ordered index id]
            4 => {
                let mut at = 1;
                read_ordered(key, &mut at).is_some_and(|id| self.indexes.contains(&id))
            }
            // collection->index mapping: [5][ordered collection][ordered index]
            5 => key.starts_with(&tagged(5, self.id.0)),
            // index name mapping: [0x11][ordered collection][name]
            0x11 => key.starts_with(&tagged(0x11, self.id.0)),
            _ => false,
        }
    }
}

/// Every persisted key and value of the primary tree, in key order.
///
/// A version-2 index keeps its entries in its OWN tree, which this walk does
/// not reach; `index_trees` is asserted empty separately, which is the
/// statement that no such tree is left at all.
fn dump(db: &Database) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let mut out = BTreeMap::new();
    db.raw_for_each(&[], &mut |k, v| {
        out.insert(k.to_vec(), v.to_vec());
    })
    .unwrap();
    out
}

/// How many persisted entries sit under one prefix.
fn under(db: &Database, prefix: &[u8]) -> u64 {
    db.raw_for_each(prefix, &mut |_, _| {}).unwrap()
}

// ── fixtures ─────────────────────────────────────────────────────────────

fn fields() -> Vec<(String, Kind)> {
    vec![
        ("name".into(), Kind::Text),
        ("text".into(), Kind::Text),
        ("born".into(), Kind::Int),
        ("emb".into(), Kind::Vector(4)),
    ]
}

fn document(i: usize) -> serde_json::Value {
    let f = i as f32;
    json!({
        "name": format!("name-{i}"),
        "text": format!("kebun sawah {} pasar", i % 37),
        "born": (1900 + (i % 120)) as i64,
        "emb": [f, f + 1.0, f + 2.0, f + 3.0],
    })
}

/// A collection with `rows` rows and three READY indexes: a scalar index (its
/// own B-tree by default), a text index and an exact vector index (both in
/// the primary tree). Vector rows mean the sidecar keyspace is populated too.
fn populate(db: &mut Database, name: &str, rows: usize) -> (CollectionId, Vec<u64>) {
    let c = db
        .create_collection(name, fields(), CollectionOptions::default())
        .unwrap();
    db.commit().unwrap();
    for i in 0..rows {
        db.put(c, &format!("{name}-{i:05}"), &document(i)).unwrap();
        if (i + 1) % 256 == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let mut ids = Vec::new();
    for (index, field) in [("born", "born"), ("text", "text"), ("emb", "emb")] {
        let id = match index {
            "born" => db
                .create_scalar_index(c, &format!("{name}_{index}"), field, false)
                .unwrap(),
            "text" => db
                .create_text_index(c, &format!("{name}_{index}"), field)
                .unwrap(),
            _ => db
                .create_exact_vector_index(c, &format!("{name}_{index}"), field)
                .unwrap(),
        };
        db.commit().unwrap();
        db.build_index_to_ready(id, 256).unwrap();
        db.commit().unwrap();
        ids.push(id.0);
    }
    (c, ids)
}

fn owned(db: &Database, name: &str, c: CollectionId, indexes: Vec<u64>) -> Owned {
    Owned {
        id: c,
        name: name.to_owned(),
        layout: db.collection_info(c).unwrap().layout.id as u32,
        indexes,
    }
}

// ── (a) a drop leaves no trace ───────────────────────────────────────────

#[test]
fn a_dropped_collection_leaves_every_keyspace_empty_and_its_name_free() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Database::create(&path, config()).unwrap();
    let (c, indexes) = populate(&mut db, "place", 3_000);
    let owner = owned(&db, "place", c, indexes.clone());

    assert_eq!(under(&db, &tagged(0x40, c.0)), 3_000, "rows before the drop");
    assert_eq!(
        under(&db, &tagged(0x60, c.0)),
        3_000,
        "vector sidecars before the drop"
    );
    assert_eq!(
        under(&db, &tagged(0x20, c.0)),
        3_000,
        "external-key mappings before the drop"
    );

    db.begin_drop_collection(c).unwrap();
    assert_eq!(db.dropping_collection(), Some(c));
    let state = db.drop_collection_state(c).unwrap().unwrap();
    assert_eq!(state.phase, DropPhase::Indexes);
    assert_eq!(state.mode, DropMode::Restrict);
    assert_eq!(state.removed, 0);

    // Bounded: no step may remove more than the budget it was given.
    let mut steps = 0u64;
    let mut phases = Vec::new();
    loop {
        let progress = db.drop_collection_step(c, 64).unwrap();
        assert!(progress.removed <= 64 + 9, "step removed {}", progress.removed);
        db.commit().unwrap();
        steps += 1;
        if phases.last() != Some(&progress.phase) {
            phases.push(progress.phase);
        }
        if progress.done {
            break;
        }
        assert!(steps < 1_000, "a 3,000-row drop should not need 1,000 steps");
    }
    assert_eq!(
        phases,
        vec![
            DropPhase::Indexes,
            DropPhase::Sidecars,
            DropPhase::Rows,
            DropPhase::Mappings,
            DropPhase::Descriptor
        ],
        "the drop walked its phases in order"
    );
    assert_eq!(db.dropping_collection(), None);
    drop(db);

    let mut db = Database::open(&path, config()).unwrap();
    // Every keyspace, probed by its own tag.
    for tag in ROW_TAGS {
        assert_eq!(under(&db, &tagged(tag, c.0)), 0, "tag {tag:#x} after reopen");
    }
    for tag in [5u8, 0x11] {
        assert_eq!(under(&db, &tagged(tag, c.0)), 0, "tag {tag:#x} after reopen");
    }
    for tag in INDEX_ENTRY_TAGS.into_iter().chain([3, 4]) {
        assert_eq!(under(&db, &[tag]), 0, "index tag {tag:#x} after reopen");
    }
    assert!(db.index_trees().unwrap().is_empty(), "no index tree is left");
    for key in dump(&db).keys() {
        assert!(!owner.owns(key), "a record of the dropped collection survived: {key:?}");
    }

    // No trace in the catalog, and the name is free again for a FRESH id --
    // identities are never reused (D13).
    assert!(db.collection("place").unwrap().is_none());
    assert!(db.collection_info(c).is_err());
    let again = db
        .create_collection("place", fields(), CollectionOptions::default())
        .unwrap();
    db.commit().unwrap();
    assert_ne!(again, c, "the name is reusable, the identity is not");
    assert_eq!(db.scan(again, None).unwrap().count(), 0);
}

// ── (b) kill-style: interrupt, reopen, resume, same end state ────────────

#[test]
fn an_interrupted_drop_resumes_to_the_state_an_uninterrupted_one_reaches() {
    // Two identical databases. One is dropped straight through; the other is
    // abandoned mid-drop, reopened and resumed. Their bytes must agree.
    let mut ends = Vec::new();
    // Three runs: one straight through, one abandoned while its indexes are
    // still going, one abandoned in the middle of the row keyspace.
    for kill_after in [usize::MAX, 3, 40] {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("db");
        let mut db = Database::create(&path, config()).unwrap();
        populate(&mut db, "keep", 200);
        let (c, _) = populate(&mut db, "place", 1_500);
        db.begin_drop_collection(c).unwrap();

        let mut steps = 0usize;
        let mut finished = false;
        while steps < kill_after {
            let progress = db.drop_collection_step(c, 128).unwrap();
            db.commit().unwrap();
            steps += 1;
            if progress.done {
                finished = true;
                break;
            }
        }
        if !finished {
            // The handle goes away with the drop half done, exactly as a kill
            // would leave it: everything up to the last commit is durable.
            drop(db);
            db = Database::open(&path, config()).unwrap();
            assert_eq!(
                db.dropping_collection(),
                Some(c),
                "a reopen finds the drop to resume without being told"
            );
            let state = db.drop_collection_state(c).unwrap().unwrap();
            if kill_after >= 40 {
                assert!(
                    state.removed > 0,
                    "the committed cursor survived the kill: {state:?}"
                );
            }
            db.drop_collection_to_end(c, 128).unwrap();
        }
        drop(db);
        let db = Database::open(&path, config()).unwrap();
        assert!(db.collection("place").unwrap().is_none());
        assert_eq!(db.scan(db.collection("keep").unwrap().unwrap(), None).unwrap().count(), 200);
        ends.push(dump(&db));
    }
    for end in &ends[1..] {
        assert_eq!(
            &ends[0], end,
            "an interrupted drop and an uninterrupted one leave the same records"
        );
    }
}

// ── (c) RESTRICT names the contexts, CASCADE removes the edges ───────────

#[test]
fn restrict_names_the_referencing_contexts_and_cascade_removes_the_edges() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Database::create(&path, config()).unwrap();
    let (place, _) = populate(&mut db, "place", 200);
    let (org, _) = populate(&mut db, "org", 20);
    db.enable_graph().unwrap();
    db.commit().unwrap();
    let routes = db.create_graph_context("routes").unwrap();
    let near = db.create_edge_type("near").unwrap();
    let owns = db.create_edge_type("owns").unwrap();
    db.commit().unwrap();

    let place_id = |n: u64| EntityId {
        collection: place,
        sequence: n,
    };
    let org_id = |n: u64| EntityId {
        collection: org,
        sequence: n,
    };
    // Inside `place`, in a named context, and from `org` into `place` in the
    // base graph: two contexts, and one of them reaches in from outside.
    for i in 1..=100u64 {
        db.put_edge(routes, place_id(i), near, place_id(i + 1), &json!({}))
            .unwrap();
    }
    for i in 1..=20u64 {
        db.put_edge(
            sekejap_core::collections::GraphContextId::BASE,
            org_id(i),
            owns,
            place_id(i),
            &json!({}),
        )
        .unwrap();
    }
    db.commit().unwrap();

    // RESTRICT refuses, and the refusal names both contexts.
    let refusal = db.begin_drop_collection(place).unwrap_err().to_string();
    assert!(refusal.contains("RESTRICT"), "{refusal}");
    assert!(refusal.contains("routes"), "{refusal}");
    assert!(refusal.contains("base graph"), "{refusal}");
    assert!(refusal.contains("CASCADE"), "{refusal}");
    // The refusal removed nothing: the collection is still readable.
    assert_eq!(db.scan(place, None).unwrap().count(), 200);
    assert!(db.drop_collection_state(place).unwrap().is_none());

    // A collection nothing references is not restricted.
    let (loose, _) = populate(&mut db, "loose", 10);
    db.begin_drop_collection(loose).unwrap();
    db.drop_collection_to_end(loose, MAX_DROP_BATCH).unwrap();

    // CASCADE removes the edges in both contexts and leaves `org` whole.
    db.begin_drop_collection_mode(place, DropMode::Cascade)
        .unwrap();
    db.drop_collection_to_end(place, MAX_DROP_BATCH).unwrap();
    drop(db);

    let db = Database::open(&path, config()).unwrap();
    for tag in EDGE_TAGS {
        assert_eq!(under(&db, &tagged(tag, place.0)), 0, "edges keyed by place");
        assert_eq!(
            under(&db, &tagged(tag, org.0)),
            0,
            "the org side of every edge into place went with it"
        );
    }
    assert!(db.collection("place").unwrap().is_none());
    assert_eq!(db.scan(org, None).unwrap().count(), 20, "org is untouched");
}

// ── (d) a DROPPING collection refuses its readers and its writers ────────

#[test]
fn a_published_dropping_mark_refuses_every_reader_and_writer() {
    use sekejap_core::collections::{
        Projection, QueryOrder, QueryRequest,
    };
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Database::create(&path, config()).unwrap();
    let (place, _) = populate(&mut db, "place", 100);
    let (org, _) = populate(&mut db, "org", 10);
    db.enable_graph().unwrap();
    db.commit().unwrap();
    let owns = db.create_edge_type("owns").unwrap();
    db.commit().unwrap();

    let request = |c| QueryRequest {
        collection: c,
        filters: &[],
        order: QueryOrder::EntityId,
        projection: Projection::Ids,
        total_limit: None,
        driver: sekejap_core::collections::CandidateDriver::Auto,
    };
    // Before the mark: a prepared query is ordinary.
    assert!(db.prepare_query(request(place)).is_ok());

    db.begin_drop_collection(place).unwrap();

    // A prepared query cannot outlive the mark -- `begin_drop_collection`
    // needs the exclusive borrow a live `PreparedQuery` holds -- so the
    // refusal that matters is the one preparing a new one gets.
    let refused = match db.prepare_query(request(place)) {
        Ok(_) => panic!("prepare_query accepted a DROPPING collection"),
        Err(e) => e.to_string(),
    };
    assert!(refused.contains("DROPPING"), "{refused}");
    let mut messages: Vec<String> = Vec::new();
    messages.push(db.get(place, "place-00000").unwrap_err().to_string());
    messages.push(
        db.get_by_id(EntityId {
            collection: place,
            sequence: 1,
        })
        .unwrap_err()
        .to_string(),
    );
    messages.push(match db.scan(place, None) {
        Ok(_) => panic!("scan accepted a DROPPING collection"),
        Err(e) => e.to_string(),
    });
    messages.push(db.collection_info(place).unwrap_err().to_string());
    messages.push(db.list_indexes(place).unwrap_err().to_string());
    messages.push(db.collection("place").unwrap_err().to_string());
    messages.push(db.put(place, "new", &document(1)).unwrap_err().to_string());
    messages.push(
        db.update(place, "place-00000", &json!({"born": 1}))
            .unwrap_err()
            .to_string(),
    );
    messages.push(db.delete(place, "place-00000").unwrap_err().to_string());
    messages.push(db.alter_collection(place, fields()).unwrap_err().to_string());
    messages.push(
        db.sql("SELECT name FROM place LIMIT 1", &[])
            .unwrap_err()
            .to_string(),
    );
    for message in &messages {
        assert!(message.contains("DROPPING"), "{message}");
    }
    // And no edge may be written onto one of its rows while it is dropping:
    // a RESTRICT admission that did not refuse must stay true.
    let edge = db
        .put_edge(
            sekejap_core::collections::GraphContextId::BASE,
            EntityId {
                collection: org,
                sequence: 1,
            },
            owns,
            EntityId {
                collection: place,
                sequence: 1,
            },
            &json!({}),
        )
        .unwrap_err()
        .to_string();
    assert!(edge.contains("DROPPING"), "{edge}");
    // A second drop is refused while one is in flight.
    assert!(db.begin_drop_collection(org).is_err());
    // Another collection is unaffected.
    assert_eq!(db.scan(org, None).unwrap().count(), 10);

    db.drop_collection_to_end(place, MAX_DROP_BATCH).unwrap();
    assert!(db.collection("place").unwrap().is_none());
    // With the drop finished, `org` can be dropped in its turn.
    db.begin_drop_collection(org).unwrap();
    db.drop_collection_to_end(org, MAX_DROP_BATCH).unwrap();
}

// ── (e) the SQL surface ──────────────────────────────────────────────────

#[test]
fn sql_drop_table_if_exists_and_cascade_end_to_end() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Database::create(&path, config()).unwrap();
    let (place, _) = populate(&mut db, "place", 300);
    let (org, _) = populate(&mut db, "org", 30);
    db.enable_graph().unwrap();
    db.commit().unwrap();
    let owns = db.create_edge_type("owns").unwrap();
    db.commit().unwrap();
    for i in 1..=30u64 {
        db.put_edge(
            sekejap_core::collections::GraphContextId::BASE,
            EntityId {
                collection: org,
                sequence: i,
            },
            owns,
            EntityId {
                collection: place,
                sequence: i,
            },
            &json!({}),
        )
        .unwrap();
    }
    db.commit().unwrap();

    // EXPLAIN prints the plan and does NOT run it.
    let explained = match db.sql("EXPLAIN DROP TABLE place", &[]).unwrap() {
        SqlResult::Explain(text) => text,
        other => panic!("expected an explanation, got {other:?}"),
    };
    for expected in [
        "DROP TABLE place RESTRICT",
        "does not run its statement",
        "begin_drop_collection",
        "indexes",
        "vector sidecars",
        "rows",
        "external-key mappings",
        "descriptor",
        "place_born",
    ] {
        assert!(explained.contains(expected), "{explained}");
    }
    assert_eq!(db.scan(place, None).unwrap().count(), 300, "EXPLAIN ran nothing");

    // RESTRICT is the default, and it refuses through SQL too.
    let refused = db.sql("DROP TABLE place", &[]).unwrap_err().to_string();
    assert!(refused.contains("RESTRICT"), "{refused}");
    assert!(refused.contains("CASCADE"), "{refused}");

    // IF EXISTS on a name that is not there is a notice, not an error.
    match db.sql("DROP TABLE IF EXISTS nowhere", &[]).unwrap() {
        SqlResult::Notice(text) => assert!(text.contains("no such collection"), "{text}"),
        other => panic!("expected a notice, got {other:?}"),
    }
    // ... and without IF EXISTS it is an error that names the table.
    assert!(db
        .sql("DROP TABLE nowhere", &[])
        .unwrap_err()
        .to_string()
        .contains("nowhere"));

    match db.sql("DROP TABLE place CASCADE", &[]).unwrap() {
        SqlResult::Affected(n) => assert!(n >= 900, "{n} entries removed"),
        other => panic!("expected an affected count, got {other:?}"),
    }
    assert!(db.collection("place").unwrap().is_none());
    assert!(db
        .sql("SELECT name FROM place LIMIT 1", &[])
        .unwrap_err()
        .to_string()
        .contains("no collection named `place`"));

    // The survivor is whole, queryable, and the name is free.
    match db.sql("SELECT name FROM org LIMIT 5", &[]).unwrap() {
        SqlResult::Rows { rows, .. } => assert_eq!(rows.len(), 5),
        other => panic!("expected rows, got {other:?}"),
    }
    db.sql(
        "CREATE TABLE place (key TEXT PRIMARY KEY, born INT)",
        &[Param::Null; 0],
    )
    .unwrap();
    assert_ne!(db.collection("place").unwrap().unwrap(), place);

    // A table nothing references drops under the default RESTRICT.
    match db.sql("DROP TABLE place", &[]).unwrap() {
        SqlResult::Affected(_) => {}
        other => panic!("expected an affected count, got {other:?}"),
    }
    // And RESTRICT may be written out.
    match db.sql("DROP TABLE org RESTRICT", &[]).unwrap() {
        SqlResult::Affected(_) => {}
        other => panic!("expected an affected count, got {other:?}"),
    }
    assert!(db.collection("org").unwrap().is_none());
}

// ── the brute-force oracle over a 2,000-row collection ───────────────────

#[test]
fn the_drop_removes_exactly_the_collections_records_and_nothing_else() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("db");
    let mut db = Database::create(&path, config()).unwrap();
    // The survivor is built FIRST and again AFTER, so the dropped collection
    // is neither the lowest nor the highest identity and neither the first
    // nor the last records in key order.
    let (before_id, before_ix) = populate(&mut db, "before", 400);
    let (place, place_ix) = populate(&mut db, "place", 2_000);
    let (after_id, after_ix) = populate(&mut db, "after", 400);
    db.enable_graph().unwrap();
    db.commit().unwrap();
    let routes = db.create_graph_context("routes").unwrap();
    let near = db.create_edge_type("near").unwrap();
    db.commit().unwrap();
    let id = |c, n| EntityId {
        collection: c,
        sequence: n,
    };
    for i in 1..=400u64 {
        // place -> place, place -> after, before -> place: every direction the
        // CASCADE has to reach, and one edge pair that touches neither.
        db.put_edge(routes, id(place, i), near, id(place, i + 1), &json!({}))
            .unwrap();
        db.put_edge(routes, id(place, i), near, id(after_id, i), &json!({}))
            .unwrap();
        db.put_edge(routes, id(before_id, i), near, id(place, i), &json!({}))
            .unwrap();
        db.put_edge(routes, id(before_id, i), near, id(after_id, i), &json!({}))
            .unwrap();
    }
    db.commit().unwrap();

    let owner = owned(&db, "place", place, place_ix.clone());
    let survivors = [
        owned(&db, "before", before_id, before_ix),
        owned(&db, "after", after_id, after_ix),
    ];
    let before = dump(&db);

    // The classifier must actually find every family, or the oracle would
    // pass by naming nothing.
    let mut families: BTreeMap<u8, usize> = BTreeMap::new();
    for key in before.keys().filter(|k| owner.owns(k)) {
        *families.entry(key[0]).or_default() += 1;
    }
    for tag in [
        0u8, 1, 2, 3, 4, 5, 0x10, 0x11, 0x20, 0x40, 0x60, 0x71, 0x72, 0x73, 0x77, 0x78, 0x7a,
        0x7b,
    ] {
        assert!(
            families.get(&tag).is_some_and(|n| *n > 0),
            "the classifier found no key with tag {tag:#x}: {families:?}"
        );
    }
    assert_eq!(families[&0x40], 2_000, "rows");
    assert_eq!(families[&0x20], 2_000, "mappings");
    assert_eq!(families[&0x60], 2_000, "sidecars");
    // 400 place->place + 400 place->after primaries, and the reverse halves of
    // the 400 before->place edges, are all keyed under `place`.
    // Of the four edges written per i, three touch `place`: place->place,
    // place->after and before->place. Each contributes one primary and one
    // reverse posting, whichever endpoint the key is sorted by.
    assert_eq!(families[&0x71], 1_200, "primary postings of edges touching place");
    assert_eq!(families[&0x72], 1_200, "reverse postings of edges touching place");

    db.begin_drop_collection_mode(place, DropMode::Cascade)
        .unwrap();
    let mut removed_total = 0u64;
    loop {
        let progress = db.drop_collection_step(place, MAX_DROP_BATCH).unwrap();
        assert!(progress.removed <= MAX_DROP_BATCH as u64 + 9);
        db.commit().unwrap();
        removed_total = progress.total_removed;
        if progress.done {
            break;
        }
    }
    assert!(removed_total >= 6_000, "{removed_total} entries removed");
    drop(db);

    let db = Database::open(&path, config()).unwrap();
    let after = dump(&db);

    // Exact, both directions.
    let expected: BTreeMap<_, _> = before
        .iter()
        .filter(|(k, _)| !owner.owns(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    let missing: Vec<_> = expected.keys().filter(|k| !after.contains_key(*k)).collect();
    let extra: Vec<_> = after.keys().filter(|k| !expected.contains_key(*k)).collect();
    assert!(missing.is_empty(), "the drop removed records it does not own: {missing:?}");
    assert!(extra.is_empty(), "records appeared that were not there: {extra:?}");
    for (key, value) in &expected {
        if key.len() == 3 && key[0] == 0 && key[1] == 0 {
            continue; // the collection header, checked field by field below
        }
        assert_eq!(after.get(key), Some(value), "a surviving record changed: {key:?}");
    }

    // The collection header is the one record a drop is entitled to change,
    // and exactly two of its fields may move. Its payload, after the 8-byte
    // magic and the 2-byte length, is
    // `next_collection:u32 | next_layout:u32 | features:u64 | next:u64 | count:u32`.
    for copy in 0..3u8 {
        let key = vec![0u8, 0, copy];
        let (was, now) = (&before[&key], &after[&key]);
        assert_eq!(
            was[10..18],
            now[10..18],
            "the identity allocators do not move: a dropped id is never reused"
        );
        assert_eq!(
            was[18..26],
            now[18..26],
            "the logical feature set is exactly what it was: DROP_FEATURE was set for the drop and cleared by its last step"
        );
        assert_eq!(was[26..34], now[26..34], "the index id allocator does not move");
        let counts = |b: &[u8]| u32::from_be_bytes(b[34..38].try_into().unwrap());
        assert_eq!(counts(was), 9, "three collections of three indexes each");
        assert_eq!(
            counts(now),
            6,
            "the three indexes of the dropped collection left the registry"
        );
    }

    // The survivors answer the same questions they did before.
    for survivor in &survivors {
        assert_eq!(db.scan(survivor.id, None).unwrap().count(), 400);
        assert!(survivor.name == "before" || survivor.name == "after");
        assert!(db.collection(&survivor.name).unwrap().is_some());
    }
    // The 400 before->after edges never touched `place` and are all still there.
    assert_eq!(
        under(&db, &tagged(0x71, before_id.0)),
        400,
        "before->after survived; before->place went with place"
    );
}
