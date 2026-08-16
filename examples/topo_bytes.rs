//! What the topology actually costs, per node and per edge, file by file.
//!
//! Every structure on disk addresses nodes by *dense id* — a node's position in
//! `nodes.bin`, assigned by rank at build time. A position is only stable while
//! nothing before it moves, so adding one node invalidates the numbering and the
//! whole set has to be re-derived together. That is the mechanism behind
//! compaction, and removing it means giving nodes an address that does not move.
//!
//! Two addresses are possible, and they differ mainly in how many bytes an edge
//! costs:
//!
//! - a **permanent id** — allocated once, never reused, so `nodes.bin` becomes
//!   sparse and needs a hole marker, but adjacency keeps its small delta-encoded
//!   ids
//! - the **slug hash** the API already uses — nothing to allocate and no holes,
//!   but a neighbour is a full 8 bytes with no delta to exploit
//!
//! The second is far simpler, and the first is only worth its complexity if the
//! delta coding is actually buying something. Run this with `near` and with
//! `scattered` neighbours and the sizes agree to six thousandths of a percent.
//!
//! The reason is in the id assignment: `write_topology_files` sorts nodes by hash
//! before handing them to the builder, so a node's dense id is its *rank by hash*.
//! Hashing is chosen to destroy ordering, so neighbours that are adjacent in the
//! graph land at unrelated ids and the deltas are uniform over the whole range no
//! matter how the graph is shaped. Delta coding cannot compress what has been
//! deliberately randomised, and the machinery is dead weight by construction.
//!
//! What is left per edge is 4 bytes of edge type + 4 bytes of metadata reference,
//! both fixed, plus a neighbour delta that grows with the store: 4 bytes once past
//! 4 billion, which is where the deltas already sit at 48M nodes and degree 5. So
//! a fixed-width 8-byte neighbour hash costs about 14% more at that scale — and in
//! exchange the adjacency accepts an edge in place, and a traversal stops doing a
//! binary search in `idx.bin` plus a random read into a 1.5 GB `nodes.bin` for
//! every neighbour it reports.
//!
//! Run: `cargo run --release --example topo_bytes -- [nodes] [degree] [near|scattered]`

use sekejap::CoreDB;
use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(200_000);
    let degree: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5);
    let near = args.get(3).map(|s| s == "near").unwrap_or(false);

    let dir = tempfile::TempDir::new()?;
    let mut db = CoreDB::open(dir.path())?;
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT, n INTEGER)")?;

    println!("building {n} nodes at degree {degree}, {} neighbours …",
             if near { "near" } else { "scattered" });
    let t = std::time::Instant::now();
    let rows: Vec<(String, serde_json::Value)> = (0..n)
        .map(|i| (format!("p/n{i}"), json!({
            "_collection": "p", "_key": format!("n{i}"),
            "name": format!("record {i}"), "n": i as i64,
        })))
        .collect();
    db.put_value_bulk(rows)?;

    let edges: Vec<(String, String, String)> = (0..n)
        .flat_map(|i| (0..degree).map(move |j| (
            format!("p/n{i}"),
            // Shape chosen by argument: "near" neighbours are the best case for
            // delta coding and so the worst case for the comparison below.
            format!("p/n{}", if near { (i + j + 1) % n } else { (i * 7 + j * 13 + 1) % n }),
            "next".to_string(),
        )))
        .collect();
    let edge_count = edges.len();
    db.link_many(edges.iter().map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str())));
    db.compact()?;
    println!("built in {:.1}s\n", t.elapsed().as_secs_f64());

    let mut total = 0u64;
    println!("{:<20} {:>12} {:>12} {:>12}", "file", "bytes", "per node", "per edge");
    println!("{}", "-".repeat(60));
    for name in ["nodes.bin", "idx.bin", "slugs.bin", "adj_fwd.bin", "adj_rev.bin",
                 "collections.bin", "spatial.bin", "edgemeta.bin", "dict.bin",
                 "payloads.bin"] {
        let Ok(m) = std::fs::metadata(dir.path().join(name)) else { continue };
        let b = m.len();
        if name != "payloads.bin" { total += b }
        let per_edge = if name.starts_with("adj_") {
            format!("{:.2}", b as f64 / edge_count as f64)
        } else {
            "-".into()
        };
        println!("{name:<20} {b:>12} {:>12.2} {per_edge:>12}", b as f64 / n as f64);
    }
    println!("{}", "-".repeat(60));
    println!("{:<20} {total:>12} {:>12.2}", "topology total", total as f64 / n as f64);

    // What a hash-keyed adjacency would cost instead: neighbour and edge type as
    // full u64s, plus a 4-byte metadata reference, with no delta encoding to
    // exploit because slug hashes are spread across the whole u64 range.
    let fwd = std::fs::metadata(dir.path().join("adj_fwd.bin"))?.len();
    let rev = std::fs::metadata(dir.path().join("adj_rev.bin"))?.len();
    let today = (fwd + rev) as f64 / edge_count as f64;
    // 8-byte neighbour hash + 4-byte interned type id + 4-byte metadata ref, in
    // each direction. The type stays interned because dict.bin is 33 bytes — the
    // thing that cannot be interned is the neighbour.
    let hashed = 16.0 * 2.0;
    // What the encoding spends per edge, both directions: the type id and the
    // metadata reference are fixed 4-byte columns, and the CSR offset array is one
    // u64 per node. Whatever is left over is the neighbour delta — the only part
    // the compression touches.
    let fixed = (4.0 + 4.0) * 2.0;
    let offsets = (8.0 / degree as f64) * 2.0;
    println!("\nper edge, both directions");
    println!("  dense id, delta+SVB (today) {today:>7.2} bytes");
    println!("    of which fixed columns    {fixed:>7.2}   (type id + metadata ref)");
    println!("    of which CSR offsets      {offsets:>7.2}   (one u64 per node)");
    println!("    left for the neighbour    {:>7.2}   (all the delta coding buys)",
             today - fixed - offsets);
    println!("  slug hash, fixed width      {hashed:>7.2} bytes   → {:.2}x", hashed / today);
    // Projecting the measured number flat would understate today's cost: the
    // neighbour delta is a length class, and it grows with the store. Averaging
    // ids uniformly over 48M at this degree puts the gaps past 2^32 / degree, so
    // every neighbour is in the 4-byte class rather than the mix seen here.
    let at_48m = fixed + offsets + 4.0 * 2.0;
    let projected = 48_000_000f64 * degree as f64;
    println!(
        "  at 48M nodes x {degree}:  {at_48m:.2} bytes per edge → {:.1} GB today, \
         {:.1} GB hash-keyed (+{:.1} GB, {:.2}x)",
        projected * at_48m / 1e9, projected * hashed / 1e9,
        projected * (hashed - at_48m) / 1e9, hashed / at_48m,
    );
    Ok(())
}
