//! Gate arm (c): the id policy IS the clustering policy (GRAPH.md lever 1).
//!
//! Same logical graph -- 2,000 traces of 100 events, edges within each trace --
//! stored twice: ids assigned SEQUENTIALLY per trace, and ids HASHED (what e1
//! did by hashing slugs). Same query: 3-hop BFS from a trace root, which in RCA
//! is "unfold this incident". If sequential ids do not cut page reads
//! decisively, lever 1 is dead and dense ids lose their main justification.

use kernel::graph::Graph;
use kernel::io::IoMode;
use kernel::store::{Config, Store, SyncMode};

const TRACES: u64 = 2_000;
const SPAN: u64 = 100;

fn cfg() -> Config {
    Config { budget_bytes: 4 << 20, io: IoMode::Buffered, sync: SyncMode::Off }
}

/// Build the trace graph with an id-mapping policy applied to every endpoint.
fn build(map: impl Fn(u64) -> u64, dir: &std::path::Path) -> Graph {
    let mut g = Graph::new(Store::create(dir, cfg()).unwrap()).unwrap();
    let n = TRACES * SPAN;
    // Nodes inserted in MAPPED order via the raw store so both variants write
    // the same keyspace shapes; the graph id allocator is bypassed on purpose
    // (the experiment is about where ids LAND, not who assigns them).
    let mut ids: Vec<u64> = (0..n).map(&map).collect();
    ids.sort_unstable();
    for &id in &ids {
        g.store().put(&kernel::keys::node(id), &vec![b'x'; 100]).unwrap();
    }
    for t in 0..TRACES {
        for e in 0..SPAN {
            let raw = t * SPAN + e;
            // event -> next event, plus a fan edge back to the trace root
            if e + 1 < SPAN {
                g.add_edge(0, map(raw), 1, map(raw + 1), b"").unwrap();
            }
            if e > 0 {
                g.add_edge(0, map(t * SPAN), 2, map(raw), b"").unwrap();
            }
        }
    }
    g.commit().unwrap();
    g
}

fn reads_per_query(map: impl Fn(u64) -> u64 + Copy) -> f64 {
    let d = tempfile::TempDir::new().unwrap();
    let mut g = build(map, d.path());
    g.store().io_stats().unwrap().take();
    let queries = 300u64;
    for q in 0..queries {
        let t = (q * 6_700_417) % TRACES;
        let got = g.bfs(0, map(t * SPAN), None, 3).unwrap();
        assert!(got.len() >= SPAN as usize - 1, "trace not fully reached");
    }
    let (_, _, reads) = g.store().io_stats().unwrap().take();
    reads as f64 / queries as f64
}

#[test]
fn sequential_ids_beat_hashed_ids_on_trace_queries() {
    let seq = reads_per_query(|i| i + 1);
    let hashed = reads_per_query(|i| 1 + i.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 1);
    eprintln!("reads per 3-hop trace query: sequential-ids={seq:.1} hashed-ids={hashed:.1}");
    assert!(
        hashed > seq * 2.0,
        "hashed {hashed:.1} vs sequential {seq:.1}: id clustering bought less \
         than 2x, lever 1 is not doing what GRAPH.md claims"
    );
}
