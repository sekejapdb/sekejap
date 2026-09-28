//! Edge tables: `docs/core/EDGE_TABLES.md`.
//!
//! An edge table is a collection that never holds a row. Its layout types an
//! edge's properties, its `REFERENCES` columns name the two ends, and its
//! primary key decides how many edges one pair may have. The edges are native
//! edges under the table's edge type; nothing is stored twice.
//!
//! What is at risk, one test each:
//!
//! * a key of (source, destination) admits one edge per pair and refuses the
//!   second as 23505 (`a_pair_key_admits_one_edge_per_pair`);
//! * extra key columns admit parallel edges and refuse an exact repeat --
//!   Jakarta twice, then Bandung on the same day
//!   (`extra_key_columns_admit_parallel_edges_and_refuse_a_repeat`);
//! * a key of the source alone is one edge per source
//!   (`a_source_only_key_is_one_edge_per_source`);
//! * no key admits every insert (`no_key_admits_every_insert`);
//! * a missing endpoint row is 23503 and writes nothing
//!   (`a_missing_endpoint_is_a_foreign_key_violation`);
//! * the layout types the properties and NOT NULL holds
//!   (`the_layout_types_the_properties`);
//! * the declaration refuses what it cannot honour, and an unbound table takes
//!   no write (`the_declaration_refuses_what_it_cannot_honour`);
//! * a bound edge type takes no untyped write, and the edge table takes no row
//!   (`a_bound_edge_type_takes_no_untyped_write_and_the_table_no_row`),
//!   and neither end's table nor the edge table can be dropped from under it;
//! * UPDATE by key rewrites one edge's properties, DELETE by endpoint removes
//!   what matches, and an upsert updates or does nothing
//!   (`update_delete_and_upsert_by_key`);
//! * the declaration survives a reopen, and the feature bit is set only by a
//!   file that declares an edge table; dropping the property graph keeps the
//!   binding and the edges (`the_declaration_persists_and_is_additive`).
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{
        CollectionId, CollectionOptions, ColumnRule, Database, Direction, EdgeTableRow,
        GraphContextId, NeighborRequest, OnConflict, EDGE_TABLE_FEATURE,
    },
    internal::logical_features,
    Kind,
};
use serde_json::{json, Value};
use tempfile::TempDir;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

struct Music {
    _dir: TempDir,
    db: Database,
    artist: CollectionId,
    song: CollectionId,
}

/// Three artists and two songs, committed.
fn music() -> Music {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("music.sekejap"), cfg()).unwrap();
    let artist = db
        .create_collection("artist", vec![("name".into(), Kind::Text)], CollectionOptions::default())
        .unwrap();
    let song = db
        .create_collection("song", vec![("title".into(), Kind::Text)], CollectionOptions::default())
        .unwrap();
    for key in ["dhani", "andra", "dewa"] {
        db.put(artist, key, &json!({"name": key})).unwrap();
    }
    for key in ["kirana", "separuh"] {
        db.put(song, key, &json!({"title": key})).unwrap();
    }
    db.commit().unwrap();
    Music {
        _dir: dir,
        db,
        artist,
        song,
    }
}

/// An edge table from `artist` (column `a`) to `song` (column `s`) with the
/// given property columns and key, declared and bound under `label`.
fn edge_table(
    m: &mut Music,
    name: &str,
    properties: Vec<(String, Kind)>,
    rules: Vec<(String, ColumnRule)>,
    key: &[&str],
) -> CollectionId {
    let mut fields = vec![("a".to_owned(), Kind::Text), ("s".to_owned(), Kind::Text)];
    fields.extend(properties);
    let c = m
        .db
        .create_collection_rules(name, fields, Vec::new(), rules, CollectionOptions::default())
        .unwrap();
    m.db
        .declare_edge_table(
            c,
            vec![("a".into(), m.artist), ("s".into(), m.song)],
            key.iter().map(|k| (*k).to_owned()).collect(),
        )
        .unwrap();
    m.db.bind_edge_table(c, "a", "s", name, "music").unwrap();
    m.db.commit().unwrap();
    c
}

fn sqlstate(error: sekejap_core::collections::Error) -> &'static str {
    match error {
        sekejap_core::collections::Error::Constraint { sqlstate, .. } => sqlstate,
        other => panic!("a constraint violation, not {other:?}"),
    }
}

/// How many edges leave `artist/key` under the table's edge type.
fn out_degree(m: &Music, table: CollectionId, key: &str) -> usize {
    let t = m.db.edge_table(table).unwrap().unwrap().binding.unwrap().edge_type;
    let entity = m.db.get(m.artist, key).unwrap().unwrap().id;
    m.db.neighbors(NeighborRequest {
        entity,
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(t),
        limit: 256,
    })
    .unwrap()
    .len()
}

