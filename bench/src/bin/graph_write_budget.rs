//! GRAPH WRITE BUDGET -- where the battery's `--graph` stage spends its time.
//!
//! The fixture is the 50K battery's own: `bench50k/places-50000.jsonl` loaded
//! into a `place` collection with the same ten declared fields, committed
//! every 256 rows, checkpointed; then the same `related` edge set the battery
//! writes -- every row's three nearest OTHER rows by `loc`, computed with the
//! same longitude/latitude grid and the same `wgs84_distance_metres` -- put
//! one edge at a time with a commit every 256.
//!
//! DEVIATION, stated rather than hidden: the fixture builds NO field indexes.
//! Each index family is its own `tree_id` (`core/engine/src/store/mod.rs`
//! `tree_create`), the edge keyspace is the main tree, and `put_edge` reads
//! and writes nothing outside it -- so an index tree changes the data file's
//! size and not one page access of the edge write path. The number the
//! harness reports is checked against the battery's own `graph` stage.
//!
//!     cargo run --release --features compact-cells,sqlite-balance,\
//!         keyspace-append,slotref-split --bin graph_write_budget -- \
//!         <jsonl> <fixture dir> [--rows N] [--commit N] [--many] [--reopen]
//!         [--hops-only]
//!
//! `--many` writes the same edge set through `link_many` instead of one
//! `put_edge` per edge; everything else about the run is identical, so the
//! two wall times are the before and after of the same work. `--reopen` runs
//! the edges on a handle that allocated nothing, which is the shape a
//! `--reuse --graph` battery run has. `--hops-only` skips the load and the
//! edge writes and runs only the hop cases on a fixture an earlier run built.
//!
//! The run ends with the battery's `graph_2hop` and `graph_2hop_born` shapes
//! beside a THIRD, `graph_2hop_born_1day`: the same node predicate over a
//! range one day wide. The difference between the last two is the membership
//! set and nothing else.

use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use sekejap_core::{
    collections::{
        BfsRequest, CandidateDriver, CollectionId, CollectionOptions, Database, Direction,
        EdgeBudget, EdgeTypeId, EntityId, GraphContextId, IndexId, NewEdge, Projection,
        QueryBudget, QueryFilter, QueryOrder, QueryRequest, QueryWork, ScalarFilter, ScalarValue,
    },
    spatial_math::{wgs84_distance_metres, Point},
    Kind,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    env, fs,
    io::{BufRead, BufReader},
    ops::Bound,
    path::Path,
    time::Instant,
};

type R<T> = Result<T, Box<dyn std::error::Error>>;

const BATCH: usize = 256;
const CACHE_BYTES: usize = 8 << 20;
const DIM: usize = 32;
const RELATED_DEGREE: usize = 3;
const RELATED: &str = "related";
const BORN_TS: &str = "born_ts";

struct Row {
    key: String,
    name: String,
    desc: String,
    born: i64,
    kind: String,
    lon: f64,
    lat: f64,
    plot: Value,
    emb: Vec<f64>,
}

#[derive(Clone, Copy)]
struct Related {
    source: usize,
    destination: usize,
    weight: f64,
    since: i64,
}

fn read_corpus(path: &Path, limit: usize) -> R<Vec<Row>> {
    let mut out = Vec::new();
    for line in BufReader::new(fs::File::open(path)?).lines() {
        if out.len() == limit {
            break;
        }
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line)?;
        let coords = v["loc"]["coordinates"]
            .as_array()
            .ok_or("a corpus row has no loc.coordinates")?;
        let emb: Vec<f64> = v["emb"]
            .as_array()
            .ok_or("a corpus row has no emb")?
            .iter()
            .map(|n| n.as_f64().unwrap_or(0.0))
            .collect();
        if emb.len() != DIM {
            return Err(format!("a corpus row has {} embedding dimensions", emb.len()).into());
        }
        out.push(Row {
            key: v["key"].as_str().ok_or("no key")?.to_owned(),
            name: v["name"].as_str().unwrap_or_default().to_owned(),
            desc: v["desc"].as_str().unwrap_or_default().to_owned(),
            born: v["born"].as_i64().ok_or("no born")?,
            kind: v["kind"].as_str().unwrap_or_default().to_owned(),
            lon: coords[0].as_f64().ok_or("no lon")?,
            lat: coords[1].as_f64().ok_or("no lat")?,
            plot: v["plot"].clone(),
            emb,
        });
    }
    Ok(out)
}

