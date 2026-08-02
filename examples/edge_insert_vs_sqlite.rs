//! Insertion-rate discrepancy: baseline vs keyed edges, anchored to SQLite.
//!
//! Answers two questions with real numbers (all in-memory, no fsync, apples-to-apples):
//!   1. How much does adding a `_key` (upsert dedup) cost EDGE insertion?
//!   2. Does it affect NODE insertion? (Spoiler: no — different code path.)
//!
//! SQLite anchor: plain INSERT vs INSERT into a UNIQUE-indexed table (ON CONFLICT)
//! — i.e. exactly "how much does a key/upsert cost SQLite" in a familiar system.

use rusqlite::Connection;
use sekejap::CoreDB;
use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::time::Instant;

fn rate(n: usize, e: std::time::Duration) -> f64 { n as f64 / e.as_secs_f64() / 1e6 }

fn main() {
    let n = 500_000usize;
    println!("== insertion rate: baseline vs keyed edge, N={n} (in-memory) ==\n");

    // ── NODES (the path the edge-key feature does NOT touch) ──
    // Spread across 1000 collections to avoid the O(N^2) single-collection
    // membership scan (a separate pre-existing issue, measured below).
    let node_pairs: Vec<(String, String)> = (0..n)
        .map(|i| (format!("c{}/{i}", i % 1000),
                  format!(r#"{{"_collection":"c{}","_key":"{i}","v":{i}}}"#, i % 1000)))
        .collect();
    let mut db = CoreDB::new();
    let t = Instant::now();
    db.put_many(node_pairs.iter().map(|(s, j)| (s.as_str(), j.as_str()))).unwrap();
    let node_rate = rate(n, t.elapsed());
    println!("NODE insert (put_many, spread)         : {:>6.2} M/s   ← unchanged by edge-key", node_rate);

    // Same N into ONE collection — was O(N^2) (`members.contains` scan), now O(N).
    let one: Vec<(String, String)> = (0..n)
        .map(|i| (format!("solo/{i}"), format!(r#"{{"_collection":"solo","_key":"{i}"}}"#))).collect();
    let mut db2 = CoreDB::new();
    let t = Instant::now();
    db2.put_many(one.iter().map(|(s, j)| (s.as_str(), j.as_str()))).unwrap();
    println!("NODE insert (put_many, 1 collection)   : {:>6.2} M/s   ← was O(N^2), now O(N), N={}", rate(n, t.elapsed()), n);

    // Prebuild endpoint slugs so edge benches don't pay string formatting in the timed loop.
    let froms: Vec<String> = (0..n).map(|i| format!("t/{}", i % 10_000)).collect(); // some shared sources
    let tos: Vec<String> = (0..n).map(|i| format!("t/{i}")).collect();

    // ── EDGE baseline: naked (current fastest) ──
    let mut db = CoreDB::new();
    let naked: Vec<(&str, &str, &str)> = (0..n).map(|i| (froms[i].as_str(), tos[i].as_str(), "rel")).collect();
    let t = Instant::now();
    db.link_many(naked.iter().copied());
    let naked_rate = rate(n, t.elapsed());
    println!("EDGE insert naked (link_many)          : {:>6.2} M/s   ← baseline", naked_rate);

    // ── EDGE attributed, no key (fair attributed baseline) ──
    let mut db = CoreDB::new();
    let metas: Vec<(String, String, String, String)> = (0..n)
        .map(|i| (froms[i].clone(), tos[i].clone(), "rel".into(), format!(r#"{{"since":{i}}}"#))).collect();
    let t = Instant::now();
    db.link_meta_many(metas.iter().map(|(f, t, ty, m)| (f.as_str(), t.as_str(), ty.as_str(), Some(m.as_str())))).unwrap();
    let attr_rate = rate(n, t.elapsed());
    println!("EDGE insert attributed (link_meta_many): {:>6.2} M/s   ← +JSON attr, no key", attr_rate);

    // ── EDGE keyed upsert: sk_hash-style key index (dedup) + attributed store ──
    let mut db = CoreDB::new();
    let keyed: Vec<(String, String, String, String)> = (0..n)
        .map(|i| (froms[i].clone(), tos[i].clone(), "rel".into(), format!(r#"{{"_key":"k{i}","since":{i}}}"#))).collect();
    let mut idx: HashSet<u64> = HashSet::with_capacity(n);
    let t = Instant::now();
    // dedup pre-pass (the ONLY new work) then batch-insert the new ones
    let mut fresh: Vec<(&str, &str, &str, Option<&str>)> = Vec::with_capacity(n);
    for (i, (f, to, ty, m)) in keyed.iter().enumerate() {
        // one hash of (from, type, key) = the edge identity (real engine uses seahash, faster)
        let mut hs = DefaultHasher::new();
        (f.as_str(), ty.as_str(), i as u64).hash(&mut hs);
        if idx.insert(hs.finish()) {
            fresh.push((f.as_str(), to.as_str(), ty.as_str(), Some(m.as_str())));
        }
    }
    db.link_meta_many(fresh.into_iter()).unwrap();
    let keyed_rate = rate(n, t.elapsed());
    println!("EDGE insert KEYED (dedup+upsert)       : {:>6.2} M/s   ← the new path", keyed_rate);

    // ── SQLite anchor ──
    println!("\n== SQLite anchor (in-memory, same N) ==");
    let sq_plain = sqlite_insert(n, false);
    let sq_upsert = sqlite_insert(n, true);
    println!("SQLite INSERT plain                    : {:>6.2} M/s", sq_plain);
    println!("SQLite INSERT upsert (UNIQUE + ON CONFLICT): {:>6.2} M/s", sq_upsert);

    // ── discrepancies ──
    println!("\n== DISCREPANCY ==");
    println!("edge keyed / naked baseline   : {:.2}x  ({:+.0}%)",
        keyed_rate / naked_rate, (keyed_rate / naked_rate - 1.0) * 100.0);
    println!("edge keyed / attributed base  : {:.2}x  ({:+.0}%)",
        keyed_rate / attr_rate, (keyed_rate / attr_rate - 1.0) * 100.0);
    println!("SQLite upsert / plain         : {:.2}x  ({:+.0}%)  ← the same 'add a key' tax in SQLite",
        sq_upsert / sq_plain, (sq_upsert / sq_plain - 1.0) * 100.0);
    let _ = node_rate;
}

fn sqlite_insert(n: usize, upsert: bool) -> f64 {
    let c = Connection::open_in_memory().unwrap();
    c.execute_batch("PRAGMA synchronous=OFF; PRAGMA journal_mode=MEMORY;").unwrap();
    if upsert {
        c.execute("CREATE TABLE e (frm TEXT, typ TEXT, k TEXT, tgt TEXT, PRIMARY KEY(frm,typ,k))", []).unwrap();
    } else {
        c.execute("CREATE TABLE e (frm TEXT, typ TEXT, k TEXT, tgt TEXT)", []).unwrap();
    }
    let sql = if upsert {
        "INSERT INTO e (frm,typ,k,tgt) VALUES (?1,?2,?3,?4) ON CONFLICT(frm,typ,k) DO UPDATE SET tgt=excluded.tgt"
    } else {
        "INSERT INTO e (frm,typ,k,tgt) VALUES (?1,?2,?3,?4)"
    };
    let t = Instant::now();
    let tx = c.unchecked_transaction().unwrap();
    {
        let mut stmt = tx.prepare(sql).unwrap();
        for i in 0..n {
            let frm = format!("t/{}", i % 10_000);
            stmt.execute(rusqlite::params![frm, "rel", format!("k{i}"), format!("t/{i}")]).unwrap();
        }
    }
    tx.commit().unwrap();
    rate(n, t.elapsed())
}
