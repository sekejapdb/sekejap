//! pulse — a comprehensive-but-compact benchmark with LIVE per-stage progress,
//! comparing sekejap to SQLite where the scenario is relational.
//!
//!     cargo bench --bench pulse 2>&1 | tee pulse.log     # watch it live
//!     PULSE_N=200000 PULSE_LOAD=2000000 cargo bench --bench pulse
//!
//! Every stage flushes stdout the instant it finishes, so you always know where it
//! is. Relational stages (filter/sort/aggregate + write load) run on BOTH engines
//! and print sekejap | sqlite | ratio (ratio > 1 ⇒ sekejap faster). Graph, keyed-
//! edge, and full-text stages are sekejap-only (no SQLite equivalent). SQLite gets
//! all applicable indexes (its best case); sekejap runs its default SQL surface.

use rusqlite::Connection;
use sekejap::CoreDB;
use std::io::Write;
use std::time::{Duration, Instant};

fn say(s: String) { println!("{s}"); let _ = std::io::stdout().flush(); }
fn env_usize(k: &str, d: usize) -> usize {
    std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d)
}
fn ms(d: Duration) -> String { format!("{:>8.2?}", d) }

/// Best-of-3 wall time for a sekejap query (returns time + row count).
fn sk_best(db: &CoreDB, sql: &str) -> (Duration, usize) {
    let mut b = Duration::from_secs(999);
    let mut n = 0;
    for _ in 0..3 { let t = Instant::now(); n = db.query(sql).unwrap().collect().len(); b = b.min(t.elapsed()); }
    (b, n)
}
/// Best-of-3 wall time for a SQLite query (drains all rows).
fn sq_best(c: &Connection, sql: &str) -> Duration {
    let mut b = Duration::from_secs(999);
    for _ in 0..3 {
        let t = Instant::now();
        let mut stmt = c.prepare(sql).unwrap();
        let mut rows = stmt.query([]).unwrap();
        while rows.next().unwrap().is_some() {}
        b = b.min(t.elapsed());
    }
    b
}

const T: usize = 17;
fn cmp(step: usize, name: &str, sk: Duration, sq: Duration) {
    let r = sq.as_secs_f64() / sk.as_secs_f64().max(1e-9);
    say(format!("├ [{step:>2}/{T}] {name:<24} sekejap {} | sqlite {}  ({:.1}x)", ms(sk), ms(sq), r));
}
fn solo(step: usize, name: &str, d: Duration, extra: &str) {
    say(format!("├ [{step:>2}/{T}] {name:<24} {}  {}", ms(d), extra));
}

