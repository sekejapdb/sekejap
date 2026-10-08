//! `DROP TABLE` on a large table stays inside the page-WAL allowance and,
//! once begun, can be finished again from SQL (reported 2026-10-08 from
//! application dogfooding: a full-text index over more text than the WAL cap
//! failed the drop and left the table stuck in DROPPING).
//!
//! The WAL allowance is shrunk with `create_limited`, so a few hundred
//! kilobytes of text stands in for the >16 MiB a real table would need;
//! the defect and its fix are the same at either size.
//!
//! What is at risk, one test each:
//!
//! * a table whose full-text index holds more than one WAL allowance of
//!   postings drops without a `ResourceLimit`
//!   (`a_table_bigger_than_the_wal_allowance_drops`);
//! * a drop interrupted after its DROPPING mark is finished by a second
//!   `DROP TABLE`, and the name is free again
//!   (`drop_table_resumes_a_dropping_collection`);
//! * a database reopened while a collection is DROPPING finishes it from SQL
//!   (`a_reopened_dropping_collection_finishes_from_sql`);
//! * RESTRICT still refuses while graph edges reference the rows
//!   (`restrict_still_refuses_referenced_rows`).

use kernel::{
    io::IoMode,
    limits::ResourceLimits,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlResult};
use tempfile::TempDir;

fn config() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// A small WAL allowance, so a drop whose one step writes more than this is
/// refused unless the step is split. `data_bytes` is generous; only the WAL
/// is the subject.
fn limits() -> ResourceLimits {
    ResourceLimits {
        data_bytes: 64 << 20,
        wal_bytes: 2 << 20,
        tracked_pages: 16384,
        readers: 2,
        record_bytes: 256 << 10,
        recovery_bytes: 8 << 10,
    }
}

fn run(db: &mut Database, sql: &str) -> SqlResult {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"))
}

/// A table with a full-text index, filled with enough distinct text that the
/// index's postings exceed one WAL allowance.
fn big_indexed_table(db: &mut Database) {
    run(db, "CREATE TABLE songs (_key TEXT PRIMARY KEY, title TEXT, lyrics TEXT)");
    // Long lyrics cannot sit in the automatic scalar btree (0.19.2); drop it,
    // exactly as the report's application did, and index the text instead.
    run(db, "DROP INDEX songs_lyrics_btree");
    run(db, "CREATE INDEX songs_lyrics_ft ON songs USING gin (to_tsvector('simple', lyrics))");
    run(db, "CREATE INDEX songs_title_ft ON songs USING gin (to_tsvector('simple', title))");
    // A shared vocabulary, so each term's posting list runs over many rows
    // and the index's keys are large; the drop of that index then has far
    // more than one WAL allowance of postings to remove. The WAL is folded
    // during the inserts so they themselves stay inside the allowance -- it
    // is the DROP, a single uncommitted step, that the fix must bound.
    // Wide rows (~12 KB of lyrics) over a shared vocabulary: the row phase of
    // the drop removes up to 256 rows per step, so one step is ~3 MiB -- more
    // than one WAL allowance -- and the full-text index over the same text is
    // large too. Both are the kind of single step the fix must bound.
    const VOCAB: usize = 64;
    for i in 0..900 {
        let lyrics: String = (0..1500)
            .map(|w| format!("word{} ", (i * 7 + w * 13) % VOCAB))
            .collect();
        let title = format!("song {} {}", i % VOCAB, (i * 3) % VOCAB);
        db.sql(
            "INSERT INTO songs (_key, title, lyrics) VALUES ($1, $2, $3)",
            &[
                sekejap_lang::Param::Text(format!("s{i:04}")),
                sekejap_lang::Param::Text(title),
                sekejap_lang::Param::Text(lyrics),
            ],
        )
        .unwrap_or_else(|e| panic!("insert {i}: {e}"));
        if i % 40 == 39 {
            db.commit().unwrap();
            db.checkpoint().unwrap_or_else(|e| panic!("checkpoint at {i}: {e}"));
        }
    }
    db.commit().unwrap();
    db.checkpoint().unwrap_or_else(|e| panic!("final checkpoint: {e}"));
}

