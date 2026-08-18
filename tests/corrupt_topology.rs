//! A damaged durable file must not kill the process.
//!
//! The topology segments — `idx.bin`, `dict.bin`, `nodes.bin`, the adjacency and
//! collection blobs — are memory-mapped and decoded directly. Every length,
//! offset and count in them comes *from the file*, and they were believed:
//!
//! * `rd_u32`/`rd_u64`, the primitives under all 47 decode sites, indexed the
//!   mapped bytes with an offset the file supplied
//! * `read_string_table` sized a `Vec` from a file count, then read each entry
//!   without checking it fitted
//! * `read_varint` indexed past the end on a truncated varint, and shifted past
//!   64 on a corrupt one
//! * `svb_decode` trusted a varint count to bound two slice walks
//! * `MappedTopology::open` reserved `count / STRIDE` entries for its sparse
//!   index straight from a `u64` in the file
//!
//! One flipped byte was enough. Four of the first forty-two single-byte
//! corruptions aborted the process, one of them with
//! `memory allocation of 574208952489738240 bytes failed` — half an exabyte, at
//! open, so there was no query to fail and nothing to report.
//!
//! **What this does not claim.** Topology segments carry no per-block checksum,
//! unlike node, adjacency and payload records. A corrupted byte landing inside a
//! valid extent is still decoded and served, so this asserts only that the
//! process survives and stays usable — not that the answer is right. Closing that
//! properly is a durable-format change (a CRC per block) and is recorded as open
//! in `.workbench/STABLE.md`.

use sekejap::{Config, CoreDB};
use serde_json::json;

/// A small graph with nodes, edges, geometry and a compaction, so every topology
/// file exists and has content.
fn build(dir: &std::path::Path) {
    let mut db = CoreDB::open_with_config(dir, Config::default()).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER, body TEXT)").unwrap();
    for i in 0..40 {
        db.put(&format!("p/n{i}"), &json!({
            "_collection": "p", "_key": format!("n{i}"), "n": i as i64,
            "body": format!("row {i} riverbank"),
            "geometry": {"type": "Point", "coordinates": [144.96 + i as f64 * 0.001, -37.81]},
        }).to_string()).unwrap();
    }
    for i in 0..39 {
        db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next");
    }
    db.build_spatial_index();
    db.compact().unwrap();
}

/// Everything a reader might ask of a database whose files are damaged.
fn interrogate(db: &CoreDB) -> usize {
    let mut n = 0;
    n += db.query("SELECT _key FROM p").map(|s| s.collect().len()).unwrap_or(0);
    n += db.query("SELECT _key FROM p WHERE n > 10").map(|s| s.collect().len()).unwrap_or(0);
    n += db.query("SELECT _key FROM p ORDER BY n DESC LIMIT 5").map(|s| s.collect().len()).unwrap_or(0);
    n += db.query("SELECT COUNT(*) FROM p").map(|s| s.collect().len()).unwrap_or(0);
    n += db.query("SELECT b._key FROM MATCH (a:p)-[:next]->(b:p)").map(|s| s.collect().len()).unwrap_or(0);
    n += db.query("SELECT _key FROM p WHERE body ILIKE '%riverbank%'").map(|s| s.collect().len()).unwrap_or(0);
    n += db.query("SELECT _key FROM p WHERE ST_DWithin(geometry, POINT(144.96 -37.81), 5000)")
            .map(|s| s.collect().len()).unwrap_or(0);
    n += db.one("p/n0").forward("next").collect().len();
    n += db.edges_from("p/n5").len();
    n += db.edge_count() + db.node_count();
    n += db.all_slugs().len();
    n += db.collection_names().len();
    n
}

/// **No single-byte corruption of a topology file may abort the process.**
///
/// A test in the same binary cannot survive an abort, so a failure here shows up
/// as the whole binary dying rather than as an assertion — which is exactly the
/// severity of the bug: uncatchable, unreportable.
#[test]
fn a_flipped_byte_in_a_topology_file_never_aborts() {
    const FILES: &[&str] = &[
        "idx.bin", "dict.bin", "nodes.bin", "slugs.bin", "collections.bin",
        "adj_fwd.bin", "adj_rev.bin", "edge_types.bin", "spatial.bin",
        "coll_names.bin", "edgemeta.bin",
    ];

    let mut damaged = 0;
    let mut opened = 0;
    for file in FILES {
        for &pos in &[3usize, 17, 41, 71, 97, 149, 211, 307, 457, 613, 733, 971] {
            let dir = tempfile::TempDir::new().unwrap();
            build(dir.path());
            let path = dir.path().join(file);
            let Ok(mut bytes) = std::fs::read(&path) else { continue };
            if bytes.is_empty() { continue }
            let at = pos % bytes.len();
            bytes[at] ^= 0xFF;
            std::fs::write(&path, bytes).unwrap();
            damaged += 1;

            // Refusing to open a file it cannot trust is a fine answer. Dying is
            // not, and neither is answering after a half-exabyte allocation.
            if let Ok(db) = CoreDB::open_with_config(dir.path(), Config::default()) {
                let _ = interrogate(&db);
                opened += 1;
            }
        }
    }

    assert!(damaged >= 100,
        "only {damaged} corruptions were actually applied — the fixture is not \
         producing the files this is meant to damage");
    assert!(opened >= damaged / 2,
        "only {opened} of {damaged} damaged databases opened at all; if nearly \
         everything refuses to open, this test is not reaching the decode paths \
         it exists to exercise");
}
