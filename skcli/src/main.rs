//! sekejap — interactive REPL and one-shot query runner
//!
//! # Usage
//!
//! ```text
//! sekejap                            open in-memory REPL
//! sekejap <path>                     open persistent DB in REPL
//! sekejap --path <path>              same (explicit flag)
//! sekejap <path> "<SQL>"             run SQL, print results, exit
//! sekejap --path <path> "<SQL>"      run SQL, print results, exit
//! echo "SQL;" | sekejap <path>       pipe SQL script, exit when stdin closes
//! ```

#[cfg(feature = "serve")]
mod serve;
#[cfg(feature = "pg")]
mod pg;

use rustyline::DefaultEditor;
use sekejap::{Config, CoreDB};
use std::io::{self, IsTerminal, Read};
use std::time::Instant;

// ── Arg parsing ───────────────────────────────────────────────────────────────

struct Args {
    path: Option<String>,
    sql:  Option<String>,
}

fn parse_args() -> Args {
    let mut args = std::env::args().skip(1).peekable();
    let mut path = None;
    let mut sql  = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--path" | "-p" => {
                path = args.next();
            }
            "--help" | "-h" => {
                print_usage();
                std::process::exit(0);
            }
            "--version" | "-V" => {
                println!("sekejap {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            other => {
                if path.is_none() {
                    path = Some(other.to_string());
                } else {
                    sql = Some(other.to_string());
                }
            }
        }
    }

    Args { path, sql }
}

fn print_usage() {
    println!(
        "sekejap {}

USAGE:
  sekejap                          open in-memory REPL
  sekejap <path>                   open persistent DB in REPL
  sekejap --path <path>            same (explicit flag)
  sekejap <path> \"<SQL>\"           run SQL and exit
  sekejap --path <path> \"<SQL>\"    run SQL and exit
  echo \"SELECT...;\" | sekejap      pipe SQL script
  sekejap migrate <path>           upgrade a DB to the latest (SKBIN) format + verify
  sekejap serve <path>             serve the DB over HTTP/JSON (default :5918; `serve --help`)
  sekejap pg <path>                serve the DB over the PostgreSQL wire protocol (default :5432; `pg --help`)

OPTIONS:
  -p, --path <path>    database directory path
  -h, --help           show this help
  -V, --version        show version",
        env!("CARGO_PKG_VERSION")
    );
}

// ── DB open/create ────────────────────────────────────────────────────────────

fn open_db(path: &Option<String>) -> (CoreDB, String) {
    match path {
        Some(p) => match CoreDB::open(p) {
            Ok(db) => (db, p.clone()),
            Err(e) => {
                eprintln!("error: cannot open '{}': {}", p, e);
                std::process::exit(1);
            }
        },
        None => (CoreDB::new(), String::from(":memory:")),
    }
}

// ── Table renderer ────────────────────────────────────────────────────────────

const MAX_COL_WIDTH: usize = 52;
const MIN_COL_WIDTH: usize = 4;

fn format_duration(ns: u128) -> String {
    if ns < 1_000 {
        format!("{ns} ns")
    } else if ns < 1_000_000 {
        format!("{:.2} µs", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.2} ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.3} s", ns as f64 / 1_000_000_000.0)
    }
}

fn cell_str(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn truncate_cell(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        let cut: String = chars[..max - 1].iter().collect();
        format!("{cut}…")
    }
}