fn rows(m: &Music, table: CollectionId, filter: Value) -> Vec<EdgeTableRow> {
    m.db.edge_rows(table, &filter, 256).unwrap()
}

#[test]
fn a_pair_key_admits_one_edge_per_pair() {
    let mut m = music();
    let wrote = edge_table(&mut m, "wrote", vec![], vec![], &["a", "s"]);
    m.db.insert_edge_row(wrote, &json!({"a": "dhani", "s": "kirana"})).unwrap();
    m.db.insert_edge_row(wrote, &json!({"a": "andra", "s": "kirana"})).unwrap();
    let second = m.db.insert_edge_row(wrote, &json!({"a": "dhani", "s": "kirana"}));
    assert_eq!(sqlstate(second.unwrap_err()), "23505");
    m.db.commit().unwrap();
    assert_eq!(out_degree(&m, wrote, "dhani"), 1);
    assert_eq!(rows(&m, wrote, json!({"s": "kirana"})).len(), 2, "read from the song's end");
}

#[test]
fn extra_key_columns_admit_parallel_edges_and_refuse_a_repeat() {
    let mut m = music();
    let performed = edge_table(
        &mut m,
        "performed",
        vec![("on".into(), Kind::Int), ("venue".into(), Kind::Text)],
        vec![],
        &["a", "s", "on", "venue"],
    );
    let jakarta = json!({"a": "dewa", "s": "kirana", "on": 19980501, "venue": "Jakarta"});
    m.db.insert_edge_row(performed, &jakarta).unwrap();
    assert_eq!(sqlstate(m.db.insert_edge_row(performed, &jakarta).unwrap_err()), "23505");
    m.db.insert_edge_row(
        performed,
        &json!({"a": "dewa", "s": "kirana", "on": 19980501, "venue": "Bandung"}),
    )
    .unwrap();
    m.db.commit().unwrap();
    assert_eq!(out_degree(&m, performed, "dewa"), 2, "two edges, one pair");
    let mut venues: Vec<String> = rows(&m, performed, json!({"a": "dewa"}))
        .into_iter()
        .map(|r| r.values["venue"].as_str().unwrap().to_owned())
        .collect();
    venues.sort();
    assert_eq!(venues, ["Bandung", "Jakarta"]);
}

#[test]
fn a_source_only_key_is_one_edge_per_source() {
    let mut m = music();
    // belongs_to runs song -> artist here as artist -> song: the key is the
    // source alone, so an artist may carry one edge of this type in all.
    let signature = edge_table(&mut m, "signature", vec![], vec![], &["a"]);
    m.db.insert_edge_row(signature, &json!({"a": "dewa", "s": "kirana"})).unwrap();
    let other = m.db.insert_edge_row(signature, &json!({"a": "dewa", "s": "separuh"}));
    assert_eq!(sqlstate(other.unwrap_err()), "23505");
    m.db.insert_edge_row(signature, &json!({"a": "dhani", "s": "separuh"})).unwrap();
    m.db.commit().unwrap();
    assert_eq!(out_degree(&m, signature, "dewa"), 1);
}

#[test]
fn no_key_admits_every_insert() {
    let mut m = music();
    let played = edge_table(&mut m, "played", vec![], vec![], &[]);
    for _ in 0..3 {
        m.db.insert_edge_row(played, &json!({"a": "dewa", "s": "kirana"})).unwrap();
    }
    m.db.commit().unwrap();
    assert_eq!(out_degree(&m, played, "dewa"), 3);
}

#[test]
fn a_missing_endpoint_is_a_foreign_key_violation() {
    let mut m = music();
    let wrote = edge_table(&mut m, "wrote", vec![], vec![], &["a", "s"]);
    let missing = m.db.insert_edge_row(wrote, &json!({"a": "dhani", "s": "no-such-song"}));
    assert_eq!(sqlstate(missing.unwrap_err()), "23503");
    let missing = m.db.insert_edge_row(wrote, &json!({"a": "nobody", "s": "kirana"}));
    assert_eq!(sqlstate(missing.unwrap_err()), "23503");
    m.db.commit().unwrap();
    assert_eq!(out_degree(&m, wrote, "dhani"), 0);
}

#[test]
fn the_layout_types_the_properties() {
    let mut m = music();
    let rated = edge_table(
        &mut m,
        "rated",
        vec![("stars".into(), Kind::Int)],
        vec![(
            "stars".into(),
            ColumnRule {
                default: None,
                not_null: true,
            },
        )],
        &["a", "s"],
    );
    let wrong = m.db.insert_edge_row(rated, &json!({"a": "dewa", "s": "kirana", "stars": "five"}));
    assert!(wrong.is_err(), "text in an INT property");
    let missing = m.db.insert_edge_row(rated, &json!({"a": "dewa", "s": "kirana"}));
    assert!(missing.is_err(), "NOT NULL property left out");
    let unknown = m
        .db
        .insert_edge_row(rated, &json!({"a": "dewa", "s": "kirana", "stars": 5, "mood": "happy"}));
    assert!(unknown.is_err(), "a column the table has not got");
    m.db.insert_edge_row(rated, &json!({"a": "dewa", "s": "kirana", "stars": 5})).unwrap();
    m.db.commit().unwrap();
    let got = rows(&m, rated, json!({"a": "dewa"}));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].values, json!({"a": "dewa", "s": "kirana", "stars": 5}));
}

