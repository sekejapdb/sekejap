//! Benchmark: sekejap_memory vs sekejap_disk vs SQLite
//!
//! sekejap_memory = CoreDB::new()   — in-RAM, ephemeral (best-case sekejap)
//! sekejap_disk   = CoreDB::open()  — WAL-backed, persistent (production sekejap)
//! Both are sekejap. Two modes, one product.
//!
//! Dataset: 10,000 nodes (products), 250 linear edges, 10 categories.
//! Scenarios: simple_filter, range_filter, sort_limit, forward_1hop, multihop_bfs

use criterion::{criterion_group, criterion_main, Criterion, black_box};
use serde_json::json;

// ── Setup helpers ────────────────────────────────────────────────────────────

fn setup_core() -> sekejap::CoreDB {
    let mut db = sekejap::CoreDB::new();
    for i in 0..10_000usize {
        let cat = format!("cat{}", i % 10);
        let price = (i % 200) as f64 + 10.0;
        let in_stock = i % 3 != 0;
        db.put(
            &format!("products/p{i}"),
            &json!({
                "_collection": "products",
                "_key": format!("p{i}"),
                "category": cat,
                "price": price,
                "in_stock": in_stock,
                "name": format!("Product {i}"),
            }).to_string(),
        ).unwrap();
    }
    // Edges for graph traversal
    for i in 0..250usize {
        db.link(
            &format!("products/p{i}"),
            &format!("products/p{}", i + 1),
            "related",
            1.0,
        );
    }
    // Btree indexes — match SQLite's CREATE INDEX idx_cat/idx_price
    db.execute("CREATE INDEX ON products USING btree (category)").unwrap();
    db.execute("CREATE INDEX ON products USING btree (price)").unwrap();
    db
}

fn setup_sekejap() -> (sekejap::CoreDB, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let mut db = sekejap::CoreDB::open(dir.path()).unwrap();
    for i in 0..10_000usize {
        let cat = format!("cat{}", i % 10);
        let price = (i % 200) as f64 + 10.0;
        let in_stock = i % 3 != 0;
        db.put(
            &format!("products/p{i}"),
            &json!({
                "_collection": "products",
                "_key": format!("p{i}"),
                "category": cat,
                "price": price,
                "in_stock": in_stock,
                "name": format!("Product {i}"),
            }).to_string(),
        ).unwrap();
    }
    for i in 0..250usize {
        db.link(
            &format!("products/p{i}"),
            &format!("products/p{}", i + 1),
            "related",
            1.0,
        );
    }
    // Btree indexes — match SQLite's CREATE INDEX idx_cat/idx_price
    db.execute("CREATE INDEX ON products USING btree (category)").unwrap();
    db.execute("CREATE INDEX ON products USING btree (price)").unwrap();
    (db, dir)
}

