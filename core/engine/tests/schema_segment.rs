//! Named schemas: a collection belongs to one (`public` unless it says
//! otherwise), and the same table name can live in two schemas at once.
//!
//! What is at risk, one test each: that a schema-qualified collection is
//! found by its schema and not by its bare name; that both survive a reopen;
//! that a rename keeps the schema and a drop frees the name; that a schema
//! with a table in it cannot be dropped out from under it; that the names
//! PostgreSQL reserves are refused; that a file with no schema in it is
//! unchanged (an older binary still opens it) and a file with one is refused
//! by that binary as `Unsupported`, not `Corrupt` (Law 8); and that the
//! verifier and the rebuild both understand the records.
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{
        rebuild::{rebuild_derived_indexes, RebuildLimits},
        verification::{verify_indexed_source, VerificationLimits},
        CollectionOptions, Database, Error, SCHEMA_FEATURE, SUPPORTED_LOGICAL_FEATURES,
    },
    internal::{admit_logical_features, logical_features},
    Kind,
};
use serde_json::json;

fn cfg() -> Config {
    Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn fields() -> Vec<(String, Kind)> {
    vec![("label".into(), Kind::Text)]
}

fn create_in(db: &mut Database, schema: &str, name: &str) -> sekejap_core::collections::CollectionId {
    db.create_collection_in(schema, name, fields(), Vec::new(), Vec::new(), CollectionOptions::default())
        .unwrap()
}

#[test]
fn the_same_name_in_two_schemas_is_two_collections() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    db.create_schema("sales").unwrap();
    let public = create_in(&mut db, "public", "orders");
    let sales = create_in(&mut db, "sales", "orders");
    db.commit().unwrap();
    assert_ne!(public, sales);
    db.put(public, "a", &json!({"label": "public row"})).unwrap();
    db.put(sales, "a", &json!({"label": "sales row"})).unwrap();
    db.commit().unwrap();

    assert_eq!(db.collection("orders").unwrap(), Some(public), "a bare name is public");
    assert_eq!(db.collection_in("public", "orders").unwrap(), Some(public));
    assert_eq!(db.collection_in("sales", "orders").unwrap(), Some(sales));
    assert_eq!(db.collection_in("sales", "missing").unwrap(), None);
    assert_eq!(db.collection_in("nowhere", "orders").unwrap(), None);
    assert_eq!(db.collection_info(sales).unwrap().schema, "sales");
    assert_eq!(db.collection_info(public).unwrap().schema, "public");
    assert_eq!(db.list_collections().unwrap(), vec!["orders".to_owned()], "list_collections is public");
    assert_eq!(
        db.list_qualified_collections().unwrap(),
        vec![("public".to_owned(), "orders".to_owned()), ("sales".to_owned(), "orders".to_owned())]
    );
    assert_eq!(db.list_schemas().unwrap(), vec!["public".to_owned(), "sales".to_owned()]);
    drop(db);

    // Reopened, every fact is the same.
    let db = Database::open(&path, cfg()).unwrap();
    assert_eq!(db.collection_in("sales", "orders").unwrap(), Some(sales));
    assert_eq!(db.collection("orders").unwrap(), Some(public));
    assert_eq!(db.list_schemas().unwrap(), vec!["public".to_owned(), "sales".to_owned()]);
    assert_eq!(db.get(sales, "a").unwrap().unwrap().document["label"], "sales row");
    assert_eq!(db.get(public, "a").unwrap().unwrap().document["label"], "public row");
}

#[test]
fn a_rename_keeps_the_schema_and_a_drop_frees_the_name() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    db.create_schema("sales").unwrap();
    let id = create_in(&mut db, "sales", "orders");
    db.commit().unwrap();
    db.rename_collection(id, "orders_old").unwrap();
    db.commit().unwrap();
    assert_eq!(db.collection_in("sales", "orders").unwrap(), None);
    assert_eq!(db.collection_in("sales", "orders_old").unwrap(), Some(id));
    assert_eq!(db.collection("orders_old").unwrap(), None, "the rename did not move it to public");

    db.drop_collection(id).unwrap();
    db.commit().unwrap();
    assert_eq!(db.collection_in("sales", "orders_old").unwrap(), None);
    // The name is free again, in the same schema.
    create_in(&mut db, "sales", "orders_old");
}

