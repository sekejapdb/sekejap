//! Files a RELEASED build wrote open in this build with the answers that
//! release gave (`CONTRACT.md` Law 8, `L8-COMPAT`,
//! `docs/core/RELEASE_FIXTURES.md`).
//!
//! `docs/release-fixtures/<release>/` holds databases the tagged release
//! wrote through SQL -- one checkpointed, one with committed writes still in
//! its WAL -- the answers it gave (`EXPECTED.json`) and every file's SHA-256
//! (`INDEX.json`). They are never regenerated to pass; a fixture that is
//! missing or changed fails.
//!
//! What is at risk, one test each:
//!
//! * the preserved bytes are the ones the release wrote, and every release
//!   and database named in [`RELEASES`] is present -- an inventory kept here,
//!   not in the editable manifest (`every_preserved_file_is_the_one_the_release_wrote`);
//! * this build opens a copy -- recovering the WAL where there is one -- and
//!   answers every query exactly as the release did, and opening changes no
//!   preserved file (`every_release_answer_is_this_builds_answer`);
//! * the copy keeps the release's logical feature word through an open, a
//!   write, a checkpoint and a reopen -- nothing converted, nothing new
//!   switched on -- takes inserts, updates and deletes that the release's
//!   indexes and graph then answer, and still refuses what the release
//!   refused (`a_release_file_takes_writes_and_reopens`).
//!
//! * `REINDEX DATABASE` on a copy -- the upgrader's rebuild -- keeps every
//!   answer, every row identity and the feature word
//!   (`a_release_file_reindexes_to_the_same_answers`).
//!
//! Not here: an OLDER binary reading the file after this build wrote to it
//! (`L8-COMPAT` remaining, `docs/core/RELEASE_FIXTURES.md`).

#[allow(dead_code)]
#[path = "../examples/release_fixtures.rs"]
mod release_fixtures;

use std::fs;
use std::path::{Path, PathBuf};

use release_fixtures::{cell, cfg};
use sekejap_core::collections::Database;
use sekejap_core::internal::logical_features;
use sekejap_lang::{SqlDatabase, SqlResult};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// `lang/` is the crate root; the corpora live at the repository root.
const ROOT: &str = "../docs/release-fixtures";

/// Every preserved release, the SHA-256 of its `INDEX.json`, and its
/// databases. A release is added here when its fixtures are; none is removed.
const RELEASES: &[(&str, &str, &[&str])] = &[(
    "0.18.3",
    "1ea602464cc54258da33cd1ec53b7ee53d1afae17ff81d3d326e0f1a47c16e08",
    &["checkpointed", "wal-pending"],
)];

struct Fixture {
    release: String,
    name: String,
    dir: PathBuf,
    record: Value,
}

fn fixtures() -> Vec<Fixture> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(ROOT);
    let mut present: Vec<String> = fs::read_dir(&root)
        .unwrap_or_else(|e| panic!("{}: the preserved release fixtures are missing: {e}", root.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_dir())
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    present.sort();
    let mut listed: Vec<String> = RELEASES.iter().map(|(r, _, _)| r.to_string()).collect();
    listed.sort();
    assert_eq!(present, listed, "the preserved releases are not the ones RELEASES names");
    let mut out = Vec::new();
    for (release, index_sha, databases) in RELEASES {
        let dir = root.join(release);
        let bytes = fs::read(dir.join("INDEX.json")).unwrap();
        assert_eq!(sha256(&bytes), *index_sha, "{release}: INDEX.json changed");
        let index: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(index["release"].as_str(), Some(*release), "{release}: INDEX.json names another release");
        let records = index["databases"].as_array().unwrap();
        let names: Vec<&str> = records.iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, *databases, "{release}: the databases are not the ones RELEASES names");
        for record in records {
            let name = record["name"].as_str().unwrap().to_owned();
            out.push(Fixture {
                release: release.to_string(),
                dir: dir.join(&name),
                name,
                record: record.clone(),
            });
        }
    }
    out
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn label(fx: &Fixture) -> String {
    format!("{}/{}", fx.release, fx.name)
}

