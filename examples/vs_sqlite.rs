//! sekejap against SQLite, across the operations both are asked to do.
//!
//! Run: `cargo run --release --example vs_sqlite -- [rows]`
//!
//! # What is being compared, and why it is a fair fight
//!
//! Three configurations, same data, same work:
//!
//! - **sqlite** — `rusqlite`, on disk, WAL journal, with an index on every column
//!   a query filters or sorts by, and bulk inserts wrapped in one transaction. A
//!   comparison that leaves SQLite un-indexed or commits per row is not a
//!   comparison, it is a way of getting the answer you wanted.
//! - **sekejap** — `CoreDB::open()`, the default as shipped.
//! - **sekejap paged** — the same with `paged_payloads`, `paged_adjacency` and
//!   `paged_nodes` on: the configuration where compaction no longer rebuilds the
//!   store. It is here because that work changed the write path, and a claim that
//!   it is free needs a number rather than an assurance.
//!
//! **Durability is matched, which is the part these comparisons usually get
//! wrong.** SQLite's `synchronous=NORMAL` under WAL fsyncs at a checkpoint rather
//! than per commit; sekejap's `SyncMode::Normal` is the same bargain. Both are set
//! to it. Running one at NORMAL against the other at FULL measures the setting,
//! not the engine — and on macOS the gap is enormous, because a real `F_FULLFSYNC`
//! is roughly a hundred times an `fsync` that only reaches the drive's cache.
//!
//! # What is not being compared
//!
//! Nothing here is a graph query beyond one hop, and nothing is spatial, vector or
//! text search. SQLite needs a recursive CTE for the first and extensions for the
//! rest, and pitting a built-in against an extension says more about packaging
//! than about either engine. `benches/mega_benchmark.rs` covers that ground.

use rusqlite::Connection;
use sekejap::{Config, CoreDB, SyncMode};
use serde_json::json;
use std::time::Instant;

const CATEGORIES: [&str; 8] = [
    "cafe", "restaurant", "hospital", "school", "park", "library", "gym", "market",
];

fn row_json(i: usize) -> String {
    json!({
        "_collection": "venues",
        "_key": format!("v{i}"),
        "name": format!("Venue {i}"),
        "category": CATEGORIES[i % CATEGORIES.len()],
        "suburb": format!("suburb{}", i % 40),
        "price": (i % 500) as i64,
        "rating": (i % 50) as f64 / 10.0,
    })
    .to_string()
}

/// One measured figure, with the winner decided at the end.
struct Case {
    name: &'static str,
    sqlite_ms: f64,
    sekejap_ms: f64,
    paged_ms: f64,
    /// Whether the SQLite figure measures the same thing. A WAL checkpoint and a
    /// compaction both make a store settled, and they are not the same operation
    /// or the same amount of work — printing a ratio between them would be a
    /// number that means nothing.
    comparable: bool,
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

// ── SQLite ───────────────────────────────────────────────────────────────────

fn sqlite_open(path: &std::path::Path) -> Connection {
    let c = Connection::open(path).unwrap();
    // WAL + NORMAL: fsync at checkpoint, not per commit — the same bargain
    // sekejap's SyncMode::Normal makes, so the two are measuring the same promise.
    c.pragma_update(None, "journal_mode", "WAL").unwrap();
    c.pragma_update(None, "synchronous", "NORMAL").unwrap();
    c.execute_batch(
        "CREATE TABLE venues (
            key TEXT PRIMARY KEY, name TEXT, category TEXT,
            suburb TEXT, price INTEGER, rating REAL);
         CREATE TABLE edges (src TEXT, dst TEXT, kind TEXT);",
    )
    .unwrap();
    c
}

fn sqlite_index(c: &Connection) {
    // Every column a query below filters, sorts or joins on.
    c.execute_batch(
        "CREATE INDEX i_cat    ON venues(category);
         CREATE INDEX i_price  ON venues(price);
         CREATE INDEX i_rating ON venues(rating);
         CREATE INDEX i_cs     ON venues(category, suburb);
         CREATE INDEX i_esrc   ON edges(src);
         CREATE INDEX i_edst   ON edges(dst);",
    )
    .unwrap();
}

// ── sekejap ──────────────────────────────────────────────────────────────────

fn sekejap_open(path: &std::path::Path, paged: bool) -> CoreDB {
    let mut db = CoreDB::open_with_config(
        path,
        Config {
            paged_topology: paged,
            paged_payloads: paged,
            paged_adjacency: paged,
            paged_nodes: paged,
            ..Config::default()
        },
    )
    .unwrap();
    db.set_wal_sync(SyncMode::Normal);
    db.execute(
        "CREATE TABLE venues (_key TEXT PRIMARY KEY, name TEXT, category TEXT, \
         suburb TEXT, price INTEGER, rating REAL)",
    )
    .unwrap();
    db
}