fn setup_sqlite() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE products (
            key TEXT PRIMARY KEY,
            category TEXT,
            price REAL,
            in_stock INTEGER,
            name TEXT
        );
        CREATE INDEX idx_cat ON products(category);
        CREATE INDEX idx_price ON products(price);
        CREATE TABLE edges (
            from_key TEXT,
            to_key TEXT,
            kind TEXT,
            strength REAL
        );
        CREATE INDEX idx_edges_from ON edges(from_key, kind);
        CREATE INDEX idx_edges_to ON edges(to_key, kind);"
    ).unwrap();

    // Bulk insert with transaction
    conn.execute_batch("BEGIN").unwrap();
    {
        let mut stmt = conn.prepare(
            "INSERT INTO products (key, category, price, in_stock, name) VALUES (?1, ?2, ?3, ?4, ?5)"
        ).unwrap();
        for i in 0..10_000usize {
            let cat = format!("cat{}", i % 10);
            let price = (i % 200) as f64 + 10.0;
            let in_stock: i32 = if i % 3 != 0 { 1 } else { 0 };
            stmt.execute(rusqlite::params![
                format!("p{i}"), cat, price, in_stock, format!("Product {i}")
            ]).unwrap();
        }
    }
    {
        let mut stmt = conn.prepare(
            "INSERT INTO edges (from_key, to_key, kind, strength) VALUES (?1, ?2, ?3, ?4)"
        ).unwrap();
        for i in 0..250usize {
            stmt.execute(rusqlite::params![
                format!("p{i}"), format!("p{}", i + 1), "related", 1.0f64
            ]).unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();
    conn
}

// ── Benchmarks ───────────────────────────────────────────────────────────────

fn bench_simple_filter(c: &mut Criterion) {
    let core_db = setup_core();
    let (sk_db, _dir) = setup_sekejap();
    let sqlite = setup_sqlite();

    let mut group = c.benchmark_group("simple_filter");

    group.bench_function("sekejap_memory", |b| {
        b.iter(|| black_box(
            core_db.query("SELECT * FROM products WHERE category = 'cat3'")
                .unwrap().count()
        ))
    });

    group.bench_function("sekejap_disk", |b| {
        b.iter(|| black_box(
            sk_db.query("SELECT * FROM products WHERE category = 'cat3'")
                .unwrap().count()
        ))
    });

    group.bench_function("sqlite", |b| {
        b.iter(|| {
            let mut stmt = sqlite.prepare_cached(
                "SELECT * FROM products WHERE category = 'cat3'"
            ).unwrap();
            let count = stmt.query_map([], |_row| Ok(())).unwrap().count();
            black_box(count)
        })
    });

    group.finish();
}

fn bench_range_filter(c: &mut Criterion) {
    let core_db = setup_core();
    let (sk_db, _dir) = setup_sekejap();
    let sqlite = setup_sqlite();

    let mut group = c.benchmark_group("range_filter");

    group.bench_function("sekejap_memory", |b| {
        b.iter(|| black_box(
            core_db.query("SELECT * FROM products WHERE price > 50 AND price <= 150")
                .unwrap().count()
        ))
    });

    group.bench_function("sekejap_disk", |b| {
        b.iter(|| black_box(
            sk_db.query("SELECT * FROM products WHERE price > 50 AND price <= 150")
                .unwrap().count()
        ))
    });

    group.bench_function("sqlite", |b| {
        b.iter(|| {
            let mut stmt = sqlite.prepare_cached(
                "SELECT * FROM products WHERE price > 50 AND price <= 150"
            ).unwrap();
            let count = stmt.query_map([], |_row| Ok(())).unwrap().count();
            black_box(count)
        })
    });

    group.finish();
}

fn bench_sort_limit(c: &mut Criterion) {
    let core_db = setup_core();
    let (sk_db, _dir) = setup_sekejap();
    let sqlite = setup_sqlite();

    let mut group = c.benchmark_group("sort_limit");

    group.bench_function("sekejap_memory", |b| {
        b.iter(|| black_box(
            core_db.query("SELECT * FROM products WHERE category = 'cat5' ORDER BY price ASC LIMIT 20")
                .unwrap().collect()
        ))
    });

    group.bench_function("sekejap_disk", |b| {
        b.iter(|| black_box(
            sk_db.query("SELECT * FROM products WHERE category = 'cat5' ORDER BY price ASC LIMIT 20")
                .unwrap().count()
        ))
    });

    group.bench_function("sqlite", |b| {
        b.iter(|| {
            let mut stmt = sqlite.prepare_cached(
                "SELECT * FROM products WHERE category = 'cat5' ORDER BY price ASC LIMIT 20"
            ).unwrap();
            let rows: Vec<String> = stmt.query_map([], |row| {
                row.get::<_, String>(0)
            }).unwrap().filter_map(|r| r.ok()).collect();
            black_box(rows)
        })
    });

    group.finish();
}

fn bench_forward_1hop(c: &mut Criterion) {
    let core_db = setup_core();
    let (sk_db, _dir) = setup_sekejap();
    let sqlite = setup_sqlite();

    let mut group = c.benchmark_group("forward_1hop");

    // core: MATCH forward
    group.bench_function("core_match", |b| {
        b.iter(|| black_box(
            core_db.query(
                "SELECT b.* FROM MATCH (a:products)-[:related]->(b) WHERE a._key = 'p0'"
            ).unwrap().count()
        ))
    });

    // core: old FOLLOW syntax
    group.bench_function("core_follow", |b| {
        b.iter(|| black_box(
            core_db.query("SELECT * FROM FOLLOW('products/p0', 'related')")
                .unwrap().count()
        ))
    });

    // sekejap: MATCH forward
    group.bench_function("sekejap_match", |b| {
        b.iter(|| black_box(
            sk_db.query(
                "SELECT b.* FROM MATCH (a:products)-[:related]->(b) WHERE a._key = 'p0'"
            ).unwrap().count()
        ))
    });

    // sqlite: JOIN on edges
    group.bench_function("sqlite", |b| {
        b.iter(|| {
            let mut stmt = sqlite.prepare_cached(
                "SELECT p.* FROM edges e JOIN products p ON p.key = e.to_key \
                 WHERE e.from_key = 'p0' AND e.kind = 'related'"
            ).unwrap();
            let count = stmt.query_map([], |_row| Ok(())).unwrap().count();
            black_box(count)
        })
    });

    group.finish();
}

fn bench_multihop_bfs(c: &mut Criterion) {
    let core_db = setup_core();
    let (sk_db, _dir) = setup_sekejap();
    let sqlite = setup_sqlite();

    let mut group = c.benchmark_group("multihop_bfs_3");

    // core: MATCH with typed hops
    group.bench_function("core_match", |b| {
        b.iter(|| black_box(
            core_db.query(
                "SELECT b.* FROM MATCH (a:products)-[:related*1..3]->(b) WHERE a._key = 'p0'"
            ).unwrap().count()
        ))
    });

    // sekejap: MATCH
    group.bench_function("sekejap_match", |b| {
        b.iter(|| black_box(
            sk_db.query(
                "SELECT b.* FROM MATCH (a:products)-[:related*1..3]->(b) WHERE a._key = 'p0'"
            ).unwrap().count()
        ))
    });

    // sqlite: recursive CTE
    group.bench_function("sqlite_cte", |b| {
        b.iter(|| {
            let mut stmt = sqlite.prepare_cached(
                "WITH RECURSIVE hops(key, depth) AS (
                    SELECT 'p0', 0
                    UNION ALL
                    SELECT e.to_key, h.depth + 1
                    FROM hops h JOIN edges e ON e.from_key = h.key AND e.kind = 'related'
                    WHERE h.depth < 3
                )
                SELECT DISTINCT key FROM hops WHERE depth > 0"
            ).unwrap();
            let count = stmt.query_map([], |_row| Ok(())).unwrap().count();
            black_box(count)
        })
    });

    group.finish();
}

