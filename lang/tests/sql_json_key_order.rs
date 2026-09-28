//! Stored JSON objects under either `serde_json::Map` ordering (fix for a
//! host-reported defect, 0.18.5).
//!
//! `serde_json::Map` iterates in sorted key order by default and in insertion
//! order when ANY crate in the build enables `serde_json/preserve_order` --
//! Cargo unifies features, so a host decides it, not sekejap. The encoder
//! wrote keys in the map's order and every reader demanded sorted keys, so a
//! host with `preserve_order` wrote edge properties (and JSON objects) that
//! its own reads then refused as "unordered/duplicate object key".
//!
//! Run in both builds:
//!
//! ```text
//! cargo test -p sekejap-lang --test sql_json_key_order
//! cargo test -p sekejap-lang --features serde_json/preserve_order --test sql_json_key_order
//! ```
//!
//! What is at risk, one test each:
//!
//! * an edge table's properties round-trip and the table stays walkable,
//!   queryable and deletable (`edge_properties_read_back_in_either_build`);
//! * a JSONB object with keys written out of order reads back whole and its
//!   `->>` index answers (`jsonb_objects_read_back_in_either_build`).

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

fn texts(db: &mut Database, sql: &str) -> Vec<String> {
    match run(db, sql) {
        SqlResult::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| {
                r.values
                    .iter()
                    .map(|v| match v {
                        SqlValue::Text(t) => t.clone(),
                        SqlValue::Bool(b) => b.to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>()
                    .join("|")
            })
            .collect(),
        other => panic!("`{sql}` answered {other:?}"),
    }
}

#[test]
fn edge_properties_read_back_in_either_build() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("order.sekejap");
    {
        let mut db = Database::create(&path, cfg()).unwrap();
        for sql in [
            "CREATE TABLE guides (_key TEXT PRIMARY KEY, name TEXT)",
            "CREATE TABLE villages (_key TEXT PRIMARY KEY, name TEXT)",
            // Property columns declared out of alphabetical order, as in the
            // report; a default fills one of them.
            "CREATE TABLE guided (guide TEXT REFERENCES guides, village TEXT REFERENCES villages,
                                  zone TEXT, is_public BOOLEAN DEFAULT true, area TEXT)",
            "ALTER PROPERTY GRAPH base ADD EDGE TABLES (guided SOURCE KEY (guide) REFERENCES guides (_key)
                                                       DESTINATION KEY (village) REFERENCES villages (_key))",
            "INSERT INTO guides (_key, name) VALUES ('g1', 'Made')",
            "INSERT INTO villages (_key, name) VALUES ('v1', 'Penglipuran'), ('v2', 'Tenganan')",
            "INSERT INTO guided (guide, village, zone, area) VALUES ('g1', 'v1', 'north', 'Bangli')",
            "INSERT INTO guided (guide, village, zone, is_public, area) VALUES ('g1', 'v2', 'east', false, 'Karangasem')",
            "COMMIT",
        ] {
            run(&mut db, sql);
        }
        assert_eq!(
            texts(&mut db, "SELECT * FROM GRAPH_TABLE (base MATCH (a:guides)-[e:guided]->(b:villages) RETURN b._key AS k, e.zone AS z, e.is_public AS p, e.area AS a) ORDER BY k"),
            ["v1|north|true|Bangli", "v2|east|false|Karangasem"]
        );
        assert_eq!(texts(&mut db, "SELECT village, zone FROM guided WHERE guide = 'g1' ORDER BY village"), ["v1|north", "v2|east"]);
    }
    // After a reopen, and a delete through the table.
    let mut db = Database::open(&path, cfg()).unwrap();
    run(&mut db, "DELETE FROM guided WHERE guide = 'g1' AND village = 'v1'");
    run(&mut db, "COMMIT");
    assert_eq!(
        texts(&mut db, "SELECT * FROM GRAPH_TABLE (base MATCH (a:guides)-[e:guided]->(b:villages) RETURN b._key AS k, e.area AS a)"),
        ["v2|Karangasem"]
    );
}

#[test]
fn jsonb_objects_read_back_in_either_build() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("order.sekejap");
    {
        let mut db = Database::create(&path, cfg()).unwrap();
        run(&mut db, "CREATE TABLE place (_key TEXT PRIMARY KEY, info JSONB)");
        run(&mut db, "CREATE INDEX place_kind ON place ((info->>'kind'))");
        // Keys written out of order, nested too: under `preserve_order` the
        // map keeps exactly this order.
        let info = serde_json::json!({"zone": "south", "kind": "beach", "meta": {"z": 1, "a": [1, {"y": 2, "b": 3}]}});
        db.sql("INSERT INTO place (_key, info) VALUES ('kuta', $1)", &[Param::Json(info.clone())])
            .unwrap();
        run(&mut db, "COMMIT");
    }
    let mut db = Database::open(&path, cfg()).unwrap();
    assert_eq!(texts(&mut db, "SELECT _key FROM place WHERE info->>'kind' = 'beach'"), ["kuta"]);
    match run(&mut db, "SELECT info FROM place WHERE _key = 'kuta'") {
        SqlResult::Rows { rows, .. } => match &rows[0].values[0] {
            SqlValue::Json(v) => assert_eq!(
                v,
                &serde_json::json!({"kind": "beach", "meta": {"a": [1, {"b": 3, "y": 2}], "z": 1}, "zone": "south"})
            ),
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}