#[test]
fn a_table_bigger_than_the_wal_allowance_drops() {
    let tmp = TempDir::new().unwrap();
    let mut db = Database::create_limited(tmp.path().join("db"), config(), limits()).unwrap();
    big_indexed_table(&mut db);
    // This is the reported failure: one index-drop step wrote more than the
    // WAL allowance and the whole DROP was refused.
    run(&mut db, "DROP TABLE songs");
    // The name is free and nothing of it is left.
    assert!(db.collection("songs").unwrap().is_none(), "the name is free after the drop");
    run(&mut db, "CREATE TABLE songs (_key TEXT PRIMARY KEY, n INT)");
    run(&mut db, "INSERT INTO songs (_key, n) VALUES ('a', 1)");
    assert!(matches!(run(&mut db, "SELECT n FROM songs WHERE _key = 'a'"), SqlResult::Rows { .. }));
}

#[test]
fn drop_table_resumes_a_dropping_collection() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("db");
    let mut db = Database::create_limited(&path, config(), limits()).unwrap();
    big_indexed_table(&mut db);

    // Interrupt: publish the DROPPING mark and take one step, no more.
    let id = db.collection("songs").unwrap().unwrap();
    db.begin_drop_collection_mode(id, sekejap_core::collections::DropMode::Cascade).unwrap();
    let _ = db.drop_collection_step(id, sekejap_core::collections::MAX_DROP_BATCH).unwrap();
    db.commit().unwrap();
    assert!(db.drop_collection_state(id).unwrap().is_some(), "still DROPPING");

    // A second DROP TABLE from SQL finishes it rather than refusing.
    run(&mut db, "DROP TABLE songs");
    assert!(db.collection("songs").unwrap().is_none(), "the name is free");
    run(&mut db, "CREATE TABLE songs (_key TEXT PRIMARY KEY, n INT)");
}

#[test]
fn a_reopened_dropping_collection_finishes_from_sql() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("db");
    {
        let mut db = Database::create_limited(&path, config(), limits()).unwrap();
        big_indexed_table(&mut db);
        let id = db.collection("songs").unwrap().unwrap();
        db.begin_drop_collection_mode(id, sekejap_core::collections::DropMode::Cascade).unwrap();
        db.commit().unwrap();
    }
    let mut db = Database::open(&path, config()).unwrap();
    assert!(db.dropping_collection().is_some(), "reopened mid-drop");
    run(&mut db, "DROP TABLE songs");
    assert!(db.collection("songs").unwrap().is_none(), "the name is free after resume");
}

#[test]
fn restrict_still_refuses_referenced_rows() {
    let tmp = TempDir::new().unwrap();
    let mut db = Database::create_limited(tmp.path().join("db"), config(), limits()).unwrap();
    run(&mut db, "CREATE TABLE person (_key TEXT PRIMARY KEY, name TEXT)");
    run(&mut db, "INSERT INTO person (_key, name) VALUES ('a', 'A'), ('b', 'B')");
    run(&mut db, "COMMIT");
    db.enable_graph().unwrap();
    db.commit().unwrap();
    let a = match run(&mut db, "SELECT _id FROM person WHERE _key = 'a'") {
        SqlResult::Rows { rows, .. } => match rows[0].values[0] {
            sekejap_lang::SqlValue::Id(id) => id,
            ref o => panic!("{o:?}"),
        },
        o => panic!("{o:?}"),
    };
    let b = match run(&mut db, "SELECT _id FROM person WHERE _key = 'b'") {
        SqlResult::Rows { rows, .. } => match rows[0].values[0] {
            sekejap_lang::SqlValue::Id(id) => id,
            ref o => panic!("{o:?}"),
        },
        o => panic!("{o:?}"),
    };
    db.link(a, "knows", b, "", &serde_json::json!({})).unwrap();
    db.commit().unwrap();
    // RESTRICT (the SQL default) refuses while an edge references a row.
    assert!(db.sql("DROP TABLE person", &[]).is_err(), "RESTRICT must refuse");
}

#[test]
fn an_oversized_delete_is_refused_with_the_remedy() {
    let tmp = TempDir::new().unwrap();
    let mut db = Database::create_limited(tmp.path().join("db"), config(), limits()).unwrap();
    big_indexed_table(&mut db);
    // Every row in one statement is one transaction, more than the WAL
    // allowance; the refusal names the remedy, not just the limit.
    match db.sql("DELETE FROM songs WHERE _key >= 's'", &[]) {
        Ok(r) => panic!("expected a refusal, got {r:?}"),
        Err(e) => {
            let m = e.to_string();
            assert!(m.contains("key ranges") && m.contains("16 MiB"), "{m}");
        }
    }
    let _ = db.sql("ROLLBACK", &[]);
    // After the rollback the rows are still there and still queryable.
    assert!(matches!(db.sql("SELECT count(*) FROM songs", &[]), Ok(SqlResult::Rows { .. })));
}
