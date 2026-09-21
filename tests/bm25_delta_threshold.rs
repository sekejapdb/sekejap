//! Folding the BM25 delta less often must not change what BM25 answers.
//!
//! The merge threshold used to be a fixed 4096 documents, and every merge
//! rebuilds the whole index, so writing N documents cost `(N / 4096) x O(N)`.
//! Measured on a node, seconds per 250 000 rows ran 82, 150, 223, 333, 411,
//! 1509 — quadratic, and the reason a fifty-million-row load could not finish.
//!
//! The threshold now scales with the corpus, which makes the number of merges
//! roughly constant and the total linear. That is purely a question of *when*
//! the delta is folded: a query has always had to consult base and delta
//! together, so the answers must be identical either way.
//!
//! Which is exactly what needs proving, because the failure mode is silent. A
//! delta consulted incorrectly does not error; it returns a slightly different
//! ranking, and nobody notices until relevance is quietly wrong. So these
//! compare SCORES, not just document sets — a set comparison would pass on an
//! index whose ranking had drifted.

use sekejap::CoreDB;

const N: usize = 12_000;

fn body(i: usize) -> String {
    // Skewed term frequencies, so BM25 has something to actually rank on: a
    // uniform corpus scores every document the same and hides ordering bugs.
    format!(
        "heron riverbank alpha{} beta{} gamma{}",
        i % 997,
        i % 89,
        i % 7
    )
}

fn build(dir: &std::path::Path, chunk: usize) -> CoreDB {
    let mut db = CoreDB::open(dir).unwrap();
    db.execute("CREATE TABLE t (body TEXT)").unwrap();
    db.execute("CREATE INDEX ON t USING bm25 (body)").unwrap();
    let mut done = 0;
    while done < N {
        let take = chunk.min(N - done);
        let rows: Vec<(String, serde_json::Value)> = (done..done + take)
            .map(|i| {
                (
                    format!("t/n{i}"),
                    serde_json::json!({"_collection":"t","_key":format!("n{i}"),"body": body(i)}),
                )
            })
            .collect();
        db.put_value_bulk(rows).unwrap();
        done += take;
    }
    db
}

/// `(doc_id, score)` in ranked order, rounded so float noise in the last bits
/// does not fail a test about ranking. Comparing ids rather than keys is the
/// stricter check: it fails if the same documents come back in a different
/// order, which a set comparison would let through.
fn ranked(db: &CoreDB, query: &str) -> Vec<(u64, i64)> {
    db.bm25_search("body", query, 50)
        .iter()
        .map(|(id, score)| (*id, (score * 10_000.0).round() as i64))
        .collect()
}

#[test]
fn scores_do_not_depend_on_how_often_the_delta_was_folded() {
    // Two identical corpora, written in very different batch sizes so the delta
    // crosses its threshold at completely different points.
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let a = build(dir_a.path(), 250);
    let b = build(dir_b.path(), 6_000);

    for q in ["heron", "alpha42", "beta7", "gamma3 heron"] {
        let ra = ranked(&a, q);
        let rb = ranked(&b, q);
        assert!(!ra.is_empty(), "query `{q}` matched nothing — the test proves nothing");
        assert_eq!(
            ra, rb,
            "BM25 ranking for `{q}` depends on batch size, so base and delta are not \
             being scored together consistently"
        );
    }
}

#[test]
fn scores_survive_a_fold_and_a_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = build(dir.path(), 1_000);

    let before = ranked(&db, "heron");
    assert!(!before.is_empty());

    // Folding the delta into the base must not move a single score.
    db.compact().unwrap();
    assert_eq!(ranked(&db, "heron"), before, "a compaction changed BM25 ranking");

    drop(db);
    let db = CoreDB::open(dir.path()).unwrap();
    assert_eq!(ranked(&db, "heron"), before, "a reopen changed BM25 ranking");
}

#[test]
fn a_document_written_after_a_fold_is_ranked_with_the_rest() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = build(dir.path(), 1_000);
    db.compact().unwrap();
    let before = ranked(&db, "heron");
    assert!(!before.is_empty());

    // Written against a folded base, so it lives only in the delta.
    db.put(
        "t/zzz",
        r#"{"_collection":"t","_key":"zzz","body":"heron heron heron riverbank"}"#,
    )
    .unwrap();

    let after = ranked(&db, "heron");
    // Not a count check: `bm25_search` is top-k and every document in this corpus
    // contains the term, so the length is pinned at k whatever happens. The
    // ranking is what carries the information.
    // Three occurrences where every other document has one: it must outrank them
    // all. The point is that it is scored against the base corpus statistics,
    // not appended with some default.
    assert!(
        after[0].1 > before[0].1,
        "the delta document scored {} against a base top of {} — it was not \
         scored against the base corpus",
        after[0].1,
        before[0].1
    );
}

/// Documents written into segments must survive a close and reopen.
///
/// Segments are what removed the O(corpus) collapse from compaction — the base
/// is no longer rewritten, so most of the index comes to live in segment files
/// beside it. If `load_segments` misses them, the store reopens holding only the
/// base: no error, no warning, just a search that quietly answers with a
/// fraction of the corpus. That is the failure this guards.
#[test]
fn documents_written_into_segments_survive_a_reopen() {
    const BIG: usize = 60_000; // many flushes, and at least one level merge

    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE t (body TEXT)").unwrap();
        db.execute("CREATE INDEX ON t USING bm25 (body)").unwrap();
        let mut done = 0;
        while done < BIG {
            let take = 5_000.min(BIG - done);
            let rows: Vec<(String, serde_json::Value)> = (done..done + take)
                .map(|i| (
                    format!("t/n{i}"),
                    serde_json::json!({"_collection":"t","_key":format!("n{i}"),"body": body(i)}),
                ))
                .collect();
            db.put_value_bulk(rows).unwrap();
            done += take;
        }
        db.compact().unwrap();
        let before = db.bm25_search("body", "heron", BIG + 10).len();
        assert_eq!(before, BIG, "the corpus was incomplete before the reopen");
    }

    // Reopened from disk alone — nothing carried over in memory.
    let db = CoreDB::open(dir.path()).unwrap();
    let after = db.bm25_search("body", "heron", BIG + 10).len();
    assert_eq!(
        after, BIG,
        "reopen lost {} of {BIG} documents — segments were written but not read \
         back, which loses data silently",
        BIG - after
    );

    // A term that only some documents carry, so this checks the postings came
    // back correctly and not merely that the document count matched.
    let alpha = db.bm25_search("body", "alpha42", BIG).len();
    assert!(alpha > 0, "a selective term matched nothing after the reopen");
    assert!(alpha < BIG, "alpha42 should be selective, not match everything");
}
