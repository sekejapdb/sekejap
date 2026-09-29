//! `sekejap-upgrade` (`docs/core/UPGRADE.md`), run as the binary a server
//! operator runs.
//!
//! What is at risk, one test each:
//!
//! * `--check` on the preserved 0.18.3 release files says the file is in the
//!   0.18 format and changes no byte
//!   (`check_reports_a_release_file_and_changes_nothing`);
//! * `--apply` on it moves it to the 0.19 format, keeps the original
//!   directory untouched as the backup, and a second `--apply` has nothing
//!   to do (`apply_moves_a_release_file_and_keeps_the_original`);
//! * a file carrying a feature 0.18.3 does not know -- a trigram index -- is
//!   reported as no longer readable by it
//!   (`check_says_when_an_older_release_can_no_longer_open_the_file`).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::SqlDatabase;
use serde_json::Value;

fn upgrade(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_sekejap-upgrade")).args(args).output().unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// A copy of the preserved checkpointed 0.18.3 database, its manifest left out.
fn release_copy(dir: &Path) -> PathBuf {
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/release-fixtures/0.18.3/checkpointed");
    let to = dir.join("db");
    fs::create_dir(&to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_name() != "EXPECTED.json" {
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
    to
}

fn bytes(dir: &Path) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.file_name().into_string().unwrap(), fs::read(e.path()).unwrap())
        })
        .collect()
}

#[test]
fn check_reports_a_release_file_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = release_copy(dir.path());
    let before = bytes(&db);
    let (ok, out, err) = upgrade(&["--check", db.to_str().unwrap()]);
    assert!(ok, "{err}");
    let report: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["format"], "0.18");
    assert!(report["advice"].as_str().unwrap().contains("--apply"));
    assert_eq!(bytes(&db), before, "--check wrote to the database");
}

#[test]
fn apply_moves_a_release_file_and_keeps_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let db = release_copy(dir.path());
    let before = bytes(&db);
    let (ok, _, err) = upgrade(&["--apply", db.to_str().unwrap()]);
    assert!(ok, "{err}");
    assert!(!sekejap_core::collections::upgrade::is_legacy_format(&db).unwrap());
    assert_eq!(bytes(&dir.path().join("db.v018-backup")), before, "the backup is the original");
    let report: Value = serde_json::from_str(&upgrade(&["--check", db.to_str().unwrap()]).1).unwrap();
    assert_eq!(report["older"], 0);
    assert_eq!(report["readable_by"]["0.18.3"], false, "no 0.18 release opens a 0.19 file");
    // A second `--apply` has nothing left to do.
    let (ok, out, err) = upgrade(&["--apply", db.to_str().unwrap()]);
    assert!(ok, "{err}");
    assert!(out.contains("nothing to upgrade"), "{out}");
}

#[test]
fn check_says_when_an_older_release_can_no_longer_open_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    {
        let mut db = Database::create(
            &path,
            Config {
                budget_bytes: 1 << 20,
                io: IoMode::Buffered,
                sync: SyncMode::Full,
            },
        )
        .unwrap();
        for sql in [
            "CREATE TABLE place (_key TEXT PRIMARY KEY, name TEXT)",
            "CREATE INDEX place_name_trgm ON place USING gin (name gin_trgm_ops)",
            "COMMIT",
        ] {
            db.sql(sql, &[]).unwrap();
        }
        db.checkpoint().unwrap();
    }
    let (ok, out, err) = upgrade(&["--check", path.to_str().unwrap()]);
    assert!(ok, "{err}");
    let report: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(report["readable_by"]["0.18.3"], false, "{report}");
    assert!(report["indexes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["family"] == "trigram"));
}
