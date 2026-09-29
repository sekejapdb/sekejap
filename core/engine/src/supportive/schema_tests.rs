//! Build steps 3b and 3c: a table on a Register file (`docs/core/SUPPORTIVE.md`
//! 2.b, 2.c, 2.i). What is at risk, one test each:
//!
//! * the table's whole description -- name, schema, declared spellings, rules,
//!   layouts -- survives reopen and lives in the Register only: no 0.18
//!   catalog record, layout descriptor or name key is written
//!   (`a_table_lives_in_the_register_only`);
//! * the entry payloads round-trip and refuse trailing bytes
//!   (`entry_payloads_round_trip`);
//! * after tables, indexes, rows, a graph and edges, the primary tree holds
//!   no 0.18 supportive keyspace at all: every supportive fact is in the
//!   Register (`a_register_file_keeps_no_legacy_supportive_keys`);
//! * salvage finds a Register file's layouts -- a renamed and a dropped
//!   column included -- and decodes its rows (`salvage_reads_register_layouts`).

use super::header::FORCE;
use super::schema::*;
use crate::collections::{CollectionOptions, ColumnRule, Database, DefaultValue};
use crate::Kind;
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;

fn cfg() -> Config {
    Config { budget_bytes: 1 << 20, io: IoMode::Buffered, sync: SyncMode::Full }
}

#[test]
fn a_table_lives_in_the_register_only() {
    FORCE.with(|f| f.set(Some(true)));
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    db.create_schema("shop").unwrap();
    let c = db
        .create_collection_in(
            "shop",
            "orders",
            vec![("at".into(), Kind::Int), ("qty".into(), Kind::Int)],
            vec![("at".into(), "TIMESTAMPTZ".into())],
            vec![("qty".into(), ColumnRule { default: Some(DefaultValue::Constant(json!(1))), not_null: true })],
            CollectionOptions { timestamps: true },
        )
        .unwrap();
    db.put(c, "a", &json!({"at": 5})).unwrap();
    db.rename_collection(c, "sales").unwrap();
    db.alter_collection(c, vec![("at".into(), Kind::Int), ("qty".into(), Kind::Int), ("note".into(), Kind::Text)])
        .unwrap();
    db.put(c, "b", &json!({"at": 6, "qty": 2, "note": "x"})).unwrap();
    db.commit().unwrap();
    let info = db.collection_info(c).unwrap();
    let a = db.get(c, "a").unwrap().unwrap().document;
    drop(db);

    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.collection_in("shop", "sales").unwrap(), Some(c));
    assert_eq!(db.collection_in("shop", "orders").unwrap(), None);
    assert_eq!(db.list_schemas().unwrap(), vec!["public".to_owned(), "shop".to_owned()]);
    assert_eq!(db.list_qualified_collections().unwrap(), vec![("shop".to_owned(), "sales".to_owned())]);
    let again = db.collection_info(c).unwrap();
    assert_eq!(format!("{again:?}"), format!("{info:?}"));
    assert_eq!(db.get(c, "a").unwrap().unwrap().document, a, "a row in the first layout");
    assert_eq!(db.get(c, "b").unwrap().unwrap().document["note"], json!("x"));
    let store = db.store().unwrap();
    for legacy in [&[1u8][..], &[0, 240], &[0x10]] {
        let first = store.range(legacy).unwrap().next().map(|r| r.unwrap().0);
        assert!(
            first.is_none_or(|k| !k.starts_with(legacy)),
            "a Register file writes no 0.18 catalog key under {legacy:?}"
        );
    }
}

#[test]
fn entry_payloads_round_trip() {
    let table = Table { timestamps: true, layout: 7, schema: 2, parts: TABLE_KEYS | TABLE_MEMB };
    assert_eq!(Table::decode(&table.encode()).unwrap(), table);
    let name = Name { parent: 3, name: "orders".into() };
    assert_eq!(Name::decode(&name.encode().unwrap()).unwrap(), name);
    // A new column's stored token is its name, as `write_layout` sets it.
    let column = String::from("at");
    let col = Column {
        live: true,
        name: column.clone(),
        stored_token: column,
        declared: Some("TIMESTAMPTZ".into()),
        rule: Some(vec![1, 2, 3]),
        missing: Some(serde_json::json!("active")),
    };
    assert_eq!(Column::decode(&col.encode().unwrap()).unwrap(), col);
    let part = LayoutPart { table: 4, slots: vec![(1, 0, 0), (16, 6, 3)] };
    assert_eq!(LayoutPart::decode(&part.encode()).unwrap(), part);
    for mut bytes in [table.encode(), name.encode().unwrap(), col.encode().unwrap(), part.encode()] {
        bytes.push(0);
        assert!(
            Table::decode(&bytes).is_err()
                && Name::decode(&bytes).is_err()
                && Column::decode(&bytes).is_err()
                && LayoutPart::decode(&bytes).is_err()
        );
    }
}

#[test]
fn a_register_file_keeps_no_legacy_supportive_keys() {
    FORCE.with(|f| f.set(Some(true)));
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db
        .create_collection("places", vec![("n".into(), Kind::Int), ("about".into(), Kind::Text)], Default::default())
        .unwrap();
    let by_n = db.create_scalar_index(c, "by_n", "n", false).unwrap();
    let text = db.create_text_index(c, "about_text", "about").unwrap();
    let a = db.put(c, "a", &json!({"n": 1, "about": "beach temple"})).unwrap();
    let b = db.put(c, "b", &json!({"n": 2, "about": "rice terrace"})).unwrap();
    db.commit().unwrap();
    while !db.build_index_step(by_n, 64).unwrap() {}
    db.commit().unwrap();
    let _ = text;
    db.enable_graph().unwrap();
    let near = db.create_edge_type("near").unwrap();
    db.create_edge(crate::index::graph::GraphContextId::BASE, a, near, b, &json!({"km": 3})).unwrap();
    db.commit().unwrap();
    drop(db);
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.row_count(c).unwrap(), Some(2));
    assert_eq!(db.edge_type("near").unwrap(), Some(near));
    let store = db.store().unwrap();
    for row in store.range(&[]).unwrap() {
        let (k, _) = row.unwrap();
        let legacy = match k.first() {
            Some(0) => !(k.len() == 3 && k[1] == 0 && k[2] <= 2),
            Some(1..=9) | Some(0x10..=0x12) => true,
            _ => false,
        };
        assert!(!legacy, "a 0.18 supportive key in a Register file: {:?}", &k[..k.len().min(12)]);
    }
}

#[test]
fn salvage_reads_register_layouts() {
    FORCE.with(|f| f.set(Some(true)));
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    let c = db
        .create_collection("t", vec![("a".into(), Kind::Int), ("b".into(), Kind::Text)], Default::default())
        .unwrap();
    db.put(c, "one", &json!({"a": 1, "b": "x"})).unwrap();
    db.commit().unwrap();
    db.rename_column(c, "a", "n").unwrap();
    db.drop_column(c, "b").unwrap();
    db.put(c, "two", &json!({"n": 2})).unwrap();
    db.commit().unwrap();
    drop(db);
    let out = t.path().join("salvaged");
    let report = crate::recovery::recover_typed_candidates(
        &path,
        &out,
        &crate::collections::CollectionRecovery,
        Default::default(),
    )
    .unwrap();
    assert_eq!(report.layouts, 2, "{report:?}");
    assert_eq!(report.decoded_records, 2, "{report:?}");
    let decoded = std::fs::read_to_string(out.join("decoded.jsonl")).unwrap();
    assert!(decoded.contains("\"n\":1") && decoded.contains("\"n\":2"), "{decoded}");
}
