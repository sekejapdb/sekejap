//! Q6 BUDGET -- where the ~500 ns of a key-only full enumeration go.
//!
//!     q6_budget <popsim-dir>/e4 [--cache-bytes N] [--passes N] [--stage all]
//!
//! Each stage adds ONE layer to the one below it, over the same rows of the
//! same database, so the differences are the budget:
//!
//!   pread     one 4 KiB `pread` per leaf, straight at the file, no pool
//!   crc32c    one page checksum, the Law 5 cost of believing a miss
//!   leafwalk  `RangeIter::for_each_ref` -- pool, pin, validate, no allocation
//!   pull      `peek_ref`/`step` + `row_id` -- the shape the entity driver uses
//!   page      `next_page` key-only, `QueryOrder::EntityId`, `Projection::Ids`
//!
//! Run it twice with `--cache-bytes` far apart: at 8 MiB every leaf is a miss
//! and at 1 GiB the second pass is all hits, and the difference between the
//! two IS the per-miss cost.
use e4_prototype::collections::{
    CandidateDriver, CollectionId, Database, Projection, QueryBudget, QueryOrder, QueryRequest,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use std::{path::PathBuf, time::Instant};

type R<T> = Result<T, Box<dyn std::error::Error>>;

const PAGE: usize = 8192;

struct Sample {
    name: &'static str,
    rows: u64,
    seconds: f64,
    accesses: u64,
    misses: u64,
    evictions: u64,
    sweeps: u64,
}

fn report(s: &Sample, rows_ref: u64) {
    let per = s.seconds * 1e9 / rows_ref.max(1) as f64;
    println!(
        "{:10} rows={:<9} {:8.1} ns/row  pool/row={:6.4} miss/row={:6.4} evict/row={:6.4} sweep/row={:6.3}  {:7.3} s",
        s.name,
        s.rows,
        per,
        s.accesses as f64 / rows_ref.max(1) as f64,
        s.misses as f64 / rows_ref.max(1) as f64,
        s.evictions as f64 / rows_ref.max(1) as f64,
        s.sweeps as f64 / rows_ref.max(1) as f64,
        s.seconds,
    );
}

fn timed(
    db: &Database,
    name: &'static str,
    f: impl FnOnce() -> R<u64>,
) -> R<Sample> {
    let before_pool = db.pool_accesses()?;
    let (h0, m0, e0, s0) = db.pool_counters()?;
    let at = Instant::now();
    let rows = f()?;
    let seconds = at.elapsed().as_secs_f64();
    let (h1, m1, e1, s1) = db.pool_counters()?;
    let _ = (h0, h1);
    Ok(Sample {
        name,
        rows,
        seconds,
        accesses: db.pool_accesses()? - before_pool,
        misses: m1 - m0,
        evictions: e1 - e0,
        sweeps: s1 - s0,
    })
}

/// Item KD: the `page` stage now asks the SAME question `SELECT _key FROM
/// person` asks in SQLite -- the mapping keyspace, not the primary rows.
fn count_page(db: &Database, person: CollectionId) -> R<u64> {
    let mut prepared = db.prepare_query(QueryRequest {
        collection: person,
        filters: &[],
        order: QueryOrder::Driver,
        projection: Projection::Ids,
        total_limit: None,
        driver: CandidateDriver::Keys,
    })?;
    let mut rows = 0u64;
    loop {
        let page = prepared.next_page(PAGE, QueryBudget::unlimited(), || false)?;
        rows += page.rows.len() as u64;
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    Ok(rows)
}

fn main() -> R<()> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().ok_or("usage: q6_budget <db-dir> [--cache-bytes N]")?);
    let mut cache = 8usize << 20;
    let mut passes = 2usize;
    let mut stages = String::from("all");
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--cache-bytes" => cache = args.next().ok_or("--cache-bytes needs a value")?.parse()?,
            "--passes" => passes = args.next().ok_or("--passes needs a value")?.parse()?,
            "--stage" => stages = args.next().ok_or("--stage needs a value")?,
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    let want = |name: &str| stages == "all" || stages.split(',').any(|s| s == name);

    // Stage `pread` and stage `crc32c` need no database handle: they are the
    // medium and the checksum on their own, over this very file.
    let data = root.join("data");
    let file_bytes = std::fs::metadata(&data).map(|m| m.len()).unwrap_or(0);
    if want("pread") && file_bytes > 0 {
        use std::os::unix::fs::FileExt;
        let f = std::fs::File::open(&data)?;
        let pages = (file_bytes / 4096).min(200_000);
        let mut buf = vec![0u8; 4096];
        let mut sum = 0u64;
        // Twice: the first pass reads the medium, the second reads the OS
        // page cache. The difference is what a pool miss costs when the file
        // is resident, which is the case the engine budget is measured in.
        for pass in 0..2 {
            let at = Instant::now();
            for p in 0..pages {
                f.read_exact_at(&mut buf, p * 4096)?;
                sum += buf[0] as u64;
            }
            let seconds = at.elapsed().as_secs_f64();
            println!(
                "pread/{pass}    pages={pages:<9} {:8.1} ns/page ({:7.3} s, {sum})",
                seconds * 1e9 / pages as f64,
                seconds
            );
        }
        // And the same bytes in runs of 8 pages, one pread each: what a
        // bounded sequential read-ahead would pay instead.
        let mut run = vec![0u8; 8 * 4096];
        let runs = pages / 8;
        let at = Instant::now();
        for r in 0..runs {
            f.read_exact_at(&mut run, r * 8 * 4096)?;
            sum += run[0] as u64;
        }
        let seconds = at.elapsed().as_secs_f64();
        println!(
            "pread8     pages={:<9} {:8.1} ns/page ({:7.3} s, {sum})",
            runs * 8,
            seconds * 1e9 / (runs * 8) as f64,
            seconds
        );
    }
    if want("crc32c") {
        let buf = vec![0x5au8; 4096];
        let rounds = 200_000u64;
        let at = Instant::now();
        let mut h = 0u32;
        for _ in 0..rounds {
            h ^= crc32c::crc32c(&buf);
        }
        let seconds = at.elapsed().as_secs_f64();
        println!(
            "crc32c     pages={rounds:<9} {:8.1} ns/page ({:7.3} s, {h})",
            seconds * 1e9 / rounds as f64,
            seconds
        );
    }

    let db = Database::open(
        &root,
        Config {
            budget_bytes: cache,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )?;
    let person = db.collection("person")?.ok_or("no `person` collection")?;
    println!("cache_bytes={cache} data_bytes={file_bytes} passes={passes}");

    let mut rows_ref = 1u64;
    for pass in 0..passes {
        println!("-- pass {pass}");
        if want("leafwalk") {
            let s = timed(&db, "leafwalk", || Ok(db.diag_scan_for_each_ref(person)?))?;
            rows_ref = s.rows.max(rows_ref);
            report(&s, s.rows);
        }
        if want("pull") {
            let s = timed(&db, "pull", || Ok(db.diag_scan_pull(person, true)?))?;
            rows_ref = s.rows.max(rows_ref);
            report(&s, s.rows);
        }
        if want("pullnoid") {
            let s = timed(&db, "pullnoid", || Ok(db.diag_scan_pull(person, false)?))?;
            report(&s, s.rows);
        }
        if want("page") {
            let s = timed(&db, "page", || count_page(&db, person))?;
            rows_ref = s.rows.max(rows_ref);
            report(&s, s.rows);
        }
    }
    Ok(())
}
