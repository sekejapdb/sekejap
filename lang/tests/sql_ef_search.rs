//! `SET LOCAL ef_search` as PostgreSQL scopes it (owner decision 2026-09-27:
//! "proper fix"): it takes effect when it RUNS, lasts until the transaction
//! ends, and a vector order reads it when the statement runs -- so one
//! prepared or cached plan follows the transaction it runs in.
//!
//! What is at risk, and the test that pins it:
//!
//! * preparing `SET LOCAL` changes nothing; running it does
//!   (`set_local_takes_effect_when_it_runs_not_when_it_is_prepared`);
//! * `COMMIT` and `ROLLBACK` end it
//!   (`commit_and_rollback_end_what_set_local_set`);
//! * a plan prepared before the knob was set answers approximately when it
//!   runs under it, and exactly again after the transaction ends
//!   (`a_prepared_vector_order_follows_the_knob_when_it_runs`).
//!
//! `ef_search = 1` makes the approximate shortlist one row long, so the
//! answer's length says which order ran.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{prepare_sql, Param, SqlDatabase, SqlResult};
use tempfile::TempDir;

const TOP3: &str = "SELECT _key FROM spot ORDER BY emb <-> '[1,0]' LIMIT 3";

fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(
        dir.path().join("ef.sekejap"),
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )
    .unwrap();
    db.sql("CREATE TABLE spot (emb VECTOR(2)) WITH (index: none)", &[]).unwrap();
    for (n, emb) in [[1.0, 0.0], [0.9, 0.1], [0.5, 0.5], [0.0, 1.0]].into_iter().enumerate() {
        db.sql(
            "INSERT INTO spot (_key, emb) VALUES ($1, $2)",
            &[Param::Text(format!("s{n}")), Param::Vector(emb.to_vec())],
        )
        .unwrap();
    }
    db.sql("COMMIT", &[]).unwrap();
    for ddl in [
        "CREATE INDEX spot_emb ON spot USING exact (emb)",
        "CREATE INDEX spot_emb_q ON spot USING quantized (emb)",
    ] {
        db.sql(ddl, &[]).unwrap();
    }
    db
}

fn answer_len(db: &mut Database, sql: &str) -> usize {
    match db.sql(sql, &[]).unwrap() {
        SqlResult::Rows { rows, .. } => rows.len(),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

#[test]
fn set_local_takes_effect_when_it_runs_not_when_it_is_prepared() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    assert_eq!(answer_len(&mut db, TOP3), 3, "exact");
    let _prepared = prepare_sql(&db, "SET LOCAL ef_search = 1", &[]).unwrap();
    assert_eq!(answer_len(&mut db, TOP3), 3, "a prepared, unrun SET LOCAL changes nothing");
    db.sql("SET LOCAL ef_search = 1", &[]).unwrap();
    assert_eq!(answer_len(&mut db, TOP3), 1, "run, it bounds the shortlist to one row");
    db.sql("COMMIT", &[]).unwrap();
}

#[test]
fn commit_and_rollback_end_what_set_local_set() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    for end in ["COMMIT", "ROLLBACK"] {
        db.sql("SET LOCAL ef_search = 1", &[]).unwrap();
        assert_eq!(answer_len(&mut db, TOP3), 1);
        db.sql(end, &[]).unwrap();
        assert_eq!(answer_len(&mut db, TOP3), 3, "after {end}");
    }
    // A caller that ends a transaction through its own API ends it too.
    db.sql("SET LOCAL ef_search = 1", &[]).unwrap();
    db.commit().unwrap();
    sekejap_lang::end_transaction();
    assert_eq!(answer_len(&mut db, TOP3), 3);
}

#[test]
fn a_prepared_vector_order_follows_the_knob_when_it_runs() {
    let dir = TempDir::new().unwrap();
    let mut db = fixture(&dir);
    let prepared = prepare_sql(&db, TOP3, &[]).unwrap();
    let len = |db: &Database| match prepared.run(db).unwrap() {
        SqlResult::Rows { rows, .. } => rows.len(),
        other => panic!("{other:?}"),
    };
    assert_eq!(len(&db), 3, "prepared with no knob: exact");
    db.sql("SET LOCAL ef_search = 1", &[]).unwrap();
    assert_eq!(len(&db), 1, "the same plan, run under the knob: approximate");
    db.sql("COMMIT", &[]).unwrap();
    assert_eq!(len(&db), 3, "the same plan after the transaction: exact again");
}
