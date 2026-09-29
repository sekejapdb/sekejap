//! FORMAT GATE CASES -- the three measurements of the 0.19 format gate that no
//! other benchmark program takes (`docs/core/FORMAT_GATE.md`).
//!
//!     format_gate_cases reopen <fresh-dir> <tables> [--reopens N]
//!     format_gate_cases narrow <fresh-dir> <rows>
//!
//! `reopen` creates `tables` one-column tables holding one row each, closes
//! the file, and opens it again: the open's page accesses (cold pool) and the
//! median wall time of `--reopens` open + resolve-the-last-table + one point
//! read. On the way it commits the LAST `create_collection` alone and reports
//! the WAL bytes and frames that one DDL commit wrote, so WAL bytes per DDL
//! commit is read at 1, 100 and 10,000 tables.
//!
//! `narrow` is a table of ONE declared integer column at `rows` rows,
//! committed every 256: insert wall time and bytes on disk per row, point read
//! and full scan wall time and page accesses. A per-table column cap must cost
//! a table this narrow nothing.
//!
//! Each prints ONE JSON object on stdout. It uses only calls the 0.18.5
//! release has, so the baseline is built from the release's own source with
//! this file copied in (`python3 tools/format_gate.py build-help`).

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{CollectionOptions, Database},
    Kind,
};
use serde_json::json;
use std::{fs, path::Path, time::Instant};

type R<T> = Result<T, Box<dyn std::error::Error>>;

const BATCH: u64 = 256;
const CACHE_BYTES: usize = 8 << 20;