#[test]
fn a_schema_holding_a_table_cannot_be_dropped_and_an_empty_one_can() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    db.create_schema("sales").unwrap();
    let id = create_in(&mut db, "sales", "orders");
    db.commit().unwrap();
    let refused = db.drop_schema("sales").unwrap_err();
    assert!(format!("{refused}").contains("orders"), "{refused}");
    db.drop_collection(id).unwrap();
    db.drop_schema("sales").unwrap();
    db.commit().unwrap();
    assert_eq!(db.list_schemas().unwrap(), vec!["public".to_owned()]);
    let refused = db
        .create_collection_in("sales", "orders", fields(), Vec::new(), Vec::new(), CollectionOptions::default())
        .unwrap_err();
    assert!(format!("{refused}").contains("does not exist"), "{refused}");
}

#[test]
fn reserved_and_duplicate_schema_names_are_refused() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    for name in ["public", "pg_catalog", "information_schema", "pg_anything", ""] {
        assert!(db.create_schema(name).is_err(), "`{name}` was accepted");
    }
    db.create_schema("sales").unwrap();
    assert!(matches!(db.create_schema("sales"), Err(Error::AlreadyExists)));
    assert!(db.drop_schema("public").is_err());
    assert!(db.drop_schema("nowhere").is_err());
}

// ── Law 8 ────────────────────────────────────────────────────────────────

/// A database that never names a schema carries no schema record and no
/// bit: every binary that opened it before still opens it.
#[test]
fn a_file_with_no_schema_does_not_carry_the_bit() {
    let t = tempfile::tempdir().unwrap();
    let mut db = Database::create(t.path().join("db"), cfg()).unwrap();
    create_in(&mut db, "public", "orders");
    db.commit().unwrap();
    assert_eq!(logical_features(&db) & SCHEMA_FEATURE, 0);
}

#[test]
fn a_schema_file_is_unsupported_to_a_binary_that_predates_the_bit() {
    assert_eq!(SUPPORTED_LOGICAL_FEATURES & SCHEMA_FEATURE, SCHEMA_FEATURE);
    let older = SUPPORTED_LOGICAL_FEATURES & !SCHEMA_FEATURE;
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    let mut db = Database::create(&path, cfg()).unwrap();
    db.create_schema("sales").unwrap();
    db.commit().unwrap();
    let written = logical_features(&db);
    drop(db);
    assert_eq!(written & SCHEMA_FEATURE, SCHEMA_FEATURE, "an empty schema sets the bit too");
    admit_logical_features(written, SUPPORTED_LOGICAL_FEATURES).unwrap();
    Database::open(&path, cfg()).unwrap();
    let refused = admit_logical_features(written, older).unwrap_err();
    assert!(
        matches!(refused, Error::Unsupported(ref m) if m.contains(&format!("{written:#x}"))),
        "{refused:?}"
    );
}

// ── the verifier and the rebuild ─────────────────────────────────────────

#[test]
fn the_verifier_is_clean_and_the_rebuild_keeps_every_schema() {
    let t = tempfile::tempdir().unwrap();
    let source = t.path().join("db");
    let destination = t.path().join("rebuilt");
    {
        let mut db = Database::create(&source, cfg()).unwrap();
        db.create_schema("sales").unwrap();
        db.create_schema("empty").unwrap();
        let public = create_in(&mut db, "public", "orders");
        let sales = create_in(&mut db, "sales", "orders");
        db.put(public, "a", &json!({"label": "p"})).unwrap();
        db.put(sales, "a", &json!({"label": "s"})).unwrap();
        db.commit().unwrap();
    }
    let report = verify_indexed_source(&source, VerificationLimits::default(), |issue| {
        panic!("unexpected issue: {issue:?}")
    })
    .unwrap();
    assert!(report.complete && report.clean);

    rebuild_derived_indexes(&source, &destination, RebuildLimits::default()).unwrap();
    let verified = verify_indexed_source(&destination, VerificationLimits::default(), |issue| {
        panic!("unexpected issue after rebuild: {issue:?}")
    })
    .unwrap();
    assert!(verified.complete && verified.clean);
    let db = Database::open(&destination, cfg()).unwrap();
    assert_eq!(
        db.list_schemas().unwrap(),
        vec!["empty".to_owned(), "public".to_owned(), "sales".to_owned()]
    );
    let sales = db.collection_in("sales", "orders").unwrap().expect("sales.orders");
    assert_eq!(db.get(sales, "a").unwrap().unwrap().document["label"], "s");
}
