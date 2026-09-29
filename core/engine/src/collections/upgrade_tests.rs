//! The format move. What is at risk, one test each:
//!
//! * a 0.18 file becomes a Register file with the same rows, the original
//!   kept as the backup, and a second call does nothing
//!   (`a_legacy_database_moves_and_keeps_its_original`);
//! * a crash between the two renames is finished by the next call
//!   (`an_interrupted_publication_is_finished`);
//! * a folder with no database in it yet -- missing, or created empty by
//!   the application before its first open -- is left alone
//!   (`a_folder_with_no_database_yet_is_left_alone`).

use super::*;
use crate::supportive::header::FORCE;
use kernel::{io::IoMode, store::SyncMode};
use serde_json::json;

fn cfg() -> Config {
    Config { budget_bytes: 1 << 20, io: IoMode::Buffered, sync: SyncMode::Full }
}

fn legacy_db(path: &Path) {
    FORCE.with(|f| f.set(Some(false)));
    let mut db = Database::create(path, cfg()).unwrap();
    let c = db.create_collection("t", vec![("n".into(), Kind::Int)], Default::default()).unwrap();
    db.put(c, "a", &json!({"n": 1})).unwrap();
    db.commit().unwrap();
    FORCE.with(|f| f.set(None));
}

#[test]
fn a_legacy_database_moves_and_keeps_its_original() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    legacy_db(&path);
    assert!(is_legacy_format(&path).unwrap());
    let original = std::fs::read(path.join("data")).unwrap();
    let done = upgrade_format(&path, Default::default()).unwrap().unwrap();
    assert!(!is_legacy_format(&path).unwrap());
    assert_eq!(std::fs::read(done.backup.join("data")).unwrap(), original, "the backup is the original");
    let db = Database::open(&path, cfg()).unwrap();
    let c = db.collection("t").unwrap().unwrap();
    assert_eq!(db.get(c, "a").unwrap().unwrap().document, json!({"n": 1}));
    drop(db);
    assert!(upgrade_format(&path, Default::default()).unwrap().is_none());
}

#[test]
fn an_interrupted_publication_is_finished() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("db");
    legacy_db(&path);
    let staging = sibling(&path, ".v019-upgrading").unwrap();
    let backup = sibling(&path, ".v018-backup").unwrap();
    super::rebuild::upgrade_to_register(&path, &staging, Default::default()).unwrap();
    std::fs::rename(&path, &backup).unwrap();
    // The crash: the second rename never happened.
    let done = upgrade_format(&path, Default::default()).unwrap().unwrap();
    assert!(done.report.is_none());
    assert!(!is_legacy_format(&path).unwrap());
    assert!(backup.exists() && !staging.exists());
}

#[test]
fn a_folder_with_no_database_yet_is_left_alone() {
    let t = tempfile::tempdir().unwrap();
    let missing = t.path().join("missing");
    assert!(upgrade_format(&missing, Default::default()).unwrap().is_none());
    assert!(!missing.exists());
    let empty = t.path().join("empty");
    std::fs::create_dir(&empty).unwrap();
    assert!(upgrade_format(&empty, Default::default()).unwrap().is_none());
    assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0, "nothing written into it");
}
