//! `REINDEX` (0.19): rebuild indexes into the CURRENT format, as PostgreSQL
//! spells it -- the SQL half of the upgrader (`sekejap-upgrade` is the other,
//! `docs/core/UPGRADE.md`).
//!
//! An old file keeps working without it (Law 8); running it on purpose
//! rewrites every index the way this build writes a new one. The new index is
//! built BESIDE the old under a temporary name, the two names are swapped in
//! one commit, and the old one is dropped in bounded steps, so a query never
//! sees a half-built index and an interrupted run resumes.
//!
//! What is at risk, one test each:
//!
//! * every index family answers exactly as before, keeps its name, gets a
//!   new identity, leaves nothing behind, and verifies clean
//!   (`every_family_keeps_its_answers_and_its_name`);
//! * `TABLE`, `SCHEMA` and `DATABASE` cover what they name
//!   (`table_schema_and_database_cover_what_they_name`);
//! * a crashed run's leftovers -- a stale temporary index, an index left
//!   dropping -- are finished by the next run
//!   (`a_crashed_run_is_finished_by_the_next`);
//! * a UNIQUE index stays unique (`a_unique_index_stays_unique`);
//! * PostgreSQL's refusals and codes (`what_postgresql_refuses_is_refused`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::{Database, IndexState};
use sekejap_lang::{SqlDatabase, SqlError, SqlResult, SqlValue};

use tempfile::TempDir;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn run(db: &mut Database, sql: &str) -> SqlResult {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"))
}

/// Every row an answer holds, as text, in order.
fn answer(db: &mut Database, sql: &str) -> Vec<String> {
    match run(db, sql) {
        SqlResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        SqlValue::Text(t) => t.clone(),
                        SqlValue::Int(i) => i.to_string(),
                        SqlValue::Float(f) => format!("{:x}", f.to_bits()),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

/// `(name, id, ready)` of every index of `table`.
fn indexes(db: &Database, table: &str) -> Vec<(String, u64, bool)> {
    let c = match table.split_once('.') {
        Some((schema, name)) => db.collection_in(schema, name).unwrap().unwrap(),
        None => db.collection(table).unwrap().unwrap(),
    };
    let mut out: Vec<(String, u64, bool)> = db
        .list_indexes(c)
        .unwrap()
        .into_iter()
        .map(|i| (i.name, i.id.0, i.state == IndexState::Ready))
        .collect();
    out.sort();
    out
}

fn verified_clean(path: &std::path::Path) {
    use sekejap_core::collections::verification::{verify_indexed_source, VerificationLimits};
    let report = verify_indexed_source(path, VerificationLimits::default(), |issue| {
        panic!("unexpected verifier issue: {issue:?}")
    })
    .unwrap();
    assert!(report.complete && report.clean, "{report:?}");
}

/// A table with an index of every family, and the questions each answers.
fn world(db: &mut Database) -> Vec<&'static str> {
    for sql in [
        "CREATE TABLE place (_key TEXT PRIMARY KEY, name TEXT, rating REAL, info JSONB, body TEXT,
            spot GEOMETRY(Point,4326), shape GEOMETRY(Polygon,4326), taste VECTOR(4), mood VECTOR(4), flavour VECTOR(4))
            WITH (index: none)",
        "CREATE INDEX place_rating ON place (rating)",
        "CREATE INDEX place_lower ON place (lower(name))",
        "CREATE INDEX place_kind ON place ((info->>'kind'))",
        "CREATE INDEX place_body ON place USING gin (to_tsvector('simple', body))",
        "CREATE INDEX place_trgm ON place USING gin (name gin_trgm_ops)",
        "CREATE INDEX place_spot ON place USING gist (spot)",
        "CREATE INDEX place_shape ON place USING gist (shape)",
        "CREATE INDEX place_taste ON place USING exact (taste)",
        "CREATE INDEX place_mood ON place USING quantized (mood vector_cosine_ops)",
        "CREATE INDEX place_flavour ON place USING vamana (flavour vector_cosine_ops)",
    ] {
        run(db, sql);
    }
    for i in 0..60u32 {
        let (lon, lat) = (115.0 + f64::from(i) / 100.0, -8.8 + f64::from(i % 7) / 50.0);
        let v = |k: u32| format!("[{}, {}, {}, {}]", (i % 5) as f32 / 5.0, (i % 3) as f32 / 3.0, k as f32 / 9.0, 0.1);
        let kind = ["beach", "temple", "market"][(i % 3) as usize];
        db.sql(
            &format!(
                r#"INSERT INTO place (_key, name, rating, info, body, spot, shape, taste, mood, flavour) VALUES
                   ('p{i:02}', 'Warung Sunset {i}', {}, $1, 'quiet {} walk number {i}',
                    '{{"type":"Point","coordinates":[{lon},{lat}]}}',
                    '{{"type":"Polygon","coordinates":[[[{lon},{lat}],[{},{lat}],[{},{}],[{lon},{}],[{lon},{lat}]]]}}',
                    '{}', '{}', '{}')"#,
                f64::from(i % 10) / 2.0,
                kind,
                lon + 0.01,
                lon + 0.01,
                lat + 0.01,
                lat + 0.01,
                v(1),
                v(2),
                v(3),
            ),
            &[sekejap_lang::Param::Json(serde_json::json!({ "kind": kind }))],
        )
        .unwrap();
    }
    run(db, "COMMIT");
    vec![
        "SELECT _key FROM place WHERE rating >= 3.5 ORDER BY rating DESC, _key",
        "SELECT _key FROM place WHERE lower(name) = 'warung sunset 7'",
        "SELECT _key FROM place WHERE info->>'kind' = 'temple' ORDER BY _key",
        "SELECT _key FROM place WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'quiet & beach') ORDER BY _key",
        "SELECT _key, bm25(body, 'temple walk') FROM place ORDER BY bm25(body, 'temple walk') DESC LIMIT 5",
        "SELECT _key FROM place WHERE name ILIKE '%set 4%' ORDER BY _key",
        "SELECT _key FROM place WHERE ST_DWithin(spot, ST_MakePoint(115.2, -8.75)::geography, 5000.0) ORDER BY _key",
        "SELECT _key FROM place WHERE ST_Contains(shape, ST_SetSRID(ST_MakePoint(115.305, -8.795), 4326)) ORDER BY _key",
        "SELECT _key FROM place ORDER BY taste <-> '[0.4, 0.3, 0.1, 0.1]' LIMIT 5",
        "SELECT _key FROM place ORDER BY mood <=> '[0.4, 0.3, 0.2, 0.1]' LIMIT 5",
        "SELECT _key FROM place ORDER BY flavour <=> '[0.4, 0.3, 0.3, 0.1]' LIMIT 5",
    ]
}

