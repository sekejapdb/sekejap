//! Incremental index maintenance must be indistinguishable from a full rebuild.
//!
//! BM25 keeps recently-written documents in an in-RAM *delta* segment so a single
//! INSERT costs `O(document)` instead of `O(corpus)`. That is only a valid trade if
//! the two-segment index answers exactly what a freshly-built one would — and if the
//! delta can never be lost. These tests pin both.

use sekejap::CoreDB;

/// Insert `n` rows of predictable text, optionally building the index first so the
/// writes take the incremental path rather than the bulk build.
fn seed(db: &mut CoreDB, range: std::ops::Range<usize>) {
    for i in range {
        let animal = if i % 3 == 0 { "fox" } else if i % 3 == 1 { "dog" } else { "heron" };
        db.execute(&format!(
            "INSERT INTO d (_key, body) VALUES ('d{i}', 'the quick brown {animal} number {i} \
             leaps over the lazy riverbank')"
        )).unwrap();
    }
}

fn hits(db: &CoreDB, term: &str) -> Vec<String> {
    let mut v: Vec<String> = db
        .query(&format!("SELECT _key FROM d WHERE BM25(body,'{term}') > 0"))
        .unwrap()
        .collect()
        .iter()
        .map(|h| h.slug.trim_start_matches("d/").to_string())
        .collect();
    v.sort();
    v
}

/// Rows written one at a time through the delta must be found exactly like rows
/// that were present when the index was built.
#[test]
fn incremental_inserts_match_a_full_rebuild() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();

    // A: index built first, then 200 rows inserted incrementally.
    let mut a = CoreDB::open(dir_a.path()).unwrap();
    a.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    seed(&mut a, 0..20);
    a.execute("CREATE INDEX ON d USING bm25 (body)").unwrap();
    seed(&mut a, 20..220);

    // B: all 220 rows present before the index is built.
    let mut b = CoreDB::open(dir_b.path()).unwrap();
    b.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    seed(&mut b, 0..220);
    b.execute("CREATE INDEX ON d USING bm25 (body)").unwrap();

    for term in ["fox", "heron", "riverbank", "quick brown fox"] {
        assert_eq!(hits(&a, term), hits(&b, term), "term {term:?} differs from a rebuild");
        assert!(!hits(&b, term).is_empty(), "term {term:?} should match something");
    }
}

/// Overwriting a row must replace its indexed text, not add a second copy of it.
#[test]
fn an_update_replaces_the_indexed_document() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    seed(&mut db, 0..50);
    db.execute("CREATE INDEX ON d USING bm25 (body)").unwrap();

    // d0 currently says "fox". Rewrite it to say "albatross".
    assert!(hits(&db, "fox").contains(&"d0".to_string()));
    db.execute("UPDATE d SET body = 'a solitary albatross' WHERE _key = 'd0'").unwrap();

    assert!(!hits(&db, "fox").contains(&"d0".to_string()), "stale term still matches");
    assert_eq!(hits(&db, "albatross"), vec!["d0".to_string()]);
}

/// A deleted row must disappear from search whether it sat in the delta or the base.
#[test]
fn a_deleted_row_leaves_the_index() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    seed(&mut db, 0..50);
    db.execute("CREATE INDEX ON d USING bm25 (body)").unwrap();
    seed(&mut db, 50..60);              // these land in the delta

    assert!(hits(&db, "fox").contains(&"d51".to_string()), "delta row is searchable");

    db.execute("DELETE FROM d WHERE _key = 'd51'").unwrap();   // delta-resident
    db.execute("DELETE FROM d WHERE _key = 'd0'").unwrap();    // base-resident

    let fox = hits(&db, "fox");
    assert!(!fox.contains(&"d51".to_string()), "deleted delta row still matches");
    assert!(!fox.contains(&"d0".to_string()), "deleted base row still matches");
}

/// The delta lives only in RAM. Anything that persists the index must fold it in
/// first, or the newest documents vanish on the next open — the exact failure the
/// write_binary guard rail exists to prevent.
#[test]
fn delta_documents_survive_a_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
        seed(&mut db, 0..50);
        db.execute("CREATE INDEX ON d USING bm25 (body)").unwrap();
        seed(&mut db, 50..80);          // delta-resident, never merged explicitly
        db.compact().unwrap();
    }
    let db = CoreDB::open_paged(dir.path()).unwrap();
    let fox = hits(&db, "fox");
    assert!(fox.contains(&"d51".to_string()), "delta row lost across reopen: {fox:?}");
    assert_eq!(fox.len(), 27, "every fox row survived: {}", fox.len());
}

/// Crossing the merge threshold must not change any answer.
#[test]
fn merging_the_delta_changes_no_answer() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    seed(&mut db, 0..10);
    db.execute("CREATE INDEX ON d USING bm25 (body)").unwrap();

    // DELTA_MERGE_DOCS is 4096, so this crosses the boundary and folds mid-run.
    seed(&mut db, 10..4200);

    let fox = hits(&db, "fox");
    let expected = (0..4200).filter(|i| i % 3 == 0).count();
    assert_eq!(fox.len(), expected, "documents lost across an automatic merge");
    assert!(fox.contains(&"d4197".to_string()), "post-merge write is searchable");
    assert!(fox.contains(&"d0".to_string()), "pre-merge document survived the merge");
}

