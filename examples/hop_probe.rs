//! One-hop traversal, CSR base against paged base, and nothing else.
//!
//!     cargo run --release --example hop_probe
//!
//! `bench_light` measures this too, but it measures fifteen other things first.
//! This exists to be run twice in a row across a single change to the read path.

use sekejap::{Config, CoreDB, SyncMode};
use serde_json::json;
use std::time::Instant;

/// Which paged structures are on, so the cost can be attributed to one of them
/// rather than to "paged mode".
#[derive(Clone, Copy)]
struct Mode { name: &'static str, adj: bool, nodes: bool, payloads: bool }

const MODES: [Mode; 4] = [
    Mode { name: "csr",       adj: false, nodes: false, payloads: false },
    Mode { name: "adj",       adj: true,  nodes: false, payloads: false },
    Mode { name: "adj+nodes", adj: true,  nodes: true,  payloads: false },
    Mode { name: "all",       adj: true,  nodes: true,  payloads: true  },
];

fn open(dir: &std::path::Path, m: Mode) -> CoreDB {
    CoreDB::open_with_config(dir, Config {
        paged_topology: true,
        paged_adjacency: m.adj,
        paged_payloads: m.payloads,
        paged_nodes: m.nodes,
        ..Config::default()
    }).unwrap()
}

fn main() {
    for n in [200_000usize] {
        for m in MODES {
            let dir = tempfile::TempDir::new().unwrap();
            {
                let mut db = open(dir.path(), m);
                db.set_wal_sync(SyncMode::Off);
                db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, n INTEGER)").unwrap();
                for i in 0..n {
                    db.put(&format!("p/n{i}"),
                           &json!({"_collection":"p","_key":format!("n{i}"),"n":i as i64}).to_string())
                      .unwrap();
                }
                for i in 0..n - 1 { db.link(&format!("p/n{i}"), &format!("p/n{}", i + 1), "next"); }
                db.compact().unwrap();
            }
            let db = open(dir.path(), m);
            // warm, then measure
            for i in 0..2000 {
                std::hint::black_box(db.one(&format!("p/n{}", i % (n - 1))).forward("next").collect().len());
            }
            let t = Instant::now();
            for i in 0..20_000 {
                std::hint::black_box(db.one(&format!("p/n{}", i % (n - 1))).forward("next").collect().len());
            }
            let hop = t.elapsed().as_secs_f64() * 1e6 / 20_000.0;
            let t = Instant::now();
            for i in 0..20_000 { std::hint::black_box(db.get(&format!("p/n{}", i % n))); }
            let get = t.elapsed().as_secs_f64() * 1e6 / 20_000.0;
            println!("{n:>7} nodes  {:<10}  {hop:6.2} us/hop   {get:6.2} us/get", m.name);
        }
    }
}
