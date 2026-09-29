//! ORDER BY an aggregate compares the aggregate's own values (finding
//! `vuln-a18`, 2026-09-29).
//!
//! The group sort read every accumulator through `f64`: a text MIN/MAX
//! became "no number" in every group, so the order fell back to the group
//! key, and two integers past 2^53 rounded into a false tie. With a LIMIT
//! the wrong groups were returned, not only in the wrong order.
//!
//! What is at risk, one test:
//!
//! * text aggregates order as text, large integers exactly, and a LIMIT
//!   keeps the right groups (`an_aggregate_orders_by_its_own_values`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlResult, SqlValue};
use tempfile::TempDir;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn groups(db: &mut Database, sql: &str) -> Vec<String> {
    match db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        SqlResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match &r.values[0] {
                SqlValue::Text(g) => g.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

#[test]
fn an_aggregate_orders_by_its_own_values() {
    let dir = TempDir::new().unwrap();
    let mut db = Database::create(dir.path().join("g.sekejap"), cfg()).unwrap();
    for sql in [
        "CREATE TABLE demo (_key TEXT PRIMARY KEY, grp TEXT, txt TEXT, v BIGINT)",
        "INSERT INTO demo (_key, grp, txt, v) VALUES \
         ('1', 'a', 'z', 9007199254740993), ('2', 'b', 'a', 9007199254740992), ('3', 'c', 'm', 9007199254740994)",
        "COMMIT",
    ] {
        db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"));
    }
    assert_eq!(groups(&mut db, "SELECT grp, MIN(txt) AS m FROM demo GROUP BY grp ORDER BY m"), ["b", "c", "a"]);
    assert_eq!(groups(&mut db, "SELECT grp, MAX(txt) AS m FROM demo GROUP BY grp ORDER BY m DESC LIMIT 1"), ["a"]);
    assert_eq!(groups(&mut db, "SELECT grp, MAX(v) AS m FROM demo GROUP BY grp ORDER BY m"), ["b", "a", "c"]);
    assert_eq!(groups(&mut db, "SELECT grp, MAX(v) AS m FROM demo GROUP BY grp ORDER BY m DESC LIMIT 1"), ["c"]);
}