fn print_table(hits: Vec<sekejap::Hit>, elapsed_ns: u128) {
    let timing = format_duration(elapsed_ns);
    let count = hits.len();

    if hits.is_empty() {
        println!("(0 rows)  [{timing}]");
        return;
    }

    // Collect column names from first hit's payload keys
    let mut columns: Vec<String> = Vec::new();
    for hit in &hits {
        if let Some(serde_json::Value::Object(map)) = &hit.payload {
            for key in map.keys() {
                if !columns.contains(key) {
                    columns.push(key.clone());
                }
            }
        }
    }

    // No structured payload — show slug column
    if columns.is_empty() {
        let slug_w = hits.iter()
            .map(|h| h.slug.chars().count())
            .max().unwrap_or(5)
            .min(MAX_COL_WIDTH)
            .max("_slug".len());
        let line = "─".repeat(slug_w + 2);
        println!("┌{line}┐");
        println!("│ {:<slug_w$} │", "_slug");
        println!("├{line}┤");
        for hit in &hits {
            println!("│ {:<slug_w$} │", truncate_cell(&hit.slug, MAX_COL_WIDTH));
        }
        println!("└{line}┘");
        if count == 1 { println!("1 row  [{timing}]"); } else { println!("{count} rows  [{timing}]"); }
        return;
    }

    // Compute column widths
    let mut widths: Vec<usize> = columns.iter()
        .map(|c| c.chars().count().max(MIN_COL_WIDTH))
        .collect();
    for hit in &hits {
        if let Some(serde_json::Value::Object(map)) = &hit.payload {
            for (i, col) in columns.iter().enumerate() {
                let val = map.get(col).map(cell_str).unwrap_or_default();
                let display = val.chars().count().min(MAX_COL_WIDTH);
                if display > widths[i] {
                    widths[i] = display;
                }
            }
        }
    }

    let top = widths.iter().map(|w| "─".repeat(w + 2)).collect::<Vec<_>>().join("┬");
    let mid = widths.iter().map(|w| "─".repeat(w + 2)).collect::<Vec<_>>().join("┼");
    let bot = widths.iter().map(|w| "─".repeat(w + 2)).collect::<Vec<_>>().join("┴");

    println!("┌{top}┐");
    let hdr: Vec<String> = columns.iter().zip(&widths)
        .map(|(c, w)| format!(" {:<w$} ", c))
        .collect();
    println!("│{}│", hdr.join("│"));
    println!("├{mid}┤");

    for hit in &hits {
        let cells: Vec<String> = columns.iter().zip(&widths).map(|(col, w)| {
            let val = hit.payload.as_ref()
                .and_then(|p| p.get(col))
                .map(cell_str)
                .unwrap_or_default();
            format!(" {:<w$} ", truncate_cell(&val, MAX_COL_WIDTH))
        }).collect();
        println!("│{}│", cells.join("│"));
    }

    println!("└{bot}┘");
    if count == 1 { println!("1 row  [{timing}]"); } else { println!("{count} rows  [{timing}]"); }
}

// ── SQL execution ─────────────────────────────────────────────────────────────

fn run_sql(db: &mut CoreDB, sql: &str) -> bool {
    let first = sql.split_whitespace().next().unwrap_or("").to_uppercase();
    let t0 = Instant::now();
    match first.as_str() {
        "SELECT" => match db.query(sql) {
            Err(e) => eprintln!("error: {e}"),
            Ok(set) => {
                let hits = set.collect();
                print_table(hits, t0.elapsed().as_nanos());
            }
        },
        "MATCH" => match db.query(sql) {
            Err(e) => eprintln!("error: {e}"),
            Ok(set) => {
                let hits = set.collect();
                print_table(hits, t0.elapsed().as_nanos());
            }
        },
        "EXPLAIN" => {
            let rest = sql.strip_prefix("EXPLAIN").unwrap_or(sql).trim();
            let (analyze, inner_sql) = if rest.to_uppercase().starts_with("ANALYZE") {
                (true, rest.strip_prefix("ANALYZE").or_else(|| rest.strip_prefix("analyze")).unwrap_or(rest).trim())
            } else {
                (false, rest)
            };
            let result = if analyze {
                db.explain_analyze(inner_sql)
            } else {
                db.explain(inner_sql)
            };
            match result {
                Err(e) => eprintln!("error: {e}"),
                Ok(hits) => print_table(hits, t0.elapsed().as_nanos()),
            }
        }
        "INSERT" | "UPDATE" | "DELETE" | "CREATE" | "DROP" | "ALTER" | "REINDEX" | "COMPACT" | "SET" => match db.execute(sql) {
            Err(e) => eprintln!("error: {e}"),
            Ok(n) => {
                let timing = format_duration(t0.elapsed().as_nanos());
                if n == 0 {
                    println!("ok  [{timing}]");
                } else if n == 1 {
                    println!("ok — 1 row affected  [{timing}]");
                } else {
                    println!("ok — {n} rows affected  [{timing}]");
                }
            }
        },
        "SHOW" => match db.show(sql) {
            Err(e) => eprintln!("error: {e}"),
            Ok(hits) => print_table(hits, t0.elapsed().as_nanos()),
        },
        _ => eprintln!("unknown statement — supported: SELECT MATCH SHOW EXPLAIN INSERT UPDATE DELETE CREATE DROP ALTER REINDEX"),
    }
    true
}

