//! Does a real multi-index store stay inside a hard memory cap?
//!
//! Every RAM figure gathered locally is a heap measurement extrapolated from a
//! million rows or fewer. This is the other kind of evidence: run under a cgroup
//! limit and let the kernel decide. It either finishes or it is OOM-killed, and
//! neither outcome needs interpreting.
//!
//! Defaults are deliberately the shipping ones — `sekejap::open`, auto-compaction
//! on, real thresholds — because a probe that disables the thing under test
//! proves nothing.
//!
//!   ROWS=1000000 INDEXES=btree,spatial,gin,search,bm25 scale_probe
//!
//! `INDEXES=` (empty) loads plain rows. Progress prints resident set as read
//! from /proc/self/statm, so a run that is about to die says so before it does.

use sekejap::CoreDB;
use std::time::Instant;

/// Resident set in MB, from /proc — the same number the cgroup accounts.
fn rss_mb() -> u64 {
    mem().0
}

/// `(total, anonymous, file-backed)` resident MB.
///
/// The distinction is the whole question for a disk-first store. Anonymous pages
/// are heap: the kernel cannot take them back, so they are what the database
/// genuinely *needs*. File-backed pages are the mapped index and payload files —
/// the kernel evicts those under pressure, so they are memory the database is
/// merely *using* because it is there.
///
/// A total RSS figure cannot tell the two apart, which is how "570 bytes a row"
/// can mean either "this does not fit" or "this fits fine and the page cache is
/// doing its job".
fn mem() -> (u64, u64, u64) {
    let s = match std::fs::read_to_string("/proc/self/status") {
        Ok(s) => s,
        Err(_) => return (0, 0, 0),
    };
    let kb = |key: &str| -> u64 {
        s.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
            / 1024
    };
    (kb("VmRSS:"), kb("RssAnon:"), kb("RssFile:"))
}

/// Ask glibc to return free arenas to the kernel.
///
/// The counting-allocator measurements said live heap was flat while the kernel
/// saw resident memory climbing monotonically. That gap is what an allocator
/// keeps: `free()` returns memory to malloc, not to the OS. A cgroup kills on
/// resident set, so bounded live heap is not by itself bounded memory.
///
/// TRIM=1 calls this after every chunk. If resident anonymous memory falls, the
/// live heap really was bounded and the growth was retention; if it does not,
/// the database is genuinely holding it.
#[cfg(target_os = "linux")]
extern "C" {
    fn malloc_trim(pad: usize) -> i32;
}

fn trim() {
    #[cfg(target_os = "linux")]
    unsafe {
        malloc_trim(0);
    }
}

fn row(i: usize) -> (String, serde_json::Value) {
    let lon = 115.0 + (i % 1000) as f64 * 0.001;
    let lat = -8.8 - (i % 997) as f64 * 0.001;
    (
        format!("items/n{i}"),
        serde_json::json!({
            "_collection": "items",
            "_key": format!("n{i}"),
            "n": i % 100_000,
            "geometry": {"type": "Point", "coordinates": [lon, lat]},
            "body": format!("riverbank heron alpha{} beta{} gamma{}", i % 997, i % 89, i % 13),
        }),
    )
}