fn bench_point_lookup(c: &mut Criterion) {
    let core_db = setup_core();
    let (sk_db, _dir) = setup_sekejap();
    let sqlite = setup_sqlite();

    let mut group = c.benchmark_group("point_lookup");

    group.bench_function("sekejap_memory", |b| {
        b.iter(|| black_box(
            core_db.query("SELECT * FROM products WHERE _key = 'p5000'")
                .unwrap().count()
        ))
    });

    group.bench_function("sekejap_disk", |b| {
        b.iter(|| black_box(
            sk_db.query("SELECT * FROM products WHERE _key = 'p5000'")
                .unwrap().count()
        ))
    });

    group.bench_function("sqlite", |b| {
        b.iter(|| {
            let mut stmt = sqlite.prepare_cached(
                "SELECT * FROM products WHERE key = 'p5000'"
            ).unwrap();
            let count = stmt.query_map([], |_row| Ok(())).unwrap().count();
            black_box(count)
        })
    });

    group.finish();
}

// ── Write benchmarks ────────────────────────────────────────────────────────

fn bench_single_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_insert");

    group.bench_function("sekejap_memory", |b| {
        let mut db = sekejap::CoreDB::new();
        let counter = std::cell::Cell::new(0u64);
        b.iter(|| {
            let i = counter.get();
            counter.set(i + 1);
            black_box(
                db.execute(&format!(
                    "INSERT INTO bench (_key, name, value) VALUES ('k{i}', 'Name {i}', {i})"
                ))
                .unwrap(),
            )
        })
    });

    group.bench_function("sqlite", |b| {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE bench (key TEXT PRIMARY KEY, name TEXT, value REAL)",
        )
        .unwrap();
        let counter = std::cell::Cell::new(0u64);
        b.iter(|| {
            let i = counter.get();
            counter.set(i + 1);
            black_box(
                conn.execute(
                    "INSERT INTO bench (key, name, value) VALUES (?1, ?2, ?3)",
                    rusqlite::params![format!("k{i}"), format!("Name {i}"), i as f64],
                )
                .unwrap(),
            )
        })
    });

    group.finish();
}

