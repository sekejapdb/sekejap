//! Folding a spatial change into the existing grid, rather than rebuilding it.
//!
//! Compaction used to rebuild the whole grid from every node in the store to
//! write `spatialgrid.bin` — 126.8 MB of heap for a thousand-row change at half
//! a million rows. It now merges the mapped base with the resident overlay, so
//! the cost follows the change.
//!
//! Which moves the risk from memory to correctness, and to a silent kind: a
//! merge that keeps a stale base entry answers with a location a row no longer
//! has, and one that drops a live entry answers with nothing. Neither errors.
//!
//! The oracle is a second database holding the *same final rows*, built once and
//! folded once. An incremental fold and a from-scratch build have to produce an
//! index that answers identically — that is the whole claim being made.

use sekejap::CoreDB;

const N: usize = 1_000;

fn point(lon: f64, lat: f64) -> String {
    format!(r#"{{"type":"Point","coordinates":[{lon},{lat}]}}"#)
}

/// Original position for row `i` — a band of points around Bali.
fn home(i: usize) -> (f64, f64) {
    (115.0 + (i % 50) as f64 * 0.01, -8.8 - (i / 50) as f64 * 0.01)
}

/// Where the moved rows end up: far enough away that no query can confuse the
/// two, so a stale base entry shows up as a wrong answer rather than a near one.
fn away(i: usize) -> (f64, f64) {
    (144.9 + (i % 20) as f64 * 0.01, -37.8 - (i / 20) as f64 * 0.01)
}

fn put_geo(db: &mut CoreDB, i: usize, lon: f64, lat: f64) {
    db.put(
        &format!("p/n{i}"),
        &format!(
            r#"{{"_collection":"p","_key":"n{i}","n":{i},"geometry":{}}}"#,
            point(lon, lat)
        ),
    )
    .unwrap();
}

/// Same row, no geometry at all. The row still exists; only its location is gone.
fn put_flat(db: &mut CoreDB, i: usize) {
    db.put(
        &format!("p/n{i}"),
        &format!(r#"{{"_collection":"p","_key":"n{i}","n":{i}}}"#),
    )
    .unwrap();
}

fn keys(db: &CoreDB, sql: &str) -> Vec<String> {
    let mut out: Vec<String> = db
        .query(sql)
        .unwrap_or_else(|e| panic!("`{sql}` did not run: {e:?}"))
        .collect()
        .iter()
        .filter_map(|h| {
            h.payload
                .as_ref()
                .and_then(|p| p.get("_key"))
                .and_then(|k| k.as_str())
                .map(|s| s.to_string())
        })
        .collect();
    out.sort();
    out
}

/// Every probe both databases must agree on. Two clusters and a miss, so a stale
/// entry, a dropped entry and a phantom all show up somewhere.
const PROBES: &[&str] = &[
    "SELECT * FROM p WHERE ST_DWithin(geometry, POINT(115.10 -8.85), 8000)",
    "SELECT * FROM p WHERE ST_DWithin(geometry, POINT(115.25 -8.95), 4000)",
    "SELECT * FROM p WHERE ST_DWithin(geometry, POINT(144.95 -37.85), 8000)",
    "SELECT * FROM p WHERE ST_DWithin(geometry, POINT(0.0 0.0), 50000)",
];

/// The final state both databases must end up holding.
fn write_final_state(db: &mut CoreDB) {
    for i in 0..N {
        match i {
            _ if i < 100 => { let (x, y) = away(i); put_geo(db, i, x, y); }
            _ if i < 200 => {}                       // deleted
            _ if i < 300 => put_flat(db, i),         // geometry dropped
            _ => { let (x, y) = home(i); put_geo(db, i, x, y); }
        }
    }
    for i in N..N + 100 {
        let (x, y) = home(i);
        put_geo(db, i, x, y);
    }
}

#[test]
fn an_incrementally_folded_grid_answers_like_one_built_from_scratch() {
    // ── A: built, folded, mutated, folded, mutated, folded ───────────────────
    let dir_a = tempfile::TempDir::new().unwrap();
    let mut a = CoreDB::open(dir_a.path()).unwrap();
    for i in 0..N {
        let (x, y) = home(i);
        put_geo(&mut a, i, x, y);
    }
    a.compact().unwrap();

    // Round one: move some, delete some.
    for i in 0..100 {
        let (x, y) = away(i);
        put_geo(&mut a, i, x, y);
    }
    for i in 100..200 {
        a.remove(&format!("p/n{i}"));
    }
    a.compact().unwrap();

    // Round two, so the merge has to consume a base that was itself merged —
    // and so the geometry-dropping case lands against a base that holds a
    // location for those rows.
    for i in 200..300 {
        put_flat(&mut a, i);
    }
    for i in N..N + 100 {
        let (x, y) = home(i);
        put_geo(&mut a, i, x, y);
    }
    a.compact().unwrap();

    // ── B: the same final rows, built once ───────────────────────────────────
    let dir_b = tempfile::TempDir::new().unwrap();
    let mut b = CoreDB::open(dir_b.path()).unwrap();
    write_final_state(&mut b);
    b.compact().unwrap();

    for q in PROBES {
        let ka = keys(&a, q);
        let kb = keys(&b, q);
        assert_eq!(
            ka, kb,
            "merged grid and rebuilt grid disagree\n  query: {q}\n  merged: {} rows\n  rebuilt: {} rows",
            ka.len(),
            kb.len()
        );
    }

    // And the same again after a reopen, which is when the file written by the
    // merge is actually read back rather than served from the process that
    // wrote it.
    drop(a);
    drop(b);
    let a = CoreDB::open(dir_a.path()).unwrap();
    let b = CoreDB::open(dir_b.path()).unwrap();
    for q in PROBES {
        assert_eq!(keys(&a, q), keys(&b, q), "after reopen: {q}");
    }
}

#[test]
fn a_row_that_loses_its_geometry_stops_matching() {
    // The case the merge gate exists for. Dropping a geometry never reaches
    // `SpatialGrid::insert` — there is nothing to insert — so gating on the
    // grid's own overlay would leave the base reporting the old location for a
    // row that no longer has one.
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    for i in 0..200 {
        let (x, y) = home(i);
        put_geo(&mut db, i, x, y);
    }
    db.compact().unwrap();

    let q = "SELECT * FROM p WHERE ST_DWithin(geometry, POINT(115.10 -8.85), 8000)";
    let before = keys(&db, q);
    assert!(before.len() > 10, "the probe must match plenty to be worth anything");

    for i in 0..50 {
        put_flat(&mut db, i);
    }
    db.compact().unwrap();

    let after = keys(&db, q);
    for i in 0..50 {
        assert!(
            !after.contains(&format!("n{i}")),
            "n{i} lost its geometry but the grid still places it"
        );
    }
    assert!(!after.is_empty(), "the rows that kept their geometry must still match");
}