#[test]
fn the_declaration_refuses_what_it_cannot_honour() {
    let mut m = music();
    let fields = vec![
        ("a".to_owned(), Kind::Text),
        ("s".to_owned(), Kind::Text),
        ("venue".to_owned(), Kind::Text),
    ];
    let c = m
        .db
        .create_collection("gig", fields, CollectionOptions::default())
        .unwrap();
    let refs = || vec![("a".to_owned(), m.artist), ("s".to_owned(), m.song)];
    assert!(
        m.db.declare_edge_table(c, refs(), vec!["venue".into()]).is_err(),
        "a key naming neither end would need an index over edge properties"
    );
    assert!(
        m.db.declare_edge_table(c, refs(), vec!["nope".into()]).is_err(),
        "a key column the table has not got"
    );
    assert!(
        m.db.declare_edge_table(c, vec![("nope".into(), m.artist)], vec![]).is_err(),
        "a REFERENCES column the table has not got"
    );
    m.db.declare_edge_table(c, refs(), vec!["a".into(), "s".into()]).unwrap();
    // Declared but not bound: no end is the source yet, so no write.
    assert!(m.db.insert_edge_row(c, &json!({"a": "dewa", "s": "kirana"})).is_err());
    assert!(m.db.bind_edge_table(c, "a", "a", "gig", "music").is_err(), "one column cannot be both ends");
    assert!(m.db.bind_edge_table(c, "a", "venue", "gig", "music").is_err(), "an end must be a REFERENCES column");
    // An edge type that already carries untyped edges cannot be adopted.
    let dewa = m.db.get(m.artist, "dewa").unwrap().unwrap().id;
    let kirana = m.db.get(m.song, "kirana").unwrap().unwrap().id;
    m.db.enable_graph().unwrap();
    m.db.link(dewa, "covered", kirana, "", &json!({})).unwrap();
    assert!(m.db.bind_edge_table(c, "a", "s", "covered", "music").is_err());
    m.db.bind_edge_table(c, "a", "s", "gig", "music").unwrap();
    // A table holding a row cannot be declared an edge table.
    let plain = m
        .db
        .create_collection("plain", vec![("a".into(), Kind::Text), ("s".into(), Kind::Text)], CollectionOptions::default())
        .unwrap();
    m.db.put(plain, "row-1", &json!({"a": "dewa", "s": "kirana"})).unwrap();
    assert!(m.db.declare_edge_table(plain, refs(), vec![]).is_err());
}

#[test]
fn a_bound_edge_type_takes_no_untyped_write_and_the_table_no_row() {
    let mut m = music();
    let wrote = edge_table(&mut m, "wrote", vec![], vec![], &["a", "s"]);
    let t = m.db.edge_table(wrote).unwrap().unwrap().binding.unwrap().edge_type;
    let dewa = m.db.get(m.artist, "dewa").unwrap().unwrap().id;
    let kirana = m.db.get(m.song, "kirana").unwrap().unwrap().id;
    assert!(m.db.link(dewa, "wrote", kirana, "", &json!({})).is_err());
    assert!(m.db.put_edge(GraphContextId::BASE, dewa, t, kirana, &json!({})).is_err());
    assert!(m.db.create_edge(GraphContextId::BASE, dewa, t, kirana, &json!({})).is_err());
    assert!(m.db.put(wrote, "r", &json!({"a": "dewa", "s": "kirana"})).is_err());
    assert_eq!(m.db.edge_table_of_type(t).unwrap(), Some(wrote));
    // Neither end's table, nor the edge table itself, can be dropped out
    // from under the edges.
    m.db.rollback().unwrap();
    let refused = |r: Result<(), sekejap_core::collections::Error>| match r {
        Err(sekejap_core::collections::Error::InvalidInput(m)) => m,
        other => panic!("refused by name, not {other:?}"),
    };
    assert!(refused(m.db.begin_drop_collection(m.song)).contains("edge table `wrote` references it"));
    assert!(refused(m.db.begin_drop_collection(wrote)).contains("is an edge table"));
}

