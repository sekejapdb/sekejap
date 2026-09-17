//! Q3 BUDGET — where the query engine's PER-CANDIDATE microseconds go.
//!
//! Four shapes, each broken into stages that differ by exactly one thing, so
//! the difference between two rows is the cost of what was added:
//!
//!   1. a key-only full scan of 20,000 rows (`scan/full_keys`)
//!   2. a range filter driving with an `Ids` projection (`filter/range_open`)
//!   3. BM25 over one term (`text/bm25_one_term`)
//!   4. the EMPTY-result fixed cost (`filter/eq_no_match`)
//!
//! Alongside the clock: the counting allocator (allocations and bytes per
//! execution) and the pager's own access counter, because a per-row constant
//! is usually one of those three and never "the code looks slow".
//!
//!     cargo run --release --features compact-cells,sqlite-balance,\
//!         keyspace-append,slotref-split --bin q3_budget -- [rows] [iters]

use e4_prototype::{
    collections::{
        CandidateDriver, CollectionId, CollectionOptions, Database, EntityId, IndexId, Projection,
        QueryBudget, QueryFilter, QueryOrder, QueryRequest, ScalarFilter, ScalarValue, SortDirection,
        TextMatch,
    },
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
    collections::BinaryHeap,
    fs,
    ops::Bound,
    path::Path,
    time::Instant,
};

type R<T> = Result<T, Box<dyn std::error::Error>>;

// ── the counting allocator ────────────────────────────────────────────────

thread_local! {
    static TRACK: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
    static BYTES: Cell<usize> = const { Cell::new(0) };
}
struct Alloc;
// SAFETY: every method forwards to `System` unchanged; the counters are
// thread-local side effects that never touch the returned pointer.
unsafe impl GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: AllocationLayout) -> *mut u8 {
        note_alloc(l.size());
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: AllocationLayout) {
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: AllocationLayout, n: usize) -> *mut u8 {
        note_alloc(n);
        unsafe { System.realloc(p, l, n) }
    }
    unsafe fn alloc_zeroed(&self, l: AllocationLayout) -> *mut u8 {
        note_alloc(l.size());
        unsafe { System.alloc_zeroed(l) }
    }
}
thread_local! {
    static SIZES: std::cell::RefCell<std::collections::HashMap<usize, usize>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    static HIST: Cell<bool> = const { Cell::new(false) };
}
fn note_alloc(size: usize) {
    TRACK
        .try_with(|t| {
            if t.get() {
                COUNT.with(|c| c.set(c.get() + 1));
                BYTES.with(|b| b.set(b.get() + size));
                if HIST.with(Cell::get) {
                    HIST.with(|h| h.set(false));
                    SIZES.with(|m| *m.borrow_mut().entry(size).or_insert(0) += 1);
                    HIST.with(|h| h.set(true));
                }
            }
        })
        .ok();
}
#[global_allocator]
static ALLOC: Alloc = Alloc;

fn counted<T>(f: impl FnOnce() -> T) -> (T, usize, usize) {
    COUNT.with(|c| c.set(0));
    BYTES.with(|b| b.set(0));
    TRACK.with(|t| t.set(true));
    let v = f();
    TRACK.with(|t| t.set(false));
    (v, COUNT.with(|c| c.get()), BYTES.with(|b| b.get()))
}

// ── the two_ways fixture, copied field for field ──────────────────────────

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

struct Ctx {
    db: Database,
    v: CollectionId,
    cat: IndexId,
    price: IndexId,
    rating: IndexId,
    note: IndexId,
    rows: u64,
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
    db.commit()?;
    db.checkpoint()?;
    Ok(Ctx {
        db,
        v,
        cat,
        price,
        rating,
        note,
        rows,
    })
}

