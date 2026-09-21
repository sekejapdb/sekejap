//! COUNTED: what one 256-row commit of the `battle50k` load must cost.
//!
//! The load stage in `bench/src/bin/battle50k.rs` (`load_e4`, ~line 1130) streams
//! rows into a fresh collection and commits after every 256th. A commit is
//! one WAL append of every page the batch dirtied, one commit frame and one
//! FULL barrier. Nothing about 256 rows of the same shape can require more
//! than that, so the cadence is assertable without a stopwatch:
//!
//!   * exactly 2,048 / 256 = 8 commits for 2,048 rows,
//!   * fsyncs per commit: exactly 1 while the WAL is below its fold
//!     threshold (the commit frame's barrier); the fold that follows a
//!     checkpoint-due commit adds one data-file barrier, so the bound is 2,
//!   * WAL frames per commit proportional to the bytes 256 rows occupy, not
//!     to the rows already in the database.
//!
//! The last one is the law: L2, work proportional to the change. A commit
//! whose frame count grows with the size of the database is re-logging pages
//! the batch did not touch.
//!
//! The rows are the `battle50k` shape: a key, a name, a description, their
//! concatenation, an Int date, a category, a Point, a Polygon with a hole and
//! a 32-dimensional vector.

use sekejap_core::{
    collections::{CollectionOptions, Database},
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;

const DIM: usize = 32;
const BATCH: usize = 256;
const ROWS: usize = 2_048;

fn config() -> Config {
    Config {
        // The same 8 MiB the benchmark gives the load.
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn row(i: usize) -> serde_json::Value {
    let name = format!("place {i:06} alpha");
    let desc = format!(
        "row {i:06} beta gamma delta epsilon zeta eta theta iota kappa lambda mu"
    );
    let lon = -180.0 + (i as f64 * 0.0137) % 360.0;
    let lat = -85.0 + (i as f64 * 0.0091) % 170.0;
    let d = 0.01;
    let emb: Vec<f32> = (0..DIM)
        .map(|k| ((i * 31 + k * 17) % 1000) as f32 / 1000.0)
        .collect();
    json!({
        "key": format!("k{i:07}"),
        "name": name,
        "desc": desc,
        "text": format!("{name} {desc}"),
        "born": 19000101i64 + (i as i64 % 900_000),
        "kind": format!("kind{}", i % 8),
        "loc": {"type": "Point", "coordinates": [lon, lat]},
        "plot": {"type": "Polygon", "coordinates": [
            [[lon, lat], [lon + d, lat], [lon + d, lat + d], [lon, lat + d], [lon, lat]],
            [[lon + d / 4.0, lat + d / 4.0], [lon + d / 2.0, lat + d / 4.0],
             [lon + d / 2.0, lat + d / 2.0], [lon + d / 4.0, lat + d / 2.0],
             [lon + d / 4.0, lat + d / 4.0]],
        ]},
        "emb": emb,
    })
}

#[test]
fn two_thousand_rows_cost_eight_commits_and_one_barrier_each() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Database::create(&path, config()).unwrap();
    let place = db
        .create_collection(
            "place",
            vec![
                ("key".into(), Kind::Text),
                ("name".into(), Kind::Text),
                ("desc".into(), Kind::Text),
                ("text".into(), Kind::Text),
                ("born".into(), Kind::Int),
                ("kind".into(), Kind::Text),
                ("loc".into(), Kind::Point),
                ("plot".into(), Kind::Geo),
                ("emb".into(), Kind::Vector(DIM)),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    db.commit().unwrap();

    let mut before = db.io_counters().unwrap();
    let start = before;
    let mut commits = 0usize;
    let mut per_commit = Vec::new();
    for i in 0..ROWS {
        let doc = row(i);
        db.put(place, doc["key"].as_str().unwrap(), &doc).unwrap();
        if (i + 1) % BATCH == 0 {
            db.commit().unwrap();
            commits += 1;
            let now = db.io_counters().unwrap();
            let d = now.saturating_sub(before);
            eprintln!(
                "commit {commits:2}: wal_frames {:5} commit_frames {:2} fsyncs {:2} \
                 (wal_commit {:2} wal_ckpt {:2} data_ckpt {:2} meta {:2}) \
                 wal_bytes {:9} data_pages {:5} checkpoints {:2}",
                d.wal_frames_appended,
                d.commit_frames,
                d.fsyncs(),
                d.wal_fsyncs_commit,
                d.wal_fsyncs_checkpoint,
                d.data_fsyncs_checkpoint,
                d.metadata_fsyncs,
                d.wal_bytes_written,
                d.data_pages_written_at_checkpoint,
                d.checkpoint_count,
            );
            per_commit.push(d);
            before = now;
        }
    }
    let total = db.io_counters().unwrap().saturating_sub(start);
    eprintln!(
        "TOTAL over {ROWS} rows: wal_frames {} commit_frames {} fsyncs {} \
         wal_bytes {} data_pages_at_checkpoint {} checkpoints {}",
        total.wal_frames_appended,
        total.commit_frames,
        total.fsyncs(),
        total.wal_bytes_written,
        total.data_pages_written_at_checkpoint,
        total.checkpoint_count,
    );

    assert_eq!(commits, ROWS / BATCH, "one commit per {BATCH} rows");
    assert_eq!(
        total.commit_frames,
        (ROWS / BATCH) as u64,
        "exactly {} commit frames, one per commit",
        ROWS / BATCH
    );

    // EXACT NUMBER, as the brief asks. One FULL barrier per commit for the
    // commit frame. A commit that also folds the WAL into the data file adds
    // exactly one more (the data-file barrier); the WAL is reset by truncation
    // and is not separately synced. So: 1 without a fold, 2 with one.
    for (n, d) in per_commit.iter().enumerate() {
        let folded = d.checkpoint_count;
        assert!(folded <= 1, "commit {} folded the WAL {folded} times", n + 1);
        assert_eq!(
            d.wal_fsyncs_commit, 1,
            "commit {}: one WAL barrier per commit",
            n + 1
        );
        assert_eq!(
            d.fsyncs(),
            1 + folded,
            "commit {}: 1 barrier, plus 1 when this commit folded the WAL",
            n + 1
        );
    }

    // L2. A 256-row batch dirties the pages those rows land on and no others.
    // 256 rows of this shape are about 175 KiB of row bytes plus 32 KiB of
    // vector sidecars plus their keys; at 4 KiB a page that is roughly 55
    // leaves, and a B-tree of this height adds a handful of interior pages.
    // The bound is deliberately loose (4x) because the point is the SHAPE: a
    // constant per commit, independent of how many rows are already stored.
    // Anything re-logging the whole database would be hundreds of frames and
    // would grow with the commit number.
    let bound = 240u64;
    for (n, d) in per_commit.iter().enumerate() {
        assert!(
            d.wal_frames_appended <= bound,
            "commit {}: {} WAL page frames for {BATCH} rows, over the {bound}-page bound",
            n + 1,
            d.wal_frames_appended
        );
    }
    // And it must not grow: the last commit may not cost materially more than
    // the second (the first carries the empty tree's start-up pages).
    let second = per_commit[1].wal_frames_appended;
    let last = per_commit[per_commit.len() - 1].wal_frames_appended;
    assert!(
        last <= second * 2,
        "per-commit page cost grew from {second} to {last} frames: work is \
         tracking database size, not batch size"
    );
}
