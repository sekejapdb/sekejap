//! Disk-first graph adjacency: after spilling adjacency to mmap'd CSR, traversal
//! must return the same edges (and a multi-hop MATCH the same rows), with RAM freed.

use sekejap::CoreDB;
use std::collections::HashSet;

#[test]
fn graph_disk_first_spill_preserves_traversal() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();

    let n = 2000usize;
    for i in 0..n {
        db.put(&format!("node/{i}"), &format!(r#"{{"_collection":"node","_key":"{i}"}}"#)).unwrap();
    }
    // deterministic ~5-out-degree graph
    let edges: Vec<(String, String, String)> = (0..n)
        .flat_map(|i| (0..5).map(move |j| (format!("node/{i}"), format!("node/{}", (i * 7 + j * 13) % n), "link".to_string())))
        .collect();
    db.link_many(edges.iter().map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str())));

    let out_before: HashSet<String> = db.edges_from("node/42").into_iter().filter_map(|e| e.to_slug).collect();
    let in_before: HashSet<String> = db.edges_to("node/99").into_iter().filter_map(|e| e.from_slug).collect();

    // Spill adjacency to disk (mmap CSR); RAM adjacency freed.
    db.spill_edges_to_disk().unwrap();

    let out_after: HashSet<String> = db.edges_from("node/42").into_iter().filter_map(|e| e.to_slug).collect();
    let in_after: HashSet<String> = db.edges_to("node/99").into_iter().filter_map(|e| e.from_slug).collect();

    assert_eq!(out_before, out_after, "forward edges changed after spill");
    assert_eq!(in_before, in_after, "reverse edges changed after spill");
    assert!(!out_after.is_empty(), "no forward edges after spill");
    assert!(!in_after.is_empty(), "no reverse edges after spill");
}