fn born_ts_micros(born: i64) -> R<i64> {
    let (year, month, day) = (born / 10_000, (born / 100) % 100, born % 100);
    if !(1..=12).contains(&month) || !(1..=28).contains(&day) {
        return Err(format!("born {born} is not a date").into());
    }
    // Days from 1970-01-01 by the civil-from-days algorithm, then microseconds.
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Ok(days * 86_400 * 1_000_000)
}

/// The battery's `related_edges`, unchanged: every row's three nearest OTHER
/// rows by `loc`, ascending by distance, ties broken by row ordinal.
fn related_edges(rows: &[Row]) -> Vec<Related> {
    let n = rows.len();
    let (mut west, mut east, mut south, mut north) =
        (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
    for row in rows {
        west = west.min(row.lon);
        east = east.max(row.lon);
        south = south.min(row.lat);
        north = north.max(row.lat);
    }
    let span = ((east - west).max(1e-6) * (north - south).max(1e-6) / n.max(1) as f64).sqrt();
    let cell = span.max(1e-5);
    let worst_lat = south.abs().max(north.abs()).min(89.0).to_radians();
    let metres_per_cell = cell * 110_574.0_f64.min(111_320.0 * worst_lat.cos()).max(1.0);
    let cell_of = |lon: f64, lat: f64| -> (i32, i32) {
        ((lon / cell).floor() as i32, (lat / cell).floor() as i32)
    };
    let mut grid: HashMap<(i32, i32), Vec<usize>> = HashMap::with_capacity(n * 2);
    for (i, row) in rows.iter().enumerate() {
        grid.entry(cell_of(row.lon, row.lat)).or_default().push(i);
    }
    let mut out = Vec::with_capacity(n * RELATED_DEGREE);
    let mut best: Vec<(f64, usize)> = Vec::new();
    for i in 0..n {
        let row = &rows[i];
        let here = Point::new(row.lon, row.lat).expect("a corpus point is valid");
        let (cx, cy) = cell_of(row.lon, row.lat);
        best.clear();
        let mut ring = 0i32;
        loop {
            for dx in -ring..=ring {
                for dy in -ring..=ring {
                    if ring > 0 && dx.abs() != ring && dy.abs() != ring {
                        continue;
                    }
                    let Some(bucket) = grid.get(&(cx + dx, cy + dy)) else {
                        continue;
                    };
                    for j in bucket {
                        if *j == i {
                            continue;
                        }
                        let other = &rows[*j];
                        let there =
                            Point::new(other.lon, other.lat).expect("a corpus point is valid");
                        best.push((wgs84_distance_metres(here, there), *j));
                    }
                }
            }
            best.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            best.dedup_by_key(|entry| entry.1);
            best.truncate(RELATED_DEGREE);
            let covered = f64::from(ring) * metres_per_cell;
            if best.len() == RELATED_DEGREE && best[RELATED_DEGREE - 1].0 <= covered {
                break;
            }
            if f64::from(ring) * cell > (east - west) + (north - south) {
                break;
            }
            ring += 1;
        }
        for (metres, j) in &best {
            out.push(Related {
                source: i,
                destination: *j,
                weight: 1.0 / (1.0 + metres / 1_000.0),
                since: row.born,
            });
        }
    }
    out
}

fn load(root: &Path, rows: &[Row]) -> R<(Database, CollectionId)> {
    let _ = fs::remove_dir_all(root);
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut db = Database::create(
        root,
        Config {
            budget_bytes: CACHE_BYTES,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )?;
    let place = db.create_collection_declared(
        "place",
        vec![
            ("key".into(), Kind::Text),
            ("name".into(), Kind::Text),
            ("desc".into(), Kind::Text),
            ("text".into(), Kind::Text),
            ("born".into(), Kind::Int),
            (BORN_TS.into(), Kind::Int),
            ("kind".into(), Kind::Text),
            ("loc".into(), Kind::Point),
            ("plot".into(), Kind::Geo),
            ("emb".into(), Kind::Vector(DIM)),
        ],
        vec![(BORN_TS.to_owned(), "TIMESTAMPTZ".to_owned())],
        CollectionOptions::default(),
    )?;
    db.commit()?;
    for (i, row) in rows.iter().enumerate() {
        db.put(
            place,
            &row.key,
            &json!({
                "key": row.key,
                "name": row.name,
                "desc": row.desc,
                "text": format!("{} {}", row.name, row.desc),
                "born": row.born,
                BORN_TS: born_ts_micros(row.born)?,
                "kind": row.kind,
                "loc": {"type": "Point", "coordinates": [row.lon, row.lat]},
                "plot": row.plot,
                "emb": row.emb,
            }),
        )?;
        if (i + 1) % BATCH == 0 {
            db.commit()?;
        }
    }
    db.commit()?;
    db.checkpoint()?;
    Ok((db, place))
}

/// Reopen the fixture, so the edge run starts on a handle that allocated
/// nothing -- which is what a `--reuse --graph` battery run does -- or keep
/// the loading handle, which is what a fresh `--graph` run does.
fn reopen(root: &Path) -> R<Database> {
    Ok(Database::open(
        root,
        Config {
            budget_bytes: CACHE_BYTES,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )?)
}

fn main() -> R<()> {
    let mut args = env::args().skip(1);
    let jsonl = args
        .next()
        .unwrap_or_else(|| "<scratch>".into());
    let root = args
        .next()
        .unwrap_or_else(|| "<scratch>".into());
    let mut limit = usize::MAX;
    let mut commit_every = BATCH;
    let mut many = false;
    let mut fresh_handle = true;
    let mut hops_only = false;
    let rest: Vec<String> = args.collect();
    let mut it = rest.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--rows" => limit = it.next().ok_or("--rows needs a number")?.parse()?,
            "--commit" => commit_every = it.next().ok_or("--commit needs a number")?.parse()?,
            "--many" => many = true,
            "--reopen" => fresh_handle = false,
            "--hops-only" => hops_only = true,
            other => return Err(format!("unknown argument {other}").into()),
        }
    }
    let root = Path::new(&root);

    let at = Instant::now();
    let rows = read_corpus(Path::new(&jsonl), limit)?;
    eprintln!("[fixture] {} rows read in {:.1}s", rows.len(), at.elapsed().as_secs_f64());
    let at = Instant::now();
    let edges = related_edges(&rows);
    eprintln!("[fixture] {} edges computed in {:.1}s", edges.len(), at.elapsed().as_secs_f64());
    if hops_only {
        return hops_only_run(root, &rows);
    }
    let at = Instant::now();
    let (db, place) = load(root, &rows)?;
    let fixture_s = at.elapsed().as_secs_f64();
    eprintln!("[fixture] loaded in {fixture_s:.1}s");

    let mut db = if fresh_handle {
        db
    } else {
        drop(db);
        reopen(root)?
    };

    // ── the stage, exactly as `load_graph_e4` runs it ─────────────────────
    let stage = Instant::now();
    db.enable_graph()?;
    db.commit()?;
    let related = match db.edge_type(RELATED)? {
        Some(id) => id,
        None => {
            let id = db.create_edge_type(RELATED)?;
            db.commit()?;
            id
        }
    };
    let at = Instant::now();
    let ids: Vec<EntityId> = rows
        .iter()
        .map(|row| {
            db.get(place, &row.key)
                .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })
                .and_then(|r| r.map(|r| r.id).ok_or_else(|| "a corpus row is missing".into()))
        })
        .collect::<R<Vec<_>>>()?;
    let lookup_s = at.elapsed().as_secs_f64();

    let mut budget = EdgeBudget::default();
    let pool0 = db.pool_accesses()?;
    let io0 = db.io_counters()?;
    let at = Instant::now();
    let mut commits = 0u64;
    let mut commit_ns = 0u64;
    if many {
        let mut batch: Vec<NewEdge> = Vec::with_capacity(commit_every);
        for (n, edge) in edges.iter().enumerate() {
            batch.push(NewEdge {
                source: ids[edge.source],
                destination: ids[edge.destination],
                properties: json!({"weight": edge.weight, "since": edge.since}),
            });
            if (n + 1) % commit_every == 0 {
                db.link_many(GraphContextId::BASE, related, &batch)?;
                batch.clear();
                let c = Instant::now();
                db.commit()?;
                commit_ns += c.elapsed().as_nanos() as u64;
                commits += 1;
            }
        }
        if !batch.is_empty() {
            db.link_many(GraphContextId::BASE, related, &batch)?;
        }
    } else {
        for (n, edge) in edges.iter().enumerate() {
            db.put_edge_measured(
                GraphContextId::BASE,
                ids[edge.source],
                related,
                ids[edge.destination],
                &json!({"weight": edge.weight, "since": edge.since}),
                &mut budget,
            )?;
            if (n + 1) % commit_every == 0 {
                let c = Instant::now();
                db.commit()?;
                commit_ns += c.elapsed().as_nanos() as u64;
                commits += 1;
            }
        }
    }
    let c = Instant::now();
    db.commit()?;
    commit_ns += c.elapsed().as_nanos() as u64;
    commits += 1;
    let write_s = at.elapsed().as_secs_f64();
    let stage_s = stage.elapsed().as_secs_f64();
    let pool = db.pool_accesses()? - pool0;
    let io = db.io_counters()?.saturating_sub(io0);

    let n = edges.len().max(1) as f64;
    println!();
    println!(
        "GRAPH WRITE BUDGET  rows={} edges={} commit every {commit_every}  path={}  handle={}",
        rows.len(),
        edges.len(),
        if many { "link_many" } else { "put_edge" },
        if fresh_handle { "loading" } else { "reopened" },
    );
    println!("fixture load {fixture_s:.3} s");
    println!(
        "stage {stage_s:.3} s  = {:.2} us/edge   (endpoint id lookup {lookup_s:.3} s, edge writes {write_s:.3} s)",
        stage_s * 1e6 / n
    );
    println!(
        "edge writes {write_s:.3} s = {:.2} us/edge   pool accesses {:.2}/edge   WAL frames {:.4}/edge   fsyncs {}",
        write_s * 1e6 / n,
        pool as f64 / n,
        io.wal_frames_appended as f64 / n,
        io.fsyncs()
    );
    println!(
        "commits {commits}  commit wall {:.2} us/edge ({:.1}% of the edge writes)",
        commit_ns as f64 / 1000.0 / n,
        commit_ns as f64 / 10.0 / (write_s * 1e6).max(1.0)
    );
    if !many {
        println!(
            "fast paths: endpoints already known {}/{} ({:.1}%)   pair probe skipped {}/{} ({:.1}%)",
            budget.fast_endpoints,
            budget.edges * 2,
            budget.fast_endpoints as f64 * 100.0 / (n * 2.0),
            budget.fast_preflight,
            budget.edges,
            budget.fast_preflight as f64 * 100.0 / n
        );
        println!();
        println!("{:<14} {:>10} {:>8} {:>12}", "stage", "us/edge", "share", "pages/edge");
        let total = budget.total_ns() as f64;
        for (label, ns, pages) in budget.stages() {
            println!(
                "{:<14} {:>10.3} {:>7.1}% {:>12.3}",
                label,
                ns as f64 / 1000.0 / n,
                ns as f64 * 100.0 / total.max(1.0),
                pages as f64 / n
            );
        }
        println!(
            "{:<14} {:>10.3} {:>7.1}% {:>12.3}",
            "TOTAL(stages)",
            total / 1000.0 / n,
            100.0,
            budget.total_pages() as f64 / n
        );
    }
    println!("{:<14} {:>10.3}", "commit", commit_ns as f64 / 1000.0 / n);

    // ── anomaly B: the per-hop node predicate ─────────────────────────────
    let at = Instant::now();
    let born = db.create_scalar_index(place, "place_born", "born", false)?;
    db.commit()?;
    db.build_index_to_ready(born, BATCH)?;
    db.commit()?;
    eprintln!("[fixture] place_born built in {:.1}s", at.elapsed().as_secs_f64());
    // The battery seeds every graph case at the row nearest `points[i]`. The
    // seed only has to be a row with outgoing edges, and every row has three,
    // so a spread of 50 ordinals stands in for the 50 points exactly.
    report_hops(&db, place, born, related, &ids, rows.len())
}