/// Every preserved file's bytes, checked against `INDEX.json`, and the
/// directory holding nothing else.
fn verify(fx: &Fixture) {
    let files = fx.record["files"].as_object().unwrap();
    let mut present: Vec<String> = fs::read_dir(&fx.dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n != "EXPECTED.json")
        .collect();
    present.sort();
    let listed: Vec<String> = files.keys().cloned().collect();
    assert_eq!(present, listed, "{}: the file inventory changed", label(fx));
    for (name, want) in files {
        let bytes = fs::read(fx.dir.join(name)).unwrap();
        assert_eq!(bytes.len() as u64, want["bytes"].as_u64().unwrap(), "{}: {name} size", label(fx));
        assert_eq!(sha256(&bytes), want["sha256"].as_str().unwrap(), "{}: {name} changed", label(fx));
    }
    assert_eq!(
        sha256(&fs::read(fx.dir.join("EXPECTED.json")).unwrap()),
        fx.record["expected_sha256"].as_str().unwrap(),
        "{}: EXPECTED.json changed",
        label(fx)
    );
}

fn copy(fx: &Fixture) -> tempfile::TempDir {
    let dst = tempfile::Builder::new()
        .prefix(&format!("release-{}-{}-", fx.release, fx.name))
        .tempdir()
        .unwrap();
    for name in fx.record["files"].as_object().unwrap().keys() {
        fs::copy(fx.dir.join(name), dst.path().join(name)).unwrap();
    }
    dst
}

fn expected(fx: &Fixture) -> Vec<Value> {
    let queries = serde_json::from_slice::<Value>(&fs::read(fx.dir.join("EXPECTED.json")).unwrap())
        .unwrap()
        .as_array()
        .unwrap()
        .clone();
    assert!(queries.len() >= 30, "{}: queries went missing", label(fx));
    for q in &queries {
        assert!(!q["rows"].as_array().unwrap().is_empty(), "{}: `{}` answers nothing", label(fx), q["sql"]);
    }
    queries
}

/// The logical feature word the release recorded, and this build's reading
/// of the open copy.
fn features(fx: &Fixture, db: &Database) -> (String, String) {
    (
        fx.record["logical_features"].as_str().unwrap().to_owned(),
        format!("{:#x}", logical_features(db)),
    )
}

/// The WAL-pending database has commits to replay; the checkpointed one
/// has none. Without this a regenerated fixture could lose its point.
fn wal_state(fx: &Fixture) {
    let wal = fx.record["files"]["wal"]["bytes"].as_u64().unwrap();
    match fx.name.as_str() {
        "wal-pending" => assert!(wal > 0, "{}: the WAL is empty", label(fx)),
        _ => assert_eq!(wal, 0, "{}: the WAL is not empty", label(fx)),
    }
}

fn refused(db: &mut Database, sql: &str, code: &str, fx: &Fixture) {
    match db.sql(sql, &[]) {
        Err(e) => assert!(e.to_string().contains(code), "{}: `{sql}` said {e}, not {code}", label(fx)),
        Ok(r) => panic!("{}: `{sql}` was accepted: {r:?}", label(fx)),
    }
    let _ = db.sql("ROLLBACK", &[]);
}