#[test]
fn every_family_keeps_its_answers_and_its_name() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("reindex.sekejap");
    let mut db = Database::create(&path, cfg()).unwrap();
    let questions = world(&mut db);
    let before: Vec<Vec<String>> = questions.iter().map(|q| answer(&mut db, q)).collect();
    let held = indexes(&db, "place");
    for (name, _, _) in &held {
        run(&mut db, &format!("REINDEX INDEX {name}"));
    }
    let after = indexes(&db, "place");
    assert_eq!(
        after.iter().map(|(n, _, r)| (n.clone(), *r)).collect::<Vec<_>>(),
        held.iter().map(|(n, _, r)| (n.clone(), *r)).collect::<Vec<_>>(),
        "the same names, all ready, nothing left behind"
    );
    for ((name, old, _), (_, new, _)) in held.iter().zip(&after) {
        assert_ne!(old, new, "`{name}` was not rebuilt");
    }
    for (question, was) in questions.iter().zip(&before) {
        assert_eq!(&answer(&mut db, question), was, "`{question}`");
    }
    drop(db);
    verified_clean(&path);
    let mut db = Database::open(&path, cfg()).unwrap();
    for (question, was) in questions.iter().zip(&before) {
        assert_eq!(&answer(&mut db, question), was, "after a reopen: `{question}`");
    }
}

