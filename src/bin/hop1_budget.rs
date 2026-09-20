//! HOP1 BUDGET — where the 1-hop projected query spends its microseconds.
//!
//! The fixture is two_ways' 20k fixture, loaded the same way (same fields,
//! same seven indexes, same 15k edge set, all edges carrying `{}`). Every
//! stage below is the SAME database answering a slightly different question,
//! so the difference between two rows is the cost of what was added.
//!
//!     cargo run --release --features compact-cells,sqlite-balance,\
//!         keyspace-append,slotref-split --bin hop1_budget -- [rows] [iters]

use e4_prototype::{
    collections::{
        BfsRequest, CandidateDriver, CollectionId, CollectionOptions, Database, Direction,
        EdgeTypeId, EntityId, GraphContextId, IndexId, NeighborRequest, Projection, QueryBudget,
        QueryFilter, QueryOrder, QueryRequest, ScalarFilter, ScalarValue,
    },
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::{
    fs,
    path::Path,
    time::Instant,
};

type R<T> = Result<T, Box<dyn std::error::Error>>;

const CATS: [&str; 8] = [
    "cafe", "bar", "gym", "clinic", "school", "museum", "park", "hotel",
];
const AREAS: [&str; 6] = ["north", "south", "east", "west", "central", "riverside"];
const WORDS: [&str; 10] = [
    "railway", "signal", "platform", "junction", "siding", "tunnel", "viaduct", "depot",
    "carriage", "timetable",
];
const BATCH: u64 = 256;
const CACHE_BYTES: usize = 8 << 20;
const PAGE: usize = 8192;
const SEED: u64 = 777;

fn word(i: u64) -> &'static str {
    WORDS[(i % 10) as usize]
}
fn key(i: u64) -> String {
    format!("k{i:08}")
}
fn rating(i: u64) -> f64 {
    (10 + i % 40) as f64 / 10.0
}
fn price(i: u64) -> f64 {
    10.0 + (i % 49) as f64 * 10.0
}
fn note(i: u64) -> String {
    format!("{} {} number {}", word(i), word(i / 7), i)
}
fn longitude(i: u64) -> f64 {
    (i % 360) as f64 * 0.01
}
fn latitude(i: u64) -> f64 {
    (i % 170) as f64 * 0.01
}
fn embedding(i: u64) -> [f32; 3] {
    [
        (i % 100) as f32 / 100.0,
        (i % 37) as f32 / 37.0,
        (i % 11) as f32 / 11.0,
    ]
}
/// two_ways' edge shape, copied exactly.
fn edge_destination(i: u64, rows: u64) -> Option<u64> {
    if i % 4 == 0 {
        return None;
    }
    let destination = i * 7 % rows + 1;
    (destination != i).then_some(destination)
}

struct Ctx {
    db: Database,
    v: CollectionId,
    cat: IndexId,
    rating: IndexId,
    near: EdgeTypeId,
    rows: u64,
}

impl Ctx {
    fn entity(&self, i: u64) -> EntityId {
        EntityId {
            collection: self.v,
            sequence: i,
        }
    }
}

fn load(root: &Path, rows: u64) -> R<Ctx> {
    fs::create_dir_all(root.parent().unwrap_or(root))?;
    let mut db = Database::create(
        root,
        Config {
            budget_bytes: CACHE_BYTES,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )?;
    let v = db.create_collection(
        "v",
        vec![
            ("cat".into(), Kind::Text),
            ("area".into(), Kind::Text),
            ("rating".into(), Kind::Real),
            ("price".into(), Kind::Real),
            ("note".into(), Kind::Text),
            ("loc".into(), Kind::Point),
            ("emb".into(), Kind::Vector(3)),
        ],
        CollectionOptions::default(),
    )?;
    db.enable_graph()?;
    let near = db.create_edge_type("near")?;
    db.commit()?;
    for i in 1..=rows {
        db.put(
            v,
            &key(i),
            &json!({
                "cat": CATS[(i % 8) as usize],
                "area": AREAS[(i % 6) as usize],
                "rating": rating(i),
                "price": price(i),
                "note": note(i),
                "loc": {"type": "Point", "coordinates": [longitude(i), latitude(i)]},
                "emb": embedding(i),
            }),
        )?;
        if i % BATCH == 0 {
            db.commit()?;
        }
    }
    db.commit()?;
    let mut edges = 0u64;
    for i in 1..=rows {
        if let Some(destination) = edge_destination(i, rows) {
            db.put_edge(
                GraphContextId::BASE,
                EntityId {
                    collection: v,
                    sequence: i,
                },
                near,
                EntityId {
                    collection: v,
                    sequence: destination,
                },
                &json!({}),
            )?;
            edges += 1;
            if edges % BATCH == 0 {
                db.commit()?;
            }
        }
    }
    db.commit()?;
    let cat = db.create_scalar_index(v, "cat_idx", "cat", false)?;
    db.build_index_to_ready(cat, BATCH as usize)?;
    let area = db.create_scalar_index(v, "area_idx", "area", false)?;
    db.build_index_to_ready(area, BATCH as usize)?;
    let rating = db.create_scalar_index(v, "rating_idx", "rating", false)?;
    db.build_index_to_ready(rating, BATCH as usize)?;
    let price = db.create_scalar_index(v, "price_idx", "price", false)?;
    db.build_index_to_ready(price, BATCH as usize)?;
    let note = db.create_text_index(v, "note_text", "note")?;
    db.build_index_to_ready(note, BATCH as usize)?;
    let loc = db.create_point_index(v, "loc_point", "loc")?;
    db.build_index_to_ready(loc, BATCH as usize)?;
    let emb = db.create_exact_vector_index(v, "emb_exact", "emb")?;
    db.build_index_to_ready(emb, BATCH as usize)?;
    db.commit()?;
    db.checkpoint()?;
    Ok(Ctx {
        db,
        v,
        cat,
        rating,
        near,
        rows,
    })
}

fn bfs(c: &Ctx, seed: u64, max_depth: usize) -> BfsRequest<'static> {
    let ceiling = c.rows as usize + 1;
    BfsRequest {
        seed: c.entity(seed),
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(c.near),
        min_depth: 1,
        max_depth,
        include_seed: false,
        max_visited: ceiling,
        max_edges: ceiling,
        result_limit: ceiling,
        edge_where: &[],
        node_where: &[],
    }
}

