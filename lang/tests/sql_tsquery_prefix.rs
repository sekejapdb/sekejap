//! `to_tsquery('john:*')`: PostgreSQL's prefix match, which was read as a
//! tsvector weight and refused. Each answer below is PostgreSQL 16's for the
//! same rows and query under the `simple` configuration.
//!
//! What is at risk, one test each:
//!
//! * a `:*` term matches every word it starts, alone and ANDed with exact
//!   and other prefix terms (`a_prefix_term_matches_every_word_it_starts`);
//! * a real weight (`:A`) and a prefix inside an OR stay refused by name
//!   (`a_weight_and_an_or_of_prefixes_are_refused_by_name`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlResult, SqlValue};
use tempfile::TempDir;

fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(
        dir.path().join("t.sekejap"),
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )
    .unwrap();
    for sql in [
        "CREATE TABLE t (_key TEXT PRIMARY KEY, v TEXT) WITH (fulltext: [v])",
        "INSERT INTO t (_key, v) VALUES ('a', 'johnny walker'), ('b', 'john doe'), ('c', 'jon snow'), ('d', 'doe re mi'), ('e', 'Johnson family')",
        "COMMIT",
    ] {
        db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
    db
}

fn keys(db: &mut Database, query: &str) -> String {
    let sql = format!("SELECT _key FROM t WHERE to_tsvector('simple', v) @@ to_tsquery('simple', '{query}')");
    match db.sql(&sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        SqlResult::Rows { rows, .. } => {
            let mut out: Vec<String> = rows
                .into_iter()
                .map(|r| match &r.values[0] {
                    SqlValue::Text(k) => k.clone(),
                    other => panic!("{other:?}"),
                })
                .collect();
            out.sort();
            out.join(",")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_prefix_term_matches_every_word_it_starts() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    for (query, postgresql) in [
        ("john:*", "a,b,e"),
        ("john:* & doe", "b"),
        ("jo:*", "a,b,c,e"),
        ("wal:* & john:*", "a"),
        ("john", "b"),
        ("fam:*", "e"),
    ] {
        assert_eq!(keys(&mut db, query), postgresql, "to_tsquery('{query}')");
    }
}

#[test]
fn a_weight_and_an_or_of_prefixes_are_refused_by_name() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    for (query, says) in [("john:A", "weight"), ("john:* | jon:*", ":*")] {
        let sql = format!("SELECT _key FROM t WHERE to_tsvector('simple', v) @@ to_tsquery('simple', '{query}')");
        match db.sql(&sql, &[]) {
            Err(e) => assert!(e.to_string().contains(says), "`{query}`: {e}"),
            Ok(r) => panic!("`{query}` refused by name, not {r:?}"),
        }
    }
}
