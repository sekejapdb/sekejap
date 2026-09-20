//! What it costs to score a BM25 query over many matching documents.
//!
//! The corpus below is deliberately the worst shape for the packed posting
//! tier: ONE term, matched by 900 of 1,000 documents, whose whole posting list
//! the late build packs into a single `0x7A` segment. Scoring a document used
//! to re-seek that segment and varint-decode all 900 of its postings to read
//! one document's frequency -- O(df) work done O(df) times. The two counters
//! here are the ones that see it: the allocation counter sees one fresh
//! `Vec<(u64, u32)>` per decode, and the pool-access counter sees the
//! posting-head probe and the segment seek that the decode came with.
use e4_prototype::{
    collections::{
        CandidateDriver, CollectionId, CollectionOptions, Database, EntityId, IndexId, OrderValue,
        Projection, QueryBudget, QueryFilter, QueryOrder, QueryRequest, ScalarFilter,
        ScalarValue, TextMatch,
    },
    pagewal::PageWalStore,
    Kind,
};
use kernel::{
    io::IoMode,
    store::{Config, SyncMode},
};
use serde_json::json;
use std::{
    alloc::{GlobalAlloc, Layout as AllocationLayout, System},
    cell::Cell,
    path::Path,
};

// The counting allocator of tests/graph_collections.rs and tests/index_vector.rs.
thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
    static BYTES: Cell<usize> = const { Cell::new(0) };
}
struct Alloc;
unsafe impl GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: AllocationLayout) -> *mut u8 {
        TRACK
            .try_with(|t| {
                if t.get() {
                    COUNT.with(|n| n.set(n.get() + 1));
                    BYTES.with(|n| n.set(n.get() + l.size()));
                }
            })
            .ok();
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: AllocationLayout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: AllocationLayout, n: usize) -> *mut u8 {
        TRACK
            .try_with(|t| {
                if t.get() {
                    COUNT.with(|c| c.set(c.get() + 1));
                    BYTES.with(|b| b.set(b.get() + n));
                }
            })
            .ok();
        System.realloc(p, l, n)
    }
}
#[global_allocator]
static ALLOC: Alloc = Alloc;

fn measured<T>(f: impl FnOnce() -> T) -> (T, usize) {
    COUNT.with(|c| c.set(0));
    BYTES.with(|b| b.set(0));
    TRACK.with(|t| t.set(true));
    let value = f();
    TRACK.with(|t| t.set(false));
    (value, COUNT.with(Cell::get))
}

fn cfg() -> Config {
    Config {
        budget_bytes: 8 << 20,
        io: IoMode::Buffered,
        sync: SyncMode::Full,
    }
}

fn budget() -> QueryBudget {
    QueryBudget {
        candidates: 1 << 22,
        primary_reads: 1 << 22,
        scalar_postings: 1 << 22,
        graph_edges: 1 << 22,
        graph_visited: 1 << 22,
        spatial_postings: 1 << 22,
        text_postings: 1 << 22,
        text_tokens: 1 << 22,
        vector_locators: 1 << 22,
        vector_sidecars: 1 << 22,
        vector_lanes: 1 << 22,
        key_postings: 1 << 22,
        groups: 1 << 22,
        output_bytes: 1 << 24,
    }
}

const ROWS: u64 = 1_000;
/// Every document whose sequence is not a multiple of ten carries the term.
const MATCHING: usize = 900;
const TERM: &str = "pipeline";

fn ordered(number: u64) -> Vec<u8> {
    let bytes = number.to_be_bytes();
    let start = bytes.iter().position(|byte| *byte != 0).unwrap_or(7);
    let mut out = vec![0x80 + (8 - start) as u8];
    out.extend_from_slice(&bytes[start..]);
    out
}

fn carries(i: u64) -> bool {
    i % 10 != 0
}

fn body(i: u64) -> String {
    if !carries(i) {
        return format!("the aqueduct carried water in the year {i}");
    }
    // A varying term frequency and a varying length, so no two documents in
    // the result share a score and the order proves itself.
    let repeats = (i % 3) + 1;
    let term = std::iter::repeat_n(TERM, repeats as usize)
        .collect::<Vec<_>>()
        .join(" ");
    let filler = std::iter::repeat_n("water", (i % 7) as usize + 1)
        .collect::<Vec<_>>()
        .join(" ");
    format!("the {term} carried {filler} in the year {i}")
}

fn tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut token = String::new();
    for ch in text.chars() {
        if ch.is_ascii_alphanumeric() {
            token.push(ch.to_ascii_lowercase());
        } else if !token.is_empty() {
            out.push(std::mem::take(&mut token));
        }
    }
    if !token.is_empty() {
        out.push(token);
    }
    out
}