// ── Dot commands ──────────────────────────────────────────────────────────────

/// Handle a `.command` line. Returns false if the user wants to quit.
fn run_dot(db: &mut CoreDB, label: &mut String, line: &str) -> bool {
    let parts: Vec<&str> = line.splitn(2, ' ').collect();
    match parts[0] {
        ".quit" | ".q" | ".exit" => return false,

        ".help" => print_repl_help(),

        ".open" => {
            let p = parts.get(1).map(|s| s.trim()).unwrap_or("");
            if p.is_empty() {
                eprintln!("usage: .open <path>");
            } else {
                match CoreDB::open(p) {
                    Ok(new_db) => {
                        *db = new_db;
                        *label = p.to_string();
                        println!("opened: {p}");
                    }
                    Err(e) => eprintln!("error: {e}"),
                }
            }
        }

        ".tables" => {
            match db.show("SHOW TABLES") {
                Err(e) => eprintln!("error: {e}"),
                Ok(hits) => {
                    if hits.is_empty() {
                        println!("(no collections)");
                    } else {
                        println!("{:<30} {}", "name", "count");
                        println!("{}", "-".repeat(38));
                        for h in &hits {
                            let name  = h.payload.as_ref().and_then(|p| p["name"].as_str()).unwrap_or("");
                            let count = h.payload.as_ref().and_then(|p| p["count"].as_u64()).unwrap_or(0);
                            println!("{:<30} {}", name, count);
                        }
                    }
                }
            }
        }

        ".schema" => {
            let target = parts.get(1).map(|s| s.trim());
            let names = db.collection_names();
            let cols: Vec<&str> = match target {
                Some(t) if !t.is_empty() => vec![t],
                _ => names.iter().map(String::as_str).collect(),
            };
            let mut found_any = false;
            for col in cols {
                if let Some(ddl) = db.schema_ddl(col) {
                    println!("{ddl};");
                    found_any = true;
                } else if target.is_some() {
                    println!("-- no CREATE TABLE for '{col}'");
                    found_any = true;
                }
            }
            if !found_any {
                println!("(no schemas declared — use CREATE TABLE to add one)");
            }
        }

        ".compact" => match db.compact() {
            Ok(_) => println!("compacted"),
            Err(e) => eprintln!("error: {e}"),
        },

        ".stats" => {
            let s = db.stats();
            println!("nodes          : {}", s.nodes);
            println!("edges          : {}", s.edges);
            println!("collections    : {}", s.collections);
            println!("mode           : {}", if s.paged { "paged (mmap base + overlay)" } else { "resident" });
            if s.paged {
                println!("write overlay  : {} nodes", s.overlay_nodes);
            }
            println!("payloads.bin   : {}", human_bytes(s.payload_bytes));
            println!("wal.log        : {}", human_bytes(s.wal_bytes));
            println!(
                "indexes        : {} field, {} vector, {} bm25, {} search, {} trigram, spatial {}",
                s.field_indexes, s.hnsw_indexes, s.bm25_indexes, s.search_indexes,
                s.trigram_indexes, if s.spatial_index { "yes" } else { "no" }
            );
            println!("since open     : {} queries, {} writes, {} compactions, {} snapshots",
                s.queries, s.writes, s.compactions, s.snapshots);
            if s.compactions > 0 {
                println!("compaction     : last {} ms, slowest {} ms",
                    s.last_compact_us / 1000, s.max_compact_us / 1000);
            }
            if s.snapshots > 0 {
                println!("snapshot mint  : last {} µs, slowest {} µs",
                    s.last_snapshot_us, s.max_snapshot_us);
            }
        }

        ".edges" => {
            let arg = parts.get(1).map(|s| s.trim()).unwrap_or("");
            let sql = if arg.is_empty() {
                "SHOW EDGES".to_string()
            } else {
                format!("SHOW EDGES FROM {arg}")
            };
            match db.show(&sql) {
                Err(e) => eprintln!("error: {e}"),
                Ok(hits) => {
                    if hits.is_empty() {
                        println!("(no edges)");
                    } else if arg.is_empty() {
                        println!("{:<25} {:<20} {:<25} {}", "from", "type", "to", "count");
                        println!("{}", "-".repeat(78));
                        for h in &hits {
                            let p     = h.payload.as_ref();
                            let from  = p.and_then(|p| p["from"].as_str()).unwrap_or("");
                            let kind  = p.and_then(|p| p["type"].as_str()).unwrap_or("");
                            let to    = p.and_then(|p| p["to"].as_str()).unwrap_or("");
                            let count = p.and_then(|p| p["count"].as_u64()).unwrap_or(0);
                            println!("{:<25} {:<20} {:<25} {}", from, kind, to, count);
                        }
                    } else {
                        println!("{:<20} {}", "type", "count");
                        println!("{}", "-".repeat(28));
                        for h in &hits {
                            let p     = h.payload.as_ref();
                            let kind  = p.and_then(|p| p["type"].as_str()).unwrap_or("");
                            let count = p.and_then(|p| p["count"].as_u64()).unwrap_or(0);
                            println!("{:<20} {}", kind, count);
                        }
                    }
                }
            }
        }

        other => eprintln!("unknown command: {other}  (try .help)"),
    }
    true
}