/// Median of `iters` timed executions, in microseconds. Same shape as
/// two_ways' `bench`: warm first, then a batch, and the median of per-batch
/// per-execution costs so one scheduler hiccup cannot set the number.
fn time<T>(iters: usize, mut f: impl FnMut() -> T) -> f64 {
    for _ in 0..1000 {
        std::hint::black_box(f());
    }
    let batches = 50usize;
    let per = (iters / batches).max(1);
    let mut samples = Vec::with_capacity(batches);
    for _ in 0..batches {
        let start = Instant::now();
        for _ in 0..per {
            std::hint::black_box(f());
        }
        samples.push(start.elapsed().as_secs_f64() / per as f64 * 1e6);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[batches / 2]
}

fn query_rows(
    c: &Ctx,
    filters: &[QueryFilter<'_>],
    projection: Projection<'_>,
) -> Result<usize, String> {
    let mut prepared = c
        .db
        .prepare_query(QueryRequest {
            collection: c.v,
            filters,
            order: QueryOrder::EntityId,
            projection,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .map_err(|e| e.to_string())?;
    let mut found = 0;
    loop {
        let page = prepared
            .next_page(PAGE, QueryBudget::unlimited(), || false)
            .map_err(|e| e.to_string())?;
        for row in &page.rows {
            for (_, value) in &row.projected {
                std::hint::black_box(value);
            }
            found += 1;
        }
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    Ok(found)
}

fn main() -> R<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rows: u64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(20_000);
    let iters: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(100_000);
    let root = std::env::temp_dir().join(format!("hop1_budget_{rows}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    eprint!("loading {rows} rows … ");
    let start = Instant::now();
    let c = load(&root.join("e4"), rows)?;
    eprintln!("{:.2}s", start.elapsed().as_secs_f64());

    // One untimed pass so the first timed stage does not absorb first touch.
    let _ = query_rows(&c, &[], Projection::Ids);

    let graph1 = [QueryFilter::Graph(bfs(&c, SEED, 1))];
    let graph2 = [QueryFilter::Graph(bfs(&c, SEED, 2))];
    let eq1 = [QueryFilter::Scalar {
        index: c.cat,
        predicate: ScalarFilter::Eq(ScalarValue::Text("cafe")),
    }];
    let eq_rating = [QueryFilter::Scalar {
        index: c.rating,
        predicate: ScalarFilter::Eq(ScalarValue::F64(rating(SEED))),
    }];

    let mut table: Vec<(&str, f64, &str)> = Vec::new();
    table.push((
        "A  neighbors(out, 1 hop)",
        time(iters, || {
            c.db.neighbors(NeighborRequest {
                entity: c.entity(SEED),
                direction: Direction::Outgoing,
                context: GraphContextId::BASE,
                edge_type: Some(c.near),
                limit: 256,
            })
            .map(|v| v.len())
            .unwrap()
        }),
        "graph adjacency only, Edge{key,properties} built",
    ));
    table.push((
        "B  traverse_bfs(depth 1)",
        time(iters, || {
            c.db.traverse_bfs(bfs(&c, SEED, 1)).map(|r| r.nodes.len()).unwrap()
        }),
        "graph_collections' own BFS, no query engine",
    ));
    table.push((
        "C  get_by_id(seed)",
        time(iters, || c.db.get_by_id(c.entity(SEED)).unwrap().is_some()),
        "one primary row read + decode (the seed probe's upper bound)",
    ));
    table.push((
        "D  prepare_query(graph, Fields) only",
        time(iters, || {
            c.db.prepare_query(QueryRequest {
                collection: c.v,
                filters: &graph1,
                order: QueryOrder::EntityId,
                projection: Projection::Fields(&["rating"]),
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .is_ok()
        }),
        "compile only; the BFS has not run yet",
    ));
    table.push((
        "E  graph1 + Ids",
        time(iters, || query_rows(&c, &graph1, Projection::Ids).unwrap()),
        "full query, graph driver, no projection",
    ));
    table.push((
        "F  graph1 + Fields([rating])  = hop1_project",
        time(iters, || {
            query_rows(&c, &graph1, Projection::Fields(&["rating"])).unwrap()
        }),
        "the case under test",
    ));
    table.push((
        "G  graph2 + Ids",
        time(iters, || query_rows(&c, &graph2, Projection::Ids).unwrap()),
        "depth 2: one more BFS level through the query engine",
    ));
    table.push((
        "H  scalar eq (cat) + Ids",
        time(iters / 40, || query_rows(&c, &eq1, Projection::Ids).unwrap()),
        "2500 rows: the non-graph baseline",
    ));
    table.push((
        "I  scalar eq (rating, 500 rows) + Ids",
        time(iters / 10, || {
            query_rows(&c, &eq_rating, Projection::Ids).unwrap()
        }),
        "narrower scalar baseline",
    ));
    table.push((
        "J  scalar eq (rating, 500 rows) + Fields",
        time(iters / 10, || {
            query_rows(&c, &eq_rating, Projection::Fields(&["rating"])).unwrap()
        }),
        "I + projection: prices projection away from the graph",
    ));
    table.push((
        "K  empty query + Ids, limit 1",
        time(iters, || {
            let mut p = c
                .db
                .prepare_query(QueryRequest {
                    collection: c.v,
                    filters: &[],
                    order: QueryOrder::EntityId,
                    projection: Projection::Ids,
                    total_limit: Some(1),
                    driver: CandidateDriver::Auto,
                })
                .unwrap();
            p.next_page(PAGE, QueryBudget::unlimited(), || false)
                .unwrap()
                .rows
                .len()
        }),
        "the query engine's own floor: prepare + one page + one row",
    ));

    // A SECOND, isolated fixture: the one question the 20k database cannot
    // answer, because every edge in it carries `{}`. Eight edges out of one
    // seed, each carrying a three-field property object, so the cost of
    // turning stored property bytes into a `serde_json::Value` is visible.
    let props_root = root.join("props");
    let mut pdb = Database::create(
        &props_root,
        Config {
            budget_bytes: CACHE_BYTES,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )?;
    let pv = pdb.create_collection(
        "v",
        vec![("name".into(), Kind::Text)],
        CollectionOptions::default(),
    )?;
    pdb.enable_graph()?;
    let pnear = pdb.create_edge_type("near")?;
    let seed = pdb.put(pv, "seed", &json!({"name": "s"}))?;
    let mut targets = Vec::new();
    for i in 0..8 {
        targets.push(pdb.put(pv, &format!("t{i}"), &json!({"name": "t"}))?);
    }
    pdb.commit()?;
    for (i, t) in targets.iter().enumerate() {
        pdb.put_edge(
            GraphContextId::BASE,
            seed,
            pnear,
            *t,
            &json!({"since": 2024 + i, "weight": 0.5, "label": "colleague"}),
        )?;
    }
    pdb.commit()?;
    pdb.checkpoint()?;
    let preq = NeighborRequest {
        entity: seed,
        direction: Direction::Outgoing,
        context: GraphContextId::BASE,
        edge_type: Some(pnear),
        limit: 256,
    };
    table.push((
        "L  neighbors, 8 edges WITH properties",
        time(iters, || pdb.neighbors(preq).map(|v| v.len()).unwrap()),
        "Edge{key, properties: Value} for each: the decode is in here",
    ));
    table.push((
        "M  neighbor_ids, the same 8 edges",
        time(iters, || pdb.neighbor_ids(preq).map(|v| v.len()).unwrap()),
        "the same walk with no property byte read",
    ));

    println!("\n══ HOP1 BUDGET ({rows} rows) ══  median µs per execution\n");
    println!("{:<44} {:>10}   {}", "stage", "µs", "what it is");
    println!("{}", "-".repeat(110));
    for (name, micros, what) in &table {
        println!("{name:<44} {micros:>10.3}   {what}");
    }
    println!();
    let get = |p: &str| table.iter().find(|r| r.0.starts_with(p)).unwrap().1;
    println!("derived:");
    println!(
        "  query-engine floor (K)                     {:>8.3} µs",
        get("K")
    );
    println!(
        "  graph filter over the floor (E - K)        {:>8.3} µs",
        get("E") - get("K")
    );
    println!(
        "  BFS done by graph_collections (B)          {:>8.3} µs",
        get("B")
    );
    println!(
        "  query engine's BFS surcharge (E - K - B)   {:>8.3} µs",
        get("E") - get("K") - get("B")
    );
    println!(
        "  projection of one field (F - E)            {:>8.3} µs",
        get("F") - get("E")
    );
    println!(
        "  prepare-only share of F (D)                {:>8.3} µs",
        get("D")
    );
    println!(
        "  properties for 8 edges (L - M)             {:>8.3} µs  ({:.3} per edge)",
        get("L") - get("M"),
        (get("L") - get("M")) / 8.0
    );
    let _ = fs::remove_dir_all(&root);
    Ok(())
}