fn main() {
    let rows: usize = std::env::var("ROWS").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000_000);
    let indexes: Vec<String> = std::env::var("INDEXES")
        .unwrap_or_else(|_| "btree,spatial,gin,search,bm25".into())
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();
    let dir = std::env::var("DBDIR").unwrap_or_else(|_| "/work/db/scale".into());

    let _ = std::fs::remove_dir_all(&dir);
    println!("rows={rows} indexes={indexes:?} dir={dir}");
    println!("rss at start: {} MB", rss_mb());

    let mut db = sekejap::open(&dir).expect("open");
    db.execute("CREATE TABLE items (n INTEGER, body TEXT)").expect("create table");

    // Indexes declared BEFORE the data, which is the ordinary order and the one
    // that used to leave a GIN index that never existed.
    for ix in &indexes {
        let sql = match ix.as_str() {
            "btree" => "CREATE INDEX ON items USING btree (n)".to_string(),
            "gin" => "CREATE INDEX ON items USING gin (body)".to_string(),
            "search" => "CREATE INDEX ON items USING search (body)".to_string(),
            "bm25" => "CREATE INDEX ON items USING bm25 (body)".to_string(),
            "spatial" => String::new(), // built implicitly from geometry
            other => panic!("unknown index {other}"),
        };
        if !sql.is_empty() {
            db.execute(&sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
        }
    }

    const CHUNK: usize = 25_000;
    let t0 = Instant::now();
    let mut done = 0usize;
    let mut peak = rss_mb();
    while done < rows {
        let take = CHUNK.min(rows - done);
        db.put_value_bulk((done..done + take).map(row).collect()).expect("bulk");
        done += take;
        if std::env::var("TRIM").is_ok() {
            trim();
        }
        let r = rss_mb();
        if r > peak { peak = r; }
        if done % 250_000 == 0 || done == rows {
            // sekejap's own accounting, so the growth can be attributed to a
            // structure rather than inferred from a total.
            let mut report: Vec<(&str, usize)> = db.memory_report();
            report.sort_by_key(|(_, b)| std::cmp::Reverse(*b));
            let top: Vec<String> = report
                .iter()
                .filter(|(_, b)| *b > 1_048_576)
                .take(6)
                .map(|(k, b)| format!("{k}={}MB", b / 1_048_576))
                .collect();
            println!("            internal: {}", top.join(" "));
            let (t, anon, file) = mem();
            println!(
                "{:>10} rows  rss {:>5} MB = anon {:>5} MB + file {:>5} MB  peak {:>5}  {:>7.1}s",
                done, t, anon, file, peak, t0.elapsed().as_secs_f64()
            );
        }
    }

    println!("load done in {:.1}s, rss {} MB, peak {} MB", t0.elapsed().as_secs_f64(), rss_mb(), peak);

    // Queries, because an index that is never asked anything can be quietly
    // broken and still not cost memory.
    let probes: Vec<(&str, String)> = vec![
        ("scan count", "SELECT COUNT(*) FROM items".into()),
        ("btree eq", "SELECT _key FROM items WHERE n = 7".into()),
        ("btree range", "SELECT _key FROM items WHERE n > 99990".into()),
        // Selective on purpose. `%heron%` matches every row, so it measured the
        // cost of materialising a million results — a fact about the probe, not
        // about the database. `alpha42` appears in roughly one row in a thousand.
        ("ilike/gin", "SELECT _key FROM items WHERE body ILIKE '%alpha42 %'".into()),
        ("search", "SELECT _key FROM items WHERE SEARCH('alpha42')".into()),
        ("bm25", "SELECT _key FROM items WHERE BM25(body,'alpha42') > 0".into()),
        // Kept deliberately: a query that really does return every row, so the
        // cost of a large result set is visible as its own line rather than
        // hiding inside a query that was meant to be selective.
        ("full result", "SELECT _key FROM items WHERE body ILIKE '%heron%' LIMIT 1000".into()),
        ("spatial", "SELECT _key FROM items WHERE ST_DWithin(geometry, POINT(115.10 -8.85), 3000)".into()),
    ];
    for (name, sql) in &probes {
        let t = Instant::now();
        match db.query(sql) {
            Ok(q) => {
                let n = q.collect().len();
                let r = rss_mb();
                if r > peak { peak = r; }
                let (tt, anon, file) = mem();
                println!("{:<12} {:>9} rows  {:>8.1} ms  rss {:>5} = anon {:>5} + file {:>5}",
                         name, n, t.elapsed().as_secs_f64() * 1000.0, tt, anon, file);
            }
            Err(e) => println!("{:<12} FAILED: {e:?}", name),
        }
    }

    println!();
    let (t, anon, file) = mem();
    println!("SURVIVED. peak rss {} MB over {} rows; final anon {} MB, file {} MB", peak, rows, anon, file);
    println!("anon is what the database needs; file is page cache the kernel can take back.");
}