/// Ranking must still be ranking: a document repeating the term should outrank one
/// that mentions it once, regardless of which segment each lives in.
#[test]
fn scores_still_rank_across_segments() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    seed(&mut db, 0..100);
    db.execute("CREATE INDEX ON d USING bm25 (body)").unwrap();
    db.execute("INSERT INTO d (_key, body) VALUES ('strong', 'otter otter otter otter')").unwrap();
    db.execute("INSERT INTO d (_key, body) VALUES ('weak', 'otter among many other unrelated words here')").unwrap();

    let ranked: Vec<String> = db
        .query("SELECT _key FROM d WHERE BM25(body,'otter') > 0 ORDER BY BM25(body,'otter') DESC")
        .unwrap()
        .collect()
        .iter()
        .map(|h| h.slug.trim_start_matches("d/").to_string())
        .collect();
    assert_eq!(ranked, vec!["strong".to_string(), "weak".to_string()]);
}

// ── positional SEARCH ────────────────────────────────────────────────────────
//
// The search index has the same problem for a different reason: its term
// dictionary is an FST, which is genuinely immutable, so a new document cannot be
// threaded into it and the whole collection was rebuilt per write. It gets the same
// treatment — a second, small segment holding recent documents — and needs the same
// guarantees.

fn found(db: &CoreDB, query: &str) -> Vec<String> {
    let mut v: Vec<String> = db
        .query(&format!("SELECT _key FROM d WHERE SEARCH('{query}')"))
        .unwrap()
        .collect()
        .iter()
        .map(|h| h.slug.trim_start_matches("d/").to_string())
        .collect();
    v.sort();
    v
}

#[test]
fn incremental_search_inserts_match_a_full_rebuild() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();

    let mut a = CoreDB::open(dir_a.path()).unwrap();
    a.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    seed(&mut a, 0..20);
    a.execute("CREATE INDEX ON d USING search (body)").unwrap();
    seed(&mut a, 20..220);

    let mut b = CoreDB::open(dir_b.path()).unwrap();
    b.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    seed(&mut b, 0..220);
    b.execute("CREATE INDEX ON d USING search (body)").unwrap();

    for q in ["fox", "heron", "quick brown", "lazy riverbank"] {
        assert_eq!(found(&a, q), found(&b, q), "query {q:?} differs from a rebuild");
        assert!(!found(&b, q).is_empty(), "query {q:?} should match something");
    }
}

#[test]
fn a_deleted_row_leaves_the_search_index() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    seed(&mut db, 0..50);
    db.execute("CREATE INDEX ON d USING search (body)").unwrap();
    seed(&mut db, 50..60);

    assert!(found(&db, "fox").contains(&"d51".to_string()));
    db.execute("DELETE FROM d WHERE _key = 'd51'").unwrap();   // delta-resident
    db.execute("DELETE FROM d WHERE _key = 'd0'").unwrap();    // base-resident

    let fox = found(&db, "fox");
    assert!(!fox.contains(&"d51".to_string()), "deleted delta row still matches");
    assert!(!fox.contains(&"d0".to_string()), "deleted base row still matches");
}

#[test]
fn an_update_replaces_the_searched_document() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    seed(&mut db, 0..50);
    db.execute("CREATE INDEX ON d USING search (body)").unwrap();

    assert!(found(&db, "fox").contains(&"d0".to_string()));
    db.execute("UPDATE d SET body = 'a solitary albatross' WHERE _key = 'd0'").unwrap();

    assert!(!found(&db, "fox").contains(&"d0".to_string()), "stale term still matches");
    assert_eq!(found(&db, "albatross"), vec!["d0".to_string()]);
}

#[test]
fn search_delta_documents_survive_a_reopen() {
    let dir = tempfile::TempDir::new().unwrap();
    {
        let mut db = CoreDB::open(dir.path()).unwrap();
        db.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
        seed(&mut db, 0..50);
        db.execute("CREATE INDEX ON d USING search (body)").unwrap();
        seed(&mut db, 50..80);
        db.compact().unwrap();
    }
    let db = CoreDB::open_paged(dir.path()).unwrap();
    let fox = found(&db, "fox");
    assert!(fox.contains(&"d51".to_string()), "delta row lost across reopen: {fox:?}");
    assert_eq!(fox.len(), 27, "every fox row survived: {}", fox.len());
}

#[test]
fn merging_the_search_delta_changes_no_answer() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE d (_key TEXT PRIMARY KEY, body TEXT)").unwrap();
    seed(&mut db, 0..10);
    db.execute("CREATE INDEX ON d USING search (body)").unwrap();

    // SEARCH_DELTA_MERGE_DOCS is 256, so this folds several times along the way.
    seed(&mut db, 10..900);

    let fox = found(&db, "fox");
    assert_eq!(fox.len(), (0..900).filter(|i| i % 3 == 0).count(),
               "documents lost across an automatic merge");
    assert!(fox.contains(&"d897".to_string()), "post-merge write is searchable");
    assert!(fox.contains(&"d0".to_string()), "pre-merge document survived");
}
