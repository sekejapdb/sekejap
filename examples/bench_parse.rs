//! How much of a query() call is SQL parsing vs execution? Decides whether a
//! prepared-statement / plan cache is worth building.
//!   cargo run --release --example bench_parse
use sekejap::{sql, CoreDB};
use std::time::Instant;

fn main() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.execute("CREATE TABLE t (_key TEXT PRIMARY KEY, v INTEGER)").unwrap();
    for i in 0..1000 {
        db.execute(&format!("INSERT INTO t (_key, v) VALUES ('k{i}', {i})")).unwrap();
    }

    let n = 200_000;
    let sql = "SELECT v FROM t WHERE _key = 'k500'";

    let _ = db.query(sql).unwrap().collect(); // warm
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(db.query(sql).unwrap().collect().len());
    }
    let full = t.elapsed().as_secs_f64() * 1e9 / n as f64;

    let _ = sql::parse_match_or_agg(sql); // warm
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(sql::parse_match_or_agg(sql).is_ok());
    }
    let parse = t.elapsed().as_secs_f64() * 1e9 / n as f64;

    let _ = sql::bench_tokenize(sql); // warm
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(sql::bench_tokenize(sql).is_ok());
    }
    let tok = t.elapsed().as_secs_f64() * 1e9 / n as f64;

    println!("full query:   {full:>6.0} ns/op");
    println!("parse only:   {parse:>6.0} ns/op   ({:.0}% of the call)", parse / full * 100.0);
    println!("  tokenize:   {tok:>6.0} ns/op   ({:.0}% of parse)", tok / parse * 100.0);
    println!("  lower:      {:>6.0} ns/op   ({:.0}% of parse)", parse - tok, (parse - tok) / parse * 100.0);
    println!("execute etc:  {:>6.0} ns/op   ({:.0}%)", full - parse, (full - parse) / full * 100.0);
    println!("\n=== prepared vs not (varying params via $1) ===");
    // Not prepared: query_params re-tokenizes + re-lowers every call.
    let psql = "SELECT v FROM t WHERE _key = $1";
    let key = serde_json::json!("k500");
    let _ = db.query_params(psql, std::slice::from_ref(&key)).unwrap().collect();
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(db.query_params(psql, std::slice::from_ref(&key)).unwrap().collect().len());
    }
    let not_prep = t.elapsed().as_secs_f64() * 1e9 / n as f64;

    // Prepared: tokenized once; each run re-lowers cached tokens with the param.
    let stmt = db.prepare(psql).unwrap();
    let _ = db.query_prepared(&stmt, std::slice::from_ref(&key)).unwrap().collect();
    let t = Instant::now();
    for _ in 0..n {
        std::hint::black_box(db.query_prepared(&stmt, std::slice::from_ref(&key)).unwrap().collect().len());
    }
    let prep = t.elapsed().as_secs_f64() * 1e9 / n as f64;

    println!("query_params (not prepared): {not_prep:>6.0} ns/op");
    println!("prepared (query_prepared):   {prep:>6.0} ns/op   ({:.2}x faster, -{:.0}%)",
        not_prep / prep, (not_prep - prep) / not_prep * 100.0);
}
