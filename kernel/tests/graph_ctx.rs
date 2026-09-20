//! The ctx contract, held to code, for the KERNEL key-value graph
//! (`kernel::graph::Graph`) -- a separate layer from the typed-collections
//! graph in `src/index/graph/mod.rs`, whose element identity is decided by
//! `docs/GRAPH_CONTRACT.md` §2.3 instead.
//!
//! Two lecturers assert overlapping-but-different KGs over SHARED nodes.
//! Each perspective must see exactly its own edges; re-assertion within a ctx
//! overwrites (set semantics); a perspective's whole KG is one range; and the
//! base graph (ctx=0) is untouched by any of it.

use kernel::graph::Graph;
use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};
use std::collections::BTreeSet;

fn cfg() -> Config {
    Config { budget_bytes: 4 << 20, io: IoMode::Buffered, sync: SyncMode::Off }
}

#[test]
fn perspectives_share_nodes_but_never_edges() {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = Graph::new(Store::create(d.path(), cfg()).unwrap()).unwrap();
    let (lect55, lect67) = (55u64, 67u64);
    let part_of = 1u64;

    // shared nodes: ml=1, data=2, stats=3, base-only=4
    for name in ["ml", "data", "stats", "base"] {
        g.add_node(Some(name.as_bytes()), 7, name.as_bytes()).unwrap();
    }
    let (ml, data, stats) = (1u64, 2, 3);

    // lect55: ml->data. lect67: ml->data AND ml->stats, different props.
    g.add_edge(lect55, ml, part_of, data, b"w=0.9").unwrap();
    g.add_edge(lect67, ml, part_of, data, b"w=0.4").unwrap();
    g.add_edge(lect67, ml, part_of, stats, b"w=0.7").unwrap();
    // base graph has its own edge, in neither perspective
    g.add_edge(0, ml, part_of, 4, b"base").unwrap();
    g.commit().unwrap();

    let hop = |g: &Graph, ctx: u64| -> BTreeSet<(u64, Vec<u8>)> {
        g.out_edges(ctx, ml, None).unwrap()
            .map(|r| { let (_, _, dst, p) = r.unwrap(); (dst, p) }).collect()
    };

    // isolation: each ctx sees exactly its own edges and props
    assert_eq!(hop(&g, lect55), [(data, b"w=0.9".to_vec())].into());
    assert_eq!(hop(&g, lect67),
               [(data, b"w=0.4".to_vec()), (stats, b"w=0.7".to_vec())].into());
    assert_eq!(hop(&g, 0), [(4, b"base".to_vec())].into());
    assert!(hop(&g, 99).is_empty(), "an unused perspective must be empty");

    // set semantics: re-asserting within a ctx overwrites, never duplicates
    g.add_edge(lect55, ml, part_of, data, b"w=1.0").unwrap();
    g.commit().unwrap();
    assert_eq!(hop(&g, lect55), [(data, b"w=1.0".to_vec())].into());
    // ...and does not leak into the other perspective
    assert_eq!(hop(&g, lect67),
               [(data, b"w=0.4".to_vec()), (stats, b"w=0.7".to_vec())].into());

    // the whole perspective is one range: exactly lect67's 2 edges, nothing else
    let kg: Vec<(u64, u64, u64)> = g.perspective(lect67).unwrap()
        .map(|r| { let (s, t, dd, _) = r.unwrap(); (s, t, dd) }).collect();
    assert_eq!(kg, vec![(ml, part_of, data), (ml, part_of, stats)]);

    // reverse hops are ctx-scoped too
    let into_data: BTreeSet<u64> = g.in_edges(lect67, data, None).unwrap()
        .map(|r| r.unwrap().0).collect();
    assert_eq!(into_data, [ml].into());
    assert!(g.in_edges(99, data, None).unwrap().next().is_none());

    // traversal is ctx-scoped: lect67 reaches stats, lect55 never does
    assert!(g.bfs(lect67, ml, None, 2).unwrap().contains(&stats));
    assert!(!g.bfs(lect55, ml, None, 2).unwrap().contains(&stats));

    // nodes are SHARED: one record, whoever asks
    assert_eq!(g.get_node(ml).unwrap().unwrap().1, b"ml");
}
