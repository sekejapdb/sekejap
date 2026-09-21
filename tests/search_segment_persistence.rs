//! Search documents written into segments must survive a close and reopen.
//!
//! Segments are what removed the full FST rebuild from compaction: the base is no
//! longer rebuilt, so most of a large index comes to live in segment files beside
//! it. If those are not read back, the store reopens holding only the base — no
//! error, no warning, just a search that quietly answers with a fraction of the
//! corpus. That is the failure this guards, and it is the one that bit BM25 twice
//! before it was caught.

use sekejap::CoreDB;
use serde_json::json;

const BIG: usize = 60_000; // many flushes, and at least one level merge

fn body(i: usize) -> String {
    format!("heron riverbank alpha{} beta{}", i % 997, i % 89)
}

fn build(dir: &std::path::Path) {
    let mut db = CoreDB::open(dir).unwrap();
    db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    db.execute("CREATE INDEX ON t USING search (body)").unwrap();
    let mut done = 0;
    while done < BIG {
        let take = 5_000.min(BIG - done);
        let rows: Vec<(String, serde_json::Value)> = (done..done + take)
            .map(|i| (
                format!("t/n{i}"),
                json!({"_collection":"t","_key":format!("n{i}"),"body": body(i)}),
            ))
            .collect();
        db.put_value_bulk(rows).unwrap();
        done += take;
    }
    db.compact().unwrap();
}

fn hits(db: &CoreDB, q: &str) -> usize {
    db.query(&format!("SELECT _key FROM t WHERE SEARCH('{q}')"))
        .unwrap()
        .collect()
        .len()
}

#[test]
fn documents_written_into_search_segments_survive_a_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    build(dir.path());

    let before = {
        let db = CoreDB::open(dir.path()).unwrap();
        let n = hits(&db, "heron");
        assert_eq!(n, BIG, "the corpus was incomplete before the reopen");
        n
    };

    // Reopened from disk alone — nothing carried over in memory.
    let db = CoreDB::open(dir.path()).unwrap();
    let after = hits(&db, "heron");
    assert_eq!(
        after, before,
        "reopen lost {} of {BIG} documents — segments were written but not read \
         back, which loses data silently",
        before.saturating_sub(after)
    );

    // A selective term, so this checks the postings came back and not merely that
    // a count matched.
    let selective = hits(&db, "alpha42");
    assert!(selective > 0, "a selective term matched nothing after the reopen");
    assert!(selective < BIG, "alpha42 should be selective, not match everything");
}
