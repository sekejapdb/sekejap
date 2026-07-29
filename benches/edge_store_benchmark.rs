//! Edge Store Benchmark — Fat vs Compact mode on real data.
//!
//! Uses the negeriku.id geodatabase (89K nodes, 89K edges, hierarchical boundary tree).
//!
//! Benchmark groups:
//!  1. open_time       — CoreDB::open / open_with_config wall time
//!  2. forward_1hop    — single forward traversal (child_of children)
//!  3. backward_1hop   — single backward traversal (child_of parent)
//!  4. bfs_3hop        — 3-hop BFS from a mid-level boundary node
//!  5. bfs_4hop        — 4-hop BFS (full depth from root)
//!  6. full_traversal  — traverse entire tree from root via hops
//!  7. link_insert     — insert 1000 new edges
//!  8. memory_stats    — report RSS / edge count for both modes

use criterion::{criterion_group, criterion_main, Criterion, black_box};
use sekejap::{CoreDB, Config, EdgeMode};
use std::path::Path;

/// Path to the negeriku.id geodatabase.
const NEGERIKU_DB: &str = concat!(
    env!("HOME"),
    "/Dev/negeriku.id/geodatabase/db"
);

/// A top-level boundary node with many children (86 children).
const NODE_MANY_CHILDREN: &str = "boundary/ID1220070";

/// A leaf node (village level, depth 4).
const NODE_LEAF: &str = "boundary/ID7201050019";

/// Root of the entire boundary tree.
const NODE_ROOT: &str = "boundary/ID";

// ── Helpers ─────────────────────────────────────────────────────────────────

fn copy_db_to_temp(mode: EdgeMode) -> (CoreDB, tempfile::TempDir) {
    let src = Path::new(NEGERIKU_DB);
    if !src.exists() {
        panic!(
            "negeriku.id geodatabase not found at {NEGERIKU_DB}. \
             This benchmark requires real data."
        );
    }

    let dir = tempfile::TempDir::new().unwrap();
    // Copy snapshot + payloads + WAL to temp dir.
    // Copy all DB files (snapshot, payloads, WAL, gin.bin, etc.)
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
        }
    }

    let config = Config { edge_mode: mode, ..Config::default() };
    let db = CoreDB::open_with_config(dir.path(), config).unwrap();
    (db, dir)
}

// ── 01 Open time ────────────────────────────────────────────────────────────

fn bench_01_open_time(c: &mut Criterion) {
    let src = Path::new(NEGERIKU_DB);
    if !src.exists() {
        eprintln!("skipping edge_store benchmark: negeriku.id DB not found");
        return;
    }

    let mut g = c.benchmark_group("01_open_time");
    g.sample_size(10); // open is expensive, fewer samples

    g.bench_function("fat", |b| b.iter_with_setup(
        || {
            let dir = tempfile::TempDir::new().unwrap();
            for entry in std::fs::read_dir(src).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_file() {
                    std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
                }
            }
            dir
        },
        |dir| {
            let config = Config { edge_mode: EdgeMode::Fat, ..Config::default() };
            let db = CoreDB::open_with_config(dir.path(), config).unwrap();
            black_box(db.edge_count())
        },
    ));

    g.bench_function("compact", |b| b.iter_with_setup(
        || {
            let dir = tempfile::TempDir::new().unwrap();
            for entry in std::fs::read_dir(src).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_file() {
                    std::fs::copy(entry.path(), dir.path().join(entry.file_name())).unwrap();
                }
            }
            dir
        },
        |dir| {
            let config = Config { edge_mode: EdgeMode::Compact, ..Config::default() };
            let db = CoreDB::open_with_config(dir.path(), config).unwrap();
            black_box(db.edge_count())
        },
    ));

    g.finish();
}

// ── 02 Forward 1-hop (get children) ────────────────────────────────────────

