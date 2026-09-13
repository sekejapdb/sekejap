//! Gate arm (a), FACT-01's falsifier: a hop's page reads follow the DEGREE,
//! not the store. 4x the nodes, same degree -> same reads per hop. Asserted on
//! the store's own counters, not time.
//!
//! The metric must discriminate: gathering the same 8 neighbours' RECORDS
//! (unrelated keys, 8 descents) must cost measurably more than one hop
//! (contiguous keys) on the identical graph -- else "flat" means the pool was
//! big enough to hide everything and the test proves nothing.

use kernel::graph::Graph;
use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};

const DEG: u64 = 8;

fn cfg() -> Config {
    // Far smaller than either store, so every hop that can miss, does.
    Config { budget_bytes: 4 << 20, io: IoMode::Buffered, sync: SyncMode::Off }
}

fn build(n: u64, dir: &std::path::Path) -> Graph {
    let mut g = Graph::new(Store::create(dir, cfg()).unwrap()).unwrap();
    for i in 0..n {
        let id = g.add_node(None, 7, &vec![b'x'; 100]).unwrap();
        assert_eq!(id, i + 1);
    }
    for src in 1..=n {
        for d in 0..DEG {
            // neighbours spread across the id space: a hop cannot be served by
            // the page the source record sits on.
            let dst = 1 + (src.wrapping_mul(2_654_435_761).wrapping_add(d * 7919)) % n;
            g.add_edge(0, src, 1, dst, b"").unwrap();
        }
    }
    g.commit().unwrap();
    g
}

fn reads_per(n: u64, gather_records: bool) -> f64 {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = build(n, d.path());
    let probes = 5_000u64.min(n);
    g.store().io_stats().unwrap().take();
    let mut seen = 0u64;
    for p in 0..probes {
        let src = 1 + (p.wrapping_mul(48_271)) % n;
        for e in g.out_edges(0, src, None).unwrap() {
            let (_, _, dst, _) = e.unwrap();
            if gather_records {
                assert!(g.get_node(dst).unwrap().is_some());
            }
            seen += 1;
        }
    }
    assert!(seen >= probes * DEG / 2, "degenerate fixture");
    let (_, _, reads) = g.store().io_stats().unwrap().take();
    reads as f64 / probes as f64
}

#[test]
fn a_hop_costs_the_degree_not_the_store() {
    let small = reads_per(50_000, false);
    let large = reads_per(200_000, false);
    eprintln!("reads/hop: 50k={small:.3} 200k={large:.3}");
    assert!(
        large < small * 1.5 + 0.3,
        "reads per hop {small:.3} -> {large:.3} across a 4x store: traversal \
         cost is following the graph size, adjacency is not contiguous"
    );
    let gather = reads_per(200_000, true);
    eprintln!("reads/op: hop={large:.3} scattered-gather={gather:.3}");
    assert!(
        gather > large * 2.0,
        "scattered gather {gather:.3} vs contiguous hop {large:.3}: the metric \
         cannot tell locality from its absence, so flatness above means nothing"
    );
}