/// Reopen a fixture an earlier run built -- rows, edges and `place_born` all
/// already there -- and run only the hop cases on it. Nothing is written, so
/// repeating it measures the same database every time.
fn hops_only_run(root: &Path, rows: &[Row]) -> R<()> {
    let db = reopen(root)?;
    let place = db
        .collection("place")?
        .ok_or("--hops-only: the fixture has no `place` collection")?;
    let born = (1..=64u64)
        .filter_map(|n| db.index_info(IndexId(n)).ok())
        .find(|info| info.name == "place_born")
        .map(|info| info.id)
        .ok_or("--hops-only: the fixture has no `place_born` index")?;
    let related = db
        .edge_type(RELATED)?
        .ok_or("--hops-only: the fixture has no `related` edge type")?;
    let ids: Vec<EntityId> = rows
        .iter()
        .map(|row| {
            db.get(place, &row.key)
                .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })
                .and_then(|r| r.map(|r| r.id).ok_or_else(|| "a corpus row is missing".into()))
        })
        .collect::<R<Vec<_>>>()?;
    report_hops(&db, place, born, related, &ids, rows.len())
}

/// The two hop cases, their medians and the work each charged.
fn report_hops(
    db: &Database,
    place: CollectionId,
    born: IndexId,
    related: EdgeTypeId,
    ids: &[EntityId],
    row_count: usize,
) -> R<()> {
    let seeds: Vec<EntityId> = (0..50).map(|i| ids[(i * (row_count / 50)) % row_count]).collect();
    println!();
    println!(
        "{:<18} {:>10} {:>11} {:>13} {:>16} {:>12} {:>14} {:>7}",
        "case", "median us", "candidates", "primary_reads", "scalar_postings", "graph_edges",
        "graph_visited", "rows"
    );
    for (label, with_born) in [
        ("graph_2hop", 0),
        ("graph_2hop_born", 1),
        ("graph_2hop_born_1day", 2),
    ] {
        // One warm pass, untimed, the way the battery warms every case.
        hop_case(db, place, born, related, &seeds, with_born)?;
        let (median, work, answered) = hop_case(db, place, born, related, &seeds, with_born)?;
        println!(
            "{label:<18} {median:>10.2} {:>11} {:>13} {:>16} {:>12} {:>14} {answered:>7}",
            work.candidates,
            work.primary_reads,
            work.scalar_postings,
            work.graph_edges,
            work.graph_visited
        );
    }
    Ok(())
}