/// The rows `sql` answers now, in the fixture's own encoding.
fn answer(db: &mut Database, sql: &str) -> (Value, Vec<Value>) {
    match db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        SqlResult::Rows { columns, rows } => (
            serde_json::json!(columns),
            rows.iter()
                .map(|r| Value::Array(r.values.iter().map(cell).collect()))
                .collect(),
        ),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

#[test]
fn every_preserved_file_is_the_one_the_release_wrote() {
    for fx in fixtures() {
        verify(&fx);
        wal_state(&fx);
    }
}

#[test]
fn every_release_answer_is_this_builds_answer() {
    for fx in fixtures() {
        verify(&fx);
        let copy = copy(&fx);
        let mut db = Database::open(copy.path(), cfg()).unwrap_or_else(|e| panic!("{}: open: {e}", label(&fx)));
        let (recorded, now) = features(&fx, &db);
        assert_eq!(now, recorded, "{}: opening changed the logical feature word", label(&fx));
        for query in expected(&fx) {
            let sql = query["sql"].as_str().unwrap();
            let (columns, rows) = answer(&mut db, sql);
            assert_eq!(columns, query["columns"], "{}: `{sql}` columns", label(&fx));
            assert_eq!(rows, query["rows"].as_array().unwrap().clone(), "{}: `{sql}`", label(&fx));
        }
        drop(db);
        // Only the copy was opened; the preserved files are untouched.
        verify(&fx);
    }
}

#[test]
fn a_release_file_takes_writes_and_reopens() {
    for fx in fixtures() {
        let copy = copy(&fx);
        let (recorded, _) = {
            let db = Database::open(copy.path(), cfg()).unwrap();
            features(&fx, &db)
        };
        {
            let mut db = Database::open(copy.path(), cfg()).unwrap();
            for sql in [
                "INSERT INTO place (_key, name, rating, description, geometry, taste) VALUES ('amed', 'Amed Beach', 4.4, 'quiet reef beach for snorkelling', '{\"type\":\"Point\",\"coordinates\":[115.65,-8.35]}', '[0.0, 0.0, 1.0, 0.0]')",
                "UPDATE place SET visits = 40 WHERE _key = 'kuta-beach'",
                "UPDATE place SET geometry = '{\"type\":\"Point\",\"coordinates\":[115.60,-8.40]}' WHERE _key = 'seminyak'",
                "UPDATE dish SET flavour = '[0.0, 0.0, 0.0, 1.0]' WHERE _key = 'lawar'",
                "DELETE FROM place WHERE _key = 'tanah-lot'",
                "INSERT INTO guided (guide, place, score) VALUES ('made', 'amed', 4.6)",
                "INSERT INTO review (_key, body) VALUES ('r999', 'lagoon lagoon')",
                "COMMIT",
            ] {
                db.sql(sql, &[]).unwrap_or_else(|e| panic!("{}: `{sql}`: {e}", label(&fx)));
            }
            // What the release refused, this build refuses on its file.
            refused(&mut db, "SELECT _key FROM scratch", "scratch", &fx);
            refused(&mut db, "INSERT INTO travel.guide (_key, name, email) VALUES ('putu', 'Putu', 'made@example.com')", "23505", &fx);
            refused(&mut db, "INSERT INTO travel.guide (_key, email) VALUES ('kadek', 'kadek@example.com')", "23502", &fx);
            assert!(db.checkpoint().unwrap(), "{}: checkpoint deferred", label(&fx));
        }
        let mut db = Database::open(copy.path(), cfg()).unwrap();
        let (_, now) = features(&fx, &db);
        assert_eq!(now, recorded, "{}: a write switched a feature on or off", label(&fx));
        // The written values are reached through the indexes and the graph
        // the release built.
        for (sql, want) in [
            ("SELECT _key FROM place WHERE to_tsvector('simple', description) @@ to_tsquery('simple', 'snorkelling')", vec!["amed"]),
            ("SELECT _key FROM place WHERE lower(name) = 'amed beach'", vec!["amed"]),
            ("SELECT _key FROM place WHERE visits >= 40", vec!["kuta-beach"]),
            ("SELECT _key FROM place WHERE _key = 'tanah-lot'", vec![]),
            ("SELECT _key FROM place WHERE ST_DWithin(geometry, ST_MakePoint(115.60, -8.40)::geography, 1000.0) ORDER BY _key", vec!["seminyak"]),
            ("SELECT _key FROM place ORDER BY taste <=> '[0.0, 0.0, 1.0, 0.0]' LIMIT 1", vec!["amed"]),
            ("SELECT _key FROM dish ORDER BY flavour <=> '[0.0, 0.0, 0.0, 1.0]' LIMIT 1", vec!["lawar"]),
            ("SELECT _key FROM review WHERE to_tsvector('simple', body) @@ to_tsquery('simple', 'lagoon')", vec!["r999"]),
            ("SELECT * FROM GRAPH_TABLE (tours MATCH (g:person WHERE g._key = 'made')-[:led]->(p:place WHERE p._key = 'amed') RETURN p._key AS place)", vec!["amed"]),
        ] {
            let (_, rows) = answer(&mut db, sql);
            let want: Vec<Value> = want.iter().map(|k| serde_json::json!([k])).collect();
            assert_eq!(rows, want, "{}: `{sql}`", label(&fx));
        }
        verify(&fx);
    }
}

#[test]
fn a_release_file_reindexes_to_the_same_answers() {
    for fx in fixtures() {
        let copy = copy(&fx);
        let mut db = Database::open(copy.path(), cfg()).unwrap();
        db.sql("REINDEX DATABASE", &[])
            .unwrap_or_else(|e| panic!("{}: REINDEX DATABASE: {e}", label(&fx)));
        let (recorded, now) = features(&fx, &db);
        assert_eq!(now, recorded, "{}: REINDEX changed the feature word", label(&fx));
        drop(db);
        let mut db = Database::open(copy.path(), cfg()).unwrap();
        for query in expected(&fx) {
            let sql = query["sql"].as_str().unwrap();
            let (columns, rows) = answer(&mut db, sql);
            assert_eq!(columns, query["columns"], "{}: `{sql}` columns", label(&fx));
            assert_eq!(rows, query["rows"].as_array().unwrap().clone(), "{}: after REINDEX: `{sql}`", label(&fx));
        }
        verify(&fx);
    }
}