fn bench_02_forward_1hop(c: &mut Criterion) {
    let (fat_db, _fat_dir) = copy_db_to_temp(EdgeMode::Fat);
    let (compact_db, _compact_dir) = copy_db_to_temp(EdgeMode::Compact);

    let mut g = c.benchmark_group("02_forward_1hop");

    g.bench_function("fat_sql", |b| b.iter(|| black_box(
        fat_db.query(
            "SELECT b.* FROM MATCH (a:boundary)-[:child_of]->(b) WHERE a._key = 'ID1220070'"
        ).unwrap().count()
    )));
    g.bench_function("compact_sql", |b| b.iter(|| black_box(
        compact_db.query(
            "SELECT b.* FROM MATCH (a:boundary)-[:child_of]->(b) WHERE a._key = 'ID1220070'"
        ).unwrap().count()
    )));

    g.bench_function("fat_atomic", |b| b.iter(|| black_box(
        fat_db.one(NODE_MANY_CHILDREN).forward("child_of").count()
    )));
    g.bench_function("compact_atomic", |b| b.iter(|| black_box(
        compact_db.one(NODE_MANY_CHILDREN).forward("child_of").count()
    )));

    g.finish();
}

// ── 03 Backward 1-hop (get parent) ─────────────────────────────────────────

fn bench_03_backward_1hop(c: &mut Criterion) {
    let (fat_db, _fat_dir) = copy_db_to_temp(EdgeMode::Fat);
    let (compact_db, _compact_dir) = copy_db_to_temp(EdgeMode::Compact);

    let mut g = c.benchmark_group("03_backward_1hop");

    g.bench_function("fat_atomic", |b| b.iter(|| black_box(
        fat_db.one(NODE_LEAF).backward("child_of").count()
    )));
    g.bench_function("compact_atomic", |b| b.iter(|| black_box(
        compact_db.one(NODE_LEAF).backward("child_of").count()
    )));

    g.finish();
}

// ── 04 BFS 3-hop (district → villages and their children) ──────────────────

fn bench_04_bfs_3hop(c: &mut Criterion) {
    let (fat_db, _fat_dir) = copy_db_to_temp(EdgeMode::Fat);
    let (compact_db, _compact_dir) = copy_db_to_temp(EdgeMode::Compact);

    let mut g = c.benchmark_group("04_bfs_3hop");

    g.bench_function("fat_sql", |b| b.iter(|| black_box(
        fat_db.query(
            "SELECT b.* FROM MATCH (a:boundary)<-[:child_of*1..3]-(b) WHERE a._key = 'ID12'"
        ).unwrap().count()
    )));
    g.bench_function("compact_sql", |b| b.iter(|| black_box(
        compact_db.query(
            "SELECT b.* FROM MATCH (a:boundary)<-[:child_of*1..3]-(b) WHERE a._key = 'ID12'"
        ).unwrap().count()
    )));

    g.bench_function("fat_atomic", |b| b.iter(|| black_box(
        fat_db.one("boundary/ID12").backward("child_of").hops_typed("child_of", 3).count()
    )));
    g.bench_function("compact_atomic", |b| b.iter(|| black_box(
        compact_db.one("boundary/ID12").backward("child_of").hops_typed("child_of", 3).count()
    )));

    g.finish();
}

// ── 05 BFS 4-hop (full depth from root) ─────────────────────────────────────

fn bench_05_bfs_4hop(c: &mut Criterion) {
    let (fat_db, _fat_dir) = copy_db_to_temp(EdgeMode::Fat);
    let (compact_db, _compact_dir) = copy_db_to_temp(EdgeMode::Compact);

    let mut g = c.benchmark_group("05_bfs_4hop_from_root");
    g.sample_size(10); // full-tree BFS is expensive

    g.bench_function("fat_atomic", |b| b.iter(|| black_box(
        fat_db.one(NODE_ROOT).backward("child_of").hops_typed("child_of", 4).count()
    )));
    g.bench_function("compact_atomic", |b| b.iter(|| black_box(
        compact_db.one(NODE_ROOT).backward("child_of").hops_typed("child_of", 4).count()
    )));

    g.finish();
}