fn bench_bulk_insert_1k(c: &mut Criterion) {
    let mut group = c.benchmark_group("bulk_insert_1k");

    group.bench_function("sekejap_memory", |b| {
        b.iter_batched(
            sekejap::CoreDB::new,
            |mut db| {
                let items: Vec<(String, String)> = (0..1000)
                    .map(|i| {
                        (
                            format!("bulk/b{i}"),
                            json!({
                                "_collection": "bulk",
                                "_key": format!("b{i}"),
                                "name": format!("Bulk {i}"),
                                "value": i,
                            })
                            .to_string(),
                        )
                    })
                    .collect();
                let refs: Vec<(&str, &str)> =
                    items.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                black_box(db.put_many(refs).unwrap())
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.bench_function("sqlite", |b| {
        b.iter_batched(
            || {
                let conn = rusqlite::Connection::open_in_memory().unwrap();
                conn.execute_batch(
                    "CREATE TABLE bulk (key TEXT PRIMARY KEY, name TEXT, value REAL)",
                )
                .unwrap();
                conn
            },
            |conn| {
                conn.execute_batch("BEGIN").unwrap();
                {
                    let mut stmt = conn
                        .prepare("INSERT INTO bulk (key, name, value) VALUES (?1, ?2, ?3)")
                        .unwrap();
                    for i in 0..1000 {
                        stmt.execute(rusqlite::params![
                            format!("b{i}"),
                            format!("Bulk {i}"),
                            i as f64,
                        ])
                        .unwrap();
                    }
                }
                conn.execute_batch("COMMIT").unwrap();
                black_box(())
            },
            criterion::BatchSize::SmallInput,
        )
    });

    group.finish();
}

fn setup_sqlite_disk() -> (rusqlite::Connection, tempfile::TempDir) {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("bench.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
        PRAGMA synchronous=FULL;
        CREATE TABLE products (
            key TEXT PRIMARY KEY,
            category TEXT,
            price REAL,
            in_stock INTEGER,
            name TEXT
        );
        CREATE INDEX idx_cat ON products(category);
        CREATE INDEX idx_price ON products(price);"
    ).unwrap();
    conn.execute_batch("BEGIN").unwrap();
    {
        let mut stmt = conn.prepare(
            "INSERT INTO products (key, category, price, in_stock, name) VALUES (?1, ?2, ?3, ?4, ?5)"
        ).unwrap();
        for i in 0..10_000usize {
            let cat = format!("cat{}", i % 10);
            let price = (i % 200) as f64 + 10.0;
            let in_stock: i32 = if i % 3 != 0 { 1 } else { 0 };
            stmt.execute(rusqlite::params![
                format!("p{i}"), cat, price, in_stock, format!("Product {i}")
            ]).unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();
    (conn, dir)
}

fn bench_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("update");

    group.bench_function("sekejap_memory", |b| {
        let mut db = setup_core();
        b.iter(|| {
            black_box(
                db.execute("UPDATE products SET name = 'Updated' WHERE category = 'cat3'")
                    .unwrap(),
            )
        })
    });

    group.bench_function("sqlite_memory", |b| {
        let sqlite = setup_sqlite();
        b.iter(|| {
            black_box(
                sqlite.execute(
                    "UPDATE products SET name = 'Updated' WHERE category = 'cat3'", [],
                ).unwrap(),
            )
        })
    });

    group.sample_size(10);

    group.bench_function("sekejap_disk", |b| {
        let (mut db, _dir) = setup_sekejap();
        b.iter(|| {
            black_box(
                db.execute("UPDATE products SET name = 'Updated' WHERE category = 'cat3'")
                    .unwrap(),
            )
        })
    });

    group.bench_function("sekejap_disk_logical", |b| {
        let (mut db, _dir) = setup_sekejap();
        db.execute("SET WAL_MODE = logical").unwrap();
        b.iter(|| {
            black_box(
                db.execute("UPDATE products SET name = 'Updated' WHERE category = 'cat3'")
                    .unwrap(),
            )
        })
    });

    // Durability-matched variant: SQLite's synchronous=FULL uses plain fsync
    // on macOS (fullfsync defaults OFF), which is sekejap's WAL_SYNC=os level.
    group.bench_function("sekejap_disk_logical_os_sync", |b| {
        let (mut db, _dir) = setup_sekejap();
        db.execute("SET WAL_MODE = logical").unwrap();
        db.execute("SET WAL_SYNC = os").unwrap();
        b.iter(|| {
            black_box(
                db.execute("UPDATE products SET name = 'Updated' WHERE category = 'cat3'")
                    .unwrap(),
            )
        })
    });

    // The reverse: SQLite at TRUE power-loss durability (F_FULLFSYNC),
    // matching sekejap's default WAL_SYNC=full.
    group.bench_function("sqlite_disk_fullfsync", |b| {
        let (sqlite, _dir) = setup_sqlite_disk();
        sqlite.execute_batch("PRAGMA fullfsync=ON;").unwrap();
        b.iter(|| {
            black_box(
                sqlite.execute(
                    "UPDATE products SET name = 'Updated' WHERE category = 'cat3'", [],
                ).unwrap(),
            )
        })
    });

    group.bench_function("sqlite_disk", |b| {
        let (sqlite, _dir) = setup_sqlite_disk();
        b.iter(|| {
            black_box(
                sqlite.execute(
                    "UPDATE products SET name = 'Updated' WHERE category = 'cat3'", [],
                ).unwrap(),
            )
        })
    });

    group.finish();
}

fn bench_delete(c: &mut Criterion) {
    let mut group = c.benchmark_group("delete");
    group.sample_size(10);

    group.bench_function("sekejap_memory", |b| {
        b.iter_batched(
            setup_core,
            |mut db| {
                black_box(
                    db.execute("DELETE FROM products WHERE category = 'cat9'")
                        .unwrap(),
                )
            },
            criterion::BatchSize::LargeInput,
        )
    });

    group.bench_function("sqlite_memory", |b| {
        b.iter_batched(
            setup_sqlite,
            |conn| {
                black_box(
                    conn.execute("DELETE FROM products WHERE category = 'cat9'", [])
                        .unwrap(),
                )
            },
            criterion::BatchSize::LargeInput,
        )
    });

    group.bench_function("sekejap_disk", |b| {
        b.iter_batched(
            setup_sekejap,
            |(mut db, _dir)| {
                black_box(
                    db.execute("DELETE FROM products WHERE category = 'cat9'")
                        .unwrap(),
                )
            },
            criterion::BatchSize::LargeInput,
        )
    });

    group.bench_function("sqlite_disk", |b| {
        b.iter_batched(
            setup_sqlite_disk,
            |(conn, _dir)| {
                black_box(
                    conn.execute("DELETE FROM products WHERE category = 'cat9'", [])
                        .unwrap(),
                )
            },
            criterion::BatchSize::LargeInput,
        )
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_simple_filter,
    bench_range_filter,
    bench_sort_limit,
    bench_forward_1hop,
    bench_multihop_bfs,
    bench_point_lookup,
    bench_single_insert,
    bench_bulk_insert_1k,
    bench_update,
    bench_delete,
);
criterion_main!(benches);
