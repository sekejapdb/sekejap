//! Does SQLite pay the same per-insert fsync cost sekejap does?
//!
//! sekejap's `SyncMode::Full` calls `File::sync_data`, and on macOS Rust's
//! `sync_data` issues `F_FULLFSYNC` — the barrier that waits for the drive to
//! actually persist. SQLite at `synchronous=FULL` issues a plain `fsync`, which
//! on macOS returns once the data reaches the drive's write cache and is *not*
//! durable across power loss. `PRAGMA fullfsync=ON` makes it use `F_FULLFSYNC`
//! too.
//!
//! So there are two honest comparisons and one dishonest one, and the dishonest
//! one is the one that gets published. All three are here.

use rusqlite::Connection;
use sekejap::{AutoCompact, Config, CoreDB, SyncMode};
use std::time::Instant;

const N: usize = 2_000;

fn sqlite_run(label: &str, fullfsync: bool, synchronous: &str) -> f64 {
    let dir = std::env::temp_dir().join(format!("sk_fs_sqlite_{}_{}", std::process::id(), label.len()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let c = Connection::open(dir.join("t.db")).unwrap();
    c.pragma_update(None, "journal_mode", "WAL").unwrap();
    c.pragma_update(None, "synchronous", synchronous).unwrap();
    c.pragma_update(None, "fullfsync", if fullfsync { "ON" } else { "OFF" }).unwrap();
    c.execute("CREATE TABLE t (k TEXT PRIMARY KEY, n INTEGER)", []).unwrap();

    let t = Instant::now();
    for i in 0..N {
        // One statement, one implicit transaction, one durability decision —
        // matching a single `put_value` rather than a batch.
        c.execute("INSERT INTO t (k, n) VALUES (?1, ?2)", rusqlite::params![format!("n{i}"), i as i64])
            .unwrap();
    }
    let us = t.elapsed().as_secs_f64() * 1_000_000.0 / N as f64;
    println!("{:<44} {:>10.1} us/insert", label, us);
    drop(c);
    let _ = std::fs::remove_dir_all(&dir);
    us
}

fn sekejap_run(label: &str, sync: SyncMode) -> f64 {
    let dir = std::env::temp_dir().join(format!("sk_fs_sk_{}_{}", std::process::id(), label.len()));
    let _ = std::fs::remove_dir_all(&dir);
    let cfg = Config { wal_sync: sync, auto_compact: AutoCompact::Off, ..Config::default() };
    let mut db = CoreDB::open_with_config(&dir, cfg).unwrap();

    let t = Instant::now();
    for i in 0..N {
        db.put_value(&format!("t/n{i}"),
            serde_json::json!({"_collection":"t","_key":format!("n{i}"),"n":i})).unwrap();
    }
    let us = t.elapsed().as_secs_f64() * 1_000_000.0 / N as f64;
    println!("{:<44} {:>10.1} us/insert", label, us);
    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
    us
}

fn main() {
    println!("{N} single-row inserts each, same machine and disk\n");

    println!("── durable to the platter (F_FULLFSYNC both sides) ──");
    let sk_full = sekejap_run("sekejap  SyncMode::Full", SyncMode::Full);
    let lt_full = sqlite_run("sqlite   synchronous=FULL, fullfsync=ON", true, "FULL");

    println!("\n── durable to the drive cache only (plain fsync) ──");
    let lt_fsync = sqlite_run("sqlite   synchronous=FULL, fullfsync=OFF", false, "FULL");

    println!("\n── no per-commit flush ──");
    let sk_norm = sekejap_run("sekejap  SyncMode::Normal", SyncMode::Normal);
    let lt_norm = sqlite_run("sqlite   synchronous=NORMAL", false, "NORMAL");

    println!("\n─────────────────────────────────────────────────────────────");
    println!("like for like (both F_FULLFSYNC):   sekejap {:.2}x sqlite", sk_full / lt_full);
    println!("like for like (neither flushing):   sekejap {:.2}x sqlite", sk_norm / lt_norm);
    println!();
    println!("the comparison that flatters sqlite (its plain fsync vs sekejap's barrier):");
    println!("                                    sekejap {:.2}x sqlite", sk_full / lt_fsync);
    println!();
    println!("F_FULLFSYNC costs sqlite {:.1}x what a plain fsync does on this disk.", lt_full / lt_fsync);
}