// ── 06 Link insert (1000 new edges) ─────────────────────────────────────────

fn bench_06_link_insert(c: &mut Criterion) {
    let mut g = c.benchmark_group("06_link_insert_1000");
    g.sample_size(10);

    g.bench_function("fat", |b| b.iter_with_setup(
        || copy_db_to_temp(EdgeMode::Fat),
        |(mut db, _dir)| {
            for i in 0..1000u64 {
                db.link(
                    &format!("boundary/ID{}", 3200000 + i),
                    &format!("boundary/ID{}", 3200000 + i + 1),
                    "test_edge",
                    1.0,
                );
            }
            black_box(db.edge_count())
        },
    ));

    g.bench_function("compact", |b| b.iter_with_setup(
        || copy_db_to_temp(EdgeMode::Compact),
        |(mut db, _dir)| {
            for i in 0..1000u64 {
                db.link(
                    &format!("boundary/ID{}", 3200000 + i),
                    &format!("boundary/ID{}", 3200000 + i + 1),
                    "test_edge",
                    1.0,
                );
            }
            black_box(db.edge_count())
        },
    ));

    g.finish();
}

// ── 07 Spatial + graph hybrid ───────────────────────────────────────────────

fn bench_07_spatial_graph(c: &mut Criterion) {
    let (fat_db, _fat_dir) = copy_db_to_temp(EdgeMode::Fat);
    let (compact_db, _compact_dir) = copy_db_to_temp(EdgeMode::Compact);

    let mut g = c.benchmark_group("07_spatial_then_graph");

    // Find boundaries near Jakarta center, then traverse children.
    let jakarta_lat = -6.2088;
    let jakarta_lon = 106.8456;

    g.bench_function("fat", |b| b.iter(|| black_box(
        fat_db.collection("boundary")
            .st_dwithin(jakarta_lat, jakarta_lon, 50.0)
            .backward("child_of")
            .count()
    )));
    g.bench_function("compact", |b| b.iter(|| black_box(
        compact_db.collection("boundary")
            .st_dwithin(jakarta_lat, jakarta_lon, 50.0)
            .backward("child_of")
            .count()
    )));

    g.finish();
}

// ── 08 Shortest path ────────────────────────────────────────────────────────

fn bench_08_shortest_path(c: &mut Criterion) {
    let (fat_db, _fat_dir) = copy_db_to_temp(EdgeMode::Fat);
    let (compact_db, _compact_dir) = copy_db_to_temp(EdgeMode::Compact);

    let mut g = c.benchmark_group("08_shortest_path");

    // child_of goes from child → parent, so shortest path is leaf → root.
    g.bench_function("fat", |b| b.iter(|| black_box(
        fat_db.query(
            "SELECT a._key AS start, b._key AS end, r.length AS hops \
             FROM MATCH SHORTEST (a:boundary)-[r*]->(b:boundary) \
             WHERE a._key = 'ID7201050019' AND b._key = 'ID'"
        ).unwrap().count()
    )));
    g.bench_function("compact", |b| b.iter(|| black_box(
        compact_db.query(
            "SELECT a._key AS start, b._key AS end, r.length AS hops \
             FROM MATCH SHORTEST (a:boundary)-[r*]->(b:boundary) \
             WHERE a._key = 'ID7201050019' AND b._key = 'ID'"
        ).unwrap().count()
    )));

    g.finish();
}

// ── Criterion wiring ────────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_01_open_time,
    bench_02_forward_1hop,
    bench_03_backward_1hop,
    bench_04_bfs_3hop,
    bench_05_bfs_4hop,
    bench_06_link_insert,
    bench_07_spatial_graph,
    bench_08_shortest_path,
);
criterion_main!(benches);
