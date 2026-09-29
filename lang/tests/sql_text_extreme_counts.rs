//! `left`/`right` with the extreme integer counts a bound parameter can
//! carry (finding `vuln-a19`, 2026-09-29).
//!
//! `right(s, n)` negated `n`, which overflows for `i64::MIN`: a panic with
//! overflow checks, and in release a wrapped value that became an
//! out-of-bounds slice index -- a crash from a valid query either way.
//! PostgreSQL answers `''` for a negative count past the text's length.
//!
//! What is at risk, one test:
//!
//! * `left` and `right` answer at `i64::MIN` and `i64::MAX` as PostgreSQL
//!   does (`extreme_counts_answer_without_a_panic`).

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

#[test]
fn extreme_counts_answer_without_a_panic() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("t.sekejap"), cfg()).unwrap();
    db.sql("CREATE TABLE demo (_key TEXT PRIMARY KEY, name TEXT)", &[]).unwrap();
    db.sql("INSERT INTO demo (_key, name) VALUES ('a', 'lagoon')", &[]).unwrap();
    db.sql("COMMIT", &[]).unwrap();
    for (function, n, want) in [
        ("right", i64::MIN, ""),
        ("left", i64::MIN, ""),
        ("right", i64::MAX, "lagoon"),
        ("left", i64::MAX, "lagoon"),
        ("right", -2, "goon"),
        ("left", -2, "lago"),
    ] {
        let sql = format!("SELECT {function}(name, $1) FROM demo");
        match db.sql(&sql, &[Param::Int(n)]) {
            Ok(SqlResult::Rows { rows, .. }) => {
                assert_eq!(rows[0].values[0], SqlValue::Text(want.into()), "{function}(name, {n})")
            }
            other => panic!("{function}(name, {n}): {other:?}"),
        }
    }
}
