//! The A1 measurement (0.19): `LIKE` / `ILIKE` with a trigram index, against
//! the same statements with no index and against PostgreSQL's pg_trgm.
//!
//! One machine, one generated table, the same rows in both engines:
//!
//! * sekejap, no index: the row check (`lang/tests/sql_like.rs`);
//! * sekejap, `USING gin (name gin_trgm_ops)`, built after the rows exist;
//! * PostgreSQL, no index (a sequential scan);
//! * PostgreSQL, `USING gin (name gin_trgm_ops)`;
//! * SQLite, no index: `LIKE` made case-sensitive (`PRAGMA case_sensitive_like`)
//!   so it asks what PostgreSQL's `LIKE` asks, and case-insensitive for `ILIKE`;
//! * SQLite, FTS5's trigram tokenizer over the table (external content): a
//!   `case_sensitive 1` index answers `GLOB` (SQLite's case-sensitive match), a
//!   `case_sensitive 0` one answers `LIKE` -- how SQLite speeds up an infix
//!   match. A pattern under three characters scans the table in both.
//!
//! Every query's ROW COUNT is compared across the four arms before any time
//! is reported: a number over a different answer is not a comparison.
//!
//! ```text
//! trigram_like ROWS WORK_DIR [PG_SOCKET_DIR PG_PORT]
//! ```
//!
//! Without the PostgreSQL arguments only the sekejap arms run. Prints one JSON
//! object: per arm the build time and index bytes, per query the median of
//! five timed runs after one warm run, in milliseconds.

use std::fs;
use std::path::Path;
use std::time::Instant;

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::collections::Database;
use sekejap_lang::{SqlDatabase, SqlResult};
use serde_json::{json, Value};

