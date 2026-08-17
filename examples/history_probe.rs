//! The write path, measured with an API old enough to compile against history.
//!
//! Run the same file at several commits and the numbers say whether a change made
//! something slower, rather than whether it feels slower. Nothing here touches
//! `Config` — the paged flags do not exist on older commits, and a probe that only
//! builds on today's code cannot answer a question about yesterday's.
//!
//! Run: `cargo run --release --example history_probe -- [rows]`

use rusqlite::Connection;
use sekejap::{CoreDB, SyncMode};
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

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1e3
}

fn main() {
    let n: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);
    let solo = 300usize;

    // SQLite, same rows, same durability, fully indexed, prepared statements.
    let sqdir = tempfile::TempDir::new().unwrap();
    let sq = Connection::open(sqdir.path().join("b.db")).unwrap();
    sq.pragma_update(None, "journal_mode", "WAL").unwrap();
    sq.pragma_update(None, "synchronous", "NORMAL").unwrap();
    sq.execute_batch(
        "CREATE TABLE venues (key TEXT PRIMARY KEY, name TEXT, category TEXT,
            suburb TEXT, price INTEGER, rating REAL);
         CREATE TABLE edges (src TEXT, dst TEXT, kind TEXT);").unwrap();
    let t = Instant::now();
    {
        let tx = sq.unchecked_transaction().unwrap();
        let mut st = tx.prepare("INSERT INTO venues VALUES (?1,?2,?3,?4,?5,?6)").unwrap();
        for i in 0..n {
            st.execute(rusqlite::params![format!("v{i}"), format!("Venue {i}"),
                CATEGORIES[i % CATEGORIES.len()], format!("suburb{}", i % 40),
                (i % 500) as i64, (i % 50) as f64 / 10.0]).unwrap();
        }
        drop(st); tx.commit().unwrap();
    }
    let sq_bulk = ms(t);
    let t = Instant::now();
    {
        let mut st = sq.prepare("INSERT INTO venues VALUES (?1,?2,?3,?4,?5,?6)").unwrap();
        for i in n..n + solo {
            st.execute(rusqlite::params![format!("v{i}"), format!("Venue {i}"),
                CATEGORIES[i % CATEGORIES.len()], format!("suburb{}", i % 40),
                (i % 500) as i64, (i % 50) as f64 / 10.0]).unwrap();
        }
    }
    let sq_insert_one = ms(t) / solo as f64 * 1000.0;
    sq.execute_batch(
        "CREATE INDEX i_cat ON venues(category);
         CREATE INDEX i_price ON venues(price);
         CREATE INDEX i_rating ON venues(rating);
         CREATE INDEX i_cs ON venues(category, suburb);
         CREATE INDEX i_esrc ON edges(src);").unwrap();
    let t = Instant::now();
    {
        let mut st = sq.prepare("UPDATE venues SET price = ?2 WHERE key = ?1").unwrap();
        for i in 0..solo {
            st.execute(rusqlite::params![format!("v{i}"), ((i % 500) + 7) as i64]).unwrap();
        }
    }
    let sq_update_one = ms(t) / solo as f64 * 1000.0;
    let t = Instant::now();
    {
        let mut st = sq.prepare("DELETE FROM venues WHERE key = ?1").unwrap();
        for i in 0..solo { st.execute(rusqlite::params![format!("v{}", n + i)]).unwrap(); }
    }
    let sq_delete_one = ms(t) / solo as f64 * 1000.0;
    let t = Instant::now();
    {
        let mut st = sq.prepare("SELECT name FROM venues WHERE key = ?1").unwrap();
        for i in 0..2_000 {
            let v: String = st.query_row(rusqlite::params![format!("v{}", i % n)], |r| r.get(0)).unwrap();
            std::hint::black_box(v);
        }
    }
    let sq_point = ms(t) / 2_000.0 * 1000.0;
    let mut sq_scan = |sql: &str| {
        let t = Instant::now();
        for _ in 0..20 {
            let mut st = sq.prepare(sql).unwrap();
            std::hint::black_box(st.query_map([], |r| r.get::<_, String>(0)).unwrap().count());
        }
        ms(t) / 20.0
    };
    let sq_eq = sq_scan("SELECT key FROM venues WHERE category = 'cafe'");
    let sq_range = sq_scan("SELECT key FROM venues WHERE price > 100 AND price <= 300");
    {
        let tx = sq.unchecked_transaction().unwrap();
        let mut st = tx.prepare("INSERT INTO edges VALUES (?1,?2,'related')").unwrap();
        for i in 0..n / 4 {
            st.execute(rusqlite::params![format!("v{i}"), format!("v{}", i + 1)]).unwrap();
        }
        drop(st); tx.commit().unwrap();
    }
    let t = Instant::now();
    {
        let mut st = sq.prepare(
            "SELECT v.key FROM edges e JOIN venues v ON v.key = e.dst \
             WHERE e.src = ?1 AND e.kind = 'related'").unwrap();
        for i in 0..2_000 {
            std::hint::black_box(st.query_map(
                rusqlite::params![format!("v{}", i % (n / 4))],
                |r| r.get::<_, String>(0)).unwrap().count());
        }
    }
    let sq_hop = ms(t) / 2_000.0 * 1000.0;
    let t = Instant::now();
    sq.pragma_update(None, "wal_checkpoint", "TRUNCATE").unwrap();
    let sq_settle = ms(t);

    let dir = tempfile::TempDir::new().unwrap();
    let mut db = CoreDB::open(dir.path()).unwrap();
    db.set_wal_sync(SyncMode::Normal);
    db.execute(
        "CREATE TABLE venues (_key TEXT PRIMARY KEY, name TEXT, category TEXT, \
         suburb TEXT, price INTEGER, rating REAL)",
    )
    .unwrap();

    let rows: Vec<(String, serde_json::Value)> = (0..n)
        .map(|i| (format!("venues/v{i}"), serde_json::from_str(&row_json(i)).unwrap()))
        .collect();
    let t = Instant::now();
    db.put_value_bulk(rows).unwrap();
    let bulk = ms(t);

    let t = Instant::now();
    for i in n..n + solo {
        db.put(&format!("venues/v{i}"), &row_json(i)).unwrap();
    }
    let insert_one = ms(t) / solo as f64 * 1000.0;

    db.execute("CREATE INDEX ON venues USING btree (price)").unwrap();
    db.execute("CREATE INDEX ON venues USING btree (rating)").unwrap();
    db.execute("CREATE INDEX ON venues USING btree (category)").unwrap();

    let t = Instant::now();
    db.compact().unwrap();
    let compact_first = ms(t);

    let t = Instant::now();
    for i in 0..solo {
        db.execute(&format!(
            "UPDATE venues SET price = {} WHERE _key = 'v{i}'",
            (i % 500) + 7
        ))
        .unwrap();
    }
    let update_one = ms(t) / solo as f64 * 1000.0;

    let t = Instant::now();
    for i in 0..solo {
        db.remove(&format!("venues/v{}", n + i));
    }
    let delete_one = ms(t) / solo as f64 * 1000.0;

    let t = Instant::now();
    for i in 0..2_000 {
        std::hint::black_box(db.get(&format!("venues/v{}", i % n)));
    }
    let point = ms(t) / 2_000.0 * 1000.0;

    let t = Instant::now();
    for _ in 0..20 {
        std::hint::black_box(
            db.query("SELECT _key FROM venues WHERE category = 'cafe'").unwrap().collect().len(),
        );
    }
    let eq_filter = ms(t) / 20.0;

    let t = Instant::now();
    for _ in 0..20 {
        std::hint::black_box(
            db.query("SELECT _key FROM venues WHERE price > 100 AND price <= 300")
                .unwrap().collect().len(),
        );
    }
    let range_filter = ms(t) / 20.0;

    let edges = n / 4;
    let links: Vec<(String, String, String)> = (0..edges)
        .map(|i| (format!("venues/v{i}"), format!("venues/v{}", i + 1), "related".into()))
        .collect();
    db.link_many(links.iter().map(|(a, b, c)| (a.as_str(), b.as_str(), c.as_str())));
    db.compact().unwrap();

    let t = Instant::now();
    for i in 0..2_000 {
        std::hint::black_box(
            db.one(&format!("venues/v{}", i % edges)).forward("related").collect().len(),
        );
    }
    let hop = ms(t) / 2_000.0 * 1000.0;

    // Steady state: 200 ordinary writes on a settled store, then fold them.
    for i in 0..200 {
        let k = n + solo + i;
        db.put(&format!("venues/v{k}"), &row_json(k)).unwrap();
    }
    let t = Instant::now();
    db.compact().unwrap();
    let compact_steady = ms(t);

    // One line per figure, tab separated, so several runs can be pasted together.
    println!("bulk_insert_ms\t{bulk:.2}\t{sq_bulk:.2}");
    println!("insert_one_us\t{insert_one:.2}\t{sq_insert_one:.2}");
    println!("update_one_us\t{update_one:.2}\t{sq_update_one:.2}");
    println!("delete_one_us\t{delete_one:.2}\t{sq_delete_one:.2}");
    println!("point_read_us\t{point:.2}\t{sq_point:.2}");
    println!("eq_filter_ms\t{eq_filter:.2}\t{sq_eq:.2}");
    println!("range_filter_ms\t{range_filter:.2}\t{sq_range:.2}");
    println!("hop_us\t{hop:.2}\t{sq_hop:.2}");
    println!("compact_first_ms\t{compact_first:.2}\t{sq_settle:.2}");
    println!("compact_steady_ms\t{compact_steady:.2}\t-");
}