fn print_repl_help() {
    println!(
        r#"
sekejap dot commands
────────────────────
.open <path>        open (or create) a persistent DB — replaces current DB
.tables             list all collections
.schema [name]      show CREATE TABLE DDL (all collections if name omitted)
.compact            flush snapshot, truncate WAL
.stats              show node / edge / collection counts
.edges              show full graph schema (from_col → type → to_col), distinct
.edges <col>        show distinct edge types leaving a collection
.help               show this help
.quit / .q / .exit  exit  (also Ctrl+D)

SQL (end each statement with ;)
────────────────────────────────
SELECT * FROM collection [WHERE ...] [ORDER BY ...] [LIMIT n] [OFFSET n];
SELECT * FROM ALL [WHERE ...];
INSERT INTO collection (_key, field, ...) VALUES ('key', val, ...);
UPDATE collection SET field = val [WHERE ...];
DELETE FROM collection [WHERE ...];
CREATE TABLE collection (_key TEXT PRIMARY KEY, field TYPE, ...);
ALTER TABLE collection ADD COLUMN field TYPE;
ALTER TABLE collection DROP COLUMN field;
ALTER TABLE collection RENAME COLUMN old TO new;
ALTER TABLE collection RENAME TO new_name;

Graph edges
───────────
INSERT ('from')-[:KIND {{strength: n}}]->('to');
DELETE ('from')-[:KIND]->('to');

Graph traversal (MATCH)
───────────────────────
SELECT b.* FROM MATCH (a:col)-[:rel*1..3]->(b:col) WHERE a._key = 'x';
SELECT a.* FROM MATCH (a:col)<-[:rel]-(b:col) WHERE b._key = 'x';   -- backward
SELECT DISTINCT b._key FROM MATCH (a:col)-[:rel]->(b:col);          -- dedup rows

Graph aggregation
─────────────────
SELECT b._key AS name, SUM(r.weight) AS total
FROM MATCH (a:col)-[r:edge]->(b:col)
GROUP BY b._key ORDER BY total DESC LIMIT 10;

Multi-FROM cross-join
─────────────────────
SELECT a.field AS af, b.field AS bf
FROM MATCH (a:col)-[:edge]->(b), collection_name AS alias;

SELECT list expressions
───────────────────────
var.field AS alias
COUNT(*) | COUNT(DISTINCT var.field) | SUM(expr) | AVG(expr) | MIN(expr) | MAX(expr)
PATH_AVG(r.field) | PATH_SUM | PATH_MIN | PATH_MAX | PATH_PRODUCT
PATH_FIRST(r.field) | PATH_LAST(r.field)
CASE WHEN r.field = val THEN 'x' WHEN ... ELSE 'y' END AS alias
AGE_DAYS(var.field) | AGE_HOURS(var.field) | NOW()
JSON_ARRAY_LENGTH(var.field)

Shortest path (0 rows = unreachable, 1 row = found)
────────────────────────────────────────────────────
SELECT a.field AS from_f, b.field AS to_f, length(r) AS hops, nodes(r) AS route
FROM MATCH SHORTEST (a:col)-[r*]->(b:col)
WHERE a._key = 'start' AND b._key = 'end'
AND ANY(n IN nodes(r) WHERE n.field = 'val')

Introspection
─────────────
SHOW TABLES;
SHOW EDGES;
SHOW EDGES FROM collection;
SHOW EDGES FROM col1 TO col2;
SHOW <collection>;

Filters
───────
=  !=  >  <  >=  <=  BETWEEN n AND n
IN (v1, v2)  NOT IN (v1, v2)
LIKE 'pat'  ILIKE 'pat'
IS NULL  IS NOT NULL
AND  OR  NOT

Spatial
───────
ST_DWithin(geometry, POINT(lon lat), metres)
ST_Contains / ST_Within / ST_Intersects
ORDER BY -ST_DISTANCE(geometry, POINT(lon lat)) DESC   -- metres

Vector
──────
WHERE VECTOR_NEAR(field, [f32, ...], k)
ORDER BY field <=> [f32, ...] ASC     -- cosine nearest-first
ORDER BY field <-> [f32, ...] ASC     -- L2 nearest-first
ORDER BY VECTOR_COSINE(field, [...]) * 0.7 + BM25(bio, 'q') * 0.3 DESC
"#
    );
}

