//! Cross-binary fixture verifier/writer. Compile this SAME harness against the
//! pinned reference engine and the current engine; the Python driver orders
//! the processes. This executable never generates or replaces source fixtures.
#[path = "format_compat/oracle.rs"]
mod oracle;
use e4_prototype::collections::Database;
use oracle::*;
use serde_json::Value;
use std::{env, fs, path::Path};

fn verify(manifest: &Value, dir: &Path, stage: &str) {
    let expected = expected_state(manifest, stage);
    let declared = [u64::from(manifest["compact_cells"].as_bool().unwrap()); 2];
    assert_eq!(features_on_disk(dir), declared, "declared features changed");
    {
        let db = Database::open_snapshot(dir, cfg()).expect("snapshot open");
        check_expected(&db, &expected, &format!("{stage}:snapshot"));
        check_limits(&db, manifest);
    }
    {
        let db = Database::open(dir, cfg()).expect("writer reopen");
        check_expected(&db, &expected, &format!("{stage}:writer-reopen"));
        check_limits(&db, manifest);
    }
    assert_eq!(features_on_disk(dir), declared, "open promoted features");
}

fn mutate(manifest: &Value, dir: &Path, action: &str, boundary: &str) {
    let (before, after, note, insert, delete) = match action {
        "upgrade" => (
            "original",
            "upgraded",
            "compat-updated",
            "p00000999",
            "p00000001",
        ),
        "rollback" => (
            "upgraded",
            "rollback",
            "compat-rollback",
            "p00000998",
            "p00000999",
        ),
        _ => panic!("unknown mutation {action}"),
    };
    assert!(matches!(boundary, "checkpointed" | "wal-pending"));
    verify(manifest, dir, before);
    let declared = features_on_disk(dir);
    let mut db = Database::open(dir, cfg()).expect("writer open");
    // Keep a published reader across the pending-WAL arm so automatic
    // checkpointing cannot erase the intended cross-binary recovery boundary.
    let pin = (boundary == "wal-pending").then(|| Database::open_snapshot(dir, cfg()).unwrap());
    let people = db.collection("people").unwrap().unwrap();
    assert_eq!(
        db.update(people, "p00000000", &serde_json::json!({"note": note}))
            .unwrap(),
        expected_entity_id("people", "p00000000")
    );
    assert_eq!(
        db.put(people, insert, &inserted_document(after)).unwrap(),
        expected_entity_id("people", insert)
    );
    assert!(db.delete(people, delete).unwrap());
    db.commit().expect("commit");
    check_expected(&db, &expected_state(manifest, after), after);
    check_limits(&db, manifest);
    if let Some(reader) = &pin {
        check_expected(
            reader,
            &expected_state(manifest, before),
            "pinned-before-mutation",
        );
    }
    drop(pin);
    if boundary == "checkpointed" {
        assert!(db.checkpoint().unwrap());
    }
    drop(db);
    let wal = fs::metadata(dir.join("wal")).unwrap().len();
    assert_eq!(
        wal == 0,
        boundary == "checkpointed",
        "incorrect WAL handoff boundary"
    );
    assert_eq!(
        features_on_disk(dir),
        declared,
        "mutation promoted features"
    );
    // Intentionally no reopen here: the NEXT binary is first to recover/open
    // the committed bytes left by this writer.
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.as_slice() == ["--version"] {
        println!(
            "{}",
            serde_json::json!({
                "harness": "format_compat-v1", "engine_revision": option_env!("E4_COMPAT_ENGINE_REVISION").unwrap_or("unrecorded"),
                "compact_cells": cfg!(feature="compact-cells"), "sqlite_balance": cfg!(feature="sqlite-balance"),
                "keyspace_append": cfg!(feature="keyspace-append"), "slotref_split": cfg!(feature="slotref-split"),
            })
        );
        return;
    }
    assert!(args.len() == 4 || args.len() == 5,
        "usage: format_compat verify MANIFEST DB_DIR original|upgraded|rollback\n       format_compat mutate MANIFEST DB_DIR upgrade|rollback checkpointed|wal-pending");
    let source = fs::canonicalize(&args[1]).expect("manifest path");
    let dir = fs::canonicalize(&args[2]).expect("copied database directory");
    let corpus = source.parent().unwrap().parent().unwrap();
    assert!(
        !dir.starts_with(corpus),
        "refusing to open any source corpus directory; supply an external copy"
    );
    let manifest: Value = serde_json::from_slice(&fs::read(&source).unwrap()).unwrap();
    match args[0].as_str() {
        "verify" if args.len() == 4 => verify(&manifest, &dir, &args[3]),
        "mutate" if args.len() == 5 => mutate(&manifest, &dir, &args[3], &args[4]),
        _ => panic!("unknown command"),
    }
    println!(
        "{}",
        serde_json::json!({"result":"PASS", "command":args[0], "stage":args[3], "database":dir})
    );
}