/// Median of `iters` timed executions, in microseconds. Same shape as
/// two_ways' `bench`.
fn time<T>(iters: usize, mut f: impl FnMut() -> T) -> f64 {
    let warm = (iters / 20).clamp(1, 1000);
    for _ in 0..warm {
        std::hint::black_box(f());
    }
    let batches = 21usize;
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

fn run_query(
    c: &Ctx,
    filters: &[QueryFilter<'_>],
    order: QueryOrder<'_>,
    projection: Projection<'_>,
    page_size: usize,
) -> (usize, u64, u64) {
    let mut prepared = c
        .db
        .prepare_query(QueryRequest {
            collection: c.v,
            filters,
            order,
            projection,
            total_limit: None,
            driver: CandidateDriver::Auto,
        })
        .unwrap();
    let mut found = 0usize;
    let mut primary = 0u64;
    let mut candidates = 0u64;
    loop {
        let page = prepared
            .next_page(page_size, QueryBudget::unlimited(), || false)
            .unwrap();
        primary += page.work.primary_reads;
        candidates += page.work.candidates;
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
    (found, primary, candidates)
}

fn prepare_only(c: &Ctx, filters: &[QueryFilter<'_>], order: QueryOrder<'_>) -> bool {
    c.db.prepare_query(QueryRequest {
        collection: c.v,
        filters,
        order,
        projection: Projection::Ids,
        total_limit: None,
        driver: CandidateDriver::Auto,
    })
    .is_ok()
}

// ── the synthetic structure stages ────────────────────────────────────────
//
// The heap entry the page actually holds, rebuilt here at the same size and
// with the same comparison, so what the clock reads is the container and not
// the query around it.

#[derive(Clone)]
enum SynthValue {
    Entity,
    #[allow(dead_code)]
    Scalar(Vec<u8>),
    #[allow(dead_code)]
    Score(u64),
}
#[derive(Clone)]
struct SynthKey {
    value: SynthValue,
    id: (u32, u64),
}
struct SynthEntry {
    key: SynthKey,
    descending: bool,
    row: Option<Box<[u8; 32]>>,
}
fn synth_cmp(a: &SynthKey, b: &SynthKey) -> std::cmp::Ordering {
    match (&a.value, &b.value) {
        (SynthValue::Entity, SynthValue::Entity) => std::cmp::Ordering::Equal,
        (SynthValue::Score(x), SynthValue::Score(y)) => x.cmp(y),
        _ => std::cmp::Ordering::Equal,
    }
    .then_with(|| a.id.cmp(&b.id))
}
impl PartialEq for SynthEntry {
    fn eq(&self, other: &Self) -> bool {
        synth_cmp(&self.key, &other.key) == std::cmp::Ordering::Equal
    }
}
impl Eq for SynthEntry {}
impl Ord for SynthEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        synth_cmp(&self.key, &other.key)
    }
}
impl PartialOrd for SynthEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
fn synth_entry(i: u64) -> SynthEntry {
    SynthEntry {
        key: SynthKey {
            value: SynthValue::Entity,
            id: (1, i),
        },
        descending: false,
        row: None,
    }
}

fn main() -> R<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rows: u64 = args.first().and_then(|a| a.parse().ok()).unwrap_or(20_000);
    let iters: usize = args.get(1).and_then(|a| a.parse().ok()).unwrap_or(100_000);
    let root = std::env::temp_dir().join(format!("q3_budget_{rows}_{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    eprint!("loading {rows} rows … ");
    let start = Instant::now();
    let c = load(&root.join("e4"), rows)?;
    eprintln!("{:.2}s", start.elapsed().as_secs_f64());

    println!("\nsizes: SynthEntry {} B", std::mem::size_of::<SynthEntry>());

    // One untimed pass so the first timed stage does not absorb first touch.
    let _ = run_query(&c, &[], QueryOrder::EntityId, Projection::Ids, PAGE);

    let no_filters: [QueryFilter<'_>; 0] = [];
    let range_open = [QueryFilter::Scalar {
        index: c.price,
        predicate: ScalarFilter::Range {
            lower: Bound::Excluded(ScalarValue::F64(400.0)),
            upper: Bound::Unbounded,
        },
    }];
    let eq_cafe = [QueryFilter::Scalar {
        index: c.cat,
        predicate: ScalarFilter::Eq(ScalarValue::Text("cafe")),
    }];
    let eq_nowhere = [QueryFilter::Scalar {
        index: c.cat,
        predicate: ScalarFilter::Eq(ScalarValue::Text("nowhere")),
    }];
    let junction = [QueryFilter::Text {
        index: c.note,
        query: "junction",
        matching: TextMatch::Any,
    }];
    let bm25_one = QueryOrder::Bm25 {
        index: c.note,
        query: "junction",
        matching: TextMatch::Any,
    };
    let by_price = QueryOrder::Scalar {
        index: c.price,
        direction: SortDirection::Ascending,
    };

    // ── shape 1: key-only full scan ───────────────────────────────────────
    let mut out: Vec<(String, f64, String)> = Vec::new();
    let prefix = {
        let mut p = vec![0x40u8];
        p.extend((c.v.0).to_be_bytes());
        p
    };
    let raw = time(iters / 200, || {
        c.db.raw_for_each(&prefix, &mut |_, _| {}).unwrap()
    });
    out.push((
        "1a raw_for_each(primary prefix)".into(),
        raw,
        format!("kernel walk, key+value Vec per row — {:.1} ns/row", raw * 1000.0 / rows as f64),
    ));
    let (n, _, _) = run_query(&c, &no_filters, QueryOrder::EntityId, Projection::Ids, PAGE);
    let full_keys = time(iters / 200, || {
        run_query(&c, &no_filters, QueryOrder::EntityId, Projection::Ids, PAGE).0
    });
    out.push((
        "1b scan/full_keys (page 8192)".into(),
        full_keys,
        format!("{n} rows — {:.1} ns/row", full_keys * 1000.0 / n as f64),
    ));
    for page in [1024usize, 2048, 4096, 8192] {
        let t = time(iters / 200, || {
            run_query(&c, &no_filters, QueryOrder::EntityId, Projection::Ids, page).0
        });
        out.push((
            format!("1c   … same, page {page}"),
            t,
            format!("{:.1} ns/row, {} pages", t * 1000.0 / n as f64, n.div_ceil(page)),
        ));
    }
    let floor = time(iters / 4, || {
        let mut p = c
            .db
            .prepare_query(QueryRequest {
                collection: c.v,
                filters: &no_filters,
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
    });
    out.push((
        "1d   prepare + one page + one row".into(),
        floor,
        "the engine's own floor, no index in the plan".into(),
    ));
    let heap_asc = time(iters / 200, || {
        let mut h: BinaryHeap<SynthEntry> = BinaryHeap::with_capacity(8193);
        for i in 0..8192u64 {
            h.push(synth_entry(i));
        }
        let mut v = h.into_vec();
        v.sort_by(|a, b| synth_cmp(&a.key, &b.key));
        v.len()
    });
    out.push((
        "1e synthetic: heap push 8192 ASCENDING + into_vec + sort".into(),
        heap_asc,
        format!("{:.1} ns/row — the worst case for a max-heap", heap_asc * 1000.0 / 8192.0),
    ));
    let vec_asc = time(iters / 200, || {
        let mut v: Vec<SynthEntry> = Vec::with_capacity(8193);
        for i in 0..8192u64 {
            v.push(synth_entry(i));
        }
        v.len()
    });
    out.push((
        "1f synthetic: Vec push 8192 ascending, no sort".into(),
        vec_asc,
        format!("{:.1} ns/row — what an id-ordered page needs", vec_asc * 1000.0 / 8192.0),
    ));
    let vec_sorted = time(iters / 200, || {
        let mut v: Vec<SynthEntry> = Vec::with_capacity(8193);
        for i in 0..8192u64 {
            v.push(synth_entry(i));
        }
        v.sort_by(|a, b| synth_cmp(&a.key, &b.key));
        v.len()
    });
    out.push((
        "1g synthetic: Vec push 8192 + sort".into(),
        vec_sorted,
        format!("{:.1} ns/row", vec_sorted * 1000.0 / 8192.0),
    ));

    // ── shape 2: range filter driving, Ids ────────────────────────────────
    let (rn, rprimary, rcand) = run_query(&c, &range_open, QueryOrder::EntityId, Projection::Ids, PAGE);
    let range_t = time(iters / 200, || {
        run_query(&c, &range_open, QueryOrder::EntityId, Projection::Ids, PAGE).0
    });
    out.push((
        "2a filter/range_open (Ids)".into(),
        range_t,
        format!(
            "{rn} rows, {rcand} candidates, {rprimary} primary reads — {:.1} ns/row",
            range_t * 1000.0 / rn as f64
        ),
    ));
    let (en, eprimary, ecand) = run_query(&c, &eq_cafe, QueryOrder::EntityId, Projection::Ids, PAGE);
    let eq_t = time(iters / 200, || {
        run_query(&c, &eq_cafe, QueryOrder::EntityId, Projection::Ids, PAGE).0
    });
    out.push((
        "2b filter/eq_indexed_many (Ids)".into(),
        eq_t,
        format!(
            "{en} rows, {ecand} candidates, {eprimary} primary reads — {:.1} ns/row (Exact walk, no winner row)",
            eq_t * 1000.0 / en as f64
        ),
    ));
    let (on, oprimary, _) = run_query(&c, &no_filters, by_price, Projection::Ids, PAGE);
    let order_t = time(iters / 200, || {
        run_query(
            &c,
            &no_filters,
            QueryOrder::Scalar {
                index: c.price,
                direction: SortDirection::Ascending,
            },
            Projection::Ids,
            PAGE,
        )
        .0
    });
    out.push((
        "2c order/by_price ascending (Ids)".into(),
        order_t,
        format!(
            "{on} rows, {oprimary} primary reads — {:.1} ns/row (Exact walk, RankValue::Scalar Vec per row)",
            order_t * 1000.0 / on as f64
        ),
    ));

    // ── shape 3: BM25 one term ────────────────────────────────────────────
    let (bn, bprimary, bcand) = run_query(&c, &junction, bm25_one, Projection::Ids, PAGE);
    let bm_t = time(iters / 400, || {
        run_query(
            &c,
            &[QueryFilter::Text {
                index: c.note,
                query: "junction",
                matching: TextMatch::Any,
            }],
            QueryOrder::Bm25 {
                index: c.note,
                query: "junction",
                matching: TextMatch::Any,
            },
            Projection::Ids,
            PAGE,
        )
        .0
    });
    out.push((
        "3a text/bm25_one_term".into(),
        bm_t,
        format!(
            "{bn} docs, {bcand} candidates, {bprimary} primary reads — {:.1} ns/doc",
            bm_t * 1000.0 / bn as f64
        ),
    ));
    let (fn_, fprimary, _) = run_query(&c, &junction, QueryOrder::EntityId, Projection::Ids, PAGE);
    let filt_t = time(iters / 400, || {
        run_query(
            &c,
            &[QueryFilter::Text {
                index: c.note,
                query: "junction",
                matching: TextMatch::Any,
            }],
            QueryOrder::EntityId,
            Projection::Ids,
            PAGE,
        )
        .0
    });
    out.push((
        "3b same term, order EntityId (no BM25 rank)".into(),
        filt_t,
        format!(
            "{fn_} docs, {fprimary} primary reads — {:.1} ns/doc",
            filt_t * 1000.0 / fn_ as f64
        ),
    ));
    let (cn, _, _) = run_query(
        &c,
        &[QueryFilter::Text {
            index: c.note,
            query: "number",
            matching: TextMatch::Any,
        }],
        QueryOrder::Bm25 {
            index: c.note,
            query: "number",
            matching: TextMatch::Any,
        },
        Projection::Ids,
        PAGE,
    );
    let bmc_t = time(iters / 1000, || {
        run_query(
            &c,
            &[QueryFilter::Text {
                index: c.note,
                query: "number",
                matching: TextMatch::Any,
            }],
            QueryOrder::Bm25 {
                index: c.note,
                query: "number",
                matching: TextMatch::Any,
            },
            Projection::Ids,
            PAGE,
        )
        .0
    });
    out.push((
        "3c text/bm25_common (every document)".into(),
        bmc_t,
        format!("{cn} docs — {:.1} ns/doc", bmc_t * 1000.0 / cn as f64),
    ));

    // ── shape 5: the projection and the non-driving field predicate ───────
    let (pn, _, _) = run_query(
        &c,
        &no_filters,
        QueryOrder::EntityId,
        Projection::Fields(&["cat"]),
        PAGE,
    );
    let proj_t = time(iters / 200, || {
        run_query(
            &c,
            &no_filters,
            QueryOrder::EntityId,
            Projection::Fields(&["cat"]),
            PAGE,
        )
        .0
    });
    out.push((
        "5a scan/full_one_col (Fields[cat])".into(),
        proj_t,
        format!("{pn} rows — {:.1} ns/row", proj_t * 1000.0 / pn as f64),
    ));
    let two_ranges = [
        QueryFilter::Scalar {
            index: c.price,
            predicate: ScalarFilter::Range {
                lower: Bound::Excluded(ScalarValue::F64(100.0)),
                upper: Bound::Unbounded,
            },
        },
        QueryFilter::Scalar {
            index: c.rating,
            predicate: ScalarFilter::Range {
                lower: Bound::Unbounded,
                upper: Bound::Excluded(ScalarValue::F64(3.0)),
            },
        },
    ];
    let and_half = [
        QueryFilter::Scalar {
            index: c.cat,
            predicate: ScalarFilter::Eq(ScalarValue::Text("cafe")),
        },
        QueryFilter::Scalar {
            index: c.rating,
            predicate: ScalarFilter::Range {
                lower: Bound::Excluded(ScalarValue::F64(3.0)),
                upper: Bound::Unbounded,
            },
        },
    ];
    let phrase = [QueryFilter::Text {
        index: c.note,
        query: "railway signal",
        matching: TextMatch::Phrase,
    }];
    let (tn, tprimary, tcand) =
        run_query(&c, &two_ranges, QueryOrder::EntityId, Projection::Ids, PAGE);
    let two_t = time(iters / 200, || {
        run_query(&c, &two_ranges, QueryOrder::EntityId, Projection::Ids, PAGE).0
    });
    out.push((
        "5b filter/two_ranges (one non-driving field predicate)".into(),
        two_t,
        format!(
            "{tn} rows, {tcand} candidates, {tprimary} primary reads — {:.1} ns/candidate",
            two_t * 1000.0 / tcand as f64
        ),
    ));

    // ── shape 4: the empty-result fixed cost ──────────────────────────────
    let prep_eq = time(iters, || prepare_only(&c, &eq_nowhere, QueryOrder::EntityId));
    out.push((
        "4a prepare_query only (one scalar index)".into(),
        prep_eq,
        "catalog + collection_info + index_info".into(),
    ));
    let prep_none = time(iters, || prepare_only(&c, &no_filters, QueryOrder::EntityId));
    out.push((
        "4b prepare_query only (no index at all)".into(),
        prep_none,
        "catalog + collection_info".into(),
    ));
    let eq_nomatch = time(iters, || {
        run_query(&c, &eq_nowhere, QueryOrder::EntityId, Projection::Ids, PAGE).0
    });
    out.push((
        "4c filter/eq_no_match (prepare + one empty page)".into(),
        eq_nomatch,
        format!("next_page setup = {:.3} µs", eq_nomatch - prep_eq),
    ));
    let prep_bm = time(iters / 4, || {
        prepare_only(
            &c,
            &no_filters,
            QueryOrder::Bm25 {
                index: c.note,
                query: "junction",
                matching: TextMatch::Any,
            },
        )
    });
    out.push((
        "4d prepare_query only (BM25 order)".into(),
        prep_bm,
        "index_info + descriptor + corpus + df per term".into(),
    ));

    println!("\n══ Q3 BUDGET ({rows} rows) ══  median µs per execution\n");
    println!("{:<52} {:>10}   {}", "stage", "µs", "what it is");
    println!("{}", "-".repeat(130));
    for (name, micros, what) in &out {
        println!("{name:<52} {micros:>10.3}   {what}");
    }

    // ── counted, not reasoned about ───────────────────────────────────────
    println!("\n── allocations and pager accesses per execution ──\n");
    let mut counts: Vec<(String, usize, usize, u64, u64)> = Vec::new();
    let mut measure = |label: &str, f: &mut dyn FnMut() -> u64| {
        // Warm once untimed so first touch is not what is counted.
        let _ = f();
        let before = c.db.pool_accesses().unwrap();
        let (candidates, n, bytes) = counted(&mut *f);
        let after = c.db.pool_accesses().unwrap();
        counts.push((label.to_owned(), n, bytes, after - before, candidates));
    };
    measure("scan/full_keys", &mut || {
        run_query(&c, &no_filters, QueryOrder::EntityId, Projection::Ids, PAGE).2
    });
    measure("filter/range_open", &mut || {
        run_query(&c, &range_open, QueryOrder::EntityId, Projection::Ids, PAGE).2
    });
    measure("filter/eq_indexed_many", &mut || {
        run_query(&c, &eq_cafe, QueryOrder::EntityId, Projection::Ids, PAGE).2
    });
    measure("text/bm25_one_term", &mut || {
        run_query(
            &c,
            &[QueryFilter::Text {
                index: c.note,
                query: "junction",
                matching: TextMatch::Any,
            }],
            QueryOrder::Bm25 {
                index: c.note,
                query: "junction",
                matching: TextMatch::Any,
            },
            Projection::Ids,
            PAGE,
        )
        .2
    });
    measure("scan/full_one_col", &mut || {
        run_query(
            &c,
            &no_filters,
            QueryOrder::EntityId,
            Projection::Fields(&["cat"]),
            PAGE,
        )
        .2
    });
    measure("filter/two_ranges", &mut || {
        run_query(&c, &two_ranges, QueryOrder::EntityId, Projection::Ids, PAGE).2
    });
    measure("filter/and_half_indexed", &mut || {
        run_query(&c, &and_half, QueryOrder::EntityId, Projection::Ids, PAGE).2
    });
    measure("text/match_phrase", &mut || {
        run_query(&c, &phrase, QueryOrder::EntityId, Projection::Ids, PAGE).2
    });
    measure("scan/full_one_col(rating)", &mut || {
        run_query(
            &c,
            &no_filters,
            QueryOrder::EntityId,
            Projection::Fields(&["rating"]),
            PAGE,
        )
        .2
    });
    let price_gt_100 = [QueryFilter::Scalar {
        index: c.price,
        predicate: ScalarFilter::Range {
            lower: Bound::Excluded(ScalarValue::F64(100.0)),
            upper: Bound::Unbounded,
        },
    }];
    let price_and_missing = [
        QueryFilter::Scalar {
            index: c.price,
            predicate: ScalarFilter::Range {
                lower: Bound::Excluded(ScalarValue::F64(100.0)),
                upper: Bound::Unbounded,
            },
        },
        QueryFilter::Scalar {
            index: c.rating,
            predicate: ScalarFilter::IsMissing,
        },
    ];
    measure("probe/price_gt_100 alone", &mut || {
        run_query(&c, &price_gt_100, QueryOrder::EntityId, Projection::Ids, PAGE).2
    });
    measure("probe/price_gt_100 + rating IS MISSING", &mut || {
        run_query(&c, &price_and_missing, QueryOrder::EntityId, Projection::Ids, PAGE).2
    });
    measure("filter/eq_no_match", &mut || {
        run_query(&c, &eq_nowhere, QueryOrder::EntityId, Projection::Ids, PAGE).2
    });
    measure("prepare_query only (one scalar index)", &mut || {
        u64::from(prepare_only(&c, &eq_nowhere, QueryOrder::EntityId))
    });
    measure("prepare_query only (no index)", &mut || {
        u64::from(prepare_only(&c, &no_filters, QueryOrder::EntityId))
    });
    println!(
        "{:<44} {:>10} {:>12} {:>14} {:>11} {:>9} {:>9}",
        "case", "allocs", "bytes", "pool accesses", "candidates", "all/cand", "acc/cand"
    );
    println!("{}", "-".repeat(116));
    for (label, n, bytes, pool, candidates) in &counts {
        let per = |v: f64| if *candidates == 0 { 0.0 } else { v / *candidates as f64 };
        println!(
            "{label:<44} {n:>10} {bytes:>12} {pool:>14} {candidates:>11} {:>9.2} {:>9.2}",
            per(*n as f64),
            per(*pool as f64)
        );
    }
    // ── where the per-candidate allocations go, by size ───────────────────
    for (label, run) in [
        ("filter/two_ranges", 0usize),
        ("filter/and_half_indexed", 1),
        ("text/match_phrase", 2),
        ("scan/full_one_col(rating)", 3),
        ("probe/price_gt_100 alone", 4),
        ("probe/price_gt_100 + rating IS MISSING", 5),
    ] {
        SIZES.with(|m| m.borrow_mut().clear());
        HIST.with(|h| h.set(true));
        let (_, n, _) = counted(|| match run {
            0 => run_query(&c, &two_ranges, QueryOrder::EntityId, Projection::Ids, PAGE).2,
            1 => run_query(&c, &and_half, QueryOrder::EntityId, Projection::Ids, PAGE).2,
            2 => run_query(&c, &phrase, QueryOrder::EntityId, Projection::Ids, PAGE).2,
            4 => run_query(&c, &price_gt_100, QueryOrder::EntityId, Projection::Ids, PAGE).2,
            5 => run_query(&c, &price_and_missing, QueryOrder::EntityId, Projection::Ids, PAGE).2,
            _ => run_query(
                &c,
                &no_filters,
                QueryOrder::EntityId,
                Projection::Fields(&["rating"]),
                PAGE,
            )
            .2,
        });
        HIST.with(|h| h.set(false));
        let mut v: Vec<(usize, usize)> =
            SIZES.with(|m| m.borrow().iter().map(|(k, c)| (*k, *c)).collect());
        v.sort_by(|a, b| b.1.cmp(&a.1));
        v.truncate(10);
        println!("\n{label}: {n} allocations, top sizes");
        for (size, count) in v {
            println!("    {size:>8} B  x {count}");
        }
    }
    println!();
    let _ = fs::remove_dir_all(&root);
    let _ = EntityId {
        collection: c.v,
        sequence: 1,
    };
    Ok(())
}