fn config() -> Config {
    Config {
        budget_bytes: CACHE_BYTES,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn dir_bytes(root: &Path) -> u64 {
    fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| e.metadata().ok())
                .filter(|m| m.is_file())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

fn table(i: u64) -> String {
    format!("t{i:05}")
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn reopen(root: &Path, tables: u64, reopens: usize) -> R<()> {
    if tables == 0 {
        return Err("tables must be at least 1".into());
    }
    let mut db = Database::create(root, config())?;
    let fields = || vec![("v".to_string(), Kind::Int)];
    let at = Instant::now();
    for i in 1..tables {
        let c = db.create_collection(&table(i), fields(), CollectionOptions::default())?;
        db.put(c, "r", &json!({"v": i as i64}))?;
        if i % BATCH == 0 {
            db.commit()?;
        }
    }
    db.commit()?;
    // The last table's DDL is its own commit, so its WAL cost is one DDL
    // commit at `tables` tables and nothing else.
    let io0 = db.io_counters()?;
    let last = db.create_collection(&table(tables), fields(), CollectionOptions::default())?;
    db.commit()?;
    let io1 = db.io_counters()?;
    db.put(last, "r", &json!({"v": tables as i64}))?;
    db.commit()?;
    let create_s = at.elapsed().as_secs_f64();
    db.checkpoint()?;
    drop(db);
    let bytes = dir_bytes(root);

    // Cold open: the pool is empty, so every access is the open's own.
    let db = Database::open(root, config())?;
    let open_pages = db.pool_accesses()?;
    let c = db.collection(&table(tables))?.ok_or("the last table is missing")?;
    let resolve_pages = db.pool_accesses()? - open_pages;
    let p0 = db.pool_accesses()?;
    let found = db.get(c, "r")?.is_some();
    let read_pages = db.pool_accesses()? - p0;
    drop(db);
    if !found {
        return Err("the last table's row is missing".into());
    }

    let mut open_us = Vec::with_capacity(reopens);
    for _ in 0..reopens.max(1) {
        let at = Instant::now();
        let db = Database::open(root, config())?;
        let c = db.collection(&table(tables))?.ok_or("the last table is missing")?;
        std::hint::black_box(db.get(c, "r")?);
        drop(db);
        open_us.push(at.elapsed().as_secs_f64() * 1e6);
    }
    println!(
        "{}",
        json!({
            "case": "reopen",
            "tables": tables,
            "create_s": create_s,
            "bytes_on_disk": bytes,
            "ddl_commit_wal_bytes": io1.wal_bytes_written - io0.wal_bytes_written,
            "ddl_commit_wal_frames": io1.wal_frames_appended - io0.wal_frames_appended,
            "open_pages": open_pages,
            "resolve_pages": resolve_pages,
            "read_pages": read_pages,
            "reopens": open_us.len(),
            "reopen_median_us": median(open_us),
        })
    );
    Ok(())
}

fn narrow(root: &Path, rows: u64) -> R<()> {
    let mut db = Database::create(root, config())?;
    let io0 = db.io_counters()?;
    let c = db.create_collection(
        "narrow",
        vec![("v".to_string(), Kind::Int)],
        CollectionOptions::default(),
    )?;
    db.commit()?;
    let io1 = db.io_counters()?;
    let key = |i: u64| format!("n{i:09}");
    let at = Instant::now();
    for i in 0..rows {
        db.put(c, &key(i), &json!({"v": i as i64}))?;
        if (i + 1) % BATCH == 0 {
            db.commit()?;
        }
    }
    db.commit()?;
    let insert_s = at.elapsed().as_secs_f64();
    db.checkpoint()?;
    drop(db);
    let bytes = dir_bytes(root);

    let db = Database::open(root, config())?;
    // Point reads: a fixed stride through the key space, so every revision
    // reads the same keys in the same order.
    let probes = rows.min(10_000).max(1);
    let stride = (rows / probes).max(1);
    let p0 = db.pool_accesses()?;
    let at = Instant::now();
    let mut hits = 0u64;
    for p in 0..probes {
        hits += u64::from(db.get(c, &key((p * stride) % rows.max(1)))?.is_some());
    }
    let read_s = at.elapsed().as_secs_f64();
    let read_pages = db.pool_accesses()? - p0;
    if hits != probes {
        return Err(format!("{hits} of {probes} point reads found their row").into());
    }
    // Full scan, twice: the first warms the pool, the second is measured.
    let warm = db.scan(c, None)?.count() as u64;
    let p0 = db.pool_accesses()?;
    let at = Instant::now();
    let scanned = db.scan(c, None)?.count() as u64;
    let scan_s = at.elapsed().as_secs_f64();
    let scan_pages = db.pool_accesses()? - p0;
    if warm != rows || scanned != rows {
        return Err(format!("the scans saw {warm} and {scanned} of {rows} rows").into());
    }
    println!(
        "{}",
        json!({
            "case": "narrow",
            "rows": rows,
            "ddl_commit_wal_bytes": io1.wal_bytes_written - io0.wal_bytes_written,
            "insert_us_per_row": insert_s * 1e6 / rows.max(1) as f64,
            "bytes_on_disk": bytes,
            "bytes_per_row": bytes as f64 / rows.max(1) as f64,
            "point_reads": probes,
            "point_read_us": read_s * 1e6 / probes as f64,
            "point_read_pages": read_pages as f64 / probes as f64,
            "scan_ns_per_row": scan_s * 1e9 / rows.max(1) as f64,
            "scan_pages_per_row": scan_pages as f64 / rows.max(1) as f64,
        })
    );
    Ok(())
}

fn main() -> R<()> {
    let usage = "usage: format_gate_cases reopen <fresh-dir> <tables> [--reopens N]\n\
                 \x20      format_gate_cases narrow <fresh-dir> <rows>";
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 3 {
        return Err(usage.into());
    }
    let root = Path::new(&args[1]);
    if root.exists() {
        return Err("the directory must not exist yet".into());
    }
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent)?;
    }
    let n: u64 = args[2].parse()?;
    match args[0].as_str() {
        "reopen" => {
            let mut reopens = 21usize;
            let mut rest = args[3..].iter();
            while let Some(flag) = rest.next() {
                match flag.as_str() {
                    "--reopens" => reopens = rest.next().ok_or("--reopens needs a value")?.parse()?,
                    other => return Err(format!("unknown flag {other}\n{usage}").into()),
                }
            }
            reopen(root, n, reopens)
        }
        "narrow" => narrow(root, n),
        _ => Err(usage.into()),
    }
}