// ── Script mode ───────────────────────────────────────────────────────────────

fn run_script(db: &mut CoreDB, script: &str) {
    let mut label = String::new();
    let mut buf = String::new();
    let mut in_str = false;
    let mut str_char = '\0';

    for line in script.lines() {
        let trimmed = line.trim();

        if !in_str && buf.trim().is_empty() && trimmed.starts_with('.') {
            if !run_dot(db, &mut label, trimmed) {
                return;
            }
            continue;
        }

        if !in_str && (trimmed.is_empty() || trimmed.starts_with("--")) {
            continue;
        }

        for ch in trimmed.chars() {
            match ch {
                '\'' | '"' if !in_str => { in_str = true; str_char = ch; buf.push(ch); }
                c if in_str && c == str_char => { in_str = false; buf.push(ch); }
                ';' if !in_str => {
                    let stmt = buf.trim().to_string();
                    buf.clear();
                    if !stmt.is_empty() {
                        run_sql(db, &stmt);
                    }
                }
                _ => buf.push(ch),
            }
        }
        if !buf.trim().is_empty() {
            buf.push(' ');
        }
    }

    let stmt = buf.trim().to_string();
    if !stmt.is_empty() {
        run_sql(db, &stmt);
    }
}

// ── REPL ──────────────────────────────────────────────────────────────────────