/// BM25 written from the published constants, not from the engine's scorer.
fn oracle(corpus: &[Vec<String>], doc: usize, term: &str) -> Option<f64> {
    let n = corpus.len() as f64;
    let average = corpus.iter().map(|t| t.len() as f64).sum::<f64>() / n;
    let length = corpus[doc].len() as f64;
    let tf = corpus[doc].iter().filter(|token| *token == term).count() as f64;
    if tf == 0.0 {
        return None;
    }
    let df = corpus
        .iter()
        .filter(|t| t.iter().any(|token| token == term))
        .count() as f64;
    let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
    Some(idf * (tf * 2.2) / (tf + 1.2 * (0.25 + 0.75 * length / average)))
}

fn tagged_rows(path: &Path, tag: u8, index: IndexId, term: Option<&str>) -> usize {
    let raw = PageWalStore::open_snapshot(path, 1 << 20).unwrap();
    let mut prefix = vec![tag];
    prefix.extend(ordered(index.0));
    if let Some(term) = term {
        prefix.extend(term.as_bytes());
        prefix.push(0);
    }
    let mut count = 0;
    for row in raw.range(&prefix).unwrap() {
        let (key, _) = row.unwrap();
        if !key.starts_with(&prefix) {
            break;
        }
        count += 1;
    }
    count
}

fn features(path: &Path) -> u64 {
    let raw = PageWalStore::open_snapshot(path, 1 << 20).unwrap();
    let header = raw.get(&[0, 0, 0]).unwrap().unwrap();
    assert_eq!(&header[..8], b"E4COLL2\0");
    u64::from_be_bytes(header[10 + 8..10 + 16].try_into().unwrap())
}

struct Fixture {
    collection: CollectionId,
    bucket: IndexId,
    text: IndexId,
    ids: Vec<EntityId>,
    corpus: Vec<Vec<String>>,
}

fn build(path: &Path) -> Fixture {
    let mut db = Database::create(path, cfg()).unwrap();
    let docs = db
        .create_collection(
            "docs",
            vec![("body".into(), Kind::Text), ("bucket".into(), Kind::Int)],
            CollectionOptions::default(),
        )
        .unwrap();
    let bucket = db
        .create_scalar_index(docs, "bucket", "bucket", false)
        .unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(bucket, 256).unwrap();
    db.commit().unwrap();

    let mut ids = Vec::new();
    for i in 0..ROWS {
        ids.push(
            db.put(
                docs,
                &format!("d{i:05}"),
                &json!({ "body": body(i), "bucket": i64::from(!carries(i)) }),
            )
            .unwrap(),
        );
        if i % 256 == 255 {
            db.commit().unwrap();
        }
    }
    db.commit().unwrap();

    // Corpus first, index afterwards: the late build takes the packed path.
    let text = db.create_text_index(docs, "body", "body").unwrap();
    db.commit().unwrap();
    db.build_index_to_ready(text, 256).unwrap();
    db.commit().unwrap();
    db.checkpoint().unwrap();
    drop(db);

    assert_eq!(
        features(path) & 0x40,
        0x40,
        "the late text build did not pack"
    );
    Fixture {
        collection: docs,
        bucket,
        text,
        ids,
        corpus: (0..ROWS).map(|i| tokens(&body(i))).collect(),
    }
}

/// `(ids in score order, score per id)` for the one-term BM25 the oracle knows.
fn expected(fixture: &Fixture) -> Vec<(EntityId, f64)> {
    let mut rows: Vec<(EntityId, f64)> = (0..ROWS)
        .filter_map(|i| {
            oracle(&fixture.corpus, i as usize, TERM).map(|score| (fixture.ids[i as usize], score))
        })
        .collect();
    rows.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.sequence.cmp(&right.0.sequence))
    });
    rows
}

fn run(
    db: &Database,
    fixture: &Fixture,
    driver: CandidateDriver,
    filters: &[QueryFilter],
    order: QueryOrder,
) -> (Vec<(EntityId, f64)>, u64, usize) {
    let mut query = db
        .prepare_query(QueryRequest {
            collection: fixture.collection,
            filters,
            order,
            projection: Projection::Ids,
            total_limit: None,
            driver,
        })
        .unwrap();
    let before = db.pool_accesses().unwrap();
    let (page, allocations) =
        measured(|| query.next_page(ROWS as usize, budget(), || false).unwrap());
    let accesses = db.pool_accesses().unwrap() - before;
    assert!(page.done, "one page was supposed to hold the whole result");
    let rows = page
        .rows
        .iter()
        .map(|row| match row.order {
            OrderValue::Bm25(score) => (row.id, score),
            _ => (row.id, 0.0),
        })
        .collect();
    (rows, accesses, allocations)
}

fn bm25(fixture: &Fixture) -> QueryOrder<'static> {
    QueryOrder::Bm25 {
        index: fixture.text,
        query: TERM,
        matching: TextMatch::Any,
    }
}

