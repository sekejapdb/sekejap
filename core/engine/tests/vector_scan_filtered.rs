//! The FILTERED approximate vector order over the page-order compact scan.
//!
//! An approximate order with a filter used to fall back to the per-candidate
//! path: the quantized cursor handed over every compact entry of the
//! collection one at a time and a non-driving equality answered each one with
//! its own posting descent. The scan now tests each entry's sequence against
//! the filters INDEX-SIDE, out of the membership sets those filters' postings
//! were walked into once, and skips a refused entry without decoding its int8
//! codes.
//!
//! What has to survive that, and is what these tests are:
//!
//!   * the answer. At an `ef` that covers the matching rows the shortlist
//!     holds every one of them, so the rerank is the exact filtered top-k and
//!     the query must agree with a brute force over the stored vectors --
//!     under a scalar equality and under a scalar range alike;
//!   * the fallback. A filter that cannot be answered from an index -- a text
//!     filter -- keeps the per-candidate path, and answers the same rows;
//!   * the bound. A candidate budget stops the walk inside itself, not after
//!     it has read the collection;
//!   * paging. Pages of 1, 3 and 7 concatenate into the single-page answer.
use sekejap_core::{
    Kind,
    collections::{
        CandidateDriver, CollectionId, CollectionOptions, Database, EntityId, IndexId, Projection,
        QueryBudget, QueryDriver, QueryError, QueryFilter, QueryOrder, QueryRequest, ScalarFilter, ScalarValue,
        TextMatch, VectorMetric, WorkResource,
    },
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::ops::Bound;

const ROWS: usize = 2_000;
/// Eight-lane body plus a four-lane tail, so both halves of the scoring loops
/// produce part of every answer.
const DIM: usize = 12;
const KINDS: usize = 8;
const K: usize = 10;

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

fn kind_of(i: usize) -> String {
    format!("k{}", i % KINDS)
}

fn born_of(i: usize) -> i64 {
    1_900 + (i % 100) as i64
}

/// Two disjoint vocabularies, so a text filter names a third of the corpus
/// and nothing else.
fn note_of(i: usize) -> &'static str {
    if i % 3 == 0 {
        "alpha beta"
    } else {
        "gamma delta"
    }
}

struct Fixture {
    db: Database,
    collection: CollectionId,
    kind: IndexId,
    born: IndexId,
    note: IndexId,
    quantized: IndexId,
    /// Row i's vector, in put order. Sequence i + 1.
    vectors: Vec<Vec<f32>>,
    query: Vec<f32>,
}

