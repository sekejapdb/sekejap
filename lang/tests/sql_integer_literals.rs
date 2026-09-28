//! Integer literals keep every digit (finding vuln-a05, 0.18.5).
//!
//! A numeric token was parsed as `f64` and turned back into an integer, so
//! any integer literal past 2^53 was rounded before it was stored or
//! compared: `9007199254740993` was stored as `9007199254740992`, and a
//! predicate on it matched the wrong row. A `BIGINT` literal must mean
//! exactly what PostgreSQL makes of it.
//!
//! What is at risk, one test: literals past 2^53, positive and negative, in
//! INSERT, WHERE, UPDATE and inside GRAPH_TABLE, against the same values
//! bound as parameters (`large_integer_literals_keep_every_digit`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{Param, SqlDatabase, SqlResult, SqlValue};
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

fn ints(db: &mut Database, sql: &str) -> Vec<i64> {
    match run(db, sql) {
        SqlResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match r.values[0] {
                SqlValue::Int(i) => i,
                ref other => panic!("`{sql}`: {other:?}"),
            })
            .collect(),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

#[test]
fn large_integer_literals_keep_every_digit() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("ints.sekejap"), cfg()).unwrap();
    run(&mut db, "CREATE TABLE demo (_key TEXT PRIMARY KEY, n INT)");
    run(&mut db, "CREATE INDEX demo_n ON demo (n)");
    run(&mut db, "INSERT INTO demo (_key, n) VALUES ('a', 9007199254740993), ('b', 9007199254740992), ('c', -9007199254740993), ('d', 9223372036854775807)");
    run(&mut db, "COMMIT");
    assert_eq!(ints(&mut db, "SELECT n FROM demo WHERE _key = 'a'"), [9_007_199_254_740_993]);
    assert_eq!(ints(&mut db, "SELECT n FROM demo WHERE _key = 'c'"), [-9_007_199_254_740_993]);
    assert_eq!(ints(&mut db, "SELECT n FROM demo WHERE _key = 'd'"), [i64::MAX]);
    // A literal predicate names exactly one row, as the bound value does.
    assert_eq!(ints(&mut db, "SELECT n FROM demo WHERE n = 9007199254740993"), [9_007_199_254_740_993]);
    let bound = match db
        .sql("SELECT n FROM demo WHERE n = $1", &[Param::Int(9_007_199_254_740_993)])
        .unwrap()
    {
        SqlResult::Rows { rows, .. } => rows.len(),
        other => panic!("{other:?}"),
    };
    assert_eq!(bound, 1);
    run(&mut db, "UPDATE demo SET n = 9007199254740995 WHERE _key = 'b'");
    run(&mut db, "COMMIT");
    assert_eq!(ints(&mut db, "SELECT n FROM demo WHERE _key = 'b'"), [9_007_199_254_740_995]);
    assert_eq!(
        ints(
            &mut db,
            "SELECT * FROM GRAPH_TABLE (base MATCH (x:demo WHERE x.n = 9007199254740993) RETURN x.n AS n)"
        ),
        [9_007_199_254_740_993]
    );
}
