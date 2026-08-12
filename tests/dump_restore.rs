//! SGQL dump / restore round-trip coverage.
//!
//! Strategy: build one database exercising every field type, adversarial string
//! values, edges (attributed + naked), and every index kind; then prove
//! `dump_sql` → `load_sql` into a fresh database reproduces it. The strongest
//! check is *dump equivalence*: dumping the reloaded DB must yield the same text
//! (modulo row order) as dumping the original — if anything failed to round-trip,
//! the two dumps diverge.

use sekejap::CoreDB;

/// Build a database that stresses the dump: all field types, apostrophes/quotes,
/// unicode, empty strings, JSON, GEO, vectors, attributed + naked edges, two
/// collections, and every index kind.
fn build_rich(db: &mut CoreDB) {
    db.execute(
        "CREATE TABLE people (name TEXT, age INTEGER, score REAL, active BOOLEAN, \
         bio TEXT, meta JSON, home GEO, emb VECTOR)",
    )
    .unwrap();
    db.execute("CREATE INDEX ON people USING gin (name)").unwrap();
    db.execute("CREATE INDEX ON people USING bm25 (bio)").unwrap();
    db.execute("CREATE INDEX ON people USING btree (age)").unwrap();
    db.execute("CREATE INDEX ON people USING hnsw (emb)").unwrap();
    db.execute("CREATE INDEX ON people USING spatial (home)").unwrap();

    // Adversarial strings: apostrophe, possessive, unicode, empty, JSON-looking,
    // SQL-looking.
    db.execute(
        "INSERT INTO people (_key, name, age, score, active, bio) \
         VALUES ('p1', 'O''Brien', 42, 3.14, TRUE, 'loves coffee & code')",
    )
    .unwrap();
    db.execute(
        "INSERT INTO people (_key, name, age, active, bio) \
         VALUES ('p2', 'Rod Laver''s Arena', 7, FALSE, 'a tennis venue')",
    )
    .unwrap();
    db.execute("INSERT INTO people (_key, name, bio) VALUES ('p3', 'Zoë Café', '')")
        .unwrap();
    db.execute(
        "INSERT INTO people (_key, name, bio) \
         VALUES ('p4', 'DROP TABLE people;--', 'not really sql')",
    )
    .unwrap();

    // JSON field (nested object + array).
    db.execute(
        r#"INSERT INTO people (_key, name, meta) VALUES ('p5', 'json', '{"a":1,"b":[2,3],"c":"x''y"}')"#,
    )
    .unwrap();

    // GEO field via GeoJSON.
    db.execute(
        "INSERT INTO people (_key, name, home) VALUES ('p6', 'geo', \
         ST_GeomFromGeoJSON('{\"type\":\"Point\",\"coordinates\":[144.96,-37.81]}'))",
    )
    .unwrap();

    // VECTOR field (stored outside the payload).
    db.execute("INSERT INTO people (_key, name, emb) VALUES ('p7', 'vec', [0.1, 0.2, 0.3, 0.4])")
        .unwrap();

    // Second collection + cross-collection edges.
    db.execute("CREATE TABLE places (name TEXT)").unwrap();
    db.execute("INSERT INTO places (_key, name) VALUES ('mcg', 'The ''G''')")
        .unwrap();

    // Edges: attributed, naked, and cross-collection with a string attr.
    db.execute("INSERT ('people/p1')-[:knows {strength: 5}]->('people/p2')")
        .unwrap();
    db.execute("INSERT ('people/p1')-[:knows]->('people/p3')").unwrap(); // naked
    db.execute("INSERT ('people/p1')-[:visited {note: 'match day'}]->('places/mcg')")
        .unwrap();
}

/// Sorted non-comment lines — normalizes row/edge ordering for comparison.
fn normalized(dump: &str) -> Vec<String> {
    let mut lines: Vec<String> = dump
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .map(String::from)
        .collect();
    lines.sort();
    lines
}

#[test]
fn dump_load_roundtrip_is_equivalent() {
    let mut orig = CoreDB::new();
    build_rich(&mut orig);
    let dump1 = orig.dump_sql();

    let mut restored = CoreDB::new();
    let n = restored.load_sql(&dump1).unwrap();
    assert!(n > 0, "load applied {n} statements");

    // The decisive check: re-dumping the restored DB reproduces the original dump.
    let dump2 = restored.dump_sql();
    assert_eq!(
        normalized(&dump1),
        normalized(&dump2),
        "restored dump diverged from original\n--- ORIGINAL ---\n{dump1}\n--- RESTORED ---\n{dump2}"
    );
}