fn fixture(path: &std::path::Path) -> Fixture {
    let mut db = Database::create(path.join("db"), cfg()).unwrap();
    let collection = db
        .create_collection(
            "places",
            vec![
                ("embedding".into(), Kind::Vector(DIM)),
                ("kind".into(), Kind::Text),
                ("born".into(), Kind::Int),
                ("note".into(), Kind::Text),
            ],
            CollectionOptions::default(),
        )
        .unwrap();
    let mut state = 0x5EE3_1A9Bu64;
    let mut vectors = Vec::with_capacity(ROWS);
    for i in 0..ROWS {
        let embedding = random_vector(&mut state, DIM);
        db.put(
            collection,
            &format!("r{i:05}"),
            &json!({
                "embedding": embedding,
                "kind": kind_of(i),
                "born": born_of(i),
                "note": note_of(i),
            }),
        )
        .unwrap();
        vectors.push(embedding);
        if i % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();
    let kind = db.create_scalar_index(collection, "kind", "kind", false).unwrap();
    db.build_index_to_ready(kind, 256).unwrap();
    let born = db.create_scalar_index(collection, "born", "born", false).unwrap();
    db.build_index_to_ready(born, 256).unwrap();
    let note = db.create_text_index(collection, "note", "note").unwrap();
    db.build_index_to_ready(note, 256).unwrap();
    let quantized = db
        .create_quantized_vector_index(collection, "emb_ann", "embedding")
        .unwrap();
    db.build_index_to_ready(quantized, 256).unwrap();
    db.commit().unwrap();
    let query = random_vector(&mut state, DIM);
    Fixture {
        db,
        collection,
        kind,
        born,
        note,
        quantized,
        vectors,
        query,
    }
}

/// Cosine distance in the same f64 arithmetic, in the same lane order, as
/// `score_f32_pre` (src/vector_indexes.rs:215): a brute force that disagrees
/// on the last decimal is not evidence about the ranking.
fn cosine_distance(stored: &[f32], query: &[f32]) -> f64 {
    let mut dot = 0.0f64;
    let mut stored_norm = 0.0f64;
    let mut query_norm = 0.0f64;
    for (s, q) in stored.iter().zip(query) {
        let (s, q) = (f64::from(*s), f64::from(*q));
        dot += s * q;
        stored_norm += s * s;
        query_norm += q * q;
    }
    1.0 - dot / (stored_norm.sqrt() * query_norm.sqrt())
}

/// The exact filtered top-k, taken by scoring every row the predicate admits.
fn brute_force(f: &Fixture, admits: impl Fn(usize) -> bool, k: usize) -> Vec<EntityId> {
    let mut scored = f
        .vectors
        .iter()
        .enumerate()
        .filter(|(i, _)| admits(*i))
        .map(|(i, v)| (cosine_distance(v, &f.query), (i + 1) as u64))
        .collect::<Vec<_>>();
    scored.sort_by(|left, right| {
        left.0
            .total_cmp(&right.0)
            .then_with(|| left.1.cmp(&right.1))
    });
    scored
        .into_iter()
        .take(k)
        .map(|(_, sequence)| EntityId {
            collection: f.collection,
            sequence,
        })
        .collect()
}

/// Every row one approximate query returns, page by page, plus the last
/// page's `examined` diagnostic.
fn run(
    f: &Fixture,
    filters: &[QueryFilter<'_>],
    ef: usize,
    total_limit: usize,
    page_size: usize,
) -> (Vec<EntityId>, usize, Option<QueryDriver>) {
    let mut query = f
        .db
        .prepare_query(QueryRequest {
            collection: f.collection,
            filters,
            order: QueryOrder::ApproximateVector {
                index: f.quantized,
                query: &f.query,
                metric: VectorMetric::Cosine,
                ef,
            },
            projection: Projection::Ids,
            total_limit: Some(total_limit),
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut ids = Vec::new();
    let mut examined = 0usize;
    let mut driver = None;
    for _ in 0..(total_limit + 2) {
        let page = query.next_page(page_size, QueryBudget::unlimited(), || false).unwrap();
        if let Some(approximation) = &page.approximation {
            examined = approximation.examined;
        }
        driver = Some(page.driver.clone());
        ids.extend(page.rows.iter().map(|row| row.id));
        if page.done {
            break;
        }
    }
    (ids, examined, driver)
}

/// A scalar EQUALITY under the approximate order equals the brute-force
/// filtered exact top-k, and the page-order scan is what produced it.
///
/// `ef` covers the 250 rows the equality admits, so the shortlist holds every
/// one of them and the rerank over it IS the exact answer. `examined` is the
/// evidence about the path: the compact scan reads every entry of the
/// collection and reports that, where the per-candidate path reports only the
/// candidates that passed.
#[test]
fn a_scalar_equality_matches_the_brute_force_filtered_top_k() {
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(temp.path());
    let wanted = "k3";
    let matching = (0..ROWS).filter(|i| kind_of(*i) == wanted).count();
    assert_eq!(matching, ROWS / KINDS, "the corpus should hold 8 even kinds");

    let filters = [QueryFilter::Scalar {
        index: f.kind,
        predicate: ScalarFilter::Eq(ScalarValue::Text(wanted)),
    }];
    let (ids, examined, driver) = run(&f, &filters, matching + 50, K, K);
    let expected = brute_force(&f, |i| kind_of(i) == wanted, K);
    assert_eq!(ids, expected, "filtered approximate answer differs from brute force");
    assert!(
        matches!(driver, Some(QueryDriver::QuantizedVector(_))),
        "the quantized index must drive the filtered approximate order: {driver:?}"
    );
    assert_eq!(
        examined, matching,
        "the scan scores exactly the entries the filter admits; {examined} were scored"
    );
}

/// A scalar RANGE under the approximate order does the same, over a set whose
/// membership the range's own postings prove.
#[test]
fn a_scalar_range_matches_the_brute_force_filtered_top_k() {
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(temp.path());
    let (lower, upper) = (1_910i64, 1_929i64);
    let admits = |i: usize| (lower..=upper).contains(&born_of(i));
    let matching = (0..ROWS).filter(|i| admits(*i)).count();
    assert_eq!(matching, ROWS / 5, "twenty of a hundred born values");

    let filters = [QueryFilter::Scalar {
        index: f.born,
        predicate: ScalarFilter::Range {
            lower: Bound::Included(ScalarValue::I64(lower)),
            upper: Bound::Included(ScalarValue::I64(upper)),
        },
    }];
    let (ids, examined, driver) = run(&f, &filters, matching + 50, K, K);
    let expected = brute_force(&f, admits, K);
    assert_eq!(ids, expected, "filtered approximate answer differs from brute force");
    assert!(
        matches!(driver, Some(QueryDriver::QuantizedVector(_))),
        "the quantized index must drive the filtered approximate order: {driver:?}"
    );
    assert_eq!(examined, matching, "the scan scores exactly the entries the filter admits");
}

/// A TEXT filter has no index-side membership set, so it keeps the
/// per-candidate path -- and still answers the same rows.
///
/// `examined` is the evidence that it did: the per-candidate path counts the
/// candidates that passed every filter, which is the third of the corpus the
/// text names, not the whole of it.
#[test]
fn a_text_filter_keeps_the_per_candidate_path_and_the_same_rows() {
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(temp.path());
    let admits = |i: usize| note_of(i) == "alpha beta";
    let matching = (0..ROWS).filter(|i| admits(*i)).count();

    let filters = [QueryFilter::Text {
        index: f.note,
        query: "alpha",
        matching: TextMatch::All,
    }];
    let (ids, examined, driver) = run(&f, &filters, matching + 50, K, K);
    let expected = brute_force(&f, admits, K);
    assert_eq!(ids, expected, "text-filtered approximate answer differs from brute force");
    assert!(
        !matches!(driver, Some(QueryDriver::QuantizedVector(_))),
        "a text filter must stay on the per-candidate path: {driver:?}"
    );
    assert_eq!(examined, matching, "the per-candidate path scores the candidates that passed");
}

/// A candidate budget of 100 over 2,000 rows stops the filtered scan at 101
/// entries.
///
/// The number is the evidence. A scan that read the collection and reported
/// the overrun afterwards would attempt 2,000; stopping one entry past the
/// budget is what proves the charge happens inside the walk. Every entry the
/// walk steps over is a candidate whether the filters admit it or not, so the
/// count is entries READ.
#[test]
fn a_candidate_budget_stops_the_filtered_scan_inside_the_walk() {
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(temp.path());
    let mut budget = QueryBudget::unlimited();
    budget.candidates = 100;
    let filters = [QueryFilter::Scalar {
        index: f.kind,
        predicate: ScalarFilter::Eq(ScalarValue::Text("k3")),
    }];
    let mut query = f
        .db
        .prepare_query(QueryRequest {
            collection: f.collection,
            filters: &filters,
            order: QueryOrder::ApproximateVector {
                index: f.quantized,
                query: &f.query,
                metric: VectorMetric::Cosine,
                ef: 100,
            },
            projection: Projection::Ids,
            total_limit: Some(K),
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let outcome = query.next_page(K, budget, || false);
    assert!(
        matches!(
            outcome,
            Err(QueryError::BudgetExceeded {
                resource: WorkResource::Candidates,
                limit: 100,
                attempted: 101,
            })
        ),
        "filtered scan gave {outcome:?}"
    );
}

/// Pages of 1, 3 and 7 concatenate into the single-page answer, under a
/// filter the scan answers index-side and under one it does not.
#[test]
fn filtered_pages_concatenate_into_the_single_page_answer() {
    let temp = tempfile::tempdir().unwrap();
    let f = fixture(temp.path());
    let scalar = [QueryFilter::Scalar {
        index: f.kind,
        predicate: ScalarFilter::Eq(ScalarValue::Text("k5")),
    }];
    let text = [QueryFilter::Text {
        index: f.note,
        query: "gamma",
        matching: TextMatch::All,
    }];
    for (name, filters) in [("scalar", &scalar[..]), ("text", &text[..])] {
        let (whole, _, _) = run(&f, filters, 400, K, K);
        assert_eq!(whole.len(), K, "{name}: single page returned {} rows", whole.len());
        for page_size in [1usize, 3, 7] {
            let (paged, _, _) = run(&f, filters, 400, K, page_size);
            assert_eq!(
                paged, whole,
                "{name}: pages of {page_size} disagree with the single page"
            );
        }
    }
}