#[test]
fn table_schema_and_database_cover_what_they_name() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("reindex.sekejap"), cfg()).unwrap();
    let questions = world(&mut db);
    run(&mut db, "CREATE SCHEMA travel");
    run(&mut db, "CREATE TABLE travel.guide (_key TEXT PRIMARY KEY, name TEXT) WITH (index: none)");
    run(&mut db, "CREATE INDEX guide_name ON travel.guide (name)");
    run(&mut db, "INSERT INTO travel.guide (_key, name) VALUES ('made', 'Made'), ('ketut', 'Ketut')");
    run(&mut db, "COMMIT");
    let before: Vec<Vec<String>> = questions.iter().map(|q| answer(&mut db, q)).collect();
    let ids = |db: &Database| (indexes(db, "place"), indexes(db, "travel.guide"));
    let (place, guide) = ids(&db);
    run(&mut db, "REINDEX TABLE place");
    let (place2, guide2) = ids(&db);
    assert!(place.iter().zip(&place2).all(|(a, b)| a.1 != b.1), "TABLE rebuilt every index of the table");
    assert_eq!(guide, guide2, "and no other table's");
    run(&mut db, "REINDEX SCHEMA travel");
    let (place3, guide3) = ids(&db);
    assert_eq!(place2, place3);
    assert_ne!(guide2[0].1, guide3[0].1);
    run(&mut db, "REINDEX DATABASE");
    let (place4, guide4) = ids(&db);
    assert!(place3.iter().zip(&place4).all(|(a, b)| a.1 != b.1));
    assert_ne!(guide3[0].1, guide4[0].1);
    for (question, was) in questions.iter().zip(&before) {
        assert_eq!(&answer(&mut db, question), was, "`{question}`");
    }
    assert_eq!(answer(&mut db, "SELECT _key FROM travel.guide WHERE name = 'Made'"), ["made"]);
}

#[test]
fn a_crashed_run_is_finished_by_the_next() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("reindex.sekejap");
    let rating_id;
    {
        let mut db = Database::create(&path, cfg()).unwrap();
        world(&mut db);
        rating_id = indexes(&db, "place").into_iter().find(|i| i.0 == "place_rating").unwrap().1;
        // What a run that died mid-way leaves: its temporary index...
        run(&mut db, &format!("CREATE INDEX __reindex_{rating_id} ON place (rating)"));
        // ...and an index marked dropping whose steps never ran.
        run(&mut db, "CREATE INDEX leftover ON place (name)");
        let c = db.collection("place").unwrap().unwrap();
        let leftover = db.list_indexes(c).unwrap().into_iter().find(|i| i.name == "leftover").unwrap().id;
        db.begin_drop_index(leftover).unwrap();
        db.commit().unwrap();
    }
    let mut db = Database::open(&path, cfg()).unwrap();
    run(&mut db, "REINDEX DATABASE");
    let names: Vec<String> = indexes(&db, "place").into_iter().map(|i| i.0).collect();
    assert!(!names.iter().any(|n| n.starts_with("__reindex") || n == "leftover"), "{names:?}");
    assert!(names.contains(&"place_rating".to_owned()));
    drop(db);
    verified_clean(&path);
}

#[test]
fn a_unique_index_stays_unique() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("reindex.sekejap"), cfg()).unwrap();
    run(&mut db, "CREATE TABLE guide (_key TEXT PRIMARY KEY, email TEXT UNIQUE)");
    run(&mut db, "INSERT INTO guide (_key, email) VALUES ('made', 'made@example.com')");
    run(&mut db, "COMMIT");
    run(&mut db, "REINDEX TABLE guide");
    match db.sql("INSERT INTO guide (_key, email) VALUES ('putu', 'made@example.com')", &[]) {
        Err(e) => assert!(e.to_string().contains("23505"), "{e}"),
        Ok(r) => panic!("a duplicate was accepted after REINDEX: {r:?}"),
    }
}

fn sqlstate(db: &mut Database, sql: &str) -> String {
    let result = db.sql(sql, &[]);
    let _ = db.sql("ROLLBACK", &[]);
    match result {
        Err(SqlError::Coded { sqlstate, message }) => format!("{sqlstate} {message}"),
        other => panic!("`{sql}` should be refused with a SQLSTATE, not {other:?}"),
    }
}

#[test]
fn what_postgresql_refuses_is_refused() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("reindex.sekejap"), cfg()).unwrap();
    run(&mut db, "CREATE TABLE t (_key TEXT PRIMARY KEY, v TEXT)");
    run(&mut db, "COMMIT");
    let said = sqlstate(&mut db, "REINDEX INDEX nosuch");
    assert!(said.starts_with("42704") && said.contains(r#"index "nosuch" does not exist"#), "{said}");
    let said = sqlstate(&mut db, "REINDEX TABLE nosuch");
    assert!(said.starts_with("42P01") && said.contains(r#"relation "nosuch" does not exist"#), "{said}");
    let said = sqlstate(&mut db, "REINDEX SYSTEM");
    assert!(said.starts_with("0A000"), "{said}");
    // CONCURRENTLY is what the rebuild already is: accepted.
    run(&mut db, "REINDEX TABLE CONCURRENTLY t");
}
