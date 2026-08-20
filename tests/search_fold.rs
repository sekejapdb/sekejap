//! Folding a search index that has nothing pending.
//!
//! `write_binary` rebuilt the sorted `(hash, slot)` reverse index every time it
//! persisted, from a `Vec<(u64, u32)>` — 16 bytes an element after alignment,
//! one element per document, then sorted. So a compaction cost 15.2 MB of heap
//! at a million rows *whether or not any indexed row had changed*: the trigger
//! was empty and the work was the whole store.
//!
//! When the index is already served from a mapping that section exists there,
//! sorted, in the same layout, and is copied through.
//!
//! What that risks is silence rather than an error: a reverse index copied
//! wrongly makes `hash_to_slot` miss, and a miss is a document that simply stops
//! being found. So these assert through `SEARCH`, and reopen — the copied bytes
//! are only actually read back after a reopen.

use sekejap::CoreDB;

const N: usize = 400;

fn rows(db: &mut CoreDB, range: std::ops::Range<usize>) {
    for i in range {
        db.put(
            &format!("t/n{i}"),
            &format!(
                r#"{{"_collection":"t","_key":"n{i}","body":"riverbank heron number{i}"}}"#
            ),
        )
        .unwrap();
    }
}

fn hits(db: &CoreDB, q: &str) -> usize {
    db.query(q).unwrap_or_else(|e| panic!("`{q}` did not run: {e:?}")).collect().len()
}

const PROBE: &str = "SELECT _key FROM t WHERE SEARCH('riverbank')";

#[test]
fn a_fold_with_nothing_pending_keeps_every_document_findable() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE t (body TEXT)").unwrap();
    db.execute("CREATE INDEX ON t USING search (body)").unwrap();
    rows(&mut db, 0..N);

    // First fold: builds the sidecar and attaches it, so the index is now served
    // from the mapping.
    db.compact().unwrap();
    assert_eq!(hits(&db, PROBE), N, "after the first fold");

    // Second fold with nothing written in between — the path that copies the
    // reverse index through instead of rebuilding it.
    db.compact().unwrap();
    assert_eq!(hits(&db, PROBE), N, "after a fold with nothing pending");

    // The reopen is what actually reads those bytes back.
    drop(db);
    let db = CoreDB::open(dir.path()).unwrap();
    assert_eq!(
        hits(&db, PROBE),
        N,
        "the copied reverse index does not resolve after a reopen"
    );
}

#[test]
fn documents_written_between_folds_stay_findable() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE t (body TEXT)").unwrap();
    db.execute("CREATE INDEX ON t USING search (body)").unwrap();
    rows(&mut db, 0..N);
    db.compact().unwrap();

    // Written against a mapped index, then folded — the delta path, which still
    // rebuilds. Kept so that making that path incremental cannot quietly lose
    // documents.
    rows(&mut db, N..N + 100);
    db.compact().unwrap();
    assert_eq!(hits(&db, PROBE), N + 100, "after folding a delta");

    drop(db);
    let db = CoreDB::open(dir.path()).unwrap();
    assert_eq!(hits(&db, PROBE), N + 100, "after reopen");
}