fn cfg() -> Config {
    Config {
        budget_bytes: 64 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

/// The table: a place name the way a directory lists one. Deterministic, so
/// both engines hold the same bytes.
fn name(i: u64) -> String {
    const KIND: &[&str] = &["Warung", "Villa", "Cafe", "Pura", "Pantai", "Pasar", "Homestay", "Studio"];
    const WORD: &[&str] = &[
        "Sunset", "Lotus", "Bamboo", "Coral", "Frangipani", "Jasmine", "Mango", "Rice Field", "Ocean",
        "Temple", "Garden", "Harbour", "Lagoon", "Cliff", "Banyan", "Monsoon",
    ];
    const AREA: &[&str] = &["Ubud", "Canggu", "Seminyak", "Sanur", "Amed", "Lovina", "Uluwatu", "Munduk"];
    // A multiplicative scramble so neighbours differ.
    let x = i.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!(
        "{} {} {} {}",
        KIND[(x % 8) as usize],
        WORD[((x >> 8) % 16) as usize],
        AREA[((x >> 16) % 8) as usize],
        i
    )
}

/// `(label, statement)`: a rare infix, a common infix, the same common word
/// under ILIKE, a two-character pattern (no piece: a row check even with the
/// index), and a prefix.
fn queries(rows: u64) -> Vec<(&'static str, String)> {
    let rare = rows / 2 + 7;
    vec![
        ("rare_infix", format!("SELECT count(*) FROM place WHERE name LIKE '% {rare}'")),
        ("common_infix", "SELECT count(*) FROM place WHERE name LIKE '%Lagoon%'".into()),
        ("common_ilike", "SELECT count(*) FROM place WHERE name ILIKE '%lagoon ubud%'".into()),
        ("two_chars", "SELECT count(*) FROM place WHERE name LIKE '%oo%'".into()),
        ("prefix", "SELECT count(*) FROM place WHERE name LIKE 'Villa Coral%'".into()),
    ]
}

fn dir_bytes(root: &Path) -> u64 {
    fs::read_dir(root)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0)
}

fn run(db: &mut Database, sql: &str) -> SqlResult {
    db.sql(sql, &[]).unwrap_or_else(|e| panic!("`{sql}`: {e}"))
}

fn count(db: &mut Database, sql: &str) -> i64 {
    match run(db, sql) {
        SqlResult::Rows { rows, .. } => match rows[0].values[0] {
            sekejap_lang::SqlValue::Int(n) => n,
            ref other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

fn median(mut times: Vec<f64>) -> f64 {
    times.sort_by(f64::total_cmp);
    times[times.len() / 2]
}

/// One warm run, then five timed; the count every run answered.
fn time_query(mut once: impl FnMut() -> i64) -> (i64, f64) {
    let answer = once();
    let mut times = Vec::new();
    for _ in 0..5 {
        let start = Instant::now();
        assert_eq!(once(), answer, "an answer changed between runs");
        times.push(start.elapsed().as_secs_f64() * 1e3);
    }
    (answer, median(times))
}

fn sekejap_arm(rows: u64, dir: &Path, indexed: bool) -> Value {
    let _ = fs::remove_dir_all(dir);
    let mut db = Database::create(dir, cfg()).unwrap();
    run(&mut db, "CREATE TABLE place (_key TEXT PRIMARY KEY, name TEXT) WITH (index: none)");
    let load = Instant::now();
    for chunk in (0..rows).collect::<Vec<_>>().chunks(1000) {
        let values: Vec<String> = chunk.iter().map(|i| format!("('p{i}', '{}')", name(*i))).collect();
        run(&mut db, &format!("INSERT INTO place (_key, name) VALUES {}", values.join(", ")));
    }
    run(&mut db, "COMMIT");
    db.checkpoint().unwrap();
    let load_ms = load.elapsed().as_secs_f64() * 1e3;
    let before = dir_bytes(dir);
    let mut build_ms = 0.0;
    if indexed {
        let start = Instant::now();
        run(&mut db, "CREATE INDEX place_name_trgm ON place USING gin (name gin_trgm_ops)");
        run(&mut db, "COMMIT");
        db.checkpoint().unwrap();
        build_ms = start.elapsed().as_secs_f64() * 1e3;
    }
    let after = dir_bytes(dir);
    let mut out = serde_json::Map::new();
    for (label, sql) in queries(rows) {
        let (answer, ms) = time_query(|| count(&mut db, &sql));
        out.insert(label.into(), json!({"rows": answer, "median_ms": ms}));
    }
    json!({
        "load_ms": load_ms,
        "build_ms": build_ms,
        "table_bytes": before,
        "index_bytes": after.saturating_sub(before),
        "queries": out,
    })
}

fn pg_arm(rows: u64, socket: &str, port: &str, indexed: bool) -> Value {
    let mut client = postgres::Config::new()
        .host(socket)
        .port(port.parse().unwrap())
        .user("bench")
        .dbname("postgres")
        .connect(postgres::NoTls)
        .unwrap();
    client
        .batch_execute("CREATE EXTENSION IF NOT EXISTS pg_trgm; DROP TABLE IF EXISTS place; CREATE TABLE place (key text PRIMARY KEY, name text)")
        .unwrap();
    let load = Instant::now();
    for chunk in (0..rows).collect::<Vec<_>>().chunks(1000) {
        let values: Vec<String> = chunk.iter().map(|i| format!("('p{i}', '{}')", name(*i))).collect();
        client
            .batch_execute(&format!("INSERT INTO place (key, name) VALUES {}", values.join(", ")))
            .unwrap();
    }
    client.batch_execute("VACUUM ANALYZE place").unwrap();
    let load_ms = load.elapsed().as_secs_f64() * 1e3;
    let table: i64 = client
        .query_one("SELECT pg_total_relation_size('place')", &[])
        .unwrap()
        .get(0);
    let mut build_ms = 0.0;
    let mut index_bytes = 0i64;
    if indexed {
        let start = Instant::now();
        client
            .batch_execute("CREATE INDEX place_name_trgm ON place USING gin (name gin_trgm_ops); ANALYZE place")
            .unwrap();
        build_ms = start.elapsed().as_secs_f64() * 1e3;
        index_bytes = client
            .query_one("SELECT pg_relation_size('place_name_trgm')", &[])
            .unwrap()
            .get(0);
    }
    let mut out = serde_json::Map::new();
    for (label, sql) in queries(rows) {
        let (answer, ms) = time_query(|| client.query_one(sql.as_str(), &[]).unwrap().get::<_, i64>(0));
        out.insert(label.into(), json!({"rows": answer, "median_ms": ms}));
    }
    json!({
        "load_ms": load_ms,
        "build_ms": build_ms,
        "table_bytes": table,
        "index_bytes": index_bytes,
        "queries": out,
    })
}

/// SQLite's statement for one query: `GLOB` for a case-sensitive pattern on
/// the indexed arm, `LIKE` otherwise.
fn sqlite_sql(label: &str, sql: &str, indexed: bool) -> String {
    let pattern = sql.split('\'').nth(1).unwrap();
    let ilike = sql.contains("ILIKE");
    match (indexed, label, ilike) {
        (true, "two_chars", _) | (false, _, _) => {
            format!("SELECT count(*) FROM place WHERE name LIKE '{pattern}'")
        }
        (true, _, true) => format!("SELECT count(*) FROM place_ci WHERE name LIKE '{pattern}'"),
        (true, _, false) => format!(
            "SELECT count(*) FROM place_cs WHERE name GLOB '{}'",
            pattern.replace('%', "*").replace('_', "?")
        ),
    }
}

fn sqlite_arm(rows: u64, dir: &Path, indexed: bool) -> Value {
    let _ = fs::remove_dir_all(dir);
    fs::create_dir_all(dir).unwrap();
    let path = dir.join("bench.sqlite");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; CREATE TABLE place (key TEXT PRIMARY KEY, name TEXT)")
        .unwrap();
    let load = Instant::now();
    for chunk in (0..rows).collect::<Vec<_>>().chunks(1000) {
        let values: Vec<String> = chunk.iter().map(|i| format!("('p{i}', '{}')", name(*i))).collect();
        conn.execute_batch(&format!("INSERT INTO place (key, name) VALUES {}", values.join(", ")))
            .unwrap();
    }
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    let load_ms = load.elapsed().as_secs_f64() * 1e3;
    let pages = |conn: &rusqlite::Connection| -> i64 {
        conn.query_row("SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()", [], |r| r.get(0))
            .unwrap()
    };
    let before = pages(&conn);
    let mut build_ms = 0.0;
    if indexed {
        let start = Instant::now();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE place_cs USING fts5(name, content='place', tokenize='trigram case_sensitive 1');
             INSERT INTO place_cs(place_cs) VALUES ('rebuild');
             CREATE VIRTUAL TABLE place_ci USING fts5(name, content='place', tokenize='trigram case_sensitive 0');
             INSERT INTO place_ci(place_ci) VALUES ('rebuild');
             PRAGMA wal_checkpoint(TRUNCATE);",
        )
        .unwrap();
        build_ms = start.elapsed().as_secs_f64() * 1e3;
    }
    let after = pages(&conn);
    let mut out = serde_json::Map::new();
    for (label, sql) in queries(rows) {
        let ilike = sql.contains("ILIKE");
        conn.execute_batch(if ilike {
            "PRAGMA case_sensitive_like = OFF"
        } else {
            "PRAGMA case_sensitive_like = ON"
        })
        .unwrap();
        let statement = sqlite_sql(label, &sql, indexed);
        let (answer, ms) = time_query(|| conn.query_row(&statement, [], |r| r.get::<_, i64>(0)).unwrap());
        out.insert(label.into(), json!({"rows": answer, "median_ms": ms, "sql": statement}));
    }
    json!({
        "load_ms": load_ms,
        "build_ms": build_ms,
        "table_bytes": before,
        // Both FTS5 tables: one per case rule.
        "index_bytes": after - before,
        "queries": out,
    })
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let rows: u64 = args.get(1).expect("ROWS").parse().unwrap();
    let work = Path::new(args.get(2).expect("WORK_DIR"));
    fs::create_dir_all(work).unwrap();
    // Diagnosis only: `TRIGRAM_LIKE_PROFILE=<seconds>` loads the no-index
    // table and repeats one row-check query for that long, for a sampler to
    // attach to. Prints no measurement.
    if let Some(seconds) = std::env::var("TRIGRAM_LIKE_PROFILE").ok().and_then(|s| s.parse::<u64>().ok()) {
        let dir = work.join("profile");
        let _ = fs::remove_dir_all(&dir);
        let mut db = Database::create(&dir, cfg()).unwrap();
        run(&mut db, "CREATE TABLE place (_key TEXT PRIMARY KEY, name TEXT) WITH (index: none)");
        for chunk in (0..rows).collect::<Vec<_>>().chunks(1000) {
            let values: Vec<String> = chunk.iter().map(|i| format!("('p{i}', '{}')", name(*i))).collect();
            run(&mut db, &format!("INSERT INTO place (_key, name) VALUES {}", values.join(", ")));
        }
        run(&mut db, "COMMIT");
        db.checkpoint().unwrap();
        eprintln!("profile: loaded, pid {}", std::process::id());
        let until = Instant::now() + std::time::Duration::from_secs(seconds);
        let mut n = 0;
        while Instant::now() < until {
            count(&mut db, "SELECT count(*) FROM place WHERE name LIKE '%Lagoon%'");
            n += 1;
        }
        eprintln!("profile: {n} queries");
        let _ = fs::remove_dir_all(&dir);
        return;
    }
    let mut arms = serde_json::Map::new();
    arms.insert("sekejap_no_index".into(), sekejap_arm(rows, &work.join("plain"), false));
    arms.insert("sekejap_trigram".into(), sekejap_arm(rows, &work.join("trgm"), true));
    arms.insert("sqlite_no_index".into(), sqlite_arm(rows, &work.join("sqlite-plain"), false));
    arms.insert("sqlite_trigram".into(), sqlite_arm(rows, &work.join("sqlite-trgm"), true));
    if let (Some(socket), Some(port)) = (args.get(3), args.get(4)) {
        arms.insert("postgresql_no_index".into(), pg_arm(rows, socket, port, false));
        arms.insert("postgresql_trigram".into(), pg_arm(rows, socket, port, true));
    }
    // Every arm must have answered every query with the same row count.
    let first = arms.values().next().unwrap()["queries"].clone();
    for (arm, value) in &arms {
        for (label, q) in first.as_object().unwrap() {
            assert_eq!(
                value["queries"][label]["rows"], q["rows"],
                "{arm} answered `{label}` with a different row count"
            );
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "rows": rows,
            "metric": "median of 5 timed runs after 1 warm run, milliseconds, count(*) of the match",
            "arms": arms,
        }))
        .unwrap()
    );
    for arm in ["plain", "trgm", "sqlite-plain", "sqlite-trgm"] {
        let _ = fs::remove_dir_all(work.join(arm));
    }
}
