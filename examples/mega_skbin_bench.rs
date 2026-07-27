//! Mega-dataset query benchmark (venues + related_to graph) comparing the
//! sekejap query surface across three configs:
//!   raw-resident   = CoreDB::open() default (the previous baseline)
//!   SKBIN-resident = payload_binary
//!   SKBIN-paged    = payload_binary + paged_topology (big-data-on-small-RAM path)
//!
//! Scenarios chosen to be a FAIR SKBIN comparison (attribute filters, sort,
//! projection, graph traversal) — paged's documented spatial/edge-metadata
//! limitations are unrelated to SKBIN, so those scenarios are excluded.
//!
//!   cargo run --release --example mega_skbin_bench

use sekejap::{Config, CoreDB};
use serde_json::json;
use std::time::Instant;

const VENUES: usize = 20_000;
const CATEGORIES: &[&str] = &["cafe","restaurant","park","hospital","school","shop","office","gym","clinic","library"];
const SUBURBS: &[&str] = &["fitzroy","melbourne","collingwood","richmond","carlton","brunswick","northcote","prahran","southbank","docklands"];

fn vcat(i: usize) -> &'static str { CATEGORIES[i % CATEGORIES.len()] }
fn vsub(i: usize) -> &'static str { SUBURBS[i % SUBURBS.len()] }
fn vrat(i: usize) -> f64 { 1.0 + (i % 40) as f64 * 0.1 }
fn vprice(i: usize) -> f64 { 10.0 + (i % 49) as f64 * 10.0 }
fn vcontent(i: usize) -> String {
    format!("A great {} in {}. Rated {:.1} stars, price {:.0}. Popular venue near the river.",
        vcat(i), vsub(i), vrat(i), vprice(i))
}

fn build(dir: &std::path::Path, binary: bool) {
    let cfg = Config { payload_binary: binary, ..Config::default() };
    let mut db = CoreDB::open_with_config(dir, cfg).unwrap();
    for i in 0..VENUES {
        db.put(&format!("venues/v{i}"), &json!({
            "_collection": "venues", "_key": format!("v{i}"),
            "name": format!("Venue {i}"), "category": vcat(i), "suburb": vsub(i),
            "rating": vrat(i), "price": vprice(i), "content": vcontent(i),
            "geometry": {"type":"Point","coordinates":[144.96 + (i%500) as f64*0.001, -37.81 + (i%400) as f64*0.001]},
        }).to_string()).unwrap();
    }
    for i in 0..VENUES {
        for d in [7usize, 13, 31] {
            db.link(&format!("venues/v{i}"), &format!("venues/v{}", (i + d) % VENUES), "related_to");
        }
    }
    db.compact().unwrap();
}

fn cfg_of(binary: bool, paged: bool) -> Config {
    Config { payload_binary: binary, paged_topology: paged, ..Config::default() }
}

fn time_q(dir: &std::path::Path, binary: bool, paged: bool, sql: &str, reps: usize) -> (usize, f64) {
    let db = CoreDB::open_with_config(dir, cfg_of(binary, paged)).unwrap();
    let n = db.query(sql).unwrap().collect().len();
    let t = Instant::now();
    for _ in 0..reps { std::hint::black_box(db.query(sql).unwrap().collect().len()); }
    (n, t.elapsed().as_secs_f64() * 1e3 / reps as f64)
}

fn main() {
    println!("== mega query benchmark: raw-resident vs SKBIN-resident vs SKBIN-paged ==");
    println!("venues={VENUES}, related_to edges={}\n", VENUES * 3);

    let d_raw = tempfile::tempdir().unwrap();
    let d_bin = tempfile::tempdir().unwrap();
    print!("building raw…");   build(d_raw.path(), false); println!(" done");
    print!("building SKBIN…"); build(d_bin.path(), true);  println!(" done");

    let raw_sz = std::fs::metadata(d_raw.path().join("payloads.bin")).unwrap().len();
    let bin_sz = std::fs::metadata(d_bin.path().join("payloads.bin")).unwrap().len();
    println!("payloads.bin: raw {:.1} MB | SKBIN {:.1} MB ({:.2}x smaller)\n",
        raw_sz as f64/1e6, bin_sz as f64/1e6, raw_sz as f64 / bin_sz as f64);

    let scenarios: &[(&str, &str, usize)] = &[
        ("eq_filter",       "SELECT * FROM venues WHERE category = 'cafe'", 5),
        ("neq_filter",      "SELECT * FROM venues WHERE category != 'hospital'", 3),
        ("range_filter",    "SELECT * FROM venues WHERE price > 100 AND price <= 300", 5),
        ("sort_limit",      "SELECT * FROM venues ORDER BY rating DESC LIMIT 50", 5),
        ("point_lookup",    "SELECT * FROM venues WHERE _key = 'v9999'", 20),
        ("compound_filter", "SELECT * FROM venues WHERE category = 'cafe' AND suburb = 'fitzroy'", 5),
        ("projection",      "SELECT _key, category, price FROM venues WHERE price > 200", 5),
        ("graph_1hop",      "SELECT b._key AS k FROM MATCH (a:venues)-[:related_to]->(b:venues) WHERE a._key = 'v5000'", 10),
        ("graph_2hop",      "SELECT b._key AS k FROM MATCH (a:venues)-[:related_to]->()-[:related_to]->(b:venues) WHERE a._key = 'v1234'", 5),
    ];

    println!("{:<18} {:>6}  {:>10} | {:>10} | {:>10}", "scenario", "rows", "raw-resid", "SKBIN-res", "SKBIN-paged");
    println!("{}", "-".repeat(70));
    for (label, sql, reps) in scenarios {
        let (rn, rt) = time_q(d_raw.path(), false, false, sql, *reps);
        let (bn, bt) = time_q(d_bin.path(), true,  false, sql, *reps);
        let (pn, pt) = time_q(d_bin.path(), true,  true,  sql, *reps);
        let flag = if rn == bn && rn == pn { "" } else { " ⚠ROWS DIFFER" };
        println!("{label:<18} {rn:>6}  {rt:>8.2}ms | {bt:>7.2}ms ({:.1}x) | {pt:>7.2}ms ({:.1}x){flag}",
            rt / bt, rt / pt);
    }
    println!("\n(SKBIN-res/SKBIN-paged multipliers are speedup vs raw-resident baseline)");
}
