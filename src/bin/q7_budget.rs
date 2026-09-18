//! Q7 BUDGET — where a popsim query's time goes, in counted work rather than
//! seconds.
//!
//!     q7_budget <popsim-dir>/e4 [--cache-bytes N] [--entity-order]
//!
//! Opens a database `popsim` already built and re-runs the cases whose per-row
//! cost grew with the population, reporting for each one: pages, the summed
//! `QueryWork` counters, buffer-pool accesses, and the wall time. The point is
//! the RATIOS -- postings per returned row says whether the candidate stream
//! is walked once or once per page, and primary reads and pool accesses per
//! returned row say whether the answer is paying a random point-get per row.
use e4_prototype::{
    collections::{
        CandidateDriver, CollectionId, Database, IndexId, PointFilter, Projection, QueryBudget,
        QueryFilter, QueryOrder, QueryRequest, ScalarFilter, ScalarValue, SortDirection, TextMatch,
    },
    spatial_math::Point,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use std::{ops::Bound, path::PathBuf, time::Instant};

type R<T> = Result<T, Box<dyn std::error::Error>>;

const PAGE: usize = 8192;
const CENTER_LON: f64 = 107.6;
const CENTER_LAT: f64 = -6.9;

struct Case {
    name: &'static str,
    filters: Vec<QueryFilter<'static>>,
    /// The order popsim asks this case in. It is part of the question: a range
    /// answered in the driving index's own order walks that range once and
    /// resumes, and the same rows in entity order do not. See popsim's
    /// deviation 8.
    order: QueryOrder<'static>,
}

fn run(db: &Database, person: CollectionId, case: &Case) -> R<()> {
    let before = db.pool_accesses()?;
    let at = Instant::now();
    let mut prepared = db.prepare_query(QueryRequest {
        collection: person,
        filters: &case.filters,
        order: case.order,
        projection: Projection::Ids,
        total_limit: None,
        driver: CandidateDriver::Auto,
    })?;
    let (mut rows, mut pages) = (0u64, 0u64);
    let (mut candidates, mut primary, mut scalar, mut text, mut spatial) = (0u64, 0u64, 0u64, 0u64, 0u64);
    loop {
        let page = prepared.next_page(PAGE, QueryBudget::unlimited(), || false)?;
        pages += 1;
        candidates += page.work.candidates;
        primary += page.work.primary_reads;
        scalar += page.work.scalar_postings;
        text += page.work.text_postings;
        spatial += page.work.spatial_postings;
        rows += page.rows.len() as u64;
        if page.done || page.rows.is_empty() {
            break;
        }
    }
    let seconds = at.elapsed().as_secs_f64();
    let accesses = db.pool_accesses()? - before;
    let per = |n: u64| n as f64 / rows.max(1) as f64;
    println!(
        "{:16} rows={:<9} pages={:<5} {:9.1} us/row | candidates/row={:7.2} \
         primary/row={:6.3} scalar/row={:7.2} text/row={:7.2} spatial/row={:7.2} \
         pool/row={:7.3}",
        case.name,
        rows,
        pages,
        seconds * 1e6 / rows.max(1) as f64,
        per(candidates),
        per(primary),
        per(scalar),
        per(text),
        per(spatial),
        per(accesses),
    );
    Ok(())
}

fn main() -> R<()> {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(args.next().ok_or("usage: q7_budget <db-dir> [--cache-bytes N]")?);
    let mut cache = 8usize << 20;
    let mut entity_order = false;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--cache-bytes" => {
                cache = args.next().ok_or("--cache-bytes needs a value")?.parse()?
            }
            "--entity-order" => entity_order = true,
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    let db = Database::open(
        &root,
        Config {
            budget_bytes: cache,
            io: IoMode::Buffered,
            sync: SyncMode::Full,
        },
    )?;
    let person = db.collection("person")?.ok_or("no `person` collection")?;
    let (mut fullname, mut born, mut addr) = (None, None, None);
    for n in 1..=8u64 {
        let Ok(info) = db.index_info(IndexId(n)) else {
            continue;
        };
        match info.name.as_str() {
            "fullname_text" => fullname = Some(info.id),
            "born_idx" => born = Some(info.id),
            "addr_point" => addr = Some(info.id),
            _ => {}
        }
    }
    let (fullname, born, addr) = (
        fullname.ok_or("no fullname_text index")?,
        born.ok_or("no born_idx index")?,
        addr.ok_or("no addr_point index")?,
    );
    println!("cache_bytes={cache} entity_order={entity_order}");
    let by_born = QueryOrder::Scalar {
        index: born,
        direction: SortDirection::Ascending,
    };
    let range = |lower: Bound<i64>, upper: Bound<i64>| {
        let map = |bound: Bound<i64>| match bound {
            Bound::Included(v) => Bound::Included(ScalarValue::I64(v)),
            Bound::Excluded(v) => Bound::Excluded(ScalarValue::I64(v)),
            Bound::Unbounded => Bound::Unbounded,
        };
        vec![QueryFilter::Scalar {
            index: born,
            predicate: ScalarFilter::Range {
                lower: map(lower),
                upper: map(upper),
            },
        }]
    };
    let cases = vec![
        Case {
            name: "count_all",
            filters: vec![],
            order: QueryOrder::EntityId,
        },
        Case {
            name: "name_fulltext",
            filters: vec![QueryFilter::Text {
                index: fullname,
                query: "sari",
                matching: TextMatch::Any,
            }],
            order: QueryOrder::EntityId,
        },
        Case {
            name: "name_two_terms",
            filters: vec![QueryFilter::Text {
                index: fullname,
                query: "sari wati",
                matching: TextMatch::Any,
            }],
            order: QueryOrder::EntityId,
        },
        // Wide enough that the answer OUTGROWS the held run (8 MiB of rank
        // keys, 149,796 rows), which is where a text walk that cannot resume
        // goes back to a pass over the posting range per run's worth of rows.
        // Ten two-syllable names out of the sixteen-syllable alphabet; each
        // matches about one document in 256.
        Case {
            name: "name_wide",
            filters: vec![QueryFilter::Text {
                index: fullname,
                query: "sari wati budi jaka mala anti kani ribu tija lasa",
                matching: TextMatch::Any,
            }],
            order: QueryOrder::EntityId,
        },
        Case {
            name: "name_and_born",
            filters: vec![
                QueryFilter::Text {
                    index: fullname,
                    query: "sari",
                    matching: TextMatch::Any,
                },
                QueryFilter::Scalar {
                    index: born,
                    predicate: ScalarFilter::Range {
                        lower: Bound::Included(ScalarValue::I64(19_800_101)),
                        upper: Bound::Excluded(ScalarValue::I64(19_900_101)),
                    },
                },
            ],
            order: QueryOrder::EntityId,
        },
        Case {
            name: "born_decade",
            filters: range(
                Bound::Included(19_900_101),
                Bound::Excluded(20_000_101),
            ),
            order: by_born,
        },
        Case {
            name: "born_ge_open",
            filters: range(Bound::Included(20_100_101), Bound::Unbounded),
            order: by_born,
        },
        Case {
            name: "born_one_year",
            filters: range(
                Bound::Included(19_870_101),
                Bound::Excluded(19_880_101),
            ),
            order: by_born,
        },
        Case {
            name: "radius_50km",
            filters: vec![QueryFilter::Point {
                index: addr,
                predicate: PointFilter::Radius {
                    center: Point::new(CENTER_LON, CENTER_LAT).expect("centre is valid"),
                    radius_metres: 50_000.0,
                },
            }],
            order: QueryOrder::EntityId,
        },
    ];
    for case in &cases {
        let case = Case {
            name: case.name,
            filters: case.filters.clone(),
            order: if entity_order { QueryOrder::EntityId } else { case.order },
        };
        run(&db, person, &case)?;
    }
    Ok(())
}