/// What the SAME driver over the SAME documents costs with and without BM25.
///
/// The unscored run pays for candidate production and for whatever the filter
/// itself re-reads; the difference is scoring and nothing else.
struct Scoring {
    rows: Vec<(EntityId, f64)>,
    accesses: u64,
    allocations: usize,
}

fn scoring_cost(
    db: &Database,
    fixture: &Fixture,
    driver: CandidateDriver,
    filters: &[QueryFilter],
) -> Scoring {
    let (plain, plain_accesses, plain_allocations) =
        run(db, fixture, driver, filters, QueryOrder::EntityId);
    let (rows, accesses, allocations) = run(db, fixture, driver, filters, bm25(fixture));
    assert_eq!(
        plain.len(),
        rows.len(),
        "the scored and unscored runs saw different documents"
    );
    Scoring {
        rows,
        accesses: accesses.saturating_sub(plain_accesses),
        allocations: allocations.saturating_sub(plain_allocations),
    }
}

#[test]
fn scoring_a_term_that_matches_most_of_the_corpus_costs_one_pass_not_one_per_document() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bm25_cost");
    let fixture = build(&path);

    let segments = tagged_rows(&path, 0x7a, fixture.text, Some(TERM));
    let norm_blocks = tagged_rows(&path, 0x7b, fixture.text, None);
    let posting_heads = tagged_rows(&path, 0x75, fixture.text, None);
    let norm_heads = tagged_rows(&path, 0x76, fixture.text, None);
    assert_eq!(
        (posting_heads, norm_heads),
        (0, 0),
        "a packed build must leave the head tiers empty"
    );
    assert_eq!(
        segments, 1,
        "the fixture is only a worst case while the term's whole posting list \
         is one segment"
    );

    let db = Database::open(&path, cfg()).unwrap();
    let wanted = expected(&fixture);
    assert_eq!(wanted.len(), MATCHING);

    // (1) Text-driven: the merge cursor produces the candidates AND is the
    //     source the scorer reads its frequencies from.
    let text_filter = [QueryFilter::Text {
        index: fixture.text,
        query: TERM,
        matching: TextMatch::Any,
    }];
    let text = scoring_cost(&db, &fixture, CandidateDriver::Auto, &text_filter);

    // (2) Scalar-driven: the same 900 documents arrive from a scalar posting
    //     list, so the scorer has to find every frequency itself.
    let scalar_filter = [QueryFilter::Scalar {
        index: fixture.bucket,
        predicate: ScalarFilter::Eq(ScalarValue::I64(0)),
    }];
    let scalar = scoring_cost(&db, &fixture, CandidateDriver::Filter(0), &scalar_filter);

    println!(
        "scoring {MATCHING} documents, {segments} segment, {norm_blocks} norm blocks\n\
         \ttext-driven:   {} pool accesses, {} allocations\n\
         \tscalar-driven: {} pool accesses, {} allocations",
        text.accesses, text.allocations, scalar.accesses, scalar.allocations
    );

    // Correctness first: both drivers, and the oracle, agree exactly.
    assert_eq!(
        text.rows.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        wanted.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        "text-driven BM25 order"
    );
    for ((id, score), (wanted_id, wanted_score)) in text.rows.iter().zip(&wanted) {
        assert_eq!(id, wanted_id);
        assert!(
            (score - wanted_score).abs() <= 1e-12,
            "text-driven BM25 score for {id:?}: {score} vs oracle {wanted_score}"
        );
    }
    assert_eq!(
        scalar.rows, text.rows,
        "the same documents scored differently behind a different driver"
    );

    // The cost of scoring, per driver.
    //
    // Pool accesses: one merged pass over the term's `segments` segment and
    // `norm_blocks` norm blocks, plus the per-page constant. NOT `MATCHING`
    // posting-head probes and `MATCHING` segment seeks, which is what a
    // per-candidate point lookup costs.
    let pages = (segments + norm_blocks) as u64;
    let accesses_ceiling = pages * 4 + 64;
    assert!(
        text.accesses <= accesses_ceiling,
        "text-driven scoring spent {} pool accesses over {MATCHING} documents, \
         budget {accesses_ceiling}",
        text.accesses
    );
    assert!(
        scalar.accesses <= accesses_ceiling,
        "scalar-driven scoring spent {} pool accesses over {MATCHING} documents, \
         budget {accesses_ceiling}",
        scalar.accesses
    );

    // Allocations: a whole-segment decode allocates a fresh vector per call,
    // so the quadratic shape shows up here as at least one allocation per
    // document per term. A single pass allocates a bounded number of buffers
    // however many documents match, so this ceiling names no multiple of
    // `MATCHING` at all.
    let allocation_ceiling = pages as usize * 4 + 64;
    assert!(
        text.allocations <= allocation_ceiling,
        "text-driven scoring made {} allocations over {MATCHING} documents, \
         budget {allocation_ceiling}",
        text.allocations
    );
    assert!(
        scalar.allocations <= allocation_ceiling,
        "scalar-driven scoring made {} allocations over {MATCHING} documents, \
         budget {allocation_ceiling}",
        scalar.allocations
    );
}

