//! G2 BUDGET -- where writing one relationship spends its microseconds.
//!
//! The fixture is the multimodel bench's graph shape, scaled down: `rows`
//! people plus 100 organizations loaded first and committed, then a run of
//! `put_edge` in the bench's exact pattern -- person `i` to `(i+1)%rows`, to
//! `(i+7)%rows`, and to organization `i%100+1` -- committed every 256 edges.
//! Loading the entities in a separate, earlier pass is what the bench does and
//! it is not incidental: it decides how many endpoints are still on the
//! handle's fresh-identity map by the time the edges are written.
//!
//!     cargo run --release --features compact-cells,sqlite-balance,\
//!         keyspace-append,slotref-split --bin g2_budget -- [rows] [edges]

use sekejap_core::{
    collections::{
        CollectionId, CollectionOptions, Database, EdgeBudget, EdgeTypeId, EntityId,
        GraphContextId,
    },
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::{env, fs, path::Path, time::Instant};

type R<T> = Result<T, Box<dyn std::error::Error>>;

/// Entities per fixture commit. Edges commit every `EDGE_BATCH`, which is the
/// cadence the 1M multimodel run actually recorded: 3,000,000 relationships
/// over 3,907 commits.
const BATCH: u64 = 256;
const EDGE_BATCH: u64 = 768;
const CACHE_BYTES: usize = 8 << 20;

fn person_key(i: u64) -> String {
    format!("person-{i:08}")
}
fn org_key(i: u64) -> String {
    format!("org-{i:03}")
}
fn document(i: u64) -> serde_json::Value {
    json!({
        "name": format!("Person {i}"),
        "city": format!("city-{}", i % 512),
        "score": (i % 1000) as f64 / 10.0,
        "note": format!("record {i} of the synthetic population"),
    })
}

struct Fixture {
    db: Database,
    people: CollectionId,
    organizations: CollectionId,
    knows: EdgeTypeId,
    member: EdgeTypeId,
}

fn load(root: &Path, rows: u64) -> R<Fixture> {
    let _ = fs::remove_dir_all(root);
    fs::create_dir_all(root.parent().unwrap_or(root))?;
    let mut db = Database::create(
        root,
        Config {
            budget_bytes: CACHE_BYTES,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )?;
    let people = db.create_collection(
        "people",
        vec![
            ("name".into(), Kind::Text),
            ("city".into(), Kind::Text),
            ("score".into(), Kind::Real),
            ("note".into(), Kind::Text),
        ],
        CollectionOptions::default(),
    )?;
    let organizations = db.create_collection(
        "organizations",
        vec![("name".into(), Kind::Text)],
        CollectionOptions::default(),
    )?;
    db.enable_graph()?;
    let knows = db.create_edge_type("knows")?;
    let member = db.create_edge_type("member_of")?;
    db.commit()?;
    for i in 0..100 {
        db.put(organizations, &org_key(i), &json!({"name": org_key(i)}))?;
    }
    for i in 0..rows {
        db.put(people, &person_key(i), &document(i))?;
        if (i + 1) % BATCH == 0 {
            db.commit()?;
        }
    }
    db.commit()?;
    db.checkpoint()?;
    Ok(Fixture {
        db,
        people,
        organizations,
        knows,
        member,
    })
}

fn main() -> R<()> {
    let mut args = env::args().skip(1);
    let rows: u64 = args.next().map_or(Ok(200_000), |a| a.parse())?;
    let edges: u64 = args.next().map_or(Ok(99_840), |a| a.parse())?;
    let root = Path::new("<scratch>");
    let build = Instant::now();
    let mut f = load(root, rows)?;
    let fixture_s = build.elapsed().as_secs_f64();

    let people = f.people;
    let organizations = f.organizations;
    let knows = f.knows;
    let member = f.member;
    let entity = |i: u64| EntityId {
        collection: people,
        sequence: i + 1,
    };

    let mut b = EdgeBudget::default();
    let pool0 = f.db.pool_accesses()?;
    let io0 = f.db.io_counters()?;
    let start = Instant::now();
    let mut written = 0u64;
    let mut commits = 0u64;
    let mut commit_ns = 0u64;
    let mut i = 0u64;
    while written < edges {
        let source = entity(i % rows);
        for destination in [entity((i + 1) % rows), entity((i + 7) % rows)] {
            if written == edges {
                break;
            }
            f.db.put_edge_measured(
                GraphContextId::BASE,
                source,
                knows,
                destination,
                &json!({}),
                &mut b,
            )?;
            written += 1;
            if written % EDGE_BATCH == 0 {
                let at = Instant::now();
                f.db.commit()?;
                commit_ns += at.elapsed().as_nanos() as u64;
                commits += 1;
            }
        }
        if written < edges {
            f.db.put_edge_measured(
                GraphContextId::BASE,
                source,
                member,
                EntityId {
                    collection: organizations,
                    sequence: i % 100 + 1,
                },
                &json!({}),
                &mut b,
            )?;
            written += 1;
            if written % EDGE_BATCH == 0 {
                let at = Instant::now();
                f.db.commit()?;
                commit_ns += at.elapsed().as_nanos() as u64;
                commits += 1;
            }
        }
        i += 1;
    }
    let at = Instant::now();
    f.db.commit()?;
    commit_ns += at.elapsed().as_nanos() as u64;
    commits += 1;
    let wall = start.elapsed().as_secs_f64();
    let pool = f.db.pool_accesses()? - pool0;
    let io = f.db.io_counters()?.saturating_sub(io0);

    let n = b.edges.max(1) as f64;
    println!("G2 BUDGET  rows={rows} edges={written}  fixture {fixture_s:.1}s");
    println!(
        "wall {:.3}s  = {:.2} us/edge   pool accesses {:.2}/edge   WAL frames {:.4}/edge",
        wall,
        wall * 1e6 / n,
        pool as f64 / n,
        io.wal_frames_appended as f64 / n
    );
    println!(
        "commits {commits}  frames/commit {:.1}  fsyncs {}  commit wall {:.2} us/edge",
        io.wal_frames_appended as f64 / commits.max(1) as f64,
        io.fsyncs(),
        commit_ns as f64 / 1000.0 / n
    );
    println!(
        "fast paths: endpoints already known {}/{} ({:.1}%)   pair probe skipped {}/{} ({:.1}%)",
        b.fast_endpoints,
        b.edges * 2,
        b.fast_endpoints as f64 * 100.0 / (n * 2.0),
        b.fast_preflight,
        b.edges,
        b.fast_preflight as f64 * 100.0 / n
    );
    println!();
    println!("{:<14} {:>10} {:>8} {:>12}", "stage", "us/edge", "share", "pages/edge");
    let total = b.total_ns() as f64;
    for (label, ns, pages) in b.stages() {
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
        b.total_pages() as f64 / n
    );
    println!(
        "{:<14} {:>10.3}",
        "commit",
        commit_ns as f64 / 1000.0 / n
    );
    Ok(())
}