fn main() {
    let n = env_usize("PULSE_N", 100_000);
    let load = env_usize("PULSE_LOAD", 500_000);
    let t_all = Instant::now();
    say(format!("╭─ pulse   nodes={n}  load={load}   (sekejap vs sqlite; ratio>1 = sekejap faster)"));

    let cats = ["cafe", "bar", "hospital", "park", "shop"];
    let cities = ["melbourne", "sydney", "perth", "hobart"];

    // ── 1. sekejap nodes ──
    let nodes: Vec<(String, String)> = (0..n).map(|i| {
        // Spread over ~2°×2° (realistic) so a small radius selects few points.
        let lon = 144.0 + (i % 1000) as f64 * 0.002;
        let lat = -38.0 + ((i / 1000) % 100) as f64 * 0.02;
        (
            format!("venues/v{i}"),
            format!(r#"{{"_collection":"venues","_key":"v{i}","category":"{}","city":"{}","price":{},"rating":{},"content":"venue {i} great coffee brunch wine bar","geometry":{{"type":"Point","coordinates":[{lon},{lat}]}}}}"#,
                cats[i % 5], cities[i % 4], i % 500, (i % 50) as f64 / 10.0),
        )
    }).collect();
    let mut db = CoreDB::new();
    let t = Instant::now();
    db.put_many(nodes.iter().map(|(a, b)| (a.as_str(), b.as_str()))).unwrap();
    // Fair comparison: index the sekejap side too (SQLite gets indexes below).
    for f in ["category", "price", "rating"] {
        db.execute(&format!("CREATE INDEX ON venues USING btree ({f})")).ok();
    }
    solo(1, "build sekejap nodes+idx", t.elapsed(), &format!("({:.0}k/s)", n as f64 / t.elapsed().as_secs_f64() / 1000.0));

    // ── 2. sqlite nodes (same data, indexed) ──
    let sq = Connection::open_in_memory().unwrap();
    sq.execute_batch("PRAGMA synchronous=OFF; PRAGMA journal_mode=MEMORY;").unwrap();
    sq.execute("CREATE TABLE venues (key TEXT PRIMARY KEY, category TEXT, city TEXT, price INTEGER, rating REAL, content TEXT)", []).unwrap();
    let t = Instant::now();
    {
        let tx = sq.unchecked_transaction().unwrap();
        {
            let mut st = tx.prepare("INSERT INTO venues VALUES (?1,?2,?3,?4,?5,?6)").unwrap();
            for i in 0..n {
                st.execute(rusqlite::params![format!("v{i}"), cats[i%5], cities[i%4], (i%500) as i64, (i%50) as f64/10.0, format!("venue {i} great coffee brunch wine bar")]).unwrap();
            }
        }
        tx.commit().unwrap();
    }
    sq.execute_batch("CREATE INDEX ix_cat ON venues(category); CREATE INDEX ix_price ON venues(price); CREATE INDEX ix_rating ON venues(rating);").unwrap();
    solo(2, "build sqlite nodes+idx", t.elapsed(), "");

    // ── 3. sekejap edges ──
    let edges: Vec<(String, String, String)> = (0..n).flat_map(|i| {
        [7usize, 13, 31].into_iter().map(move |k| (format!("venues/v{i}"), format!("venues/v{}", (i + k) % n), "related_to".to_string()))
    }).collect();
    let ec = edges.len();
    let t = Instant::now();
    db.link_many(edges.iter().map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str())));
    solo(3, "build sekejap edges", t.elapsed(), &format!("({ec} edges, {:.0}k/s)", ec as f64 / t.elapsed().as_secs_f64() / 1000.0));

    // ── 4–7. Relational comparison ──
    let (sk, _) = sk_best(&db, "SELECT COUNT(*) FROM venues WHERE category='cafe'");
    cmp(4, "eq filter (category)", sk, sq_best(&sq, "SELECT COUNT(*) FROM venues WHERE category='cafe'"));
    let (sk, _) = sk_best(&db, "SELECT COUNT(*) FROM venues WHERE price > 100 AND price < 300");
    cmp(5, "range filter (price)", sk, sq_best(&sq, "SELECT COUNT(*) FROM venues WHERE price > 100 AND price < 300"));
    let (sk, _) = sk_best(&db, "SELECT rating FROM venues ORDER BY rating DESC LIMIT 50");
    cmp(6, "sort + limit (rating)", sk, sq_best(&sq, "SELECT rating FROM venues ORDER BY rating DESC LIMIT 50"));
    let (sk, _) = sk_best(&db, "SELECT category, COUNT(*) FROM venues GROUP BY category");
    cmp(7, "group by (category)", sk, sq_best(&sq, "SELECT category, COUNT(*) FROM venues GROUP BY category"));

    // ── 8–9. Graph (sekejap-only) ──
    let (d, r) = sk_best(&db, "SELECT COUNT(*) FROM MATCH (a:venues)-[:related_to]->(b:venues)");
    solo(8, "graph 1-hop (MATCH)", d, &format!("rows={r}  [sqlite: n/a]"));
    let (d, r) = sk_best(&db, "SELECT COUNT(*) FROM MATCH (a:venues)-[:related_to]->(m:venues)-[:related_to]->(b:venues) WHERE a._key='v0'");
    solo(9, "graph 2-hop (MATCH)", d, &format!("rows={r}  [sqlite: n/a]"));

    // ── 10–11. Keyed edges (sekejap-only) ──
    let kn = (n / 5).max(1);
    let keyed: Vec<(String, String, String, String)> = (0..kn).map(|i| (
        "venues/v0".to_string(), format!("venues/v{}", i + 1), "reco".to_string(), format!(r#"{{"_key":"k{i}","by":"u{}"}}"#, i % 100),
    )).collect();
    let t = Instant::now();
    db.link_meta_many(keyed.iter().map(|(a, b, c, m)| (a.as_str(), b.as_str(), c.as_str(), Some(m.as_str())))).unwrap();
    solo(10, "keyed-edge upsert", t.elapsed(), &format!("({kn}, {:.0}k/s)", kn as f64 / t.elapsed().as_secs_f64() / 1000.0));
    let (d, r) = sk_best(&db, "SELECT r.by AS by FROM MATCH (a:venues)-[r:reco]->(b:venues) WHERE a._key='v0'");
    solo(11, "per-edge attr scan", d, &format!("rows={r}  [sqlite: n/a]"));

    // ── 12–13. Full-text (sekejap-only) ──
    let t = Instant::now();
    let _ = db.execute("CREATE INDEX ON venues USING search(content)");
    solo(12, "build search index", t.elapsed(), "");
    let (d, r) = sk_best(&db, "SELECT COUNT(*) FROM venues WHERE SEARCH('coffee')");
    solo(13, "full-text SEARCH", d, &format!("rows={r}  [sqlite: LIKE-only]"));

    // ── 14. Spatial (sekejap-only): ST_Distance within radius (grid-accelerated) ──
    let bt = Instant::now();
    db.build_spatial_index();
    let gbuild = bt.elapsed();
    let (d, r) = sk_best(&db, "SELECT COUNT(*) FROM venues WHERE ST_Distance(geometry, POINT(145.0 -37.0), 3.0)");
    solo(14, "spatial ST_Distance", d, &format!("rows={r}  grid-build {}  [sqlite: R*Tree, n/a]", ms(gbuild)));

    // ── 15. Vector KNN (sekejap-only): vectors on a subset, HNSW, VECTOR_NEAR ──
    let vn = (n / 5).clamp(1, 20_000);
    for i in 0..vn {
        let b = (i % 97) as f32;
        db.put_vector(&format!("venues/v{i}"), "emb",
            &[b, b + 1.0, b + 2.0, b + 3.0, b + 4.0, b + 5.0, b + 6.0, b + 7.0]).ok();
    }
    let bt = Instant::now();
    let _ = db.execute("CREATE INDEX ON venues USING hnsw (emb)");
    let build = bt.elapsed();
    let (d, r) = sk_best(&db, "SELECT _key FROM venues WHERE VECTOR_NEAR(emb, [5.0,6.0,7.0,8.0,9.0,10.0,11.0,12.0], 10)");
    solo(15, "vector KNN (hnsw)", d, &format!("rows={r}  hnsw-build {}  ({vn} vecs)", ms(build)));

    // ── 16. Write throughput comparison (both in-memory, no fsync) ──
    let wk: Vec<(String, String)> = (0..load).map(|i| (format!("w/{i}"), format!(r#"{{"_collection":"w","_key":"{i}","v":{i}}}"#))).collect();
    let mut wdb = CoreDB::new();
    let t = Instant::now();
    wdb.put_many(wk.iter().map(|(a, b)| (a.as_str(), b.as_str()))).unwrap();
    let sk_w = t.elapsed();
    let sqw = Connection::open_in_memory().unwrap();
    sqw.execute_batch("PRAGMA synchronous=OFF; PRAGMA journal_mode=MEMORY;").unwrap();
    sqw.execute("CREATE TABLE w (key TEXT PRIMARY KEY, v INTEGER)", []).unwrap();
    let t = Instant::now();
    {
        let tx = sqw.unchecked_transaction().unwrap();
        { let mut st = tx.prepare("INSERT INTO w VALUES (?1,?2)").unwrap(); for i in 0..load { st.execute(rusqlite::params![format!("{i}"), i as i64]).unwrap(); } }
        tx.commit().unwrap();
    }
    let sq_w = t.elapsed();
    say(format!("├ [16/{T}] write {load} (in-mem)     sekejap {} ({:.0}k/s) | sqlite {} ({:.0}k/s)  ({:.1}x)",
        ms(sk_w), load as f64/sk_w.as_secs_f64()/1000.0, ms(sq_w), load as f64/sq_w.as_secs_f64()/1000.0, sq_w.as_secs_f64()/sk_w.as_secs_f64()));

    // ── 15. Disk write load (sekejap durability, chunked, % progress) ──
    let dir = std::env::temp_dir().join("pulse_load");
    let _ = std::fs::remove_dir_all(&dir);
    let mut ldb = CoreDB::open(dir.to_str().unwrap()).unwrap();
    ldb.execute("CREATE TABLE load (_key TEXT PRIMARY KEY, v INTEGER)").ok();
    let chunks = 10usize;
    let per = load / chunks;
    say(format!("├ [17/{T}] disk write load ({load}, durable):"));
    let t = Instant::now();
    for c in 0..chunks {
        let base = c * per;
        let batch: Vec<(String, String)> = (base..base + per).map(|i| (format!("load/{i}"), format!(r#"{{"_collection":"load","_key":"{i}","v":{i}}}"#))).collect();
        let ct = Instant::now();
        ldb.put_many(batch.iter().map(|(a, b)| (a.as_str(), b.as_str()))).unwrap();
        say(format!("│    {:>3}%  +{per} in {}  cumulative {}", (c + 1) * 100 / chunks, ms(ct.elapsed()), ms(t.elapsed())));
    }
    say(format!("├        disk write done      {}  ({:.0}k/s durable)", ms(t.elapsed()), load as f64 / t.elapsed().as_secs_f64() / 1000.0));
    let _ = std::fs::remove_dir_all(&dir);

    say(format!("╰─ pulse complete   total {}", ms(t_all.elapsed())));
}