fn repl(mut db: CoreDB, mut label: String) {
    let history_path = std::env::var("HOME").ok()
        .map(|h| std::path::PathBuf::from(h).join(".sekejap_history"));

    let mut rl = DefaultEditor::new().expect("failed to init readline");
    if let Some(ref p) = history_path {
        let _ = rl.load_history(p);
    }

    println!("sekejap {}  —  {label}", env!("CARGO_PKG_VERSION"));
    println!("type .help for commands, .quit to exit\n");

    let mut buf = String::new();

    loop {
        let prompt = if buf.trim().is_empty() {
            "sekejap> ".to_string()
        } else {
            "      ...> ".to_string()
        };

        let line = match rl.readline(&prompt) {
            Ok(l) => l,
            Err(rustyline::error::ReadlineError::Eof)
            | Err(rustyline::error::ReadlineError::Interrupted) => break,
            Err(e) => {
                eprintln!("readline error: {e}");
                break;
            }
        };

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let _ = rl.add_history_entry(trimmed);

        if trimmed.starts_with('.') {
            buf.clear();
            if !run_dot(&mut db, &mut label, trimmed) {
                break;
            }
            continue;
        }

        if !buf.is_empty() {
            buf.push(' ');
        }
        buf.push_str(trimmed);

        if buf.trim_end().ends_with(';') {
            let sql = buf.trim_end_matches(';').trim().to_string();
            buf.clear();
            if !sql.is_empty() {
                run_sql(&mut db, &sql);
            }
        }
    }

    if let Some(ref p) = history_path {
        let _ = rl.save_history(p);
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

// ── migrate: upgrade a DB's payloads to the latest format, verify, report ──────

fn payloads_mb(dir: &str) -> f64 {
    std::fs::metadata(std::path::Path::new(dir).join("payloads.bin"))
        .map(|m| m.len() as f64 / (1024.0 * 1024.0))
        .unwrap_or(0.0)
}

/// Compare two payloads ignoring engine-injected timestamps (which are already
/// baked into stored records and unchanged by re-encoding, but excluded for safety).
fn same_record(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    fn strip(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(m) => serde_json::Value::Object(
                m.iter()
                    .filter(|(k, _)| *k != "_created_unix" && *k != "_updated_unix")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    strip(a) == strip(b)
}

/// Returns a process exit code (0 = success).
fn migrate(path: &str) -> i32 {
    println!("sekejap migrate — upgrading '{path}' to SKBIN (Level-1 binary payloads)");
    println!("  (compaction is atomic and lossless; this run also verifies every record)");

    let cfg = Config { payload_binary: true, ..Config::default() };
    let mut db = match CoreDB::open_with_config(path, cfg.clone()) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error: cannot open '{path}': {e}");
            return 1;
        }
    };

    // 1. Snapshot every record's value BEFORE compaction.
    let slugs = db.all_slugs();
    let n = slugs.len();
    if n == 0 {
        println!("  no records found — nothing to migrate.");
        return 0;
    }
    println!("  {n} records found — snapshotting values…");
    let before: Vec<(String, serde_json::Value)> = slugs
        .iter()
        .filter_map(|s| {
            db.get(s)
                .and_then(|raw| serde_json::from_str(&raw).ok())
                .map(|v| (s.clone(), v))
        })
        .collect();
    let size_before = payloads_mb(path);

    // 2. Compact → re-encodes live records into SKBIN (atomic tmp→rename).
    println!("  compacting to SKBIN…");
    if let Err(e) = db.compact() {
        eprintln!("error: compaction failed: {e}  (original data is untouched)");
        return 1;
    }
    drop(db);

    // 3. Reopen and verify EVERY record round-trips byte-identical.
    let db2 = match CoreDB::open_with_config(path, cfg) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error: reopen after compaction failed: {e}");
            return 1;
        }
    };
    let mut diffs = 0usize;
    for (slug, vb) in &before {
        let ok = db2
            .get(slug)
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .map(|va| same_record(&va, vb))
            .unwrap_or(false);
        if !ok {
            diffs += 1;
            if diffs <= 5 {
                eprintln!("  ⚠ record differs after migration: {slug}");
            }
        }
    }
    let size_after = payloads_mb(path);

    // 4. Report.
    if diffs == 0 {
        let ratio = if size_after > 0.0 { size_before / size_after } else { 1.0 };
        println!("  ✓ verified {n} records — 0 differences");
        println!("  payloads.bin: {size_before:.1} MB → {size_after:.1} MB ({ratio:.2}x)");
        println!("done — '{path}' now uses the official SKBIN format.");
        0
    } else {
        eprintln!(
            "  ✗ {diffs}/{n} records did not verify. This indicates a bug, not data loss —\n     \
             the records are still present and readable. Please report this with the DB.\n     \
             (Keep a backup of '{path}' before relying on it.)"
        );
        1
    }
}