/// The battery's `born_range(i)`, unchanged.
fn born_range(i: usize) -> (i64, i64) {
    (19_500_101, 19_500_101 + 10_000 * (i as i64 % 7))
}

/// One profiled run of the battery's `graph_2hop` / `graph_2hop_born` shape:
/// the same `BfsRequest`, the same 50 instances, the same seeds, the median
/// microseconds beside the `QueryWork` the answer charged.
fn hop_case(
    db: &Database,
    place: CollectionId,
    born: IndexId,
    related: EdgeTypeId,
    seeds: &[EntityId],
    with_born: u8,
) -> R<(f64, QueryWork, u64)> {
    let mut times = Vec::with_capacity(seeds.len());
    let mut work = QueryWork::default();
    let mut rows = 0u64;
    for (i, seed) in seeds.iter().enumerate() {
        // 1 = the battery's own range; 2 = the same predicate over a range
        // one day wide, so the walk finds almost nothing and what is left is
        // the traversal. The difference between the two IS the set build.
        let (lower, upper) = if with_born == 2 {
            (19_500_101, 19_500_101)
        } else {
            born_range(i)
        };
        let node_where = [QueryFilter::Scalar {
            index: born,
            predicate: ScalarFilter::Range {
                lower: Bound::Included(ScalarValue::I64(lower)),
                upper: Bound::Included(ScalarValue::I64(upper)),
            },
        }];
        let at = Instant::now();
        let filters = [QueryFilter::Graph(BfsRequest {
            seed: *seed,
            direction: Direction::Outgoing,
            context: GraphContextId::BASE,
            edge_type: Some(related),
            min_depth: 1,
            max_depth: 2,
            include_seed: false,
            max_visited: 1 << 16,
            max_edges: 1 << 18,
            result_limit: 1 << 16,
            edge_where: &[],
            node_where: if with_born > 0 { &node_where } else { &[] },
        })];
        let mut prepared = db.prepare_query(QueryRequest {
            collection: place,
            filters: &filters,
            order: QueryOrder::Driver,
            projection: Projection::Ids,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })?;
        loop {
            let page = prepared.next_page(8192, QueryBudget::unlimited(), || false)?;
            rows += page.rows.len() as u64;
            work.candidates += page.work.candidates;
            work.primary_reads += page.work.primary_reads;
            work.scalar_postings += page.work.scalar_postings;
            work.graph_edges += page.work.graph_edges;
            work.graph_visited += page.work.graph_visited;
            if page.done || page.rows.is_empty() {
                break;
            }
        }
        times.push(at.elapsed().as_secs_f64() * 1e6);
    }
    times.sort_by(f64::total_cmp);
    Ok((times[times.len() / 2], work, rows))
}