#[test]
fn dump_preserves_adversarial_strings() {
    let mut orig = CoreDB::new();
    build_rich(&mut orig);
    let mut db = CoreDB::new();
    db.load_sql(&orig.dump_sql()).unwrap();

    assert!(db.get("people/p1").unwrap().contains(r#""name":"O'Brien""#));
    assert!(db.get("people/p2").unwrap().contains(r#""name":"Rod Laver's Arena""#));
    assert!(db.get("people/p3").unwrap().contains(r#""name":"Zoë Café""#));
    assert!(db.get("people/p3").unwrap().contains(r#""bio":"""#)); // empty string preserved
    assert!(db.get("people/p4").unwrap().contains("DROP TABLE people")); // stored, not executed
    assert!(db.get("places/mcg").unwrap().contains(r#""name":"The 'G'""#));
    // The injection-looking row is data, not a dropped table:
    assert!(db.get("people/p1").is_some(), "people table intact after loading p4");
}

#[test]
fn dump_roundtrips_all_scalar_types() {
    let mut orig = CoreDB::new();
    build_rich(&mut orig);
    let mut db = CoreDB::new();
    db.load_sql(&orig.dump_sql()).unwrap();

    let p1 = db.get("people/p1").unwrap();
    assert!(p1.contains(r#""age":42"#));
    assert!(p1.contains(r#""score":3.14"#));
    assert!(p1.contains(r#""active":true"#));
    assert!(db.get("people/p2").unwrap().contains(r#""active":false"#));
    // JSON field survives (nested object with an escaped quote inside).
    let p5 = db.get("people/p5").unwrap();
    assert!(p5.contains(r#""a":1"#) && p5.contains("x'y"));
}

#[test]
fn dump_roundtrips_vectors() {
    let mut orig = CoreDB::new();
    build_rich(&mut orig);
    let mut db = CoreDB::new();
    db.load_sql(&orig.dump_sql()).unwrap();

    assert_eq!(db.get_vector("people/p7", "emb"), Some(vec![0.1, 0.2, 0.3, 0.4]));
    let hits = db
        .query("SELECT _key FROM people WHERE VECTOR_NEAR(emb, [0.1, 0.2, 0.3, 0.4], 1)")
        .unwrap()
        .collect();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].slug, "people/p7");
}

#[test]
fn dump_roundtrips_geo() {
    let mut orig = CoreDB::new();
    build_rich(&mut orig);
    let mut db = CoreDB::new();
    db.load_sql(&orig.dump_sql()).unwrap();

    // A spatial query near the stored point finds it (ST_DWithin takes POINT(lon lat)).
    let hits = db
        .query("SELECT _key FROM people WHERE ST_DWithin(home, POINT(144.96 -37.81), 100)")
        .unwrap()
        .collect();
    assert!(hits.iter().any(|h| h.slug == "people/p6"), "geo point round-tripped");
}

#[test]
fn dump_roundtrips_edges() {
    let mut orig = CoreDB::new();
    build_rich(&mut orig);
    let mut db = CoreDB::new();
    db.load_sql(&orig.dump_sql()).unwrap();

    let out = db.edges_from("people/p1");
    // Three outgoing edges: knows->p2 (strength 5), knows->p3 (naked), visited->mcg (note).
    assert_eq!(out.len(), 3, "all three edges restored");
    let to_p2 = out.iter().find(|e| e.to_slug.as_deref() == Some("people/p2")).unwrap();
    assert!(
        to_p2.meta.as_ref().unwrap().get("strength").is_some(),
        "attributed edge kept its strength"
    );
    let to_p3 = out.iter().find(|e| e.to_slug.as_deref() == Some("people/p3")).unwrap();
    assert_eq!(to_p3.edge_type.as_deref(), Some("knows"));
}

#[test]
fn dump_roundtrips_indexes() {
    let mut orig = CoreDB::new();
    build_rich(&mut orig);
    let dump = orig.dump_sql();
    // Every declared index appears as CREATE INDEX in the dump.
    for method in ["gin", "bm25", "btree", "hnsw", "spatial"] {
        assert!(
            dump.contains(&format!("USING {method}")),
            "dump missing CREATE INDEX USING {method}\n{dump}"
        );
    }
    let mut db = CoreDB::new();
    db.load_sql(&dump).unwrap();
    // Indexed queries work after reload.
    let ilike = db.query("SELECT _key FROM people WHERE name ILIKE '%brien%'").unwrap().collect();
    assert_eq!(ilike.len(), 1);
    let bm25 = db.query("SELECT _key FROM people WHERE BM25(bio, 'coffee') > 0.0").unwrap().collect();
    assert_eq!(bm25.len(), 1);
}

#[test]
fn dump_empty_database() {
    let db = CoreDB::new();
    let dump = db.dump_sql();
    assert!(dump.starts_with("-- sekejap dump"));
    let mut fresh = CoreDB::new();
    assert_eq!(fresh.load_sql(&dump).unwrap(), 0, "empty dump applies no statements");
}

#[test]
fn dump_is_idempotent() {
    let mut orig = CoreDB::new();
    build_rich(&mut orig);
    let mut a = CoreDB::new();
    a.load_sql(&orig.dump_sql()).unwrap();
    let mut b = CoreDB::new();
    b.load_sql(&a.dump_sql()).unwrap();
    assert_eq!(normalized(&a.dump_sql()), normalized(&b.dump_sql()));
}