/// `sekejap dump <db>` — write the whole database as portable SGQL to stdout.
fn dump_cmd(path: &str) -> i32 {
    let db = match CoreDB::open(path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error: cannot open '{path}': {e}");
            return 1;
        }
    };
    print!("{}", db.dump_sql());
    0
}

/// `sekejap load <db> <file.sql>` — replay an SGQL dump into a database.
fn load_cmd(path: &str, file: &str) -> i32 {
    let sql = match std::fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: cannot read '{file}': {e}");
            return 1;
        }
    };
    let mut db = match CoreDB::open(path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!("error: cannot open '{path}': {e}");
            return 1;
        }
    };
    match db.load_sql(&sql) {
        Ok(n) => {
            eprintln!("loaded {n} statements into '{path}'");
            0
        }
        Err(e) => {
            eprintln!("load failed: {e}");
            1
        }
    }
}

fn main() {
    // `sekejap migrate <db>` — upgrade payloads to the latest (SKBIN) format,
    // verifying every record round-trips byte-identical before reporting success.
    let raw: Vec<String> = std::env::args().skip(1).collect();

    // `sekejap serve <db> [flags]` — HTTP/JSON server (behind the `serve` feature).
    if raw.first().map(String::as_str) == Some("serve") {
        #[cfg(feature = "serve")]
        { std::process::exit(serve::run(&raw[1..])); }
        #[cfg(not(feature = "serve"))]
        {
            eprintln!("this build lacks the `serve` feature — rebuild: cargo install sekejap-cli --features serve");
            std::process::exit(2);
        }
    }

    // `sekejap pg <db> [flags]` — PostgreSQL wire-protocol listener (behind `pg`).
    if raw.first().map(String::as_str) == Some("pg") {
        #[cfg(feature = "pg")]
        { std::process::exit(pg::run(&raw[1..])); }
        #[cfg(not(feature = "pg"))]
        {
            eprintln!("this build lacks the `pg` feature — rebuild: cargo install sekejap-cli --features pg");
            std::process::exit(2);
        }
    }

    if raw.first().map(String::as_str) == Some("migrate") {
        match raw.get(1) {
            Some(p) => std::process::exit(migrate(p)),
            None => {
                eprintln!("usage: sekejap migrate <db-path>");
                std::process::exit(2);
            }
        }
    }

    // `sekejap dump <db>` — write the whole database as portable SGQL to stdout.
    // `sekejap load <db> <file.sql>` — replay an SGQL dump into a database.
    // The version-independent migration path (see docs/developer/invariants.md).
    if raw.first().map(String::as_str) == Some("dump") {
        match raw.get(1) {
            Some(p) => std::process::exit(dump_cmd(p)),
            None => {
                eprintln!("usage: sekejap dump <db-path>   (writes SGQL to stdout)");
                std::process::exit(2);
            }
        }
    }
    if raw.first().map(String::as_str) == Some("load") {
        match (raw.get(1), raw.get(2)) {
            (Some(p), Some(f)) => std::process::exit(load_cmd(p, f)),
            _ => {
                eprintln!("usage: sekejap load <db-path> <dump.sql>");
                std::process::exit(2);
            }
        }
    }

    let args = parse_args();
    let (mut db, label) = open_db(&args.path);

    if let Some(sql) = args.sql {
        run_script(&mut db, &sql);
        return;
    }

    if !io::stdin().is_terminal() {
        let mut script = String::new();
        io::stdin()
            .read_to_string(&mut script)
            .expect("failed to read stdin");
        run_script(&mut db, &script);
        return;
    }

    repl(db, label);
}

/// Human-readable byte size for `.stats`.
fn human_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let n = n as f64;
    if n < K { format!("{n:.0} B") }
    else if n < K * K { format!("{:.1} KB", n / K) }
    else if n < K * K * K { format!("{:.1} MB", n / (K * K)) }
    else { format!("{:.2} GB", n / (K * K * K)) }
}
