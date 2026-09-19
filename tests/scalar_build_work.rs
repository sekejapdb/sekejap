//! COUNTED: what one late scalar build must cost.
//!
//! `build_index_to_ready` takes the sorted path for a scalar index
//! (src/indexes.rs, `build_sorted_once`): read every row once, push one
//! derived key per row into the external sorter, merge, and pack the sorted
//! stream into the index's empty tree bottom-up. Each posting is therefore
//! written to the store EXACTLY ONCE, in one pack, and the build publishes in
//! a small fixed number of transactions -- not one per chunk.
//!
//! Both are counted, so neither needs a stopwatch:
//!
//!   * COMMITS. The create is published, the pack and the READY flip share
//!     one transaction, and a `commit` with nothing pending places no
//!     barrier. At most 3, and the number of barriers may not grow with the
//!     row count.
//!   * POSTINGS WRITTEN ONCE. 10,000 Int postings are about 20 bytes of key
//!     and no value each; packed at 0.9 fill that is on the order of 80 leaf
//!     pages plus one interior level. Every page the build logs is a WAL page
//!     frame, so if the pack were followed by a second ascending pass over
//!     the same postings -- or if the sorted stream were consumed twice --
//!     the frame count would roughly double. The bound below is well under
//!     that, and it does not move when the build is re-run.

use e4_prototype::{
    collections::{CollectionOptions, Database, ScalarPredicate},
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;

const ROWS: usize = 10_000;
const BATCH: usize = 256;

fn config() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

#[test]
fn a_ten_thousand_row_scalar_build_writes_each_posting_once() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, config()).unwrap();
    let docs = db
        .create_collection(
            "docs",
            vec![("key".into(), Kind::Text), ("born".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();
    for i in 0..ROWS {
        let key = format!("k{i:07}");
        db.put(
            docs,
            &key,
            &json!({"key": key, "born": 19000101i64 + (i as i64 * 7) % 900_000}),
        )
        .unwrap();
        if (i + 1) % BATCH == 0 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();

    // The create, then the build. Both are inside the measured interval: the
    // create's publication is a barrier the build cannot avoid, and counting
    // it is what stops the count from being moved rather than removed.
    let before = db.io_counters().unwrap();
    let born = db.create_scalar_index(docs, "born_ix", "born", false).unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(born, BATCH).unwrap();
    db.commit().unwrap();
    let d = db.io_counters().unwrap().saturating_sub(before);
    eprintln!(
        "build over {ROWS} rows: commit_frames {} wal_frames {} wal_bytes {} \
         fsyncs {} (wal_commit {} data_ckpt {} meta {}) checkpoints {}",
        d.commit_frames,
        d.wal_frames_appended,
        d.wal_bytes_written,
        d.fsyncs(),
        d.wal_fsyncs_commit,
        d.data_fsyncs_checkpoint,
        d.metadata_fsyncs,
        d.checkpoint_count,
    );

    // The build answers, so the postings that were written once are all there.
    let found = db
        .query_scalar(
            born,
            ScalarPredicate::Range {
                lower: None,
                upper: None,
            },
            ROWS,
        )
        .unwrap()
        .len();
    assert_eq!(found, ROWS, "every row is in the index");

    assert!(
        d.commit_frames <= 3,
        "{} transactions to create and build one scalar index over {ROWS} rows",
        d.commit_frames
    );
    assert_eq!(
        d.wal_fsyncs_commit, d.commit_frames,
        "one barrier per published transaction and none without one"
    );

    // EACH POSTING ONCE. 10,000 postings of ~20 bytes pack into on the order
    // of 80 leaves plus an interior level; the descriptor replicas and the
    // meta page add a handful more. 200 frames is comfortably under the ~170+
    // a second pass over the same postings would cost, and comfortably over
    // what one pass costs.
    assert!(
        d.wal_frames_appended <= 200,
        "{} WAL page frames to write {ROWS} postings: the posting set is \
         reaching the store more than once",
        d.wal_frames_appended
    );

    // A COMMIT WITH NOTHING TO PUBLISH PLACES NO BARRIER. This is what makes
    // the count above 2 rather than 3: the `commit` a caller writes after
    // `build_index_to_ready` has nothing left of its own to publish.
    let quiet = db.io_counters().unwrap();
    db.commit().unwrap();
    db.commit().unwrap();
    let after = db.io_counters().unwrap().saturating_sub(quiet);
    assert_eq!(
        (after.commit_frames, after.wal_fsyncs_commit, after.wal_bytes_written),
        (0, 0, 0),
        "a commit on a clean handle published a transaction and placed a barrier"
    );
}