#[test]
fn update_delete_and_upsert_by_key() {
    let mut m = music();
    let rated = edge_table(&mut m, "rated", vec![("stars".into(), Kind::Int)], vec![], &["a", "s"]);
    m.db.insert_edge_row(rated, &json!({"a": "dewa", "s": "kirana", "stars": 4})).unwrap();
    m.db.insert_edge_row(rated, &json!({"a": "dewa", "s": "separuh", "stars": 3})).unwrap();
    // UPDATE by every key column rewrites that one edge.
    let n = m
        .db
        .update_edge_rows(rated, &json!({"a": "dewa", "s": "kirana"}), &json!({"stars": 5}))
        .unwrap();
    assert_eq!(n, 1);
    assert!(
        m.db.update_edge_rows(rated, &json!({"a": "dewa", "s": "kirana"}), &json!({"s": "separuh"}))
            .is_err(),
        "an end is the edge's identity, not a property"
    );
    // ON CONFLICT DO NOTHING leaves it; DO UPDATE rewrites the named columns.
    let nothing = m
        .db
        .upsert_edge_row(rated, &json!({"a": "dewa", "s": "kirana", "stars": 1}), &OnConflict::Nothing)
        .unwrap();
    assert!(nothing.is_none());
    m.db.upsert_edge_row(
        rated,
        &json!({"a": "dewa", "s": "separuh", "stars": 2}),
        &OnConflict::Update(vec!["stars".into()]),
    )
    .unwrap();
    m.db.upsert_edge_row(
        rated,
        &json!({"a": "dhani", "s": "kirana", "stars": 3}),
        &OnConflict::Update(vec!["stars".into()]),
    )
    .unwrap();
    m.db.commit().unwrap();
    let mut got: Vec<(String, i64)> = rows(&m, rated, json!({"s": "kirana"}))
        .into_iter()
        .chain(rows(&m, rated, json!({"s": "separuh"})))
        .map(|r| (format!("{}->{}", r.values["a"].as_str().unwrap(), r.values["s"].as_str().unwrap()), r.values["stars"].as_i64().unwrap()))
        .collect();
    got.sort();
    assert_eq!(
        got,
        [
            ("dewa->kirana".to_owned(), 5),
            ("dewa->separuh".to_owned(), 2),
            ("dhani->kirana".to_owned(), 3)
        ]
    );
    // DELETE by one end removes every edge that matches it.
    assert_eq!(m.db.delete_edge_rows(rated, &json!({"a": "dewa"})).unwrap(), 2);
    assert!(
        m.db.delete_edge_rows(rated, &json!({"stars": 3})).is_err(),
        "a filter naming neither end would read every edge of the type"
    );
    m.db.commit().unwrap();
    assert_eq!(out_degree(&m, rated, "dewa"), 0);
    assert_eq!(out_degree(&m, rated, "dhani"), 1);
}

#[test]
fn the_declaration_persists_and_is_additive() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("plain.sekejap");
    {
        let mut db = Database::create(&path, cfg()).unwrap();
        db.create_collection("artist", vec![("name".into(), Kind::Text)], CollectionOptions::default())
            .unwrap();
        db.commit().unwrap();
        assert_eq!(logical_features(&db) & EDGE_TABLE_FEATURE, 0);
    }
    let mut m = music();
    let path = m._dir.path().join("music.sekejap");
    assert_eq!(logical_features(&m.db) & EDGE_TABLE_FEATURE, 0);
    let wrote = edge_table(&mut m, "wrote", vec![], vec![], &["a", "s"]);
    m.db.insert_edge_row(wrote, &json!({"a": "dhani", "s": "kirana"})).unwrap();
    m.db.commit().unwrap();
    assert_ne!(logical_features(&m.db) & EDGE_TABLE_FEATURE, 0);
    let Music { _dir, db, artist, song } = m;
    drop(db);
    let mut db = Database::open(&path, cfg()).unwrap();
    let spec = db.edge_table(wrote).unwrap().unwrap();
    assert_eq!(spec.references, vec![("a".to_owned(), artist), ("s".to_owned(), song)]);
    assert_eq!(spec.key, ["a", "s"]);
    let binding = spec.binding.unwrap();
    assert_eq!((binding.source.as_str(), binding.destination.as_str()), ("a", "s"));
    assert_eq!(db.edge_rows(wrote, &json!({"a": "dhani"}), 256).unwrap().len(), 1);
    // The graph that declared it is recorded (here the pre-0.18.3 way, as a
    // name on the edge table); dropping the graph forgets it and keeps the
    // binding and every edge.
    let elements = db.property_graph("music").unwrap();
    assert!(elements.iter().any(|e| e.edge && e.table == wrote), "{elements:?}");
    db.set_property_graph("music", &[]).unwrap();
    db.commit().unwrap();
    assert!(db.property_graph("music").unwrap().is_empty());
    assert!(db.edge_table(wrote).unwrap().unwrap().binding.is_some());
    assert_eq!(db.edge_rows(wrote, &json!({"a": "dhani"}), 256).unwrap().len(), 1);
}
