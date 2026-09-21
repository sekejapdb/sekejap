//! A paged spatial grid must answer exactly what the packed one answers.
//!
//! `spatialgrid.bin` cannot absorb a point, so every compaction rewrites it —
//! measured at O(N^1.95) over a load. The paged grid removes that by putting
//! cells and metadata in B+tree records, which compaction *applies* the overlay
//! to rather than rebuilding.
//!
//! That is only worth having if the answers are identical, including after a
//! compaction and after a reopen, and including for rows that moved or lost
//! their geometry — the cases a fold gets wrong by forgetting its gate.

use sekejap::{Config, CoreDB};
use serde_json::json;

const N: usize = 4_000;

fn cfg(paged_spatial: bool) -> Config {
    Config { paged_spatial, ..Config::default() }
}

fn row(i: usize) -> String {
    json!({
        "_collection": "p", "_key": format!("n{i}"), "n": i as i64,
        // Unique per row on purpose. Repeating coordinates put rows at exactly
        // equal distances, and which of a tied pair lands in a top-k is arbitrary
        // — not a property either layout owes the other. Ties would make this
        // test fail on a difference that is not a difference.
        "geometry": {"type": "Point",
                     "coordinates": [115.0 + i as f64 * 0.0005,
                                     -8.8 - i as f64 * 0.0003]},
    })
    .to_string()
}

/// Every spatial query this store offers, as one comparable string.
fn probe(db: &mut CoreDB) -> String {
    let mut out = Vec::new();
    for sql in [
        "SELECT _key FROM p WHERE ST_DWithin(geometry, POINT(115.10 -8.85), 3000) ORDER BY _key LIMIT 20",
        "SELECT _key FROM p WHERE ST_DWithin(geometry, POINT(115.60 -9.10), 40000) ORDER BY _key LIMIT 20",
        "SELECT _key FROM p ORDER BY ST_DISTANCE(geometry, POINT(115.5 -9.2)) ASC LIMIT 10",
    ] {
        let rows: Vec<String> = db.query(sql).unwrap().collect()
            .iter().map(|r| format!("{r:?}")).collect();
        out.push(format!("{}=>{}", sql.len(), rows.join(",")));
    }
    out.join(" | ")
}

fn build(dir: &std::path::Path, paged: bool) -> String {
    let mut db = CoreDB::open_with_config(dir, cfg(paged)).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
    let items: Vec<(String, String)> = (0..N).map(|i| (format!("p/n{i}"), row(i))).collect();
    db.put_many(items.iter().map(|(s, j)| (s.as_str(), j.as_str()))).unwrap();

    // A row that MOVES, and a row that LOSES its geometry entirely. The second is
    // the case a fold gets wrong by gating on the grid's own keys: it never
    // reaches `insert`, so the base keeps reporting a location it no longer has.
    db.put("p/n7", &json!({"_collection":"p","_key":"n7","n":7,
        "geometry":{"type":"Point","coordinates":[120.0, -5.0]}}).to_string()).unwrap();
    db.put("p/n9", &json!({"_collection":"p","_key":"n9","n":9}).to_string()).unwrap();
    db.remove("p/n11");

    db.compact().unwrap();
    let after_compact = probe(&mut db);
    drop(db);

    let mut re = CoreDB::open_with_config(dir, cfg(paged)).unwrap();
    let after_reopen = probe(&mut re);
    assert_eq!(
        after_compact, after_reopen,
        "paged={paged}: a reopen changed the answers"
    );
    after_reopen
}

#[test]
fn the_paged_grid_answers_exactly_what_the_packed_one_answers() {
    let packed_dir = tempfile::TempDir::new().unwrap();
    let paged_dir = tempfile::TempDir::new().unwrap();
    let packed = build(packed_dir.path(), false);
    let paged = build(paged_dir.path(), true);
    assert!(!packed.is_empty(), "the probe matched nothing — it proves nothing");
    assert_eq!(paged, packed, "the paged grid disagrees with the packed grid");
}
