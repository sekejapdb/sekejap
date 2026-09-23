//! A database closed while another thread starts a child process can be
//! reopened at once, and its reader slots do not look held afterwards.
//!
//! An OS file lock belongs to the open file, and a child process inherits a
//! copy of every descriptor for the instant between fork and exec. A lock
//! released by CLOSING our descriptor stayed held by that copy, so a reopen
//! right after a close was refused as `WriterLocked` and a checkpoint right
//! after a checkpoint was deferred for a reader that did not exist. Every lock
//! is now released by an explicit unlock (`kernel::io::Locked`), which acts on
//! the open file the copy shares. This is the regression test: the same
//! close-reopen-checkpoint loop the flaky tests ran, with a thread that does
//! nothing but start child processes beside it.

use kernel::io::IoMode;
use kernel::store::{Config, SyncMode};
use sekejap_core::collections::Database;
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[test]
fn close_then_reopen_and_checkpoint_are_never_refused_while_processes_start() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let config = Config {
        budget_bytes: 1 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Off,
    };
    let mut db = Database::create(&path, config).unwrap();
    let c = db
        .create_collection("c", vec![("n".into(), sekejap_core::Kind::Int)], Default::default())
        .unwrap();
    db.commit().unwrap();
    drop(db);

    let stop = Arc::new(AtomicBool::new(false));
    let spawner = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut spawned = 0u64;
            while !stop.load(Ordering::Relaxed) {
                std::process::Command::new("true").status().unwrap();
                spawned += 1;
            }
            spawned
        })
    };
    let rounds = 400;
    let mut refused = Vec::new();
    for round in 0..rounds {
        let mut db = match Database::open(&path, config) {
            Ok(db) => db,
            Err(e) => {
                refused.push(format!("round {round}: reopen: {e:?}"));
                continue;
            }
        };
        db.put(c, &format!("k{round}"), &json!({"n": round})).unwrap();
        db.commit().unwrap();
        if !db.checkpoint().unwrap() {
            refused.push(format!("round {round}: checkpoint deferred"));
        }
        // The WAL is empty now: a second checkpoint has no reader to defer to.
        if !db.checkpoint().unwrap() {
            refused.push(format!("round {round}: empty checkpoint deferred"));
        }
    }
    stop.store(true, Ordering::Relaxed);
    let spawned = spawner.join().unwrap();
    assert!(spawned > 0, "the spawner never ran, so this proved nothing");
    assert!(
        refused.is_empty(),
        "{} of {rounds} rounds were refused with {spawned} processes started:\n{}",
        refused.len(),
        refused.join("\n")
    );
    let db = Database::open(&path, config).unwrap();
    assert_eq!(db.scan(c, None).unwrap().count(), rounds);
}