/// The same query, scored behind a driver whose candidates do NOT ascend.
///
/// A scalar RANGE cursor walks `(value, sequence)` order, so document
/// sequences arrive interleaved. The term window and the norm gap amortize
/// against ascending order; correctness must not.
#[test]
fn a_driver_that_hands_out_documents_out_of_order_still_scores_them_correctly() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bm25_unordered");
    let fixture = build(&path);
    let db = Database::open(&path, cfg()).unwrap();

    let range = [QueryFilter::Scalar {
        index: fixture.bucket,
        predicate: ScalarFilter::Range {
            lower: std::ops::Bound::Included(ScalarValue::I64(0)),
            upper: std::ops::Bound::Included(ScalarValue::I64(1)),
        },
    }];
    let order = bm25(&fixture);
    let (rows, _, _) = run(&db, &fixture, CandidateDriver::Filter(0), &range, order);
    let wanted = expected(&fixture);
    assert_eq!(
        rows, wanted,
        "BM25 behind a scalar range driver disagreed with the oracle"
    );
}

/// A BM25 page proves its winners from the norm it already read, not from one
/// primary probe each.
///
/// The winner stage exists to refuse an orphan: a posting can outlive the
/// record it names, so a key-only page went back to the primary tree once per
/// RETURNED row and threw the bytes away. A BM25 page has no orphan to refuse.
/// Every candidate it ranks went through `text_score`, which reads the
/// document's `0x76` norm first, and a deleted document's head norm is the
/// EMPTY tombstone -- it decodes as "not in the index", the score is `None`,
/// and the candidate never reaches the heap. The probe re-proves what the
/// ranking already proved.
///
/// This counts the whole page, scoring and winner stage together, against the
/// pages the answer genuinely has to touch: the term's segments, the norm
/// blocks, and a per-page constant. `primary_reads` is the exact number the
/// removal is worth -- one per returned row, which is `MATCHING`.
#[test]
fn a_bm25_page_proves_its_winners_without_one_primary_probe_each() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("bm25_winners");
    let fixture = build(&path);
    let segments = tagged_rows(&path, 0x7a, fixture.text, Some(TERM));
    let norm_blocks = tagged_rows(&path, 0x7b, fixture.text, None);
    let db = Database::open(&path, cfg()).unwrap();

    let filters = [QueryFilter::Text {
        index: fixture.text,
        query: TERM,
        matching: TextMatch::Any,
    }];
    let page = |order: QueryOrder| {
        let mut query = db
            .prepare_query(QueryRequest {
                collection: fixture.collection,
                filters: &filters,
                order,
                projection: Projection::Ids,
                total_limit: None,
                driver: CandidateDriver::Auto,
            })
            .unwrap();
        let before = db.pool_accesses().unwrap();
        let page = query.next_page(ROWS as usize, budget(), || false).unwrap();
        let accesses = db.pool_accesses().unwrap() - before;
        (page, accesses)
    };
    // Warm every cache a first execution fills, so what is counted is the
    // steady state and not first touch.
    page(bm25(&fixture));
    let (ranked, accesses) = page(bm25(&fixture));
    assert_eq!(ranked.rows.len(), MATCHING);

    // The page ranked by ENTITY ID over the same documents used to keep its
    // probe -- its candidates never passed through the scorer. Since item T3
    // the text driver stands on the same guarantee the scorer did: a term
    // posting retires in the transaction that deletes its row, so there is no
    // orphan to refuse and the id-ordered page reads no primary row either.
    let (plain, _) = page(QueryOrder::EntityId);
    assert_eq!(plain.rows.len(), MATCHING);
    assert_eq!(
        plain.work.primary_reads, 0,
        "the id-ordered text page still probes {} winners",
        plain.work.primary_reads
    );

    println!(
        "bm25 page over {MATCHING} documents: {accesses} pool accesses, \
         {} primary reads ({segments} segment, {norm_blocks} norm blocks)",
        ranked.work.primary_reads
    );
    assert_eq!(
        ranked.work.primary_reads, 0,
        "a bm25 page still read {} primary rows for {MATCHING} winners it had \
         already scored",
        ranked.work.primary_reads
    );
    // Segments, norm blocks, and a page constant: no term of this ceiling is a
    // multiple of `MATCHING`.
    let ceiling = (segments + norm_blocks) as u64 * 4 + 64;
    assert!(
        accesses <= ceiling,
        "a bm25 page over {MATCHING} documents spent {accesses} pool accesses, \
         budget {ceiling}"
    );
}