fn sekejap_index(db: &mut CoreDB) {
    db.execute("CREATE INDEX ON venues USING btree (price)").unwrap();
    db.execute("CREATE INDEX ON venues USING btree (rating)").unwrap();
    db.execute("CREATE INDEX ON venues USING btree (category)").unwrap();
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);
    let edges = n / 4;

    println!("\n  sekejap vs SQLite — {n} rows, {edges} edges");
    println!("  both at WAL + synchronous NORMAL (fsync at checkpoint, not per commit)\n");

    let d_sq = tempfile::TempDir::new().unwrap();
    let d_sk = tempfile::TempDir::new().unwrap();
    let d_pg = tempfile::TempDir::new().unwrap();

    let mut cases: Vec<Case> = Vec::new();
    let sq = sqlite_open(&d_sq.path().join("bench.db"));
    let mut sk = sekejap_open(d_sk.path(), false);
    let mut pg = sekejap_open(d_pg.path(), true);

    // ── 1. bulk insert ───────────────────────────────────────────────────────
    // SQLite in one transaction with a prepared statement, which is how anyone
    // loading data into it would actually do it. sekejap through put_value_bulk,
    // its equivalent.
    let t = Instant::now();
    {
        let tx = sq.unchecked_transaction().unwrap();
        let mut st = tx
            .prepare("INSERT INTO venues VALUES (?1,?2,?3,?4,?5,?6)")
            .unwrap();
        for i in 0..n {
            st.execute(rusqlite::params![
                format!("v{i}"),
                format!("Venue {i}"),
                CATEGORIES[i % CATEGORIES.len()],
                format!("suburb{}", i % 40),
                (i % 500) as i64,
                (i % 50) as f64 / 10.0,
            ])
            .unwrap();
        }
        drop(st);
        tx.commit().unwrap();
    }
    let sq_bulk = ms(t);

    let rows: Vec<(String, serde_json::Value)> = (0..n)
        .map(|i| {
            (
                format!("venues/v{i}"),
                serde_json::from_str(&row_json(i)).unwrap(),
            )
        })
        .collect();
    let t = Instant::now();
    sk.put_value_bulk(rows.clone()).unwrap();
    let sk_bulk = ms(t);
    let t = Instant::now();
    pg.put_value_bulk(rows).unwrap();
    let pg_bulk = ms(t);
    cases.push(Case {
        name: "bulk insert",
        sqlite_ms: sq_bulk,
        sekejap_ms: sk_bulk,
        paged_ms: pg_bulk,
        comparable: true,
    });

    // ── 2. single-row inserts ────────────────────────────────────────────────
    // The shape a running application actually has: one row, committed, repeat.
    // This is where today's storage change would show up if it cost anything.
    let single = 2_000usize;
    let t = Instant::now();
    {
        // Prepared once and reused, as SQLite is meant to be used — re-preparing
        // per row would be measuring the parser.
        let mut st = sq.prepare("INSERT INTO venues VALUES (?1,?2,?3,?4,?5,?6)").unwrap();
        for i in n..n + single {
            st.execute(rusqlite::params![
                format!("v{i}"),
                format!("Venue {i}"),
                CATEGORIES[i % CATEGORIES.len()],
                format!("suburb{}", i % 40),
                (i % 500) as i64,
                (i % 50) as f64 / 10.0,
            ])
            .unwrap();
        }
    }
    let sq_one = ms(t) / single as f64 * 1000.0; // µs per row
    let t = Instant::now();
    for i in n..n + single {
        sk.put(&format!("venues/v{i}"), &row_json(i)).unwrap();
    }
    let sk_one = ms(t) / single as f64 * 1000.0;
    let t = Instant::now();
    for i in n..n + single {
        pg.put(&format!("venues/v{i}"), &row_json(i)).unwrap();
    }
    let pg_one = ms(t) / single as f64 * 1000.0;
    cases.push(Case {
        name: "single insert (us/row)",
        sqlite_ms: sq_one,
        sekejap_ms: sk_one,
        paged_ms: pg_one,
        comparable: true,
    });

    let total = n + single;

    // ── indexes, then settle both stores ─────────────────────────────────────
    sqlite_index(&sq);
    sekejap_index(&mut sk);
    sekejap_index(&mut pg);
    let t = Instant::now();
    sk.compact().unwrap();
    let sk_settle = ms(t);
    let t = Instant::now();
    pg.compact().unwrap();
    let pg_settle = ms(t);
    // SQLite's equivalent of "fold what has accumulated into the main file" is a
    // WAL checkpoint. It is not the same operation, but it is the one that has to
    // happen before the store is settled, so it is the honest thing to time.
    let t = Instant::now();
    sq.pragma_update(None, "wal_checkpoint", "TRUNCATE").unwrap();
    let sq_settle = ms(t);
    cases.push(Case {
        name: "settle after bulk load",
        sqlite_ms: sq_settle,
        sekejap_ms: sk_settle,
        paged_ms: pg_settle,
        comparable: false,
    });

    // The same operation again, on a store that is already settled and has taken
    // 200 ordinary writes. This is the number the paged design exists for, and the
    // one above is not it: a bulk load lands in the write overlay, so the first
    // compaction after it folds every row that was just loaded. Both are real, and
    // reporting only one of them would misrepresent the design in one direction or
    // the other.
    for i in 0..200 {
        let k = total + i;
        sk.put(&format!("venues/v{k}"), &row_json(k)).unwrap();
        pg.put(&format!("venues/v{k}"), &row_json(k)).unwrap();
        sq.execute(
            "INSERT INTO venues VALUES (?1,?2,?3,?4,?5,?6)",
            rusqlite::params![
                format!("v{k}"), format!("Venue {k}"), CATEGORIES[k % CATEGORIES.len()],
                format!("suburb{}", k % 40), (k % 500) as i64, (k % 50) as f64 / 10.0,
            ],
        ).unwrap();
    }
    let t = Instant::now();
    sq.pragma_update(None, "wal_checkpoint", "TRUNCATE").unwrap();
    let sq_s2 = ms(t);
    let t = Instant::now();
    sk.compact().unwrap();
    let sk_s2 = ms(t);
    let t = Instant::now();
    pg.compact().unwrap();
    let pg_s2 = ms(t);
    cases.push(Case {
        name: "settle after 200 writes",
        sqlite_ms: sq_s2,
        sekejap_ms: sk_s2,
        paged_ms: pg_s2,
        comparable: false,
    });

    // ── 3. reads ─────────────────────────────────────────────────────────────
    macro_rules! read_case {
        ($name:expr, $sql:expr, $reps:expr) => {{
            let t = Instant::now();
            for _ in 0..$reps {
                let mut st = sq.prepare($sql).unwrap();
                let count = st
                    .query_map([], |r| r.get::<_, String>(0))
                    .unwrap()
                    .filter_map(|x| x.ok())
                    .count();
                std::hint::black_box(count);
            }
            let a = ms(t) / $reps as f64;
            let t = Instant::now();
            for _ in 0..$reps {
                std::hint::black_box(sk.query($sql).unwrap().collect().len());
            }
            let b = ms(t) / $reps as f64;
            let t = Instant::now();
            for _ in 0..$reps {
                std::hint::black_box(pg.query($sql).unwrap().collect().len());
            }
            let c = ms(t) / $reps as f64;
                cases.push(Case { name: $name, sqlite_ms: a, sekejap_ms: b, paged_ms: c,
                              comparable: true });
        }};
    }

    read_case!(
        "eq filter",
        "SELECT key FROM venues WHERE category = 'cafe'",
        20
    );
    read_case!(
        "range filter",
        "SELECT key FROM venues WHERE price > 100 AND price <= 300",
        20
    );
    read_case!(
        "compound filter",
        "SELECT key FROM venues WHERE category = 'cafe' AND suburb = 'suburb7'",
        20
    );
    read_case!(
        "sort + limit",
        "SELECT key FROM venues ORDER BY rating DESC LIMIT 50",
        20
    );
    read_case!("count(*)", "SELECT COUNT(*) FROM venues", 20);

    // Point lookup, by the primary key, one row at a time.
    let reps = 2_000;
    let t = Instant::now();
    for i in 0..reps {
        let mut st = sq.prepare("SELECT name FROM venues WHERE key = ?1").unwrap();
        let v: String = st
            .query_row(rusqlite::params![format!("v{}", i % total)], |r| r.get(0))
            .unwrap();
        std::hint::black_box(v);
    }
    let a = ms(t) / reps as f64 * 1000.0;
    let t = Instant::now();
    for i in 0..reps {
        std::hint::black_box(sk.get(&format!("venues/v{}", i % total)));
    }
    let b = ms(t) / reps as f64 * 1000.0;
    let t = Instant::now();
    for i in 0..reps {
        std::hint::black_box(pg.get(&format!("venues/v{}", i % total)));
    }
    let c = ms(t) / reps as f64 * 1000.0;
    cases.push(Case {
        name: "point lookup (us)",
        sqlite_ms: a,
        sekejap_ms: b,
        paged_ms: c,
        comparable: true,
    });

    // ── 4. update and delete ─────────────────────────────────────────────────
    //
    // Two fairness traps here, both of which this got wrong before.
    //
    // *Batching.* Giving SQLite one transaction for a thousand operations while
    // the other side commits each one separately compares transactions, not
    // engines. Both shapes are measured instead, matched on both sides: an import
    // batches, a request handler does not.
    //
    // *Semantics.* `UPDATE ... SET price` changes one column. Reaching for
    // sekejap's `put` instead makes it rewrite the whole row and every index entry
    // on it — a different, larger operation that happens to have the same effect.
    // The comparison is against sekejap's own `UPDATE`, which is the same
    // statement doing the same thing.
    let churn = 1_000usize;

    // Set-based: one statement, many rows. What both engines are actually built to
    // do, and the shape with no per-row overhead on either side.
    let t = Instant::now();
    sq.execute("UPDATE venues SET price = price + 1 WHERE price < 20", []).unwrap();
    let a = ms(t);
    let mut sek_set_update = |db: &mut CoreDB| {
        let t = Instant::now();
        db.execute("UPDATE venues SET rating = 1.5 WHERE price < 20").unwrap();
        ms(t)
    };
    let b = sek_set_update(&mut sk);
    let c = sek_set_update(&mut pg);
    cases.push(Case { name: "update, set-based (ms)", sqlite_ms: a,
                      sekejap_ms: b, paged_ms: c, comparable: true });

    // Per key, one statement at a time, auto-commit on both sides.
    let solo = 300usize;
    let t = Instant::now();
    {
        let mut st = sq.prepare("UPDATE venues SET price = ?2 WHERE key = ?1").unwrap();
        for i in 0..solo {
            st.execute(rusqlite::params![format!("v{i}"), ((i % 500) + 7) as i64]).unwrap();
        }
    }
    let a = ms(t) / solo as f64 * 1000.0;
    let mut sek_solo_update = |db: &mut CoreDB| {
        let t = Instant::now();
        for i in 0..solo {
            db.execute(&format!(
                "UPDATE venues SET price = {} WHERE _key = 'v{i}'", (i % 500) + 7)).unwrap();
        }
        ms(t) / solo as f64 * 1000.0
    };
    let b = sek_solo_update(&mut sk);
    let c = sek_solo_update(&mut pg);
    cases.push(Case { name: "update, per key (us/row)", sqlite_ms: a,
                      sekejap_ms: b, paged_ms: c, comparable: true });

    // Delete, set-based.
    let t = Instant::now();
    sq.execute("DELETE FROM venues WHERE price = 499", []).unwrap();
    let a = ms(t);
    let mut sek_set_delete = |db: &mut CoreDB| {
        let t = Instant::now();
        db.execute("DELETE FROM venues WHERE price = 499").unwrap();
        ms(t)
    };
    let b = sek_set_delete(&mut sk);
    let c = sek_set_delete(&mut pg);
    cases.push(Case { name: "delete, set-based (ms)", sqlite_ms: a,
                      sekejap_ms: b, paged_ms: c, comparable: true });

    // Delete, per key, auto-commit on both.
    let t = Instant::now();
    {
        let mut st = sq.prepare("DELETE FROM venues WHERE key = ?1").unwrap();
        for i in 0..solo {
            st.execute(rusqlite::params![format!("v{}", n + i)]).unwrap();
        }
    }
    let a = ms(t) / solo as f64 * 1000.0;
    let mut sek_solo_delete = |db: &mut CoreDB| {
        let t = Instant::now();
        for i in 0..solo { db.remove(&format!("venues/v{}", n + i)); }
        ms(t) / solo as f64 * 1000.0
    };
    let b = sek_solo_delete(&mut sk);
    let c = sek_solo_delete(&mut pg);
    cases.push(Case { name: "delete, per key (us/row)", sqlite_ms: a,
                      sekejap_ms: b, paged_ms: c, comparable: true });

    // Batched writes, matched: one transaction each, same operations.
    let t = Instant::now();
    {
        let tx = sq.unchecked_transaction().unwrap();
        let mut st = tx.prepare("DELETE FROM venues WHERE key = ?1").unwrap();
        for i in 0..churn {
            st.execute(rusqlite::params![format!("v{}", n + solo + i)]).unwrap();
        }
        drop(st);
        tx.commit().unwrap();
    }
    let a = ms(t) / churn as f64 * 1000.0;
    let mut sek_batch_delete = |db: &mut CoreDB| {
        let t = Instant::now();
        let mut tx = db.begin();
        for i in 0..churn { tx.remove(&format!("venues/v{}", n + solo + i)); }
        tx.commit().unwrap();
        ms(t) / churn as f64 * 1000.0
    };
    let b = sek_batch_delete(&mut sk);
    let c = sek_batch_delete(&mut pg);
    cases.push(Case { name: "delete, in a txn (us/row)", sqlite_ms: a,
                      sekejap_ms: b, paged_ms: c, comparable: true });

    // ── 5. one graph hop ─────────────────────────────────────────────────────
    // SQLite has no adjacency, so this is the join it would have to do, with both
    // directions indexed. One hop is where a join is still perfectly reasonable —
    // the gap opens further out, which this deliberately does not claim.
    {
        let tx = sq.unchecked_transaction().unwrap();
        for i in 0..edges {
            tx.execute(
                "INSERT INTO edges VALUES (?1,?2,'related')",
                rusqlite::params![format!("v{i}"), format!("v{}", i + 1)],
            )
            .unwrap();
        }
        tx.commit().unwrap();
    }
    let links: Vec<(String, String, String)> = (0..edges)
        .map(|i| (format!("venues/v{i}"), format!("venues/v{}", i + 1), "related".into()))
        .collect();
    for db in [&mut sk, &mut pg] {
        db.link_many(links.iter().map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str())));
    }

    let reps = 2_000;
    let t = Instant::now();
    for i in 0..reps {
        let mut st = sq
            .prepare(
                "SELECT v.key FROM edges e JOIN venues v ON v.key = e.dst \
                 WHERE e.src = ?1 AND e.kind = 'related'",
            )
            .unwrap();
        let c = st
            .query_map(rusqlite::params![format!("v{}", i % edges)], |r| {
                r.get::<_, String>(0)
            })
            .unwrap()
            .count();
        std::hint::black_box(c);
    }
    let a = ms(t) / reps as f64 * 1000.0;
    let t = Instant::now();
    for i in 0..reps {
        std::hint::black_box(
            sk.one(&format!("venues/v{}", i % edges)).forward("related").collect().len(),
        );
    }
    let b = ms(t) / reps as f64 * 1000.0;
    let t = Instant::now();
    for i in 0..reps {
        std::hint::black_box(
            pg.one(&format!("venues/v{}", i % edges)).forward("related").collect().len(),
        );
    }
    let c = ms(t) / reps as f64 * 1000.0;
    cases.push(Case {
        name: "1-hop traversal (us)",
        sqlite_ms: a,
        sekejap_ms: b,
        paged_ms: c,
        comparable: true,
    });

    // ── report ───────────────────────────────────────────────────────────────
    println!(
        "  {:<28}{:>12}{:>12}{:>12}{:>10}{:>10}",
        "", "sqlite", "sekejap", "sk paged", "vs sqlite", "paged"
    );
    println!("  {}", "-".repeat(86));
    let mut wins = 0;
    let mut losses = 0;
    for c in &cases {
        if !c.comparable {
            println!(
                "  {:<28}{:>12.2}{:>12.2}{:>12.2}{:>10}{:>10}",
                c.name, c.sqlite_ms, c.sekejap_ms, c.paged_ms, "-", "-"
            );
            continue;
        }
        let ratio = c.sqlite_ms / c.sekejap_ms;
        let paged_ratio = c.sqlite_ms / c.paged_ms;
        if ratio >= 1.0 { wins += 1 } else { losses += 1 }
        println!(
            "  {:<28}{:>12.2}{:>12.2}{:>12.2}{:>9.2}x{:>9.2}x",
            c.name, c.sqlite_ms, c.sekejap_ms, c.paged_ms, ratio, paged_ratio
        );
    }
    println!("  {}", "-".repeat(86));
    println!(
        "\n  'vs sqlite' is how many times faster sekejap is; below 1.00x is a loss.\n  \
         {wins} of {} comparable cases faster, {losses} slower.",
        wins + losses
    );
    println!(
        "  The two settle rows carry no ratio: a WAL checkpoint and a compaction\n           both leave a store settled, and they are not the same work."
    );
    println!(
        "  'paged' is the same ratio for the configuration where compaction no\n  \
         longer rebuilds the store — the column that says what that cost."
    );
}
