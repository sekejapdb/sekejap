//! Gate arm (d): commit is the graph's boundary, demonstrated.
//! Committed nodes+edges+extkeys+allocator all survive a crash before any
//! checkpoint (rebuilt from the log). And a reopened allocator never reuses an
//! id -- the graph-level corruption that would silently merge two nodes.

use kernel::graph::Graph;
use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};

fn cfg() -> Config {
    Config { budget_bytes: 4 << 20, io: IoMode::Buffered, sync: SyncMode::Off }
}

#[test]
fn committed_graph_survives_a_crash_intact() {
    let d = tempfile::TempDir::new().unwrap();
    let n = 3_000u64;
    {
        let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
        for i in 0..n {
            let ext = format!("u-{i}").into_bytes();
            let id = g.add_node(Some(&ext), 7, b"payload").unwrap();
            if id > 1 { g.add_edge(0, id - 1, 1, id, b"e").unwrap(); }
        }
        g.commit().unwrap();
        // Graph/Store have no Drop flush; dropping closes the descriptor as
        // a real crash does while leaving the committed WAL uncheckpointed.
        drop(g);
    }
    let mut g = Graph::new(Store::open(d.path(), cfg()).unwrap()).unwrap();
    for i in 0..n {
        let ext = format!("u-{i}").into_bytes();
        let id = g.resolve(&ext).unwrap().unwrap_or_else(|| panic!("ext u-{i} lost"));
        assert!(g.get_node(id).unwrap().is_some(), "node {id} lost");
    }
    let chain = g.bfs(0, 1, Some(1), n as usize).unwrap();
    assert_eq!(chain.len() as u64, n - 1, "committed edge chain broken after crash");
    let fresh = g.add_node(None, 7, b"x").unwrap();
    assert_eq!(fresh, n + 1, "allocator reused an id after crash recovery");
}
