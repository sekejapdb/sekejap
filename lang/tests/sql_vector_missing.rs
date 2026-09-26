//! A row with no vector, and an all-zero vector, under a vector ORDER BY --
//! as PostgreSQL with pgvector answers them (owner decision 2026-09-27: "use
//! what pg do").
//!
//! PostgreSQL sorts NULL after every value ascending, and pgvector's cosine
//! distance to an all-zero vector is NaN, which PostgreSQL sorts after every
//! number and before NULL. An exact vector order therefore returns:
//!
//! 1. the rows with a vector, nearest first;
//! 2. then a row whose distance is NaN;
//! 3. then the rows with no vector (missing or written NULL), by id.
//!
//! What is at risk, and the test that pins it:
//!
//! * the rows with no vector come LAST rather than being dropped, with and
//!   without a filter beside the order, and the projected distance is NULL
//!   there (`rows_without_a_vector_come_last_as_postgresql_sorts_null`);
//! * an all-zero vector under `<=>` is NaN, after every number and before
//!   NULL (`a_zero_vector_is_nan_under_cosine_after_numbers_before_null`);
//! * a LIMIT the vectors fill never reaches a NULL row, and paging a row or
//!   two at a time hands out the one-shot answer
//!   (`a_limit_and_pages_cross_from_vectors_into_nulls_in_order`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::{Database, QueryBudget};
use sekejap_lang::{prepare_sql, Param, SqlDatabase, SqlResult, SqlRow, SqlValue};
use tempfile::TempDir;

fn fixture(dir: &TempDir) -> Database {
    let mut db = Database::create(
        dir.path().join("vectors.sekejap"),
        Config {
            budget_bytes: 1 << 20,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )
    .unwrap();
    db.sql("CREATE TABLE spot (grp INT, emb VECTOR(2)) WITH (index: none)", &[]).unwrap();
    // a, b: vectors; c: no vector written; d: written NULL; z: all zero.
    for (key, grp, emb) in [
        ("a", 1, Some(vec![1.0, 0.0])),
        ("b", 2, Some(vec![0.0, 1.0])),
        ("c", 1, None),
        ("d", 2, None),
        ("z", 1, Some(vec![0.0, 0.0])),
    ] {
        match (key, emb) {
            ("d", None) => db
                .sql("INSERT INTO spot (_key, grp, emb) VALUES ($1, $2, NULL)", &[Param::Text(key.into()), Param::Int(grp)])
                .unwrap(),
            (_, None) => db
                .sql("INSERT INTO spot (_key, grp) VALUES ($1, $2)", &[Param::Text(key.into()), Param::Int(grp)])
                .unwrap(),
            (_, Some(emb)) => db
                .sql(
                    "INSERT INTO spot (_key, grp, emb) VALUES ($1, $2, $3)",
                    &[Param::Text(key.into()), Param::Int(grp), Param::Vector(emb)],
                )
                .unwrap(),
        };
    }
    db.sql("COMMIT", &[]).unwrap();
    for ddl in [
        "CREATE INDEX spot_emb ON spot USING exact (emb)",
        "CREATE INDEX spot_grp ON spot USING btree (grp)",
    ] {
        db.sql(ddl, &[]).unwrap();
    }
    db
}

fn rows(db: &Database, sql: &str) -> Vec<Vec<SqlValue>> {
    match prepare_sql(db, sql, &[]).and_then(|p| p.run(db)).unwrap_or_else(|e| panic!("`{sql}`: {e}")) {
        SqlResult::Rows { rows, .. } => rows.into_iter().map(|row| row.values).collect(),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

fn keys(rows: &[Vec<SqlValue>]) -> Vec<String> {
    rows.iter()
        .map(|row| match &row[0] {
            SqlValue::Text(key) => key.clone(),
            other => panic!("a key, not {other:?}"),
        })
        .collect()
}

#[test]
fn rows_without_a_vector_come_last_as_postgresql_sorts_null() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    // L2 from (1, 0): a 0, z 1, b sqrt 2; then c and d, which have none.
    let all = rows(&db, "SELECT _key, emb <-> '[1,0]' AS d FROM spot ORDER BY emb <-> '[1,0]' LIMIT 10");
    assert_eq!(keys(&all), ["a", "z", "b", "c", "d"]);
    assert_eq!(all[3][1], SqlValue::Null, "no vector, no distance");
    assert_eq!(all[4][1], SqlValue::Null, "a NULL vector, no distance");
    // The same beside a filter the index answers: group 1 is a, c, z.
    let group = rows(&db, "SELECT _key FROM spot WHERE grp = 1 ORDER BY emb <-> '[1,0]' LIMIT 10");
    assert_eq!(keys(&group), ["a", "z", "c"]);
}

#[test]
fn a_zero_vector_is_nan_under_cosine_after_numbers_before_null() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    let all = rows(&db, "SELECT _key, emb <=> '[1,0]' AS d FROM spot ORDER BY emb <=> '[1,0]' LIMIT 10");
    assert_eq!(keys(&all), ["a", "b", "z", "c", "d"]);
    match &all[2][1] {
        SqlValue::Float(d) => assert!(d.is_nan(), "{d}"),
        other => panic!("the zero vector's cosine distance is NaN, not {other:?}"),
    }
    assert_eq!(all[3][1], SqlValue::Null);
}

#[test]
fn a_limit_and_pages_cross_from_vectors_into_nulls_in_order() {
    let dir = TempDir::new().unwrap();
    let db = fixture(&dir);
    assert_eq!(
        keys(&rows(&db, "SELECT _key FROM spot ORDER BY emb <-> '[1,0]' LIMIT 3")),
        ["a", "z", "b"],
        "the vectors fill the LIMIT: no NULL row"
    );
    assert_eq!(keys(&rows(&db, "SELECT _key FROM spot ORDER BY emb <-> '[1,0]' LIMIT 4")), ["a", "z", "b", "c"]);
    let sql = "SELECT _key FROM spot ORDER BY emb <-> '[1,0]' LIMIT 10";
    let whole = keys(&rows(&db, sql));
    for page_rows in [1, 2] {
        let prepared = prepare_sql(&db, sql, &[]).unwrap();
        let mut paged = Vec::new();
        prepared
            .for_each_row_with(&db, page_rows, QueryBudget::unlimited(), &mut || false, &mut |row: &SqlRow| {
                paged.push(row.values.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(keys(&paged), whole, "paged at {page_rows}");
    }
}
