//! Paging and budgeting of the unfiltered page-order vector scans.
//!
//! The page-order scan takes a bounded top-k over sidecar or compact leaves
//! instead of walking candidates one at a time, and two properties of the
//! candidate walk have to survive that: a page must start strictly after the
//! previous page's last rank key, and a candidate budget must bound the walk
//! rather than describe it afterwards.
use sekejap_core::{
    Kind,
    collections::{
        CandidateDriver, CollectionOptions, Database, Projection, QueryBudget, QueryError,
        QueryOrder, QueryRequest, QueryRow, VectorMetric, WorkResource,
    },
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;

const ROWS: usize = 2_000;
/// Eight-lane body plus a four-lane tail, so the paged answers are produced
/// by both halves of the scoring loops and not only the wide one.
const DIM: usize = 12;
const EF: usize = 64;

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn lcg(state: &mut u64) -> u32 {
    *state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
    (*state >> 32) as u32
}

fn random_vector(state: &mut u64, dim: usize) -> Vec<f32> {
    (0..dim)
        .map(|_| lcg(state) as f32 / u32::MAX as f32 * 2.0 - 1.0)
        .collect()
}

struct Fixture {
    db: Database,
    collection: sekejap_core::collections::CollectionId,
    exact: sekejap_core::collections::IndexId,
    quantized: sekejap_core::collections::IndexId,
    query: Vec<f32>,
}

fn fixture(path: &std::path::Path) -> Fixture {
    let mut db = Database::create(path.join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "vectors",
            vec![
                ("embedding".into(), Kind::Vector(DIM)),
                ("label".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut state = 0x9E37_79B9_7F4Au64;
    for i in 0..ROWS {
        let embedding = random_vector(&mut state, DIM);
        db.put(
            collection,
            &format!("r{i:05}"),
            &json!({ "embedding": embedding, "label": format!("row {i}") }),
        )
        .unwrap();
        if i % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let exact = db
        .create_exact_vector_index(collection, "emb_exact", "embedding")
        .unwrap();
    db.build_index_to_ready(exact, 256).unwrap();
    let quantized = db
        .create_quantized_vector_index(collection, "emb_ann", "embedding")
        .unwrap();
    db.build_index_to_ready(quantized, 256).unwrap();
    db.commit().unwrap();
    let query = random_vector(&mut state, DIM);
    Fixture {
        db,
        collection,
        exact,
        quantized,
        query,
    }
}

fn order<'a>(f: &'a Fixture, approximate: bool) -> QueryOrder<'a> {
    if approximate {
        QueryOrder::ApproximateVector {
            index: f.quantized,
            query: &f.query,
            metric: VectorMetric::Cosine,
            ef: EF,
        }
    } else {
        QueryOrder::ExactVector {
            index: f.exact,
            query: &f.query,
            metric: VectorMetric::Cosine,
        }
    }
}

/// Every row the query hands back, page by page, plus how many pages it took.
fn drain(
    f: &Fixture,
    approximate: bool,
    projected: bool,
    total_limit: usize,
    page_size: usize,
) -> (Vec<QueryRow>, usize) {
    let fields: [&str; 1] = ["label"];
    let mut query = f
        .db
        .prepare_query(QueryRequest {
            collection: f.collection,
            filters: &[],
            order: order(f, approximate),
            projection: if projected {
                Projection::Fields(&fields)
            } else {
                Projection::Ids
            },
            total_limit: Some(total_limit),
            driver: CandidateDriver::Order,
        })
        .unwrap();
    let mut rows = Vec::new();
    let mut pages = 0usize;
    loop {
        let page = query
            .next_page(page_size, QueryBudget::unlimited(), || false)
            .unwrap();
        pages += 1;
        rows.extend(page.rows);
        if page.done {
            break;
        }
        assert!(pages <= total_limit + 2, "paging did not terminate");
    }
    (rows, pages)
}

/// Pages of 1, 3 and 7 concatenate into the single-page answer, under both
/// vector orders, with and without a projection.
///
/// The projection is not decoration: a projected page cannot hold a run, so
/// it re-walks the driver for every page and is the case where a top-k taken
/// over the whole corpus and filtered by the cursor afterwards returns four
/// of ten rows and calls the query done.
#[test]
fn pages_concatenate_into_the_single_page_answer() {
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(temp.path());

    for approximate in [false, true] {
        for projected in [false, true] {
            for total_limit in [10usize, 25] {
                let (whole, pages) = drain(&f, approximate, projected, total_limit, total_limit);
                assert_eq!(pages, 1, "one page of {total_limit} rows should suffice");
                assert_eq!(
                    whole.len(),
                    total_limit,
                    "single page returned {} of {total_limit} rows (approximate={approximate}, projected={projected})",
                    whole.len()
                );
                for page_size in [1usize, 3, 7] {
                    let (paged, pages) =
                        drain(&f, approximate, projected, total_limit, page_size);
                    assert_eq!(
                        paged.len(),
                        total_limit,
                        "pages of {page_size} returned {} of {total_limit} rows over {pages} pages (approximate={approximate}, projected={projected})",
                        paged.len()
                    );
                    assert_eq!(
                        paged, whole,
                        "pages of {page_size} disagree with the single page (approximate={approximate}, projected={projected})"
                    );
                }
            }
        }
    }
}

/// `ef` bounds the whole result set across pages, not each page.
///
/// With `ef` below the total limit the query runs out of shortlist and stops,
/// and what it returns is the prefix of the same ranking.
#[test]
fn approximate_pages_stop_at_ef() {
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(temp.path());
    let fields: [&str; 1] = ["label"];
    let mut query = f
        .db
        .prepare_query(QueryRequest {
            collection: f.collection,
            filters: &[],
            order: QueryOrder::ApproximateVector {
                index: f.quantized,
                query: &f.query,
                metric: VectorMetric::Cosine,
                ef: 5,
            },
            projection: Projection::Fields(&fields),
            total_limit: Some(20),
            driver: CandidateDriver::Order,
        })
        .unwrap();
    let mut rows = Vec::new();
    for _ in 0..10 {
        let page = query
            .next_page(2, QueryBudget::unlimited(), || false)
            .unwrap();
        rows.extend(page.rows);
        if page.done {
            break;
        }
    }
    assert_eq!(rows.len(), 5, "ef=5 bounds the result set to five rows");
    let mut ids: Vec<_> = rows.iter().map(|row| row.id).collect();
    let seen = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), seen, "a paged shortlist returned a row twice");
}

/// A candidate budget of 100 over 2,000 rows stops the scan at 101 records.
///
/// The number is the evidence. A scan that read the collection and reported
/// the overrun afterwards would attempt 2,000; stopping one record past the
/// budget is what proves the charge happens inside the walk.
#[test]
fn a_candidate_budget_stops_the_scan_inside_the_walk() {
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(temp.path());
    for approximate in [false, true] {
        let mut budget = QueryBudget::unlimited();
        budget.candidates = 100;
        let mut query = f
            .db
            .prepare_query(QueryRequest {
                collection: f.collection,
                filters: &[],
                order: order(&f, approximate),
                projection: Projection::Ids,
                total_limit: Some(10),
                driver: CandidateDriver::Order,
            })
            .unwrap();
        let outcome = query.next_page(10, budget, || false);
        assert!(
            matches!(
                outcome,
                Err(QueryError::BudgetExceeded {
                    resource: WorkResource::Candidates,
                    limit: 100,
                    attempted: 101,
                })
            ),
            "approximate={approximate} gave {outcome:?}"
        );
    }
}

/// The same for the vector-specific resources, which is what bounds the scan
/// when the candidate budget is generous.
#[test]
fn vector_resource_budgets_bound_the_scan() {
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(temp.path());

    let mut sidecars = QueryBudget::unlimited();
    sidecars.vector_sidecars = 50;
    let mut exact = f
        .db
        .prepare_query(QueryRequest {
            collection: f.collection,
            filters: &[],
            order: order(&f, false),
            projection: Projection::Ids,
            total_limit: Some(10),
            driver: CandidateDriver::Order,
        })
        .unwrap();
    assert!(matches!(
        exact.next_page(10, sidecars, || false),
        Err(QueryError::BudgetExceeded {
            resource: WorkResource::VectorSidecars,
            limit: 50,
            attempted: 51,
        })
    ));

    let mut lanes = QueryBudget::unlimited();
    lanes.vector_lanes = (40 * DIM) as u64;
    let mut approximate = f
        .db
        .prepare_query(QueryRequest {
            collection: f.collection,
            filters: &[],
            order: order(&f, true),
            projection: Projection::Ids,
            total_limit: Some(10),
            driver: CandidateDriver::Order,
        })
        .unwrap();
    assert!(matches!(
        approximate.next_page(10, lanes, || false),
        Err(QueryError::BudgetExceeded {
            resource: WorkResource::VectorLanes,
            ..
        })
    ));
}

/// Cancellation is polled on records SEEN, not on records kept, so a scan
/// cannot run to the end without asking again.
#[test]
fn a_cancelled_scan_stops() {
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(temp.path());
    for approximate in [false, true] {
        let mut query = f
            .db
            .prepare_query(QueryRequest {
                collection: f.collection,
                filters: &[],
                order: order(&f, approximate),
                projection: Projection::Ids,
                total_limit: Some(10),
                driver: CandidateDriver::Order,
            })
            .unwrap();
        let mut polls = 0usize;
        let outcome = query.next_page(10, QueryBudget::unlimited(), || {
            polls += 1;
            polls > 2
        });
        assert!(
            matches!(outcome, Err(QueryError::Cancelled)),
            "approximate={approximate} gave {outcome:?}"
        );
    }
}
