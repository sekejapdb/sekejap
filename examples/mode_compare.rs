// Do regular (resident) and service (paged + snapshot reads) return the SAME
// results, at the same speed, for the same queries?
use sekejap::{engine::Engine, CoreDB};
use std::time::Instant;

fn med(mut v: Vec<f64>) -> f64 { v.sort_by(|a, b| a.partial_cmp(b).unwrap()); v[v.len() / 2] }

fn seed(dir: &std::path::Path) {
    let mut db = CoreDB::open(dir).unwrap();
    db.execute("CREATE TABLE p (_key TEXT PRIMARY KEY, name TEXT, cat TEXT, n INTEGER, \
                descr TEXT, geometry GEO, emb VECTOR)").unwrap();
    for i in 0..20_000 {
        let cat = ["cafe", "bar", "gym"][i % 3];
        let lon = 115.0 + (i % 500) as f64 * 0.001;
        let lat = -8.6 - (i % 500) as f64 * 0.001;
        db.execute(&format!(
            "INSERT INTO p (_key,name,cat,n,descr,geometry,emb) VALUES \
             ('p{i}','Place {i}','{cat}',{i},'grilled chicken {i}',\
              '{{\"type\":\"Point\",\"coordinates\":[{lon},{lat}]}}',[{},{},{}])",
            (i % 7) as f64 / 7.0, (i % 5) as f64 / 5.0, (i % 3) as f64 / 3.0)).unwrap();
    }
    for i in 0..5_000 { db.execute(&format!("INSERT ('p/p{i}')-[:near]->('p/p{}')", i + 1)).unwrap(); }
    for ix in ["btree (cat)", "btree (n)", "gin (name)", "bm25 (descr)", "spatial (geometry)", "hnsw (emb)"] {
        db.execute(&format!("CREATE INDEX ON p USING {ix}")).unwrap();
    }
    db.compact().unwrap();
}

fn main() {
    let dir = tempfile::TempDir::new().unwrap();
    seed(dir.path());

    let cases: Vec<(&str, &str)> = vec![
        ("point lookup",    "SELECT _key FROM p WHERE _key = 'p9999'"),
        ("indexed filter",  "SELECT _key FROM p WHERE cat = 'cafe'"),
        ("range filter",    "SELECT _key FROM p WHERE n > 19000"),
        ("sort + limit",    "SELECT _key FROM p ORDER BY n DESC LIMIT 20"),
        ("aggregate",       "SELECT cat, COUNT(*) FROM p GROUP BY cat"),
        ("graph 1-hop",     "SELECT b._key AS k FROM MATCH (a:p)-[:near]->(b:p) WHERE a._key = 'p1'"),
        ("graph 1..3",      "SELECT b._key AS k FROM MATCH (a:p)-[:near*1..3]->(b:p) WHERE a._key = 'p1'"),
        ("ILIKE",           "SELECT _key FROM p WHERE name ILIKE '%ace 123%'"),
        ("BM25",            "SELECT _key FROM p WHERE BM25(descr,'grilled chicken') > 0 LIMIT 10"),
        ("spatial",         "SELECT _key FROM p WHERE ST_DWithin(geometry, POINT(115.1 -8.7), 3000.0)"),
        ("vector",          "SELECT _key FROM p WHERE VECTOR_NEAR(emb,[0.5,0.5,0.5],10)"),
    ];

    let regular = CoreDB::open_paged(dir.path()).unwrap();  // read side of "regular"
    let mut reg_rows = vec![];
    let mut reg_us = vec![];
    for (_, sql) in &cases {
        let n = regular.query(sql).map(|s| s.collect().len()).unwrap_or(usize::MAX);
        let t = med((0..60).map(|_| { let s = Instant::now(); let _ = regular.query(sql).map(|x| x.collect()); s.elapsed().as_secs_f64() * 1e6 }).collect());
        reg_rows.push(n); reg_us.push(t);
    }
    drop(regular);

    let svc = Engine::open_as_service(dir.path().to_str().unwrap()).unwrap();
    println!("{:<16} {:>8} {:>12} {:>12} {:>9}  {}", "query", "rows", "regular µs", "service µs", "ratio", "same?");
    println!("{}", "-".repeat(72));
    let mut mismatches = 0;
    for (i, (label, sql)) in cases.iter().enumerate() {
        let n = svc.query(sql).map(|r| r.len()).unwrap_or(usize::MAX);
        let t = med((0..60).map(|_| { let s = Instant::now(); let _ = svc.query(sql); s.elapsed().as_secs_f64() * 1e6 }).collect());
        let same = n == reg_rows[i];
        if !same { mismatches += 1; }
        println!("{label:<16} {n:>8} {:>12.1} {t:>12.1} {:>8.2}x  {}",
            reg_us[i], t / reg_us[i], if same { "yes" } else { "NO — MISMATCH" });
    }
    println!("\nrow-count mismatches: {mismatches}");
}
