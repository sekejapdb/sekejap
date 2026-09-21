//! Writing rows must not rewrite a field index's base sidecar.
//!
//! Compaction rewrote every field sidecar every time, streaming the whole base
//! into a new file. For a column nothing touched, that is O(store) work to
//! produce a byte-identical file — Law 2 broken in the plainest way: the trigger
//! is the change, the work is the database.
//!
//! It costs nothing on a load where every indexed column is written, and
//! everything on a store with several indexed columns where a batch touches one.
//!
//! The skip is per COLLECTION, not per column: a `put` re-indexes the whole row,
//! so every indexed column of a written collection is dirty even if its value
//! did not change. An earlier version of this test asserted the narrower thing
//! and failed, correctly.
//!
//! This once also asserted that the WRITTEN collection's base survives, because
//! its delta went to a segment beside it. Segmentation was reverted for being
//! measurably slower, so that base is rewritten again and the assertion below
//! says so.

use sekejap::CoreDB;
use serde_json::json;

fn mtime(p: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(p).ok()?.modified().ok()
}

fn sidecars(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut v: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with("fieldidx_"))
                .unwrap_or(false)
        })
        .collect();
    v.sort();
    v
}

#[test]
fn writing_one_collection_leaves_every_base_sidecar_alone() {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, a INTEGER)").unwrap();
    db.execute("CREATE TABLE q (_key TEXT PRIMARY KEY, a INTEGER)").unwrap();
    db.execute("CREATE INDEX ON p USING btree (a)").unwrap();
    db.execute("CREATE INDEX ON q USING btree (a)").unwrap();

    for (coll, n) in [("p", 20_000usize), ("q", 20_000usize)] {
        let rows: Vec<(String, serde_json::Value)> = (0..n)
            .map(|i| (format!("{coll}/n{i}"),
                      json!({"_collection": coll, "_key": format!("n{i}"), "a": i as i64})))
            .collect();
        db.put_value_bulk(rows).unwrap();
    }
    db.compact().unwrap();

    let before = sidecars(dir.path());
    assert_eq!(before.len(), 2, "expected one sidecar per indexed collection");
    let stamps: Vec<_> = before.iter().map(|p| (p.clone(), mtime(p))).collect();

    // Past filesystem mtime granularity, then write to `p` only.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    for i in 0..200 {
        db.put(&format!("p/n{i}"),
               &json!({"_collection":"p","_key":format!("n{i}"),
                       "a": (i + 1_000_000) as i64}).to_string()).unwrap();
    }
    db.compact().unwrap();

    // The written collection's base IS rewritten -- the delta is folded into it.
    // Only the untouched collection's base must survive.
    let rewritten: Vec<_> = stamps
        .iter()
        .filter(|(p, t)| mtime(p) != *t)
        .map(|(p, _)| p.clone())
        .collect();
    assert_eq!(
        rewritten.len(),
        1,
        "exactly one base -- the written collection's -- should be rewritten, got {:?}",
        rewritten
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect::<Vec<_>>()
    );
    let p_side = rewritten[0].clone();

    // Segmentation is reverted, so nothing may appear beside a base.
    let segs: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("fieldidx_") && n.contains(".s"))
        .collect();
    assert!(segs.is_empty(), "no segment files should be written, got {segs:?}");

    // Which base was spared is the whole claim, and a filename does not say which
    // collection it belongs to. So write the OTHER collection and require the
    // other file to move: a skip that always spared the same sidecar, or one that
    // spared neither, cannot pass both halves.
    let stamps2: Vec<_> = sidecars(dir.path())
        .iter()
        .map(|p| (p.clone(), mtime(p)))
        .collect();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    for i in 0..200 {
        db.put(
            &format!("q/n{i}"),
            &json!({"_collection":"q","_key":format!("n{i}"),
                    "a": (i + 2_000_000) as i64})
            .to_string(),
        )
        .unwrap();
    }
    db.compact().unwrap();
    let rewritten2: Vec<_> = stamps2
        .iter()
        .filter(|(p, t)| mtime(p) != *t)
        .map(|(p, _)| p.clone())
        .collect();
    assert_eq!(rewritten2.len(), 1, "writing q should rewrite exactly one base");
    assert_ne!(
        rewritten2[0], p_side,
        "writing q rewrote the base that writing p rewrote -- the skip is not \
         tracking which collection was written"
    );

    // A skip that lost the index would also pass every "was not rewritten" check.
    let untouched = db.query("SELECT _key FROM p WHERE a = 1000007").unwrap().collect();
    assert_eq!(untouched.len(), 1, "the collection skipped this round stopped answering");
    let moved = db.query("SELECT _key FROM q WHERE a = 2000007").unwrap().collect();
    assert_eq!(moved.len(), 1, "the written collection did not pick up the update");
}
